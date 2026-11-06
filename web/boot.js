// boot.js — the pre-login loading experience, shared by the dev page (web/index.html) and the
// realm page (crates/wenilla-realm/templates/play.html).
//
// Why this exists: the client cannot show its own boot. Everything a fresh tab waits on — the
// 15 MB (brotli) wasm download, instantiation, and the first update's catalog pile (~100+
// synchronous XHR chain reads in ONE rAF task, the "unresponsive tab" window) — happens before
// the first frame the app could draw. So the page owns a DOM overlay, and it does two jobs:
//
//   1. SHOW what is happening: a byte-counted wasm download bar (the counting stream is
//      re-wrapped in a Response so `WebAssembly.compileStreaming` still streams), a
//      "game data N/M" line for the prefetch, then indeterminate "Starting…" states driven by
//      the client's own `window.__wenilla_progress(stage)` calls (benilla-app/src/webprogress.rs)
//      until "ready" — the first glue screen — fades the overlay out.
//   2. SHRINK the unresponsive window: the overlay creates the page's data store
//      (datastore.js, `window.__wenilla_data`) and fills it with the boot read-set — a
//      checked-in manifest (boot-manifest.json) captured from a traced boot, plus every name a
//      synchronous read missed on earlier visits — in parallel with the wasm download. The
//      client's synchronous chain reads then answer from memory: no XHR, no round trip, no
//      byte-by-byte decode. Every miss (drift, missing manifest) degrades to a synchronous XHR
//      and is learned for next time, and a returning visitor reads the store's persistent cache
//      instead of the network: progress is measured, never scripted.
//
// Page contract: `const w = await boot(init); bind(w.memory);` — bind must stay immediately
// after (the Lua WASI shims need the module's linear memory). Query switches: `?boottrace=1`
// arms window.__wenilla_boottrace and skips the prefetch (a manifest-capture boot must show the
// true read order); `?noprefetch=1` skips the prefetch only (the A/B lever).

// The wasm travels brotli-compressed: content-length counts .br bytes while the counted stream
// yields decompressed ones. Dividing the two on a real build gives ~5.0; it drifts a little per
// build and that is cosmetic — the bar is clamped at 99% until the stream actually ends.
const BR_RATIO = 5.0;
const READY_TIMEOUT_MS = 20000;
// How long the client's reads stay at zero before a screen counts as drawn. A loader asks for its
// next file a frame or more after the last one lands, and the screen's first reads follow the
// long frame that built it.
const SETTLE_MS = 1500;

export async function boot(init, opts = {}) {
  const q = new URLSearchParams(location.search);
  const ui = createBootUi();
  // Without WebGPU the client panics creating its surface and the page stays black: say why
  // before downloading anything.
  const missing = missingWebGpu();
  if (missing) {
    ui.fail(missing);
    throw new Error(missing);
  }
  if (q.get('boottrace') === '1') window.__wenilla_boottrace = [];
  const skipPrefetch = q.get('boottrace') === '1' || q.get('noprefetch') === '1';
  installProgressHook(ui, skipPrefetch);

  // The store must exist before the client starts: its synchronous reads look for it once.
  const store = dataStore();
  const prefetch = skipPrefetch
    ? store
    : store
        .then((s) =>
          prefetchManifest(s, './boot-manifest.json', 'boot', (done, total) => ui.setData(done, total))
        )
        .catch(() => ui.setData(0, 0));
  // The world-entry set follows at once, while the client instantiates and builds its glue: the
  // network is otherwise idle then, and a player who logs straight in reaches world entry
  // seconds later.
  if (!skipPrefetch) prefetch.then(() => store).then(startWorldPrefetch, () => {});

  // Compile immediately: merely fetching a Response leaves its body backpressured until
  // someone consumes it. Instantiation must still wait for the cache warmup because the
  // game's Startup performs synchronous reads of the prefetched catalogs.
  const wasm = compileWasm(new URL('./wenilla_bg.wasm', import.meta.url), (got, total) =>
    ui.setWasmProgress(got, total)
  );
  let w;
  try {
    // Attach rejection handlers to both operations now, including while prefetch is pending.
    const [module] = await Promise.all([wasm, prefetch, store]);
    w = await init({ module_or_path: module });
  } catch (e) {
    ui.fail('The client failed to start: ' + (e && e.message ? e.message : e));
    throw e;
  }
  // init() resolved = module instantiated and the plugin graph built; the heavy catalog frame
  // is still ahead. From here the client drives the stages; the timeout is the drift backstop
  // (an old bundle without webprogress under a new page must not strand the overlay).
  ui.setStage('Starting…');
  ui.armTimeout(opts.readyTimeoutMs ?? READY_TIMEOUT_MS);
  return w;
}

// Match wasm-bindgen's non-streaming fallback for servers without the WASM MIME type.
// Choose before consuming the body, avoiding a cloned response buffering the whole module.
async function compileWasm(url, onBytes) {
  const response = await fetchWasm(url, onBytes);
  if (!response.ok) throw new Error(`Client download failed (HTTP ${response.status})`);
  if (typeof WebAssembly.compileStreaming === 'function' &&
      response.headers.get('content-type')?.trim().toLowerCase() === 'application/wasm') {
    return WebAssembly.compileStreaming(response);
  }
  return WebAssembly.compile(await response.arrayBuffer());
}

/// Byte-counting fetch of the wasm, re-wrapped so streaming compilation survives. Returns a
/// Promise<Response> consumed immediately by compileWasm, before game instantiation.
async function fetchWasm(url, onBytes) {
  const r = await fetch(url);
  if (!r.ok || !r.body) return r; // compileWasm surfaces HTTP errors / handles a missing stream
  const contentLength = Number(r.headers.get('content-length')) || 0;
  const compressed = !!(r.headers.get('content-encoding') || '').trim();
  const total = contentLength ? contentLength * (compressed ? BR_RATIO : 1) : 0;
  let got = 0;
  const reader = r.body.getReader();
  const counted = new ReadableStream({
    async pull(c) {
      const { done, value } = await reader.read();
      if (done) {
        onBytes(got, got); // the honest total, now that we know it
        c.close();
        return;
      }
      got += value.byteLength;
      onBytes(got, total);
      c.enqueue(value);
    },
    cancel(reason) {
      return reader.cancel(reason);
    },
  });
  // Headers are copied so Content-Type: application/wasm survives — compileStreaming
  // refuses anything else.
  return new Response(counted, { status: r.status, statusText: r.statusText, headers: r.headers });
}

/// Why this build cannot draw here, or `null`: a WebGPU build (`web-build.sh` stamps the backend
/// into the page) needs `navigator.gpu`, which browsers only expose to a secure context — https,
/// or `localhost` — and which a browser can still turn off for a GPU.
export function missingWebGpu() {
  const backend = globalThis.document?.querySelector?.('meta[name="wenilla-backend"]')?.content;
  if (backend === 'webgl2' || globalThis.navigator?.gpu) return null;
  if (globalThis.isSecureContext === false) {
    return (
      `WebGPU needs a secure page, and ${location.origin} is not one. Open it over https ` +
      '(see web/README.md, "Over https"), or from this machine as http://localhost. ' +
      'For testing, Chrome can allow this address: chrome://flags/#unsafely-treat-insecure-origin-as-secure.'
    );
  }
  return (
    'This browser has WebGPU turned off (or not for this GPU): see chrome://gpu, or ' +
    'chrome://flags/#enable-unsafe-webgpu. It needs Chrome or Edge, or Safari 26+.'
  );
}

/// The page's data store, created once (datastore.js). Loaded by URL so the module resolves
/// beside this one wherever the page serves it.
async function dataStore() {
  if (!window.__wenilla_data) {
    const { createStore } = await import(new URL('./datastore.js', import.meta.url).href);
    window.__wenilla_data ??= createStore();
  }
  return window.__wenilla_data;
}

/// Fill the store with a phase's read-set: the manifest's names plus the names synchronous reads
/// missed in that phase on earlier visits. Failures are counted as done and otherwise ignored — a
/// miss just means that file loads the old way. Used twice: the boot set before `init()`, the
/// world-entry set right after it.
async function prefetchManifest(store, manifestUrl, phase, onProgress) {
  let names = [];
  try {
    const r = await fetch(manifestUrl);
    if (r.ok) {
      const manifest = await r.json();
      if (Array.isArray(manifest && manifest.names)) names = manifest.names;
    }
  } catch (_) {
    /* no manifest: the learned names alone */
  }
  // The name index answers every existence question the client asks; ~12 MB of JSON that a
  // synchronous read would hold the thread for. The locale names the archives' language.
  if (phase === 'boot') names = ['__index', '__locale', ...names];
  const all = Array.from(new Set([...names, ...store.learned(phase)]));
  if (!all.length) return onProgress(0, 0);
  onProgress(0, all.length);
  // The world-entry set is ahead of what the player may do: the screen's own reads go first.
  await store.prefetch(all, { priority: phase === 'world' ? 'low' : 'high', onProgress });
}

/// The world-entry prefetch's progress, for the corner line once the glue is up.
const world = { done: 0, total: 0, finished: false, onChange: null };

function startWorldPrefetch(store) {
  prefetchManifest(store, './world-manifest.json', 'world', (done, total) => {
    world.done = done;
    world.total = total;
    world.onChange?.();
  })
    .catch(() => {})
    .finally(() => {
      world.finished = true;
      world.onChange?.();
    });
}

/// `window.__wenilla_progress(stage)` — the client's calls land here (webprogress.rs).
///
/// `ready` switches the store to learning world-entry misses, and shows the SECOND prefetch —
/// the world-entry read-set, started after the boot set — in a corner line while it is still
/// running. Entering the world used to be one frozen frame of hundreds of synchronous reads (the
/// FrameXML files and their sprites on the first UI draw, the spell and item tables). That set
/// is `world-manifest.json` (captured the same way as the boot manifest, minus the boot set) plus
/// what earlier visits learned; whatever is still in flight when the player clicks Enter World
/// just loads the old way.
function installProgressHook(ui, skipPrefetch) {
  window.__wenilla_progress = (stage) => {
    if (stage === 'startup') ui.setStage('Loading interface…');
    else if (stage === 'charselect') {
      if (!ui.gone()) return; // the boot cover is still up and already waits for these reads
      const cover = createBootUi();
      cover.setStage('Loading characters…');
      cover.armTimeout(READY_TIMEOUT_MS);
      whenSettled(window.__wenilla_data).then(() => cover.hide());
    } else if (stage === 'ready') {
      ui.setStage('Preparing the screen…');
      whenSettled(window.__wenilla_data).then(() => ui.hide());
      window.__wenilla_data?.setPhase('world');
      if (!skipPrefetch && !world.finished) {
        const corner = createCornerLine();
        world.onChange = () => {
          if (world.finished) {
            world.onChange = null;
            corner.remove();
          } else {
            corner.set(world.total > 0 ? `preparing world data — ${world.done}/${world.total}` : '');
          }
        };
        world.onChange();
      }
    }
  };
}

/// Resolves once `store.reading()` has stayed at zero for `quietMs`.
export function whenSettled(store, { quietMs = SETTLE_MS, everyMs = 100 } = {}) {
  if (!store) return Promise.resolve();
  return new Promise((resolve) => {
    let last = performance.now();
    const timer = setInterval(() => {
      const now = performance.now();
      if (store.reading() > 0) {
        last = now;
      } else if (now - last >= quietMs) {
        clearInterval(timer);
        resolve();
      }
    }, everyMs);
  });
}

/// The unobtrusive bottom-left status line the world prefetch reports on.
function createCornerLine() {
  const el = document.createElement('div');
  el.id = 'wenilla-corner';
  el.style.cssText =
    'position:fixed;left:.6rem;bottom:.5rem;z-index:15;font:12px system-ui,sans-serif;' +
    'color:#888;background:rgba(0,0,0,.55);padding:.2rem .5rem;border-radius:4px;' +
    'pointer-events:none;transition:opacity .4s ease';
  document.body.appendChild(el);
  return {
    set(text) {
      el.textContent = text;
      el.style.opacity = text ? '1' : '0';
    },
    remove() {
      el.style.opacity = '0';
      setTimeout(() => el.remove(), 500);
    },
  };
}

/// The overlay itself. Same dark theme as the pages' own chrome; no dependencies, no images.
function createBootUi() {
  const root = document.createElement('div');
  root.id = 'wenilla-boot';
  root.innerHTML = `
    <style>
      #wenilla-boot { position: fixed; inset: 0; z-index: 20; display: grid; place-items: center;
        background: #000; color: #ccc; font: 15px system-ui, sans-serif;
        transition: opacity .4s ease; }
      #wenilla-boot.gone { opacity: 0; pointer-events: none; }
      #wenilla-boot .box { width: min(420px, 80vw); }
      #wenilla-boot .stage { margin-bottom: .6rem; text-align: center; }
      #wenilla-boot .bar { height: 6px; background: #222; border-radius: 3px; overflow: hidden; }
      #wenilla-boot .bar > i { display: block; height: 100%; width: 0; background: #c9a227;
        border-radius: 3px; transition: width .15s linear; }
      #wenilla-boot .bar.pulse > i { width: 30% !important; animation: wboot-slide 1.2s ease-in-out infinite; }
      @keyframes wboot-slide { 0% { margin-left: 0 } 50% { margin-left: 70% } 100% { margin-left: 0 } }
      #wenilla-boot .data { margin-top: .5rem; text-align: center; font-size: 12px; color: #888;
        min-height: 1.2em; }
      #wenilla-boot .err { color: #e07070; text-align: center; }
    </style>
    <div class="box">
      <div class="stage">Loading…</div>
      <div class="bar"><i></i></div>
      <div class="data"></div>
    </div>`;
  document.body.appendChild(root);
  const stageEl = root.querySelector('.stage');
  const barEl = root.querySelector('.bar');
  const fillEl = root.querySelector('.bar > i');
  const dataEl = root.querySelector('.data');
  let gone = false;
  let timeout = 0;
  const mb = (n) => (n / (1024 * 1024)).toFixed(0);
  return {
    setWasmProgress(got, total) {
      if (gone) return;
      if (total > 0) {
        const pct = Math.min(99, Math.floor((got / total) * 100));
        const done = got >= total;
        stageEl.textContent = done
          ? 'Downloading client — done'
          : `Downloading client — ${pct}% (${mb(got)} MB)`;
        fillEl.style.width = (done ? 100 : pct) + '%';
      } else {
        stageEl.textContent = `Downloading client — ${mb(got)} MB`;
        barEl.classList.add('pulse');
      }
    },
    setData(done, total) {
      if (gone) return;
      dataEl.textContent = total > 0 ? `game data — ${done}/${total}` : '';
    },
    setStage(text) {
      if (gone) return;
      stageEl.textContent = text;
      barEl.classList.add('pulse');
    },
    fail(text) {
      clearTimeout(timeout);
      barEl.remove();
      dataEl.remove();
      stageEl.className = 'err';
      stageEl.textContent = text;
    },
    gone: () => gone,
    armTimeout(ms) {
      timeout = setTimeout(() => this.hide(), ms);
    },
    hide() {
      if (gone) return;
      gone = true;
      clearTimeout(timeout);
      root.classList.add('gone');
      setTimeout(() => root.remove(), 500);
    },
  };
}
