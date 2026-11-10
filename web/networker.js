// networker.js — the data store's network, read off the main thread.
//
// Why: the client's synchronous chain reads are synchronous XHRs, which block the main thread
// until their answer arrives. Every request to the host shares one HTTP/2 (or HTTP/3) connection,
// and a response body nobody reads holds that connection's receive window: with fetches ahead in
// flight (the world prefetch's multi-MB sounds and screens) and the main thread blocked, their
// bodies cannot drain, the window fills, and the blocking XHR's own answer never arrives — the tab
// freezes for good. Measured on play.honeywow.net/es with the data uncached at the CDN: 2 of 3
// loads hung with 8 fetches ahead and one XHR open for 30+ s; 0 of 3 with HTTP/2 and QUIC off.
//
// So the store's `fetch` runs in a Worker, which reads each body to its end whatever the main
// thread is doing and hands the bytes over as a transferred ArrayBuffer (no copy). The main thread
// gets a `Response` built from those bytes. A page that cannot start the Worker, or whose Worker
// script fails to load, uses its own `fetch`, as before.
//
// This one file is both halves: loaded as a module Worker it answers requests; imported on the
// page, `createWorkerFetch()` makes the store's `fetch`.

const inWorker =
  typeof WorkerGlobalScope !== 'undefined' && globalThis instanceof WorkerGlobalScope;

if (inWorker) {
  globalThis.onmessage = async ({ data: { id, url, init } }) => {
    try {
      const r = await fetch(url, init);
      const body = await r.arrayBuffer();
      globalThis.postMessage(
        { id, status: r.status, statusText: r.statusText, headers: [...r.headers], body },
        [body],
      );
    } catch (e) {
      globalThis.postMessage({ id, error: String((e && e.message) || e) });
    }
  };
}

/// Statuses whose `Response` cannot carry a body.
const NULL_BODY = new Set([101, 103, 204, 205, 304]);

/// The request options that cross to the Worker: those `fetch` reads that structured clone can
/// carry (`headers` as a plain record).
function cloneableInit(init = {}) {
  const out = {};
  for (const k of ['method', 'body', 'priority', 'cache', 'credentials', 'mode', 'redirect']) {
    if (init[k] !== undefined) out[k] = init[k];
  }
  if (init.headers !== undefined) out.headers = Object.fromEntries(new Headers(init.headers));
  return out;
}

/// A `fetch(input, init)` whose bodies are read in a Worker; resolves to a `Response` holding the
/// whole body. `opts.Worker` and `opts.fallback` are for tests; `opts.url` is the worker script.
export function createWorkerFetch(opts = {}) {
  const fallback = opts.fallback ?? ((...a) => globalThis.fetch(...a));
  const WorkerCtor = opts.Worker ?? globalThis.Worker;
  const base = opts.base ?? globalThis.location?.href;
  let worker = null;
  try {
    if (WorkerCtor) worker = new WorkerCtor(opts.url ?? new URL(import.meta.url), { type: 'module' });
  } catch (_) {
    worker = null; // no module Workers here: the page reads its own bodies
  }
  if (!worker) return fallback;

  const pending = new Map(); // id -> { input, init, resolve, reject }
  let next = 0;
  let broken = false;

  // The script did not load (an older host without this file) or the Worker died: every request
  // still waiting, and every later one, goes the page's way.
  const fail = () => {
    if (broken) return;
    broken = true;
    worker.terminate?.();
    for (const p of pending.values()) fallback(p.input, p.init).then(p.resolve, p.reject);
    pending.clear();
  };
  worker.addEventListener('error', fail);
  worker.addEventListener('messageerror', fail);
  worker.addEventListener('message', ({ data }) => {
    const p = pending.get(data.id);
    if (!p) return;
    pending.delete(data.id);
    if (data.error !== undefined) {
      p.reject(new TypeError(data.error));
      return;
    }
    const body = NULL_BODY.has(data.status) ? null : data.body;
    p.resolve(new Response(body, { status: data.status, statusText: data.statusText, headers: data.headers }));
  });

  return (input, init) => {
    if (broken) return fallback(input, init);
    return new Promise((resolve, reject) => {
      const id = next++;
      pending.set(id, { input, init, resolve, reject });
      try {
        const url = new URL(String(input), base).href;
        worker.postMessage({ id, url, init: cloneableInit(init) });
      } catch (_) {
        // Something in `init` the Worker cannot take: this one request goes the page's way.
        pending.delete(id);
        fallback(input, init).then(resolve, reject);
      }
    });
  };
}
