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
//     a read something is waiting on.
//
// The network is batched: every name that misses memory and the persistent cache within one
// task joins a `POST /data/__batch`, which answers many files in one response, streamed file by
// file. A browser runs six HTTP/1.1 requests to a host at once, so a world of small files fetched
// one per request is a queue of round trips (Dalaran is ~5,500 files); batched, it is a few dozen.
// High-priority batches go first and low-priority ones never take more than `maxLowBatches` of
// the slots. A host without the route gets one `GET /data/<name>` per file, as before.
//
// Memory is a byte-bounded LRU. Under it sits the Cache API, keyed by the host's data version
// (`GET /data/__version`), so a second visit reads the disk instead of the network and a changed
// install drops the old copy. Names are matched as the MPQ hash matches them: any case, `/` ≡ `\`.

const DEFAULT_BUDGET = 384 * 1024 * 1024;
const LEARNED_KEY = 'wenilla:data:learned';
const LEARNED_MAX = 6000;
const CACHE_PREFIX = 'wenilla-data-';

/// The MPQ-equivalence key: lowercase, backslash-separated.
export function dataKey(name) {
  return String(name).replace(/\//g, '\\').toLowerCase();
}

export function createStore(opts = {}) {
  const base = opts.base ?? '/data';
  const budget = opts.budgetBytes ?? DEFAULT_BUDGET;
  const fetchFn = opts.fetch ?? ((...a) => globalThis.fetch(...a));
  const cacheStorage = opts.persist === false ? null : (opts.caches ?? globalThis.caches ?? null);
  const storage = opts.storage === undefined ? safeLocalStorage() : opts.storage;

  const mem = new Map(); // key -> Uint8Array, oldest first
  let memBytes = 0;
  const flights = new Map(); // key -> flight
  const missing = new Set(); // keys the host answered 404 for
  let cachePromise = null;
  const learnedMap = new Map(loadLearned(storage)); // name -> phase
  let phase = 'boot';
  let learnedDirty = false;
  const stats = {
    memHits: 0, getMisses: 0, joined: 0, cacheHits: 0, network: 0, networkBytes: 0,
    batches: 0, notFound: 0, errors: 0, evicted: 0,
  };
  const urlFor = (key) => `${base}/${encodeURIComponent(key)}`;
  const net = createBatcher({
    base,
    fetchFn,
    urlFor,
    stats,
    maxNames: opts.batchNames ?? 64,
    maxBatches: opts.maxBatches ?? 4,
    maxLowBatches: opts.maxLowBatches ?? 2,
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
    while (memBytes > budget) {
      const [k, v] = mem.entries().next().value;
      mem.delete(k);
      memBytes -= v.byteLength;
      stats.evicted += 1;
    }
  }

  function take(key, keep) {
    const bytes = mem.get(key);
    mem.delete(key);
    if (keep) mem.set(key, bytes); // touch: now the newest
    else memBytes -= bytes.byteLength;
    return bytes;
  }

  function openCache() {
    if (!cachePromise) {
      cachePromise = (async () => {
        if (!cacheStorage) return null;
        let version = opts.version;
        if (!version) {
          const r = await fetchFn(`${base}/__version`);
          if (!r.ok) return null;
          version = (await r.text()).trim();
        }
        if (!version) return null;
        const name = CACHE_PREFIX + version;
        for (const n of await cacheStorage.keys()) {
          if (n.startsWith(CACHE_PREFIX) && n !== name) cacheStorage.delete(n).catch(() => {});
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
    const bytes = await net.request(f.key, f.priority);
    // A Response built from bytes copies them, so they stay ours.
    if (bytes && cache) cache.put(url, new Response(bytes)).catch(() => {});
    return bytes;
  }

  function flight(key, priority) {
    let f = flights.get(key);
    if (f) {
      stats.joined += 1;
      if (priority !== 'low' && f.priority === 'low') {
        f.priority = priority; // a reader waits on it now
        net.promote(key);
      }
      return f;
    }
    f = { key, priority, keep: false, consumers: 0 };
    f.promise = new Promise((resolve, reject) => {
      f.resolve = resolve;
      f.reject = reject;
    });
    f.promise.catch(() => {}); // a prefetch nobody awaits must not report an unhandled rejection
    flights.set(key, f);
    fetchBytes(f).then(
      (bytes) => {
        flights.delete(key);
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
        stats.errors += 1;
        f.reject(err);
      },
    );
    return f;
  }

  /// The bytes for `name`: `Uint8Array`, or `null` when the host has no such file.
  function load(name, { priority = 'high', keep = false } = {}) {
    const key = dataKey(name);
    if (mem.has(key)) {
      stats.memHits += 1;
      return Promise.resolve(take(key, keep));
    }
    if (missing.has(key)) return Promise.resolve(null);
    const f = flight(key, priority);
    if (keep) f.keep = true;
    else f.consumers += 1;
    return f.promise;
  }

  /// Fetch `names` ahead into memory; resolves when every one has landed or failed.
  function prefetch(names, { priority = 'low', onProgress } = {}) {
    const list = Array.from(names ?? []);
    let done = 0;
    const one = (n) =>
      load(n, { priority, keep: true })
        .catch(() => null)
        .then(() => {
          done += 1;
          if (onProgress) onProgress(done, list.length);
        });
    return Promise.all(list.map(one)).then(() => undefined);
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
      storage.setItem(LEARNED_KEY, JSON.stringify(entries));
    } catch (_) {
      /* quota or private mode: the next visit just learns again */
    }
  }

  return {
    get,
    load,
    prefetch,
    has: (name) => mem.has(dataKey(name)),
    /// The names synchronous reads missed on earlier visits during `phase`.
    learned: (p) => Array.from(learnedMap).filter(([, ph]) => ph === p).map(([n]) => n),
    /// Name the phase later misses are learned under (`boot`, then `world` once the glue is up).
    setPhase: (p) => {
      phase = p;
    },
    stats: () => ({ ...stats, memBytes, memCount: mem.size, inflight: flights.size, ...net.state() }),
  };
}

/// The network half: names in, bytes (or `null` for a 404) out, gathered into batches.
function createBatcher({ base, fetchFn, urlFor, stats, maxNames, maxBatches, maxLowBatches }) {
  const queues = { high: [], low: [] };
  const queued = new Map(); // key -> entry not yet sent
  const active = { high: 0, low: 0 };
  let scheduled = false;
  let batching = true; // false once the host answers that it has no batch route

  function request(key, priority) {
    // The host's own routes (`__index`, …) are not chain files: always a plain GET.
    if (!batching || key.startsWith('__')) return single(key, priority);
    return new Promise((resolve, reject) => {
      const e = { key, priority: priority === 'low' ? 'low' : 'high', resolve, reject };
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
      let p = null;
      if (queues.high.length && busy < maxBatches) p = 'high';
      else if (queues.low.length && busy < maxBatches && active.low < maxLowBatches) p = 'low';
      if (!p) return;
      const entries = queues[p].splice(0, maxNames);
      for (const e of entries) queued.delete(e.key);
      run(entries, p);
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

  async function single(key, priority) {
    const r = await fetchFn(urlFor(key), { priority });
    if (r.status === 404) return null;
    if (!r.ok) throw new Error(`${urlFor(key)}: HTTP ${r.status}`);
    const bytes = new Uint8Array(await r.arrayBuffer());
    stats.network += 1;
    stats.networkBytes += bytes.byteLength;
    return bytes;
  }

  return {
    request,
    promote,
    state: () => ({
      queuedHigh: queues.high.length,
      queuedLow: queues.low.length,
      activeBatches: active.high + active.low,
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

function loadLearned(storage) {
  try {
    const v = JSON.parse(storage?.getItem(LEARNED_KEY) ?? '[]');
    return Array.isArray(v)
      ? v.filter((e) => Array.isArray(e) && typeof e[0] === 'string' && typeof e[1] === 'string')
      : [];
  } catch (_) {
    return [];
  }
}
