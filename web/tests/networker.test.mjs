import assert from 'node:assert/strict';
import test from 'node:test';
import { createWorkerFetch } from '../networker.js';

/// A Worker double: records what the page posts, and answers through `reply` / `crash`.
function fakeWorker() {
  const posted = [];
  let made = null;
  class FakeWorker extends EventTarget {
    constructor(url, opts) {
      super();
      this.url = String(url);
      this.opts = opts;
      this.terminated = false;
      made = this;
    }
    postMessage(msg) {
      structuredClone(msg); // what a real Worker would refuse, this refuses too
      posted.push(msg);
    }
    terminate() {
      this.terminated = true;
    }
  }
  return {
    Worker: FakeWorker,
    posted,
    get worker() { return made; },
    reply(data) {
      made.dispatchEvent(Object.assign(new Event('message'), { data }));
    },
    crash() {
      made.dispatchEvent(new Event('error'));
    },
  };
}

const neverFallback = () => assert.fail('the page must not fetch for itself');

test('the Worker reads the body and the page gets a Response holding it', async () => {
  const w = fakeWorker();
  const fetch = createWorkerFetch({ Worker: w.Worker, fallback: neverFallback, base: 'https://h.test/play' });
  assert.equal(w.worker.opts.type, 'module');
  const answer = fetch('/data/a.dbc?v=1', { priority: 'low' });
  assert.deepEqual(w.posted, [{ id: 0, url: 'https://h.test/data/a.dbc?v=1', init: { priority: 'low' } }]);
  const body = new Uint8Array([1, 2, 3]).buffer;
  w.reply({ id: 0, status: 200, statusText: 'OK', headers: [['content-type', 'application/x-test']], body });
  const r = await answer;
  assert.equal(r.status, 200);
  assert.equal(r.headers.get('content-type'), 'application/x-test');
  assert.deepEqual([...new Uint8Array(await r.arrayBuffer())], [1, 2, 3]);
});

test('answers match their requests in any order, and a 404 is a 404', async () => {
  const w = fakeWorker();
  const fetch = createWorkerFetch({ Worker: w.Worker, fallback: neverFallback, base: 'https://h.test/' });
  const a = fetch('/data/a');
  const b = fetch('/data/b');
  w.reply({ id: 1, status: 404, statusText: '', headers: [], body: new ArrayBuffer(0) });
  w.reply({ id: 0, status: 200, statusText: '', headers: [], body: new TextEncoder().encode('A').buffer });
  assert.equal((await b).status, 404);
  assert.equal(await (await a).text(), 'A');
});

test('a status that carries no body still resolves', async () => {
  const w = fakeWorker();
  const fetch = createWorkerFetch({ Worker: w.Worker, fallback: neverFallback, base: 'https://h.test/' });
  const r = fetch('/x');
  w.reply({ id: 0, status: 204, statusText: '', headers: [], body: new ArrayBuffer(0) });
  assert.equal((await r).status, 204);
});

test('a network failure in the Worker rejects like fetch does', async () => {
  const w = fakeWorker();
  const fetch = createWorkerFetch({ Worker: w.Worker, fallback: neverFallback, base: 'https://h.test/' });
  const r = fetch('/x');
  w.reply({ id: 0, error: 'Failed to fetch' });
  await assert.rejects(r, (e) => e instanceof TypeError && /Failed to fetch/.test(e.message));
});

test('the batch request crosses whole: method, JSON body, headers as a record; the signal stays', async () => {
  const w = fakeWorker();
  const fetch = createWorkerFetch({ Worker: w.Worker, fallback: neverFallback, base: 'https://h.test/' });
  fetch('/data/__batch', {
    method: 'POST',
    headers: new Headers({ 'content-type': 'application/json' }),
    body: '["a"]',
    priority: 'high',
    signal: new AbortController().signal,
  });
  assert.deepEqual(w.posted[0].init, {
    method: 'POST', body: '["a"]', priority: 'high', headers: { 'content-type': 'application/json' },
  });
});

test('a Worker whose script never loads hands every request, waiting or later, to the page', async () => {
  const w = fakeWorker();
  const asked = [];
  const fetch = createWorkerFetch({
    Worker: w.Worker,
    base: 'https://h.test/',
    fallback: async (url) => {
      asked.push(String(url));
      return new Response('page');
    },
  });
  const waiting = fetch('/data/a');
  w.crash();
  assert.equal(w.worker.terminated, true);
  assert.equal(await (await waiting).text(), 'page');
  assert.equal(await (await fetch('/data/b')).text(), 'page');
  assert.deepEqual(asked, ['/data/a', '/data/b']);
  assert.equal(w.posted.length, 1, 'nothing more goes to the dead Worker');
});

test('without module Workers the page fetches for itself', () => {
  const fallback = () => 'page';
  assert.equal(createWorkerFetch({ Worker: undefined, fallback }), fallback);
  class Refuses { constructor() { throw new TypeError('module workers unsupported'); } }
  assert.equal(createWorkerFetch({ Worker: Refuses, fallback }), fallback);
});
