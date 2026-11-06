import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const source = (await readFile(new URL('../boot.js', import.meta.url), 'utf8'))
  .replaceAll('import.meta.url', JSON.stringify(new URL('../boot.js', import.meta.url).href));
const { boot, missingWebGpu, whenSettled } = await import('data:text/javascript;base64,' + Buffer.from(source).toString('base64'));
const wasmBytes = new Uint8Array([0, 97, 115, 109, 1, 0, 0, 0]);
const deferred = () => {
  let resolve;
  const promise = new Promise(r => { resolve = r; });
  return { promise, resolve };
};

function setup(t, fetch) {
  const children = new Map();
  const element = () => ({
    style: {}, classList: { add() {} }, remove() {},
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
