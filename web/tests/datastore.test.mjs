import assert from 'node:assert/strict';
import test from 'node:test';
import { createStore, dataKey, dataScope, readFrames, scopeNames } from '../datastore.js';

const tick = () => new Promise((r) => setTimeout(r, 0));
const settle = async () => {
  for (let i = 0; i < 6; i++) await tick();
};

/// One `/data/__batch` frame, as the host writes it.
function frame(name, body) {
  const n = new TextEncoder().encode(name);
  const len = body === null ? -1 : body === undefined ? -2 : body.length;
  const out = new Uint8Array(8 + n.length + Math.max(len, 0));
  const v = new DataView(out.buffer);
  v.setUint32(0, n.length, true);
  out.set(n, 4);
  v.setInt32(4 + n.length, len, true);
  if (len > 0) out.set(body, 8 + n.length);
  return out;
}

/// A fake host: `files` maps key -> bytes; every request waits on a gate the test opens.
/// `batch: false` is a host without the batch route, `version: null` one that names no version.
function host(files, { batch = true, version = 'v1' } = {}) {
  const calls = [];
  const fetch = (url, init = {}) => {
    const [rawPath, query] = url.slice('/data/'.length).split('?');
    const path = decodeURIComponent(rawPath);
    const keys = path === '__batch' ? JSON.parse(init.body) : [path];
    const method = init.method ?? 'GET';
    let open;
    const gate = new Promise((r) => (open = r));
    calls.push({ url, method, path, query, keys, priority: init.priority, open });
    return gate.then(() => {
      if (path === '__version') {
        return version ? new Response(version) : new Response('', { status: 404 });
      }
      if (path === '__batch') {
        if (!batch) return new Response('', { status: 404 });
        const parts = keys.map((k) => frame(k, files[k] ?? null));
        return new Response(new Blob(parts));
      }
      const b = files[path];
      return b ? new Response(b) : new Response('', { status: 404 });
    });
  };
  return { fetch, calls, openAll: () => calls.forEach((c) => c.open()) };
}

const bytes = (...v) => new Uint8Array(v);
const plain = (h, o = {}) =>
  createStore({ fetch: h.fetch, persist: false, storage: null, transport: 'batch', ...o });
const versioned = (h, o = {}) =>
  createStore({ fetch: h.fetch, persist: false, storage: null, version: 'v1', ...o });

test('keys match the way the MPQ hash does', () => {
  assert.equal(dataKey('World/Maps/A.ADT'), 'world\\maps\\a.adt');
});

test('one task of reads is one batch, and concurrent reads of a name share it', async () => {
  const h = host({ 'a.blp': bytes(1, 2, 3), 'b.blp': bytes(4) });
  const s = plain(h);
  const p1 = s.load('A.blp');
  const p2 = s.load('a.BLP');
  const p3 = s.load('b.blp');
  await settle();
  assert.equal(h.calls.length, 1);
  assert.deepEqual(h.calls[0].keys, ['a.blp', 'b.blp']);
  h.openAll();
  assert.deepEqual(await p1, bytes(1, 2, 3));
  assert.deepEqual(await p2, bytes(1, 2, 3));
  assert.deepEqual(await p3, bytes(4));
  assert.equal(s.has('a.blp'), false, 'a reader consumes; nothing is kept');
});

test('a prefetch keeps the bytes for the synchronous reader, which leaves them resident', async () => {
  const h = host({ 'x.dbc': bytes(9) });
  const s = plain(h);
  assert.equal(s.get('x.dbc'), undefined);
  const done = s.prefetch(['X.dbc']);
  await settle();
  h.openAll();
  await done;
  assert.deepEqual(s.get('x.dbc'), bytes(9));
  assert.deepEqual(s.get('x.dbc'), bytes(9));
  assert.deepEqual(s.get('x.dbc', true), bytes(9));
  assert.equal(s.has('x.dbc'), false, 'a consuming get drops it');
  const again = s.prefetch(['x.dbc']);
  await settle();
  h.openAll();
  await again;
  assert.deepEqual(await s.load('x.dbc'), bytes(9));
  assert.equal(s.has('x.dbc'), false, 'the async reader takes it');
});

test('a reader that joins a prefetch takes the bytes instead of leaving them in memory', async () => {
  const h = host({ 'm.m2': bytes(5) });
  const s = plain(h);
  s.prefetch(['m.m2']);
  const read = s.load('m.m2');
  await settle();
  h.openAll();
  assert.deepEqual(await read, bytes(5));
  await tick();
  assert.equal(s.has('m.m2'), false);
});

test('a name the chain lacks resolves null and is remembered', async () => {
  const h = host({});
  const s = plain(h);
  const p = s.load('gone.blp');
  await settle();
  h.openAll();
  assert.equal(await p, null);
  assert.equal(await s.load('GONE.blp'), null);
  assert.equal(h.calls.length, 1);
});

test('memory is a byte-bounded LRU', async () => {
  const h = host({ a: bytes(1, 1), b: bytes(2, 2), c: bytes(3, 3) });
  const s = plain(h, { budgetBytes: 4 });
  const p = s.prefetch(['a', 'b']);
  await settle();
  h.openAll();
  await p;
  s.get('a'); // a is now newer than b
  const q = s.prefetch(['c']);
  await settle();
  h.openAll();
  await q;
  assert.equal(s.has('b'), false);
  assert.equal(s.has('a'), true);
  assert.equal(s.has('c'), true);
  assert.equal(s.stats().memBytes, 4);
});

test('batches split at the name cap; fetches ahead take at most their slots, a reader jumps them', async () => {
  const files = Object.fromEntries('abcdefg'.split('').map((k, i) => [k, bytes(i)]));
  const h = host(files);
  const s = plain(h, { batchNames: 2, maxBatches: 3, maxLowBatches: 1 });
  s.prefetch(['a', 'b', 'c', 'd', 'e']);
  await settle();
  assert.deepEqual(h.calls.map((c) => [c.keys, c.priority]), [[['a', 'b'], 'low']]);
  const read = s.load('e'); // queued low: promoted, and sent ahead of c and d
  const other = s.load('g');
  await settle();
  assert.deepEqual(h.calls.slice(1).map((c) => [c.keys, c.priority]), [[['e', 'g'], 'high']]);
  h.openAll();
  assert.deepEqual(await read, bytes(4));
  assert.deepEqual(await other, bytes(6));
  await settle();
  assert.deepEqual(h.calls.slice(2).map((c) => c.keys), [['c', 'd']]);
});

test('a host without the batch route gets one request per file', async () => {
  const h = host({ a: bytes(1), b: bytes(2) }, { batch: false });
  const s = plain(h);
  const pa = s.load('a');
  const pb = s.load('b');
  await settle();
  h.openAll();
  await settle();
  h.openAll();
  assert.deepEqual(await pa, bytes(1));
  assert.deepEqual(await pb, bytes(2));
  const pc = s.load('a');
  await settle();
  h.openAll();
  await pc;
  assert.deepEqual(h.calls.map((c) => c.path), ['__batch', 'a', 'b', 'a']);
});

test('frames parse across any chunk boundary, and a cut-off stream is an error', async () => {
  const big = new Uint8Array(300).map((_, i) => i & 0xff);
  const all = new Uint8Array(
    [frame('a', big), frame('', bytes()), frame('gone', null), frame('bad', undefined)].flatMap((f) => [...f]),
  );
  for (const size of [1, 3, 7, 64, all.length]) {
    const chunks = [];
    for (let i = 0; i < all.length; i += size) chunks.push(all.slice(i, i + size));
    const got = [];
    await readFrames(new Response(new Blob(chunks)), (n, b) => got.push([n, b]));
    assert.deepEqual(got, [['a', big], ['', bytes()], ['gone', null], ['bad', undefined]], `chunk ${size}`);
  }
  await assert.rejects(readFrames(new Response(all.slice(0, 20)), () => {}), /mid-frame/);
});

test('a persistent cache answers before the network and is filled by it', async () => {
  const store = new Map();
  const opened = [];
  const caches = {
    keys: async () => ['wenilla-data-old', ...opened],
    delete: async (n) => opened.push('deleted:' + n),
    open: async (n) => {
      opened.push(n);
      return {
        match: async (u) => (store.has(u) ? new Response(store.get(u)) : undefined),
        put: async (u, r) => store.set(u, new Uint8Array(await r.arrayBuffer())),
      };
    },
  };
  const h = host({ 'a.blp': bytes(7) });
  const s = createStore({ fetch: h.fetch, caches, storage: null });
  const p = s.load('a.blp');
  for (let i = 0; i < 5; i++) {
    h.openAll();
    await settle();
  }
  assert.deepEqual(await p, bytes(7));
  assert.ok(opened.includes('wenilla-data-v1'));
  assert.ok(opened.includes('deleted:wenilla-data-old'), 'an older data version is dropped');
  await settle();
  assert.deepEqual(store.get('/data/a.blp'), bytes(7), 'kept without the version in its key');
  assert.deepEqual(h.calls.map((c) => c.url), ['/data/__version', '/data/a.blp?v=v1']);

  const fresh = createStore({ fetch: h.fetch, caches, storage: null });
  const before = h.calls.length;
  const q = fresh.load('A.blp');
  for (let i = 0; i < 5; i++) {
    h.openAll();
    await settle();
  }
  assert.deepEqual(await q, bytes(7));
  assert.deepEqual(h.calls.slice(before).map((c) => c.path), ['__version'], 'served from the cache');
});

test('synchronous misses are learned by phase and kept across visits', async (t) => {
  const kv = new Map();
  const storage = { getItem: (k) => kv.get(k) ?? null, setItem: (k, v) => kv.set(k, v) };
  t.mock.timers.enable({ apis: ['setTimeout'] });
  const s = createStore({ fetch: host({}).fetch, persist: false, storage });
  s.get('DBFilesClient\\Spell.dbc');
  s.setPhase('world');
  s.get('Interface\\A.blp');
  t.mock.timers.tick(2000);
  const next = createStore({ fetch: host({}).fetch, persist: false, storage });
  assert.deepEqual(next.learned('boot'), ['DBFilesClient\\Spell.dbc']);
  assert.deepEqual(next.learned('world'), ['Interface\\A.blp']);
});

test('reading counts the client\'s waiting reads, not fetches ahead', async () => {
  const h = host({ 'a.blp': bytes(1), 'b.blp': bytes(2) });
  const s = plain(h);
  const ahead = s.prefetch(['a.blp']);
  assert.equal(s.reading(), 0);
  const read = s.load('b.blp');
  assert.equal(s.reading(), 1);
  await settle();
  h.openAll();
  await Promise.all([ahead, read]);
  await settle();
  assert.equal(s.reading(), 0);
});

test('ready starts what is missing, asks once, and answers true when it lands', async () => {
  const h = host({ 'a.blp': bytes(1) });
  const s = plain(h);
  assert.equal(s.ready(['a.blp']), false);
  await settle();
  assert.equal(s.ready(['a.blp']), false);
  assert.equal(s.reading(), 1);
  h.openAll();
  await settle();
  assert.equal(s.ready(['a.blp']), true);
  assert.equal(s.reading(), 0);
  assert.equal(h.calls.length, 1);
  assert.deepEqual(Array.from(s.get('a.blp')), [1]);
});

test('ready gives up on a name that cannot arrive instead of asking every frame', async () => {
  let asked = 0;
  const s = createStore({
    persist: false,
    storage: null,
    fetch: async () => {
      asked += 1;
      throw new Error('offline');
    },
  });
  assert.equal(s.ready(['a.blp']), false);
  await settle();
  assert.equal(s.ready(['a.blp']), true);
  assert.equal(s.reading(), 0);
  assert.ok(asked <= 2);
});

test('ready answers true for a name the host lacks', async () => {
  const h = host({});
  const s = plain(h);
  assert.equal(s.ready(['gone.blp']), false);
  await settle();
  h.openAll();
  await settle();
  assert.equal(s.ready(['gone.blp']), true);
});

test('dropAhead forgets unsent fetches ahead, leaves the rest, and a dropped name can come back', async () => {
  const h = host({ 'a.adt': bytes(1), 'b.adt': bytes(2), 'c.blp': bytes(3), 'x.adt': bytes(4) });
  const s = plain(h, { batchNames: 1, maxBatches: 1, maxLowBatches: 1 });
  s.prefetch(['x.adt'], { ahead: true }); // sent, so it finishes
  s.prefetch(['a.adt'], { ahead: true });
  const list = s.prefetch(['c.blp']); // the page's own list stays
  const b = s.prefetch(['b.adt'], { ahead: true });
  await settle();
  s.dropAhead();
  await b;
  for (let i = 0; i < 4; i++) {
    h.openAll();
    await settle();
  }
  await list;
  assert.deepEqual(h.calls.map((c) => c.keys).flat().sort(), ['c.blp', 'x.adt']);
  assert.equal(s.stats().errors, 0);
  assert.equal(s.landed(), 2);
  s.ready(['a.adt']);
  await settle();
  h.openAll();
  await settle();
  assert.equal(s.ready(['a.adt']), true);
});

test('with a version each file is one GET naming it, and nothing is batched', async () => {
  const h = host({ 'a.blp': bytes(1, 2), 'b.blp': bytes(3) });
  const s = versioned(h);
  const pa = s.load('a.blp');
  const pb = s.load('b.blp');
  await settle();
  assert.deepEqual(
    h.calls.map((c) => [c.method, c.url, c.priority]),
    [['GET', '/data/a.blp?v=v1', 'high'], ['GET', '/data/b.blp?v=v1', 'high']],
  );
  h.openAll();
  assert.deepEqual(await pa, bytes(1, 2));
  assert.deepEqual(await pb, bytes(3));
  const st = s.stats();
  assert.deepEqual([st.requests, st.batches, st.network, st.networkBytes], [2, 0, 2, 3]);
});

test('the version is asked once, even with no persistent cache to key', async () => {
  const h = host({ a: bytes(1), b: bytes(2) });
  const s = createStore({ fetch: h.fetch, persist: false, storage: null });
  const pa = s.load('a');
  const pb = s.load('b');
  await settle();
  assert.deepEqual(h.calls.map((c) => c.path), ['__version']);
  for (let i = 0; i < 2; i++) {
    h.openAll();
    await settle();
  }
  await Promise.all([pa, pb]);
  assert.deepEqual(h.calls.map((c) => c.url), ['/data/__version', '/data/a?v=v1', '/data/b?v=v1']);
});

test('every spelling of a name asks the same URL', async () => {
  const urls = [];
  for (const name of ['World/Maps/A.ADT', 'world\\maps\\a.adt', 'WORLD\\Maps/a.Adt']) {
    const h = host({ 'world\\maps\\a.adt': bytes(1) });
    const p = versioned(h).load(name);
    await settle();
    h.openAll();
    assert.deepEqual(await p, bytes(1));
    urls.push(h.calls[0].url);
  }
  assert.deepEqual(urls, Array(3).fill('/data/world%5Cmaps%5Ca.adt?v=v1'));
});

test('a host that names no version gets batches', async () => {
  const h = host({ a: bytes(1) }, { version: null });
  const s = createStore({ fetch: h.fetch, persist: false, storage: null });
  const p = s.load('a');
  for (let i = 0; i < 3; i++) {
    await settle();
    h.openAll();
  }
  assert.deepEqual(await p, bytes(1));
  assert.deepEqual(h.calls.map((c) => [c.method, c.path]), [['GET', '__version'], ['POST', '__batch']]);
});

test('a version request that fails falls back to batches', async () => {
  const h = host({ a: bytes(1) });
  const fetch = (url, init) => (url.endsWith('/__version') ? Promise.reject(new Error('offline')) : h.fetch(url, init));
  const s = createStore({ fetch, persist: false, storage: null });
  const p = s.load('a');
  await settle();
  h.openAll();
  assert.deepEqual(await p, bytes(1));
  assert.deepEqual(h.calls.map((c) => c.path), ['__batch']);
});

test('the transport option forces either way', async () => {
  const batched = host({ a: bytes(1) });
  const s = versioned(batched, { transport: 'batch' });
  const p = s.load('a');
  await settle();
  batched.openAll();
  assert.deepEqual(await p, bytes(1));
  assert.deepEqual(batched.calls.map((c) => c.path), ['__batch'], 'even with a version');

  const got = host({ a: bytes(1) }, { version: null });
  const t = createStore({ fetch: got.fetch, persist: false, storage: null, transport: 'get' });
  const q = t.load('a');
  for (let i = 0; i < 3; i++) {
    await settle();
    got.openAll();
  }
  assert.deepEqual(await q, bytes(1));
  assert.deepEqual(got.calls.map((c) => c.url), ['/data/__version', '/data/a'], 'no version to name');
});

test('the host\'s own routes stay a plain GET and do not wait for the version', async () => {
  const h = host({ __index: bytes(1) });
  const s = createStore({ fetch: h.fetch, persist: false, storage: null });
  const p = s.load('__index');
  await settle();
  assert.deepEqual(h.calls.map((c) => c.url), ['/data/__index']);
  h.openAll();
  assert.deepEqual(await p, bytes(1));
});

test('a name the host lacks resolves null over GET and is remembered', async () => {
  const h = host({});
  const s = versioned(h);
  const p = s.load('gone.blp');
  await settle();
  h.openAll();
  assert.equal(await p, null);
  assert.equal(await s.load('GONE.blp'), null);
  assert.equal(h.calls.length, 1);
  assert.equal(s.stats().notFound, 1);
  assert.equal(s.stats().errors, 0);
});

test('a failed GET rejects its read and counts as an error', async () => {
  const s = versioned({ fetch: async () => new Response('', { status: 503 }) });
  await assert.rejects(s.load('a.blp'), /a\.blp\?v=v1: HTTP 503/);
  assert.equal(s.stats().errors, 1);
});

test('GETs in flight are capped; fetches ahead take at most their slots, a reader jumps them', async () => {
  const files = Object.fromEntries('abcdefg'.split('').map((k, i) => [k, bytes(i)]));
  const h = host(files);
  const s = versioned(h, { maxRequests: 3, maxLowRequests: 1 });
  s.prefetch(['a', 'b', 'c', 'd', 'e']);
  await settle();
  assert.deepEqual(h.calls.map((c) => [c.path, c.priority]), [['a', 'low']]);
  const read = s.load('e'); // queued low: promoted, and sent ahead of b, c and d
  const other = s.load('g');
  await settle();
  assert.deepEqual(h.calls.slice(1).map((c) => [c.path, c.priority]), [['e', 'high'], ['g', 'high']]);
  assert.equal(s.stats().activeRequests, 3);
  h.openAll();
  assert.deepEqual(await read, bytes(4));
  assert.deepEqual(await other, bytes(6));
  await settle();
  assert.deepEqual(h.calls.slice(3).map((c) => [c.path, c.priority]), [['b', 'low']]);
});

test('a reader that joins while the version is on its way raises the fetch', async () => {
  const h = host({ a: bytes(1) });
  const s = createStore({ fetch: h.fetch, persist: false, storage: null });
  s.prefetch(['a']);
  const read = s.load('a');
  await settle();
  assert.deepEqual(h.calls.map((c) => c.path), ['__version']);
  h.openAll();
  await settle();
  assert.deepEqual(h.calls.slice(1).map((c) => [c.path, c.priority]), [['a', 'high']]);
  h.openAll();
  assert.deepEqual(await read, bytes(1));
});

test('dropAhead forgets unsent GETs ahead, leaves the rest, and a dropped name can come back', async () => {
  const h = host({ 'a.adt': bytes(1), 'b.adt': bytes(2), 'c.blp': bytes(3), 'x.adt': bytes(4) });
  const s = versioned(h, { maxRequests: 1, maxLowRequests: 1 });
  s.prefetch(['x.adt'], { ahead: true }); // sent, so it finishes
  s.prefetch(['a.adt'], { ahead: true });
  const list = s.prefetch(['c.blp']); // the page's own list stays
  const b = s.prefetch(['b.adt'], { ahead: true });
  await settle();
  s.dropAhead();
  await b;
  for (let i = 0; i < 4; i++) {
    h.openAll();
    await settle();
  }
  await list;
  assert.deepEqual(h.calls.map((c) => c.path).sort(), ['c.blp', 'x.adt']);
  assert.equal(s.stats().errors, 0);
  assert.equal(s.landed(), 2);
  s.ready(['a.adt']);
  await settle();
  h.openAll();
  await settle();
  assert.equal(s.ready(['a.adt']), true);
});

/// A persistent cache a test can look into, and a host that answers at once.
function diskCache() {
  const disk = new Map();
  return {
    disk,
    caches: {
      keys: async () => [],
      delete: async () => {},
      open: async () => ({
        match: async (u) => (disk.has(u) ? new Response(disk.get(u)) : undefined),
        put: async (u, r) => disk.set(u, new Uint8Array(await r.arrayBuffer())),
      }),
    },
  };
}

function instantHost(files) {
  const calls = [];
  const fetch = async (url) => {
    const path = decodeURIComponent(url.slice('/data/'.length).split('?')[0]);
    calls.push(path);
    if (path === '__version') return new Response('v1');
    return files[path] ? new Response(files[path]) : new Response('', { status: 404 });
  };
  return { fetch, calls };
}

test('a pinned name outlives eviction that takes older and newer neighbours', async () => {
  const h = host({ pin: bytes(1, 1, 1, 1), a: bytes(2, 2, 2, 2), b: bytes(3, 3, 3, 3) });
  const s = versioned(h, { budgetBytes: 8 });
  const first = s.prefetch(['pin'], { pin: true });
  await settle();
  h.openAll();
  await first;
  const rest = s.prefetch(['a']);
  await settle();
  h.openAll();
  await rest;
  const last = s.prefetch(['b']);
  await settle();
  h.openAll();
  await last;
  assert.equal(s.has('pin'), true, 'the oldest entry, but pinned');
  assert.equal(s.has('a'), false, 'the oldest unpinned one goes instead');
  assert.equal(s.has('b'), true);
  assert.equal(s.stats().evicted, 1);
});

test('a fill reaches the disk and leaves memory alone', async () => {
  const { disk, caches } = diskCache();
  const h = instantHost({ 'a.blp': bytes(1), 'b.blp': bytes(2) });
  const s = createStore({ fetch: h.fetch, caches, storage: null, version: 'v1' });
  assert.equal(await s.fill(['A.blp', 'b.blp', 'gone.blp'], { idleMs: 0 }), true);
  await settle();
  assert.deepEqual(disk.get('/data/a.blp'), bytes(1));
  assert.deepEqual(disk.get('/data/b.blp'), bytes(2));
  assert.equal(s.stats().memBytes, 0, 'nothing held in memory');
  assert.equal(s.has('a.blp'), false);
  assert.deepEqual(await s.load('a.blp'), bytes(1), 'a later reader finds it');
});

test('a fill skips what is on disk and does not walk a finished tag again', async () => {
  const { disk, caches } = diskCache();
  disk.set('/data/a.blp', bytes(9));
  const h = instantHost({ 'a.blp': bytes(1), 'b.blp': bytes(2) });
  const kv = new Map();
  const storage = { getItem: (k) => kv.get(k) ?? null, setItem: (k, v) => kv.set(k, v) };
  const s = createStore({ fetch: h.fetch, caches, storage, version: 'v1' });
  assert.equal(await s.fill(['a.blp', 'b.blp'], { tag: 'ui', idleMs: 0 }), true);
  assert.equal(h.calls.includes('a.blp'), false, 'already on disk: no request');
  assert.equal(h.calls.includes('b.blp'), true);

  const again = createStore({ fetch: h.fetch, caches, storage, version: 'v1' });
  const before = h.calls.length;
  assert.equal(await again.fill(['a.blp', 'c.blp'], { tag: 'ui', idleMs: 0 }), false);
  assert.equal(h.calls.length, before, 'a finished tag sends nothing');

  const changed = createStore({ fetch: h.fetch, caches, storage, version: 'v2' });
  assert.equal(await changed.fill(['b.blp'], { tag: 'ui', idleMs: 0 }), true, 'a new install walks again');
});

test('a fill with no persistent cache does nothing', async () => {
  const h = instantHost({ 'a.blp': bytes(1) });
  const s = createStore({ fetch: h.fetch, persist: false, storage: null, version: 'v1' });
  assert.equal(await s.fill(['a.blp'], { idleMs: 0 }), false);
  assert.equal(h.calls.includes('a.blp'), false);
});

test('a fill waits while a reader or a fetch ahead wants the network', async () => {
  const { caches } = diskCache();
  const h = host({ 'x.m2': bytes(1), 'a.blp': bytes(2) });
  const s = createStore({ fetch: h.fetch, caches, storage: null, version: 'v1' });
  const reader = s.load('x.m2');
  await settle();
  h.openAll();
  await settle();
  const filling = s.fill(['a.blp'], { workers: 1, idleMs: 1 });
  for (let i = 0; i < 5; i++) await settle();
  assert.equal(h.calls.some((c) => c.path === 'a.blp') && s.reading() > 0, false, 'held back while a reader waits');
  for (let i = 0; i < 40; i++) {
    h.openAll();
    await settle();
    await new Promise((r) => setTimeout(r, 2));
  }
  await reader;
  assert.equal(await filling, true);
  assert.equal(h.calls.some((c) => c.path === 'a.blp'), true, 'then it goes');
});

test('a data root names its scope: its locale under /data/__l, else none', () => {
  assert.equal(dataScope('/data'), '');
  assert.equal(dataScope('https://example.test/data'), '');
  assert.equal(dataScope('/data/__l/esES'), 'esES');
  assert.equal(dataScope('/data/__l/esMX/'), 'esMX');
  const plain = scopeNames('');
  const es = scopeNames('esES');
  assert.equal(plain.cache('v1'), 'wenilla-data-v1', 'plain /data keeps the name it had');
  assert.equal(es.cache('v1'), 'wenilla-data-esES-v1');
  assert.deepEqual([plain.learned, plain.filled], ['wenilla:data:learned', 'wenilla:data:filled']);
  assert.deepEqual([es.learned, es.filled], ['wenilla:data:learned:esES', 'wenilla:data:filled:esES']);
});

/// A Cache API holding `names`, recording what a store opens and deletes.
function namedCaches(names) {
  const kept = new Set(names);
  const opened = [];
  return {
    kept,
    opened,
    caches: {
      keys: async () => [...kept],
      delete: async (n) => kept.delete(n),
      open: async (n) => {
        opened.push(n);
        kept.add(n);
        return { match: async () => undefined, put: async () => {} };
      },
    },
  };
}

test('opening a locale\'s cache drops only that locale\'s older copies', async () => {
  const existing = [
    'wenilla-data-v0', // the default root's older copy
    'wenilla-data-v1', // the default root's copy from before locales
    'wenilla-data-esES-v0',
    'wenilla-data-esMX-v9',
    'other-cache',
  ];
  const es = namedCaches(existing);
  const h = instantHost({ 'a.blp': bytes(1) });
  const fetch = (url) => h.fetch(url.replace('/data/__l/esES/', '/data/'));
  await createStore({ fetch, caches: es.caches, storage: null, base: '/data/__l/esES', version: 'v1' }).load('a.blp');
  assert.deepEqual(es.opened, ['wenilla-data-esES-v1']);
  assert.deepEqual(
    [...es.kept].sort(),
    ['other-cache', 'wenilla-data-esES-v1', 'wenilla-data-esMX-v9', 'wenilla-data-v0', 'wenilla-data-v1'],
  );

  const plain = namedCaches(existing);
  await createStore({ fetch: h.fetch, caches: plain.caches, storage: null, version: 'v1' }).load('a.blp');
  assert.deepEqual(plain.opened, ['wenilla-data-v1'], 'an enUS player keeps the cache they had');
  assert.deepEqual(
    [...plain.kept].sort(),
    ['other-cache', 'wenilla-data-esES-v0', 'wenilla-data-esMX-v9', 'wenilla-data-v1'],
  );
});

test('learned names and filled tags are kept per locale', async (t) => {
  const kv = new Map();
  const storage = { getItem: (k) => kv.get(k) ?? null, setItem: (k, v) => kv.set(k, v) };
  t.mock.timers.enable({ apis: ['setTimeout'] });
  const es = createStore({ fetch: host({}).fetch, persist: false, storage, base: '/data/__l/esES' });
  es.get('Interface\Spanish.blp');
  t.mock.timers.tick(2000);
  const en = createStore({ fetch: host({}).fetch, persist: false, storage });
  en.get('Interface\English.blp');
  t.mock.timers.tick(2000);
  assert.deepEqual(
    createStore({ fetch: host({}).fetch, persist: false, storage, base: '/data/__l/esES' }).learned('boot'),
    ['Interface\Spanish.blp'],
  );
  assert.deepEqual(createStore({ fetch: host({}).fetch, persist: false, storage }).learned('boot'), [
    'Interface\English.blp',
  ]);
  assert.ok(kv.has('wenilla:data:learned') && kv.has('wenilla:data:learned:esES'));
});

test('a fill finished for one locale is walked again for another', async (t) => {
  const kv = new Map();
  const storage = { getItem: (k) => kv.get(k) ?? null, setItem: (k, v) => kv.set(k, v) };
  const h = instantHost({ 'a.blp': bytes(1) });
  const esFetch = (url) => h.fetch(url.replace('/data/__l/esES/', '/data/'));
  const en = createStore({ fetch: h.fetch, caches: diskCache().caches, storage, version: 'v1' });
  assert.equal(await en.fill(['a.blp'], { tag: 'ui', idleMs: 0 }), true);
  const es = createStore({
    fetch: esFetch, caches: diskCache().caches, storage, version: 'v1', base: '/data/__l/esES',
  });
  assert.equal(await es.fill(['a.blp'], { tag: 'ui', idleMs: 0 }), true, 'its own tag');
  const enAgain = createStore({ fetch: h.fetch, caches: diskCache().caches, storage, version: 'v1' });
  assert.equal(await enAgain.fill(['a.blp'], { tag: 'ui', idleMs: 0 }), false, 'enUS kept its tag');
});

test('a fill first, not idle, goes ahead of queued fetches ahead and behind a reader', async () => {
  const { caches } = diskCache();
  const files = Object.fromEntries(['a', 'b', 'c', 'x', 'r'].map((k, i) => [k, bytes(i + 1)]));
  const h = host(files);
  const s = createStore({ fetch: h.fetch, caches, storage: null, version: 'v1', maxRequests: 2, maxLowRequests: 1 });
  s.prefetch(['a', 'b', 'c']); // the world set, queued low
  await settle();
  assert.deepEqual(h.calls.map((c) => c.path), ['a']);
  const filling = s.fill(['x'], { workers: 1, idle: false, first: true });
  const read = s.load('r');
  for (let i = 0; i < 3; i++) await settle();
  assert.deepEqual(h.calls.map((c) => [c.path, c.priority]), [['a', 'low'], ['r', 'high']], 'the reader first');
  for (let i = 0; i < 20 && !h.calls.some((c) => c.path === 'x'); i++) {
    h.calls.forEach((c) => c.open());
    await settle();
  }
  const order = h.calls.map((c) => c.path).filter((p) => p !== 'r');
  assert.deepEqual(order.slice(0, 2), ['a', 'x'], 'the fill jumps b and c');
  for (let i = 0; i < 20; i++) {
    h.calls.forEach((c) => c.open());
    await settle();
  }
  assert.deepEqual(await read, bytes(5));
  assert.equal(await filling, true);
});

test('filled names a finished tag; cached reads the disk without a request or a reader', async () => {
  const { disk, caches } = diskCache();
  const h = instantHost({ 'a.m2': bytes(1, 2) });
  const kv = new Map();
  const storage = { getItem: (k) => kv.get(k) ?? null, setItem: (k, v) => kv.set(k, v) };
  const s = createStore({ fetch: h.fetch, caches, storage, version: 'v1' });
  assert.equal(await s.filled('glue'), false);
  assert.equal(await s.fill(['a.m2'], { tag: 'glue', idle: false }), true);
  await settle();
  assert.equal(await s.filled('glue'), true);
  const before = h.calls.length;
  assert.deepEqual(await s.cached('A.M2'), bytes(1, 2));
  assert.equal(await s.cached('b.m2'), null);
  assert.equal(h.calls.length, before, 'no request');
  assert.equal(s.reading(), 0);
  assert.equal(disk.size, 1);
});

test('query names the version once it is known', async () => {
  const h = instantHost({ 'a.blp': bytes(1) });
  const s = createStore({ fetch: h.fetch, persist: false, storage: null });
  assert.equal(s.query(), '');
  await s.load('a.blp');
  assert.equal(s.query(), '?v=v1');
});
