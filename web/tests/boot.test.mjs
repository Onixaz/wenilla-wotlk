import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const source = (await readFile(new URL('../boot.js', import.meta.url), 'utf8'))
  .replaceAll('import.meta.url', JSON.stringify(new URL('../boot.js', import.meta.url).href));
const { boot, chooseDataRoot, missingWebGpu, pickLocale, reloadUrl, whenSettled } = await import('data:text/javascript;base64,' + Buffer.from(source).toString('base64'));
const wasmBytes = new Uint8Array([0, 97, 115, 109, 1, 0, 0, 0]);
const deferred = () => {
  let resolve;
  const promise = new Promise(r => { resolve = r; });
  return { promise, resolve };
};

function setup(t, fetch) {
  const children = new Map();
  const element = () => ({
    style: {}, classList: { add() {} }, remove() {}, appendChild() {},
    querySelector(selector) {
      if (!children.has(selector)) children.set(selector, element());
      return children.get(selector);
    },
  });
  for (const [key, value] of Object.entries({
    fetch, location: { search: '' }, window: {}, navigator: { gpu: {} },
    document: { createElement: element, body: { appendChild() {} } },
    setTimeout: () => 0, clearTimeout: () => {},
  })) {
    const descriptor = Object.getOwnPropertyDescriptor(globalThis, key);
    Object.defineProperty(globalThis, key, { configurable: true, writable: true, value });
    t.after(() => descriptor ? Object.defineProperty(globalThis, key, descriptor) : delete globalThis[key]);
  }
  return children;
}

for (const mime of ['application/wasm', 'application/octet-stream', 'application/wasm; charset=utf-8']) {
  test(`compilation overlaps prefetch and init waits (${mime})`, async t => {
    const manifest = deferred();
    const compiled = deferred();
    const original = WebAssembly[mime === 'application/wasm' ? 'compileStreaming' : 'compile'];
    const method = mime === 'application/wasm' ? 'compileStreaming' : 'compile';
    t.mock.method(WebAssembly, method, async input => {
      const module = await original.call(WebAssembly, input);
      compiled.resolve();
      return module;
    });
    setup(t, async url => String(url).endsWith('boot-manifest.json') ? manifest.promise :
      new Response(wasmBytes, { headers: { 'content-type': mime } }));
    let started = false;
    const result = boot(async ({ module_or_path }) => {
      started = true;
      assert.ok(module_or_path instanceof WebAssembly.Module);
      return { memory: 'bound by caller' };
    });
    await compiled.promise;
    assert.equal(started, false);
    manifest.resolve(new Response(JSON.stringify({ names: [] })));
    assert.deepEqual(await result, { memory: 'bound by caller' });
  });
}

test('download failure is reported while prefetch is still pending', async t => {
  const manifest = deferred();
  const children = setup(t, async url => String(url).endsWith('boot-manifest.json') ? manifest.promise :
    new Response('unavailable', { status: 503 }));
  await assert.rejects(boot(() => assert.fail('must not initialize')), /HTTP 503/);
  assert.match(children.get('.stage').textContent, /client failed to start.*HTTP 503/);
  manifest.resolve(new Response('{}'));
});

test('missing manifest degrades to ordinary startup', async t => {
  setup(t, async url => String(url).endsWith('boot-manifest.json') ? new Response('', { status: 404 }) :
    new Response(wasmBytes, { headers: { 'content-type': 'application/wasm' } }));
  assert.equal(await boot(async () => 'started'), 'started');
});

test('invalid wasm reports compilation failure and never starts the client', async t => {
  const children = setup(t, async url => String(url).endsWith('boot-manifest.json') ? new Response('{}') :
    new Response('invalid', { headers: { 'content-type': 'application/wasm' } }));
  await assert.rejects(boot(() => assert.fail('must not initialize')), WebAssembly.CompileError);
  assert.match(children.get('.stage').textContent, /client failed to start/);
});

test('a WebGPU build without navigator.gpu says why, by cause', async t => {
  const set = (key, value) => {
    const descriptor = Object.getOwnPropertyDescriptor(globalThis, key);
    Object.defineProperty(globalThis, key, { configurable: true, writable: true, value });
    t.after(() => descriptor ? Object.defineProperty(globalThis, key, descriptor) : delete globalThis[key]);
  };
  set('location', { origin: 'http://192.168.1.231:8090', search: '' });
  set('navigator', {});
  set('isSecureContext', false);
  assert.match(missingWebGpu(), /secure page.*192\.168\.1\.231:8090.*unsafely-treat-insecure-origin-as-secure/);
  set('isSecureContext', true);
  assert.match(missingWebGpu(), /WebGPU turned off.*chrome:\/\/gpu/);
  set('document', { querySelector: () => ({ content: 'webgl2' }) });
  assert.equal(missingWebGpu(), null, 'a WebGL2 build does not need it');
  set('document', { querySelector: () => null });
  set('navigator', { gpu: {} });
  assert.equal(missingWebGpu(), null);
});

test('boot stops before any download when WebGPU is missing', async t => {
  const fetched = [];
  const children = setup(t, async url => { fetched.push(String(url)); return new Response(''); });
  Object.defineProperty(globalThis, 'navigator', { configurable: true, writable: true, value: {} });
  Object.defineProperty(globalThis, 'isSecureContext', { configurable: true, writable: true, value: false });
  t.after(() => { delete globalThis.isSecureContext; });
  await assert.rejects(boot(() => assert.fail('must not initialize')), /secure page/);
  assert.deepEqual(fetched, []);
  assert.match(children.get('.stage').textContent, /secure page/);
});

test('a screen settles only after the client\'s reads stay at zero', async () => {
  let reading = 2;
  let settled = false;
  const done = whenSettled({ reading: () => reading }, { quietMs: 40, everyMs: 5 }).then(() => {
    settled = true;
  });
  await new Promise(r => setTimeout(r, 80));
  assert.equal(settled, false, 'reads in flight hold the cover');
  reading = 0;
  await done;
  assert.equal(settled, true);
});

test('no store has nothing to wait for', async () => {
  await whenSettled(undefined);
});

test('the locale is the URL\'s, then the saved one, then the browser\'s, then the host\'s', () => {
  const listing = { default: 'enUS', locales: ['enUS', 'esES', 'esMX'] };
  const pick = (o) => pickLocale(listing, o);
  assert.equal(pick({}), 'enUS');
  assert.equal(pick({ query: 'esMX', saved: 'esES', languages: ['es-ES'] }), 'esMX');
  assert.equal(pick({ query: 'frFR', saved: 'esES' }), 'esES', 'an uninstalled locale does not count');
  assert.equal(pick({ query: 'eses' }), 'esES', 'any case');
  assert.equal(pick({ saved: 'esES', languages: ['en-US'] }), 'esES');
  assert.equal(pick({ saved: 'deDE', languages: ['fr-FR', 'es-MX', 'en-US'] }), 'esMX');
  assert.equal(pick({ languages: ['es'] }), 'esES', 'a language alone: the first installed in table order');
  assert.equal(pick({ languages: ['es-ES'] }), 'esES');
  assert.equal(pick({ languages: ['es-AR'] }), 'esES');
  assert.equal(pick({ languages: ['en-GB'] }), 'enUS', 'the default when it speaks the language');
  assert.equal(pick({ languages: ['de-DE', 'ja'] }), 'enUS');
  const noMx = { default: 'enUS', locales: ['enUS', 'esES'] };
  assert.equal(pickLocale(noMx, { languages: ['es-MX'] }), 'esES', 'es-MX with no esMX');
  assert.equal(pickLocale(noMx, { query: 'esMX' }), 'enUS');
});

/// `chooseDataRoot` against a host answering `/data/__locales` with `listing` (or `status`).
async function rootFor(t, { listing, status = 200, search = '', saved = null, languages = ['en-US'] }) {
  const store = new Map(saved ? [['wenilla:locale', saved]] : []);
  for (const [key, value] of Object.entries({
    window: {},
    navigator: { languages },
    localStorage: { getItem: (k) => store.get(k) ?? null },
  })) {
    const descriptor = Object.getOwnPropertyDescriptor(globalThis, key);
    Object.defineProperty(globalThis, key, { configurable: true, writable: true, value });
    t.after(() => descriptor ? Object.defineProperty(globalThis, key, descriptor) : delete globalThis[key]);
  }
  const asked = [];
  const fetch = async (url) => {
    asked.push(url);
    if (status === 'throw') throw new TypeError('network');
    return new Response(JSON.stringify(listing), { status });
  };
  const base = await chooseDataRoot(new URLSearchParams(search), fetch);
  assert.deepEqual(asked, ['/data/__locales']);
  assert.equal(window.__wenilla_data_base, base);
  return base;
}

test('the data root is plain /data for the default locale and /data/__l/<locale> otherwise', async (t) => {
  const listing = { default: 'enUS', locales: ['enUS', 'esES'] };
  assert.equal(await rootFor(t, { listing }), '/data');
  assert.deepEqual(window.__wenilla_locales, { ...listing, current: 'enUS' });
  assert.equal(await rootFor(t, { listing, search: '?locale=esES' }), '/data/__l/esES');
  assert.deepEqual(window.__wenilla_locales, { ...listing, current: 'esES' });
  assert.equal(await rootFor(t, { listing, saved: 'esES' }), '/data/__l/esES');
  assert.equal(await rootFor(t, { listing, languages: ['es-ES', 'en'] }), '/data/__l/esES');
  assert.equal(await rootFor(t, { listing: { default: 'esES', locales: ['enUS', 'esES'] }, languages: ['es'] }), '/data');
});

test('a host without __locales is plain /data, silently', async (t) => {
  assert.equal(await rootFor(t, { listing: null, status: 404, search: '?locale=esES' }), '/data');
  assert.equal(window.__wenilla_locales, undefined);
  assert.equal(await rootFor(t, { listing: null, status: 'throw' }), '/data');
  assert.equal(await rootFor(t, { listing: { locales: 'nope' } }), '/data');
});

test('the boot prefetch asks the chosen locale\'s root', async t => {
  const asked = [];
  setup(t, async url => {
    asked.push(String(url));
    if (String(url) === '/data/__locales') {
      return new Response(JSON.stringify({ default: 'enUS', locales: ['enUS', 'esES'] }));
    }
    if (String(url).endsWith('manifest.json')) return new Response(JSON.stringify({ names: [] }));
    if (String(url).startsWith('/data/')) return new Response('esES');
    return new Response(wasmBytes, { headers: { 'content-type': 'application/wasm' } });
  });
  globalThis.location.search = '?locale=esES';
  await boot(async () => ({ memory: null }));
  const data = asked.filter(u => u.startsWith('/data/'));
  assert.equal(data[0], '/data/__locales');
  assert.ok(data.includes('/data/__l/esES/__index'), data.join(' '));
  assert.ok(data.includes('/data/__l/esES/__locale'), data.join(' '));
  assert.ok(!data.includes('/data/__index') && !data.includes('/data/__locale'), data.join(' '));
});

test('a locale change in game is saved and offers a reload that drops ?locale=', async t => {
  const saved = new Map();
  const appended = [];
  setup(t, async url => String(url) === '/data/__locales'
    ? new Response(JSON.stringify({ default: 'enUS', locales: ['enUS', 'esES'] }))
    : String(url).endsWith('manifest.json') ? new Response(JSON.stringify({ names: [] }))
    : String(url).startsWith('/data/') ? new Response('')
    : new Response(wasmBytes, { headers: { 'content-type': 'application/wasm' } }));
  globalThis.localStorage = { getItem: k => saved.get(k) ?? null, setItem: (k, v) => saved.set(k, v) };
  t.after(() => delete globalThis.localStorage);
  globalThis.document.body.appendChild = el => appended.push(el);
  await boot(async () => ({ memory: null }));
  assert.equal(window.__wenilla_locales.current, 'enUS');
  window.__wenilla_locale('esES');
  assert.equal(saved.get('wenilla:locale'), 'esES');
  assert.ok(appended.some(el => el.id === 'wenilla-reload'), 'the reload is offered');
  assert.equal(reloadUrl('https://h.test/play?locale=enUS&x=1'), 'https://h.test/play?x=1');
  assert.equal(reloadUrl('https://h.test/?locale=esES'), 'https://h.test/');
});

test('the cover reads Downloading over the sliding bar until the client starts, as the later covers do', async t => {
  const gate = deferred();
  const children = setup(t, async url => {
    const u = String(url);
    if (u.endsWith('manifest.json')) return new Response(JSON.stringify({ names: [] }));
    if (u === '/data/__index') return gate.promise;
    if (u.startsWith('/data/')) return new Response('', { status: 404 });
    return new Response(wasmBytes, { headers: { 'content-type': 'application/wasm', 'content-length': '8' } });
  });
  const turn = () => new Promise(r => setImmediate(r));
  const started = boot(async () => ({ memory: null }));
  for (let i = 0; i < 50; i++) await turn();
  // The client is in, the boot's data is not: still the one title, no count, no percentage.
  assert.equal(children.get('.stage').textContent, 'Downloading…');
  assert.equal(children.has('.data'), false);
  gate.resolve(new Response('{}'));
  await started;
  assert.equal(children.get('.stage').textContent, 'Starting…');
});

test('Character Create shows its cover only while the creation set is still coming in', async t => {
  const children = setup(t, async url => String(url).endsWith('manifest.json')
    ? new Response(JSON.stringify({ names: [] }))
    : String(url).startsWith('/data/') ? new Response('', { status: 404 })
    : new Response(wasmBytes, { headers: { 'content-type': 'application/wasm' } }));
  await boot(async () => ({ memory: null }));
  let timers = [];
  let id = 0;
  globalThis.setTimeout = (f, ms) => (timers.push({ f, ms, id: ++id }), id);
  globalThis.clearTimeout = n => { timers = timers.filter(x => x.id !== n); };
  const covers = [];
  globalThis.document.body.appendChild = el => covers.push(el);
  const create = globalThis.document.createElement;
  globalThis.document.createElement = () => {
    const el = create();
    el.classes = new Set();
    el.classList = { add: c => el.classes.add(c) };
    return el;
  };
  window.__wenilla_data = undefined; // nothing left to read: the screen counts as settled
  const fire = () => { const due = timers; timers = []; due.forEach(x => x.f()); };

  // Already on disk: ready lands inside the grace, no cover.
  window.__wenilla_progress('charcreate');
  window.__wenilla_progress('charcreate-ready');
  fire();
  assert.equal(covers.length, 0);

  // Still downloading: the cover shows, with the later covers' one title, until ready.
  window.__wenilla_progress('charcreate');
  assert.equal(timers[0].ms, 300);
  fire();
  assert.equal(covers.length, 1);
  assert.equal(children.get('.stage').textContent, 'Loading character creation…');
  window.__wenilla_progress('charcreate');
  assert.equal(timers.length, 1, 'one cover at a time (its timeout only)');
  window.__wenilla_progress('charcreate-ready');
  await new Promise(r => setImmediate(r));
  assert.ok(covers[0].classes.has('gone'));
});
