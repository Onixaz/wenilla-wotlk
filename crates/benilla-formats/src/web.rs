//! Web-target chain plumbing: `wasm32-unknown-unknown` has no filesystem, so [`crate::Chain`]
//! answers every read/existence/listing question with an HTTP call against the Data URL scheme a
//! companion web host (`wenilla-host`, Lane H) serves — `GET {origin}/data/{encoded name}`,
//! `HEAD` for existence, `GET /data/__index` for the name list. This module is that HTTP shim.
//!
//! Every read goes through the page's data store first (`web/datastore.js`,
//! `window.__wenilla_data`), which holds bytes fetched ahead: a synchronous read the store can
//! answer never becomes a synchronous `XMLHttpRequest`, and an asynchronous one joins a fetch
//! already in flight. `expand` is what fills it as the world streams: the moment a file's bytes
//! arrive, the names its loader reads next ([`crate::web_deps`]) are fetched ahead, so a loader's
//! serial reads (a WMO's groups, an M2's skin and `.anim` files) land in one round trip.
//!
//! [`encode_name`] and [`index_key`] are plain string math with no browser dependency, so they
//! are exercised natively (`tests/web_names.rs`); everything else needs `web-sys` and only
//! compiles its body on `wasm32` ([`prefetch_parents`] is a no-op elsewhere).

/// Percent-encode `name` exactly like JavaScript's `encodeURIComponent`: the unreserved set is
/// `A-Za-z0-9-_.!~*'()`; every other byte, including `\` (chain names are internally
/// backslash-separated), becomes `%XX` uppercase-hex. This is the client half of the Data URL
/// scheme's "the full name percent-encoded as one component" rule — the web host's decode must be
/// this function's exact inverse, so it is kept pure and unit-tested rather than left to whatever
/// a URL-building crate happens to escape.
pub fn encode_name(name: &str) -> String {
    const UNRESERVED: &[u8] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.!~*'()";
    let mut out = String::with_capacity(name.len());
    for &byte in name.as_bytes() {
        if UNRESERVED.contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The chain's key for a name, as the MPQ hash matches names: any case, `/` ≡ `\`. Both the name
/// index and the URLs use it, so every spelling of one file is one HTTP cache entry and one store
/// entry (`dataKey` in `web/datastore.js` is its twin).
pub fn index_key(name: &str) -> String {
    name.replace('/', "\\").to_ascii_lowercase()
}

/// Fetch `names` ahead, each with whatever its loader reads after it, walked as their bytes land
/// (the page's `expand`): the terrain streamer's tiles ahead of the viewer. Nothing off the page.
#[cfg(not(target_arch = "wasm32"))]
pub fn prefetch_parents<I: IntoIterator<Item = String>>(_names: I) {}

#[cfg(target_arch = "wasm32")]
mod wasm {
    use wasm_bindgen::JsValue;
    use web_sys::XmlHttpRequest;

    /// The root every chain fetch is served from: `/data` under this page's own origin. The web
    /// host answers both the client bundle and the `/data/*` routes from one process (Lane H), so
    /// there is never a cross-origin question to configure — this is the whole answer.
    pub fn data_base() -> String {
        let origin = web_sys::window()
            .expect("benilla_formats::web only runs inside a browser tab")
            .location()
            .origin()
            .expect("window.location.origin");
        format!("{origin}/data")
    }

    /// Append `name` to the page's boot-read trace, if the page armed one — the input to
    /// `web/boot-manifest.json` (see `web/boot.js`). A page that wants the trace defines
    /// `window.__wenilla_boottrace = []` before `init()` (the `?boottrace=1` switch does);
    /// every chain read then pushes its raw backslash name in true first-need order, and a
    /// person copies the deduplicated array out of the console into the manifest. Without the
    /// array this is one snapshotted `Reflect::get` for the whole session and per-call nothing —
    /// tracing must cost the boot it measures as close to zero as possible.
    pub fn trace(name: &str) {
        use wasm_bindgen::JsCast;
        thread_local! {
            static TRACE: std::cell::OnceCell<Option<js_sys::Array>> =
                const { std::cell::OnceCell::new() };
        }
        TRACE.with(|cell| {
            let arr = cell.get_or_init(|| {
                let window = web_sys::window()?;
                js_sys::Reflect::get(&window, &"__wenilla_boottrace".into())
                    .ok()?
                    .dyn_into::<js_sys::Array>()
                    .ok()
            });
            if let Some(arr) = arr {
                arr.push(&name.into());
            }
        });
    }

    /// A blocking `XMLHttpRequest` GET, returning the raw response bytes.
    ///
    /// Synchronous on purpose: [`crate::Chain::read`] is called from Bevy systems on the main
    /// thread today — `WorldAssets` and ~60 other call sites read the chain synchronously — and
    /// making the whole call chain async to reach it would ripple through every one of them. A
    /// *synchronous* `XMLHttpRequest` is the one browser primitive that can return bytes from a
    /// call that must return before the function does; the Bevy `AssetReader` path
    /// (`benilla-assets`) is already `async` end to end, so it uses `fetch` instead (no such
    /// constraint there).
    ///
    /// A sync XHR's `responseType` is stuck at the default `""` (text), so an `arraybuffer`
    /// response type — the normal way to get binary out of an XHR — isn't available here. The
    /// standard workaround, used below, is `override_mime_type("text/plain; charset=x-user-defined")`:
    /// it forces the browser to decode the response body as one code unit (0x00-0xFF) per byte
    /// instead of guessing UTF-8 and mangling anything non-ASCII into U+FFFD, so `response_text()`
    /// round-trips arbitrary binary losslessly through `& 0xFF`.
    pub fn fetch_sync(url: &str) -> std::io::Result<Vec<u8>> {
        let xhr = XmlHttpRequest::new().map_err(js_err)?;
        xhr.open_with_async("GET", url, false).map_err(js_err)?;
        xhr.override_mime_type("text/plain; charset=x-user-defined")
            .map_err(js_err)?;
        xhr.send().map_err(js_err)?;
        match xhr.status().map_err(js_err)? {
            200 => {
                let text = xhr.response_text().map_err(js_err)?.unwrap_or_default();
                Ok(text.chars().map(|c| (c as u32 & 0xff) as u8).collect())
            }
            404 => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                url.to_string(),
            )),
            status => Err(std::io::Error::other(format!("{url}: HTTP {status}"))),
        }
    }

    /// A blocking `HEAD` request: does the web host have this name? No body to decode, so no mime
    /// override is needed — just the status line.
    pub fn exists_sync(url: &str) -> bool {
        let Ok(xhr) = XmlHttpRequest::new() else {
            return false;
        };
        if xhr.open_with_async("HEAD", url, false).is_err() {
            return false;
        }
        if xhr.send().is_err() {
            return false;
        }
        xhr.status().map(|status| status == 200).unwrap_or(false)
    }

    fn js_err(e: JsValue) -> std::io::Error {
        std::io::Error::other(format!("{e:?}"))
    }

    /// The page's data store, `window.__wenilla_data` (`web/datastore.js`): its object and the
    /// three methods the client calls, looked up once. `None` on a page that predates it, where
    /// every read takes the path it took before the store existed.
    struct Store {
        obj: JsValue,
        get: js_sys::Function,
        load: js_sys::Function,
        prefetch: js_sys::Function,
    }

    thread_local! {
        static STORE: std::cell::OnceCell<Option<Store>> = const { std::cell::OnceCell::new() };
        /// The chain's name index (`GET /data/__index`), shared by every `Chain`; `None` inside
        /// when it could not be fetched or parsed, and callers fall back to asking per name.
        static INDEX: std::cell::OnceCell<Option<std::collections::HashSet<String>>> =
            const { std::cell::OnceCell::new() };
        /// Parents [`expand`] has walked lately, so one model on forty tiles is walked once.
        static WALKED: std::cell::RefCell<std::collections::HashSet<String>> =
            std::cell::RefCell::new(std::collections::HashSet::new());
    }

    /// How many parents [`WALKED`] remembers before it starts over: a revisited area walks again.
    const WALKED_MAX: usize = 8192;

    fn with_store<R>(f: impl FnOnce(&Store) -> R) -> Option<R> {
        use wasm_bindgen::JsCast;
        STORE.with(|cell| {
            cell.get_or_init(|| {
                let window = web_sys::window()?;
                let obj = js_sys::Reflect::get(&window, &"__wenilla_data".into()).ok()?;
                if !obj.is_object() {
                    return None;
                }
                let method = |name: &str| -> Option<js_sys::Function> {
                    js_sys::Reflect::get(&obj, &name.into())
                        .ok()?
                        .dyn_into()
                        .ok()
                };
                Some(Store {
                    get: method("get")?,
                    load: method("load")?,
                    prefetch: method("prefetch")?,
                    obj,
                })
            })
            .as_ref()
            .map(f)
        })
    }

    /// The store's bytes for `name` if they are in memory now, without a request; `consume`
    /// drops the store's copy. A miss is recorded by the store, which fetches the name ahead on
    /// the next visit.
    pub fn resident(name: &str, consume: bool) -> Option<Vec<u8>> {
        use wasm_bindgen::JsCast;
        with_store(|s| {
            let v = s
                .get
                .call2(&s.obj, &name.into(), &JsValue::from_bool(consume))
                .ok()?;
            Some(v.dyn_into::<js_sys::Uint8Array>().ok()?.to_vec())
        })
        .flatten()
    }

    /// Read `name` through the store: from memory, a fetch already in flight, the persistent
    /// cache, or the network. `high` is a reader waiting on it; `keep` leaves the bytes in the
    /// store for a later reader. `None` without a store; `Ok(None)` when the host has no such file.
    pub async fn load(
        name: &str,
        high: bool,
        keep: bool,
    ) -> Option<Result<Option<Vec<u8>>, String>> {
        use wasm_bindgen::JsCast;
        let promise = with_store(|s| {
            let opts = js_sys::Object::new();
            let priority = if high { "high" } else { "low" };
            let _ = js_sys::Reflect::set(&opts, &"priority".into(), &priority.into());
            let _ = js_sys::Reflect::set(&opts, &"keep".into(), &JsValue::from_bool(keep));
            s.load.call2(&s.obj, &name.into(), &opts)
        })?;
        let promise: js_sys::Promise = match promise.map(JsCast::dyn_into) {
            Ok(Ok(p)) => p,
            Ok(Err(v)) | Err(v) => return Some(Err(format!("{name}: store.load: {v:?}"))),
        };
        Some(match wasm_bindgen_futures::JsFuture::from(promise).await {
            Ok(v) if v.is_null() => Ok(None),
            Ok(v) => v
                .dyn_into::<js_sys::Uint8Array>()
                .map(|a| Some(a.to_vec()))
                .map_err(|v| format!("{name}: store.load gave {v:?}")),
            Err(e) => Err(format!("{name}: {e:?}")),
        })
    }

    /// Fetch `names` ahead into the store at low priority, without waiting.
    fn prefetch(names: &[String]) {
        if names.is_empty() {
            return;
        }
        with_store(|s| {
            let arr: js_sys::Array = names.iter().map(|n| JsValue::from_str(n)).collect();
            let _ = s.prefetch.call1(&s.obj, &arr);
        });
    }

    /// Whether the chain has `name`, from the shared name index: `None` when there is no index.
    /// The index arrives through the store when the page fetched it ahead (it is ~12 MB of JSON,
    /// which a synchronous request would hold the thread for), else by one synchronous fetch.
    pub fn index_has(name: &str) -> Option<bool> {
        INDEX.with(|cell| {
            cell.get_or_init(|| {
                let bytes = match resident("__index", true) {
                    Some(b) => b,
                    None => fetch_sync(&format!("{}/__index", data_base())).ok()?,
                };
                let names: Vec<String> = serde_json::from_slice(&bytes).ok()?;
                Some(names.iter().map(|n| super::index_key(n)).collect())
            })
            .as_ref()
            .map(|set| set.contains(&super::index_key(name)))
        })
    }

    /// Fetch ahead what `name`'s loader reads after it, from its `bytes` ([`crate::web_deps`]):
    /// the leaves straight into the store, the parents fetched, kept and walked in turn. Names the
    /// chain index lacks are dropped (an ADT layer's `_s` variant is only sometimes there).
    pub fn expand(name: &str, bytes: &[u8]) {
        let d = crate::web_deps::deps(name, bytes);
        let leaves: Vec<String> = d
            .leaves
            .into_iter()
            .filter(|n| index_has(n) != Some(false))
            .collect();
        prefetch(&leaves);
        prefetch_parents(d.parents);
    }

    /// Fetch `names` ahead and walk each one's own reads as its bytes land (see [`expand`]).
    pub fn prefetch_parents<I: IntoIterator<Item = String>>(names: I) {
        for name in names {
            if index_has(&name) == Some(false) || !first_walk(&name) {
                continue;
            }
            wasm_bindgen_futures::spawn_local(async move {
                if let Some(Ok(Some(bytes))) = load(&name, false, true).await {
                    expand(&name, &bytes);
                }
            });
        }
    }

    /// Mark `name` walked; false when it already was.
    fn first_walk(name: &str) -> bool {
        WALKED.with(|w| {
            let mut w = w.borrow_mut();
            if w.len() >= WALKED_MAX {
                w.clear();
            }
            w.insert(super::index_key(name))
        })
    }
}

#[cfg(target_arch = "wasm32")]
pub use wasm::{
    data_base, exists_sync, expand, fetch_sync, index_has, load, prefetch_parents, resident, trace,
};
