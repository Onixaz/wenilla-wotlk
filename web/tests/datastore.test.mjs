import assert from 'node:assert/strict';
import test from 'node:test';
import { createStore, dataKey, readFrames } from '../datastore.js';

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
/// `batch: false` is a host without the batch route.
function host(files, { batch = true } = {}) {
  const calls = [];
  const fetch = (url, init = {}) => {
    const path = decodeURIComponent(url.slice('/data/'.length));
    const keys = path === '__batch' ? JSON.parse(init.body) : [path];
    let open;
    const gate = new Promise((r) => (open = r));
    calls.push({ path, keys, priority: init.priority, open });
    return gate.then(() => {
      if (path === '__version') return new Response('v1');
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
const plain = (h, o = {}) => createStore({ fetch: h.fetch, persist: false, storage: null, ...o });

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
  assert.deepEqual(store.get('/data/a.blp'), bytes(7));

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
