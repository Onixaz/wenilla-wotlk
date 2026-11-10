// datastore.js — the page's game-data store: every `/data/*` byte the client reads goes through it.
//
// The wasm reads chain files two ways (crates/benilla-formats/src/web.rs): ~60 call sites read
// synchronously (`Chain::read`), and the Bevy asset reader reads asynchronously. A synchronous
// read on the page is a synchronous XHR, which blocks the one thread for a round trip plus a
// byte-by-byte decode; this store lets both paths read bytes that were fetched ahead instead:
//
//   - `get(name)` (sync) answers from memory, or `undefined` and the caller falls back to its XHR;
//     the miss is remembered under the current phase (`learned(phase)`), so the next visit
//     fetches that name ahead too, at the point it is needed: `boot` before the client starts,
//     `world` while the player is at the glue screens.
//   - `load(name, {priority, keep})` (async) answers from memory, joins a fetch already in
//     flight, reads the persistent cache, or fetches. `keep` holds the bytes in memory for a
//     later reader (a fetch ahead); without it the reader takes them (memory → consumed).
//   - `prefetch(names)` is `load` with `keep` at low priority, so fetches ahead never crowd out
//     a read something is waiting on. `pin` keeps them from eviction.
//   - `fill(names, {tag})` downloads to the persistent cache alone, a few at a time and only when
//     nothing else wants the network: art a `ready` then reads off the disk, in no one's memory.
//   - `ready(names)` (sync) says whether a frame can read `names` now, and otherwise starts them:
//     the caller draws without them and asks again next frame, so a miss costs a late image and
//     not a frozen frame. A name that cannot arrive answers true, and its read fails as it would.
//   - `dropAhead()` forgets the fetches ahead of the player not yet sent: the character select's
//     tiles when the selection moves on. `landed()` counts the files answered so far, the one
//     live measure of a load whose size nobody knows until its parents have landed.
//   - `reading()` counts the `load`s without `keep` and the `ready` names still waiting: the
//     client's own reads, which the page's cover waits on so a screen is not shown before its art.
//
// The network goes one of two ways, and high priority goes first on both. With a data version,
// each name that misses memory and the persistent cache is one `GET /data/<key>?v=<version>`, a
// URL every player spells the same so a CDN can cache it (the host answers it `public`, and a
// POST is never cached): at most `maxRequests` in flight, `maxLowRequests` of them low priority.
// Without a version (an older host, or `__version` failed) every such name within one task joins
// a `POST /data/__batch`, which answers many files in one response, streamed file by file: a
// browser runs six HTTP/1.1 requests to a host at once, so a world of small files fetched one per
// request is a queue of round trips (Dalaran is ~5,500 files); batched, it is a few dozen, with
// `maxBatches` in flight, `maxLowBatches` of them low priority. A host without that route gets one
// `GET /data/<name>` per file. `opts.transport` ('get' or 'batch') forces a way.
//
// Memory is a byte-bounded LRU. Under it sits the Cache API, keyed by the host's data version
// (`GET /data/__version`), so a second visit reads the disk instead of the network and a changed
// install drops the old copy. Names are matched as the MPQ hash matches them: any case, `/` ≡ `\`.
//
// A host serves each locale's chain under its own root (`/data/__l/<locale>`, plain `/data` the
// default one's), and each root is its own install: the Cache API copy, the learned names and the
// filled tags are named by the root's scope (`dataScope`), and dropping a stale copy drops only
// that scope's, so switching locale keeps the other locale's game data. Plain `/data` keeps the
// names it had before there were locales.

const DEFAULT_BUDGET = 384 * 1024 * 1024;
const LEARNED_KEY = 'wenilla:data:learned';
const LEARNED_MAX = 6000;
const FILLED_KEY = 'wenilla:data:filled';
const CACHE_PREFIX = 'wenilla-data-';
const DROPPED = new Error('fetch ahead dropped');
/// A scoped Cache API name: `wenilla-data-<locale>-<version>`.
const SCOPED_CACHE = /^wenilla-data-[a-z]{2}[A-Z]{2}-/;

/// The scope of a data root: its locale under `/data/__l/<locale>`, else `''`, the unscoped names.
export function dataScope(base) {
  const m = /\/__l\/([^/]+)\/?$/.exec(String(base));
  return m ? decodeURIComponent(m[1]) : '';
}

/// The persistent names of a scope: its Cache API name for a version, whether a cache name is one
/// of the scope's, and its localStorage keys.
export function scopeNames(scope) {
  const tag = scope ? `${scope}-` : '';
  const suffix = scope ? `:${scope}` : '';
  return {
    cache: (version) => CACHE_PREFIX + tag + version,
    owns: (name) =>
      scope ? name.startsWith(CACHE_PREFIX + tag) : name.startsWith(CACHE_PREFIX) && !SCOPED_CACHE.test(name),
    learned: LEARNED_KEY + suffix,
    filled: FILLED_KEY + suffix,
  };
}

/// The MPQ-equivalence key: lowercase, backslash-separated.
export function dataKey(name) {
  return String(name).replace(/\//g, '\\').toLowerCase();
}

export function createStore(opts = {}) {
  const base = opts.base ?? '/data';
  const names = scopeNames(opts.scope ?? dataScope(base));
  const budget = opts.budgetBytes ?? DEFAULT_BUDGET;
  const fetchFn = opts.fetch ?? ((...a) => globalThis.fetch(...a));
  const cacheStorage = opts.persist === false ? null : (opts.caches ?? globalThis.caches ?? null);
  const storage = opts.storage === undefined ? safeLocalStorage() : opts.storage;

  const mem = new Map(); // key -> Uint8Array, oldest first
  const pinned = new Set(); // keys eviction leaves alone
  let memBytes = 0;
  const flights = new Map(); // key -> flight
  const missing = new Set(); // keys the host answered 404 for
  let versionPromise = null;
  let cachePromise = null;
  const learnedMap = new Map(loadLearned(storage, names.learned)); // name -> phase
  let phase = 'boot';
  let learnedDirty = false;
  let reading = 0;
  let answered = 0;
  const arriving = new Set(); // keys `ready` is waiting on
  const settled = new Set(); // keys `ready` asked for that did not land: never asked for again
  const stats = {
    memHits: 0, getMisses: 0, joined: 0, cacheHits: 0, network: 0, networkBytes: 0,
    batches: 0, requests: 0, notFound: 0, errors: 0, evicted: 0, filled: 0,
  };
  const urlFor = (key) => `${base}/${encodeURIComponent(key)}`;
  const net = createNetwork({
    base,
    fetchFn,
    urlFor,
    stats,
    maxNames: opts.batchNames ?? 64,
    batchLimits: { all: opts.maxBatches ?? 4, low: opts.maxLowBatches ?? 2 },
    requestLimits: { all: opts.maxRequests ?? 24, low: opts.maxLowRequests ?? 8 },
  });

  function put(key, bytes) {
    const old = mem.get(key);
    if (old) {
      memBytes -= old.byteLength;
      mem.delete(key);
    }
    if (bytes.byteLength > budget) return;
    mem.set(key, bytes);
    memBytes += bytes.byteLength;
    if (memBytes <= budget) return;
    for (const [k, v] of mem) {
      if (pinned.has(k)) continue;
      mem.delete(k);
      memBytes -= v.byteLength;
      stats.evicted += 1;
      if (memBytes <= budget) break;
    }
  }

  function take(key, keep) {
    const bytes = mem.get(key);
    mem.delete(key);
    if (keep) mem.set(key, bytes); // touch: now the newest
    else memBytes -= bytes.byteLength;
    return bytes;
  }

  /// The host's data version, asked once: `opts.version`, or `GET /data/__version`. `null` when
  /// the host names none.
  function dataVersion() {
    if (!versionPromise) {
      versionPromise = (async () => {
        if (opts.version) return opts.version;
        const r = await fetchFn(`${base}/__version`);
        if (!r.ok) return null;
        return (await r.text()).trim() || null;
      })().catch(() => null);
    }
    return versionPromise;
  }

  /// What follows a chain file's URL on the network: `?v=<version>`, nothing for a forced GET
  /// with no version to name, or `null` for the batch route.
  async function fileQuery() {
    if (opts.transport === 'batch') return null;
    const version = await dataVersion();
    if (version) return `?v=${version}`;
    return opts.transport === 'get' ? '' : null;
  }

  function openCache() {
    if (!cachePromise) {
      cachePromise = (async () => {
        if (!cacheStorage) return null;
        const version = await dataVersion();
        if (!version) return null;
        // Only this root's older copies go: another locale's is still that locale's.
        const name = names.cache(version);
        for (const n of await cacheStorage.keys()) {
          if (names.owns(n) && n !== name) cacheStorage.delete(n).catch(() => {});
        }
        return await cacheStorage.open(name);
      })().catch(() => null);
    }
    return cachePromise;
  }

  async function fetchBytes(f) {
    const url = urlFor(f.key);
    const cache = await openCache();
    if (cache) {
      const hit = await cache.match(url).catch(() => undefined);
      if (hit) {
        stats.cacheHits += 1;
        return new Uint8Array(await hit.arrayBuffer());
      }
    }
    // The host's own routes need no version. `f.priority` is read after this wait: a reader may
    // have joined and raised it while the version was on its way.
    const query = f.key.startsWith('__') ? null : await fileQuery();
    const bytes = await net.request(f.key, f.priority, f.ahead, query);
    // A Response built from bytes copies them, so they stay ours.
    if (bytes && cache) cache.put(url, new Response(bytes)).catch(() => {});
    return bytes;
  }

  function flight(key, priority, ahead) {
    let f = flights.get(key);
    if (f) {
      stats.joined += 1;
      if (priority !== 'low' && f.priority === 'low') {
        f.priority = priority; // a reader waits on it now
        net.promote(key);
      }
      return f;
    }
    f = { key, priority, ahead, keep: false, consumers: 0 };
    f.promise = new Promise((resolve, reject) => {
      f.resolve = resolve;
      f.reject = reject;
    });
    f.promise.catch(() => {}); // a prefetch nobody awaits must not report an unhandled rejection
    flights.set(key, f);
    fetchBytes(f).then(
      (bytes) => {
        flights.delete(key);
        if (bytes !== null) answered += 1;
        if (bytes === null) {
          missing.add(key);
          stats.notFound += 1;
        } else if (f.keep && f.consumers === 0) {
          put(key, bytes);
        }
        f.resolve(bytes);
      },
      (err) => {
        flights.delete(key);
        if (err !== DROPPED) stats.errors += 1;
        f.reject(err);
      },
    );
    return f;
  }

  /// The bytes for `name`: `Uint8Array`, or `null` when the host has no such file.
  function load(name, { priority = 'high', keep = false, ahead = false } = {}) {
    const key = dataKey(name);
    if (mem.has(key)) {
      stats.memHits += 1;
      answered += 1;
      return Promise.resolve(take(key, keep));
    }
    if (missing.has(key)) return Promise.resolve(null);
    const f = flight(key, priority, ahead);
    if (keep) {
      f.keep = true;
    } else {
      f.consumers += 1;
      reading += 1;
      const landed = () => {
        reading -= 1;
      };
      f.promise.then(landed, landed);
    }
    return f.promise;
  }

  /// Whether every name is in memory now; those that are not start at high priority, kept for
  /// the read that follows. A miss is learned for the next visit, as `get`'s is.
  function ready(names) {
    let all = true;
    for (const name of names) {
      const key = dataKey(name);
      if (mem.has(key) || missing.has(key) || settled.has(key)) continue;
      all = false;
      if (arriving.has(key)) continue;
      arriving.add(key);
      reading += 1;
      learn(name);
      const landed = () => {
        arriving.delete(key);
        reading -= 1;
        if (!mem.has(key)) settled.add(key);
      };
      load(name, { priority: 'high', keep: true }).then(landed, landed);
    }
    return all;
  }

  /// Fetch `names` ahead into memory; resolves when every one has landed or failed. `pin` keeps
  /// them from eviction: what a blocking reader needs on the player's click, not the next frame.
  function prefetch(names, { priority = 'low', ahead = false, pin = false, onProgress } = {}) {
    const list = Array.from(names ?? []);
    if (pin) for (const n of list) pinned.add(dataKey(n));
    let done = 0;
    const one = (n) =>
      load(n, { priority, keep: true, ahead })
        .catch(() => null)
        .then(() => {
          done += 1;
          if (onProgress) onProgress(done, list.length);
        });
    return Promise.all(list.map(one)).then(() => undefined);
  }

  /// Download `names` into the persistent cache without holding them in memory, a few at a time
  /// and only while nothing else wants the network, so a later `ready` reads the disk. A `tag`
  /// that finished for this data version is not walked again. Resolves to whether it ran.
  async function fill(names, { tag, workers = 6, idleMs = 250 } = {}) {
    if (globalThis.navigator?.connection?.saveData) return false; // the player asked for less data
    const cache = await openCache();
    if (!cache) return false;
    const version = await dataVersion();
    if (tag && readFilled(version).includes(tag)) return false;
    const list = Array.from(names ?? []);
    const busy = () => {
      const n = net.state();
      return reading > 0 || n.queuedHigh > 0 || n.queuedLow > 0;
    };
    let next = 0;
    let failed = 0;
    const worker = async () => {
      while (next < list.length) {
        while (busy()) await new Promise((r) => setTimeout(r, idleMs));
        if (next >= list.length) return;
        const key = dataKey(list[next++]);
        if (mem.has(key) || missing.has(key)) continue;
        if (await cache.match(urlFor(key)).catch(() => undefined)) continue;
        try {
          await flight(key, 'low', false).promise;
          stats.filled += 1;
        } catch (_) {
          failed += 1;
        }
      }
    };
    await Promise.all(Array.from({ length: workers }, worker));
    if (tag && failed === 0) writeFilled(version, tag);
    return true;
  }

  function readFilled(version) {
    try {
      const v = JSON.parse(storage?.getItem(names.filled) ?? 'null');
      return v?.version === version && Array.isArray(v.tags) ? v.tags : [];
    } catch (_) {
      return [];
    }
  }

  function writeFilled(version, tag) {
    try {
      const tags = [...readFilled(version), tag];
      storage?.setItem(names.filled, JSON.stringify({ version, tags }));
    } catch (_) {
      /* quota or private mode: the next visit walks the list again and finds it cached */
    }
  }

  /// Memory only, synchronous: the bytes, or `undefined`. A miss is learned for the next visit.
  /// `consume` drops them from memory (a reader that parses once and keeps its own form).
  function get(name, consume = false) {
    const key = dataKey(name);
    if (mem.has(key)) {
      stats.memHits += 1;
      return take(key, !consume);
    }
    stats.getMisses += 1;
    learn(name);
    return undefined;
  }

  function learn(name) {
    if (!storage || learnedMap.has(name)) return;
    learnedMap.set(name, phase);
    if (!learnedDirty) {
      learnedDirty = true;
      setTimeout(saveLearned, 2000);
    }
  }

  function saveLearned() {
    learnedDirty = false;
    try {
      const entries = Array.from(learnedMap).slice(-LEARNED_MAX);
      storage.setItem(names.learned, JSON.stringify(entries));
    } catch (_) {
      /* quota or private mode: the next visit just learns again */
    }
  }

  return {
    get,
    load,
    prefetch,
    fill,
    has: (name) => mem.has(dataKey(name)),
    reading: () => reading,
    ready,
    landed: () => answered,
    dropAhead: () => net.dropAhead(),
    /// The names synchronous reads missed on earlier visits during `phase`.
    learned: (p) => Array.from(learnedMap).filter(([, ph]) => ph === p).map(([n]) => n),
    /// Name the phase later misses are learned under (`boot`, then `world` once the glue is up).
    setPhase: (p) => {
      phase = p;
    },
    stats: () => ({ ...stats, memBytes, memCount: mem.size, inflight: flights.size, ...net.state() }),
  };
}

/// The network half: names in, bytes (or `null` for a 404) out, one GET per file or gathered into
/// batches. `query` on a request says which: the string to append to the GET, `null` for a batch.
function createNetwork({ base, fetchFn, urlFor, stats, maxNames, batchLimits, requestLimits }) {
  const queues = { high: [], low: [] };
  const queued = new Map(); // key -> entry not yet sent
  const active = { high: 0, low: 0 };
  let scheduled = false;
  let batching = true; // false once the host answers that it has no batch route

  function request(key, priority, ahead, query) {
    // The host's own routes (`__index`, …) are not chain files: always a plain GET.
    if (key.startsWith('__') || (query === null && !batching)) return single(key, priority);
    return new Promise((resolve, reject) => {
      const e = { key, priority: priority === 'low' ? 'low' : 'high', ahead, query, resolve, reject };
      queues[e.priority].push(e);
      queued.set(key, e);
      schedule();
    });
  }

  function promote(key) {
    const e = queued.get(key);
    if (!e || e.priority === 'high') return;
    queues.low.splice(queues.low.indexOf(e), 1);
    e.priority = 'high';
    queues.high.push(e);
    schedule();
  }

  // Fetches ahead still queued go; a reader waiting on one promoted it out of the low queue.
  function dropAhead() {
    for (const e of queues.low.filter((x) => x.ahead)) {
      queued.delete(e.key);
      e.reject(DROPPED);
    }
    queues.low = queues.low.filter((x) => !x.ahead);
  }

  const limits = (e) => (e.query === null ? batchLimits : requestLimits);

  // One task's requests gather before a batch goes out: a frame's loads arrive together.
  function schedule() {
    if (scheduled) return;
    scheduled = true;
    setTimeout(pump, 0);
  }

  function pump() {
    scheduled = false;
    for (;;) {
      const busy = active.high + active.low;
      const high = queues.high[0];
      const low = queues.low[0];
      let p = null;
      if (high && busy < limits(high).all) p = 'high';
      else if (low && busy < limits(low).all && active.low < limits(low).low) p = 'low';
      if (!p) return;
      if (queues[p][0].query === null) {
        const entries = queues[p].splice(0, maxNames);
        for (const e of entries) queued.delete(e.key);
        run(entries, p);
      } else {
        const e = queues[p].shift();
        queued.delete(e.key);
        runGet(e, p);
      }
    }
  }

  async function runGet(e, p) {
    active[p] += 1;
    stats.requests += 1;
    try {
      e.resolve(await single(e.key, p, e.query));
    } catch (err) {
      e.reject(err);
    } finally {
      active[p] -= 1;
      pump();
    }
  }

  async function run(entries, p) {
    active[p] += 1;
    stats.batches += 1;
    const pending = new Map(entries.map((e) => [e.key, e]));
    try {
      const r = await fetchFn(`${base}/__batch`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(entries.map((e) => e.key)),
        priority: p,
      });
      if (r.status === 404 || r.status === 405) {
        batching = false; // an older host: one request per file from now on
        for (const e of entries) single(e.key, e.priority).then(e.resolve, e.reject);
        pending.clear();
        return;
      }
      if (!r.ok) throw new Error(`${base}/__batch: HTTP ${r.status}`);
      await readFrames(r, (name, bytes) => {
        const e = pending.get(name);
        if (!e) return;
        pending.delete(name);
        if (bytes === undefined) {
          stats.errors += 1;
          e.reject(new Error(`${urlFor(name)}: read failed on the host`));
          return;
        }
        if (bytes) {
          stats.network += 1;
          stats.networkBytes += bytes.byteLength;
        }
        e.resolve(bytes);
      });
      // A name the stream never answered: ask it alone rather than fail the read.
      for (const e of pending.values()) single(e.key, e.priority).then(e.resolve, e.reject);
      pending.clear();
    } catch (err) {
      for (const e of pending.values()) e.reject(err);
    } finally {
      active[p] -= 1;
      pump();
    }
  }

  async function single(key, priority, query = '') {
    const url = urlFor(key) + query;
    const r = await fetchFn(url, { priority });
    if (r.status === 404) return null;
    if (!r.ok) throw new Error(`${url}: HTTP ${r.status}`);
    const bytes = new Uint8Array(await r.arrayBuffer());
    stats.network += 1;
    stats.networkBytes += bytes.byteLength;
    return bytes;
  }

  return {
    request,
    promote,
    dropAhead,
    state: () => ({
      queuedHigh: queues.high.length,
      queuedLow: queues.low.length,
      activeRequests: active.high + active.low,
    }),
  };
}

/// Read a `/data/__batch` response frame by frame as it streams: `u32 name length, name,
/// i32 body length (−1 not in the chain, −2 a host read failure), body`, little-endian.
/// `onFile(name, bytes)` gets a `Uint8Array`, `null` (absent) or `undefined` (failed).
export async function readFrames(response, onFile) {
  const decoder = new TextDecoder();
  // Chunks as they arrived; every byte is copied once, into the field it belongs to.
  const chunks = [];
  let head = 0; // offset into chunks[0]
  let avail = 0;
  const takeBytes = (n) => {
    const out = new Uint8Array(n);
    let filled = 0;
    while (filled < n) {
      const c = chunks[0];
      const k = Math.min(n - filled, c.length - head);
      out.set(c.subarray(head, head + k), filled);
      filled += k;
      head += k;
      if (head === c.length) {
        chunks.shift();
        head = 0;
      }
    }
    avail -= n;
    return out;
  };
  const u32 = (b) => new DataView(b.buffer).getUint32(0, true);
  let need = 4;
  let stage = 'nameLength';
  let name = '';
  let len = 0;
  const parse = () => {
    while (avail >= need) {
      const b = takeBytes(need);
      if (stage === 'nameLength') {
        need = u32(b);
        stage = 'name';
      } else if (stage === 'name') {
        name = decoder.decode(b);
        need = 4;
        stage = 'bodyLength';
      } else if (stage === 'bodyLength') {
        len = new DataView(b.buffer).getInt32(0, true);
        if (len >= 0) {
          need = len;
          stage = 'body';
        } else {
          onFile(name, len === -1 ? null : undefined);
          need = 4;
          stage = 'nameLength';
        }
      } else {
        onFile(name, b);
        need = 4;
        stage = 'nameLength';
      }
    }
  };
  const push = (chunk) => {
    if (!chunk.length) return;
    chunks.push(chunk);
    avail += chunk.length;
    parse();
  };
  if (response.body && response.body.getReader) {
    const reader = response.body.getReader();
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      push(value);
    }
  } else {
    push(new Uint8Array(await response.arrayBuffer()));
  }
  if (avail !== 0 || stage !== 'nameLength') {
    throw new Error('/data/__batch: response ended mid-frame');
  }
}

function safeLocalStorage() {
  try {
    return globalThis.localStorage ?? null;
  } catch (_) {
    return null;
  }
}

function loadLearned(storage, key) {
  try {
    const v = JSON.parse(storage?.getItem(key) ?? '[]');
    return Array.isArray(v)
      ? v.filter((e) => Array.isArray(e) && typeof e[0] === 'string' && typeof e[1] === 'string')
      : [];
  } catch (_) {
    return [];
  }
}
