//! `GET`/`HEAD /data/{*name}` and `GET /data/__index` — the Data URL scheme Lane A's wasm
//! `Chain` fetches against verbatim (see the plan's "Shared interfaces"): the browser build has
//! no filesystem, so every asset load the client makes becomes one of these requests, answered
//! straight from the same [`Chain`] the native client reads off disk.
//!
//! The name in the URL is percent-decoded here, not left to axum's own path-segment decoding —
//! `Chain` names use `\` as their separator (`Interface\Glues\...`), and the wildcard capture
//! would otherwise hand us a single decoded segment with no way to tell an encoded `/` (a literal
//! path separator in some other scheme) apart from an encoded `\`. Decoding the raw tail
//! ourselves and mapping `/` -> `\` afterward matches exactly what the client's `encode_name`
//! produces on the way out.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use benilla_formats::Chain;
use futures_util::StreamExt;
use tokio::sync::OnceCell;
use tower_http::compression::predicate::{DefaultPredicate, NotForContentType, Predicate};
use tower_http::compression::CompressionLayer;
use tower_http::set_header::SetResponseHeaderLayer;

/// Router state: the opened patch chain. `Chain::read`/`list` are `&self` and lock-free (see the
/// chain module doc). The serialized index is shared across requests for this mounted chain.
#[derive(Clone)]
pub struct DataState {
    pub chain: Arc<Chain>,
    index: Arc<IndexCache>,
    /// The `Data` folder's locale (`enUS`, …), which the wasm `data_locale` asks for; `None` when
    /// the chain was opened from a single `.MPQ`.
    locale: Option<Arc<str>>,
    /// The install's identity ([`data_version`]), which the page keys its persistent copy of
    /// `/data` by; `None` answers 404 and the page keeps no copy.
    version: Option<Arc<str>>,
}

/// Single-flight initialization; transient list errors are retried on the next request.
#[derive(Default)]
struct IndexCache(Arc<OnceCell<Bytes>>);

impl IndexCache {
    async fn get(
        &self,
        build: impl FnOnce() -> anyhow::Result<Vec<u8>> + Send + 'static,
    ) -> anyhow::Result<Bytes> {
        if let Some(bytes) = self.0.get() {
            return Ok(bytes.clone());
        }
        let cell = Arc::clone(&self.0);
        // The initializer owns the cell independently of this request. Dropping a request
        // cannot cancel spawn_blocking, so keep its result and single-flight guard alive too.
        tokio::spawn(async move {
            cell.get_or_try_init(|| async move {
                tokio::task::spawn_blocking(build).await?.map(Bytes::from)
            })
            .await
            .cloned()
        })
        .await?
    }
}

/// Build the `/data/*` router. Kept separate from `static_site`'s and `ws`'s so `main.rs` can
/// merge them with `Router::merge` and each test file can stand its half up alone.
pub fn router(chain: Arc<Chain>) -> Router {
    router_with_locale(chain, None)
}

/// [`router`], answering `GET /data/__locale` with `locale` — the 3.3.5a client mounts its locale
/// archives from `Data\<locale>\`, and the page has no folder to look in.
pub fn router_with_locale(chain: Arc<Chain>, locale: Option<String>) -> Router {
    router_with(chain, locale, None)
}

/// [`router_with_locale`], also answering `GET /data/__version` with `version`: the page keeps
/// its fetched game files across visits under that name, and drops them when it changes.
pub fn router_with(chain: Arc<Chain>, locale: Option<String>, version: Option<String>) -> Router {
    Router::new()
        .route("/data/__index", get(index))
        .route("/data/__locale", get(locale_route))
        .route("/data/__version", get(version_route))
        .route("/data/__batch", post(batch))
        // axum's matchit picks the more specific literal route above over this wildcard on its
        // own — registration order here doesn't matter.
        .route("/data/{*name}", get(file).head(file))
        // On the fly, not precompressed like `static_site`'s: that route serves the handful of
        // files `web-build.sh` writes and can pay brotli once at build time, while this one
        // serves an arbitrary slice of a 5 GB install nobody can enumerate ahead of time.
        // What keeps the cost sane is the predicate, not the level — see [`content_type`].
        .layer(CompressionLayer::new().compress_when(compressible()))
        // COEP `require-corp` on the document (see `static_site`) makes every subresource prove
        // it consents to being embedded. Same-origin responses pass that check without a header,
        // and today `/data` is always same-origin with the page — this is the belt to that
        // braces, so an operator who ever fronts the two from different origins gets a
        // recognisable failure instead of a world that loads with holes in it.
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("cross-origin-resource-policy"),
            HeaderValue::from_static("same-origin"),
        ))
        .with_state(DataState {
            chain,
            index: Arc::default(),
            locale: locale.map(Arc::from),
            version: version.map(Arc::from),
        })
}

/// The identity of the install under `data`: a hash of every archive's path (relative, any case),
/// size and modification time. Any patch added, removed or replaced changes it; reading the files
/// is never needed. `None` when the folder cannot be walked.
pub fn data_version(data: &std::path::Path) -> Option<String> {
    fn walk(
        dir: &std::path::Path,
        root: &std::path::Path,
        depth: u32,
        out: &mut Vec<(String, u64, u64)>,
    ) -> Option<()> {
        for entry in std::fs::read_dir(dir).ok()? {
            let entry = entry.ok()?;
            let path = entry.path();
            let meta = entry.metadata().ok()?;
            if meta.is_dir() {
                if depth > 0 {
                    walk(&path, root, depth - 1, out)?;
                }
            } else if path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("mpq"))
            {
                let rel = path
                    .strip_prefix(root)
                    .ok()?
                    .to_string_lossy()
                    .to_ascii_lowercase();
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_secs());
                out.push((rel, meta.len(), mtime));
            }
        }
        Some(())
    }
    let mut files = Vec::new();
    if data.is_dir() {
        walk(data, data, 2, &mut files)?;
    } else {
        let meta = std::fs::metadata(data).ok()?;
        let name = data.file_name()?.to_string_lossy().to_ascii_lowercase();
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());
        files.push((name, meta.len(), mtime));
    }
    files.sort();
    // FNV-1a, 64-bit: stable across builds and platforms, unlike `DefaultHasher`.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for (name, len, mtime) in &files {
        for b in name
            .bytes()
            .chain([0])
            .chain(len.to_le_bytes())
            .chain(mtime.to_le_bytes())
        {
            h = (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    Some(format!("{h:016x}"))
}

/// The most names one `POST /data/__batch` may ask for.
pub const BATCH_MAX: usize = 256;
/// Chain reads one batch runs at once, on the blocking pool.
const BATCH_READS: usize = 8;

/// `POST /data/__batch` — many chain files in one response. The body is a JSON array of names;
/// the response streams one frame per name as its read finishes, in completion order:
/// `u32 name length, name (UTF-8, exactly as asked), i32 body length (−1 not in the chain, −2 a
/// read failure), body`, all little-endian. A browser on HTTP/1.1 runs six requests to a host at
/// once, so a world of small files is a queue of round trips; one request per batch is not.
async fn batch(State(state): State<DataState>, body: Bytes) -> Response {
    let Ok(names) = serde_json::from_slice::<Vec<String>>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if names.len() > BATCH_MAX {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    let chain = Arc::clone(&state.chain);
    let frames = futures_util::stream::iter(names)
        .map(move |name| {
            let chain = Arc::clone(&chain);
            async move {
                let path = name.replace('/', "\\");
                let read = tokio::task::spawn_blocking(move || chain.read(&path)).await;
                let body = match read {
                    Ok(Ok(bytes)) => Ok(bytes),
                    Ok(Err(e)) if is_missing(&e) => Err(-1),
                    Ok(Err(e)) => {
                        tracing::error!(error = %e, "chain read failed");
                        Err(-2)
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "chain read task panicked");
                        Err(-2)
                    }
                };
                Ok::<_, std::io::Error>(Bytes::from(batch_frame(&name, body)))
            }
        })
        .buffer_unordered(BATCH_READS);
    (
        [
            (header::CONTENT_TYPE, "application/octet-stream"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        axum::body::Body::from_stream(frames),
    )
        .into_response()
}

/// One `/data/__batch` frame: `name`, then the file's bytes or a negative status.
pub fn batch_frame(name: &str, body: Result<Vec<u8>, i32>) -> Vec<u8> {
    let payload: &[u8] = body.as_deref().unwrap_or(&[]);
    let mut out = Vec::with_capacity(8 + name.len() + payload.len());
    out.extend_from_slice(&(name.len() as u32).to_le_bytes());
    out.extend_from_slice(name.as_bytes());
    let len = match &body {
        Ok(b) => b.len() as i32,
        Err(code) => *code,
    };
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// `GET /data/__version` — the install's identity as plain text, never cached by the browser: the
/// page asks it once per visit to know whether its kept copy still matches.
async fn version_route(State(state): State<DataState>) -> Response {
    match state.version {
        Some(version) => (
            [
                (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            version.to_string(),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// `GET /data/__locale` — the `Data` folder's locale as plain text, or 404 when there is none.
async fn locale_route(State(state): State<DataState>) -> Response {
    match state.locale {
        Some(locale) => (
            [(
                axum::http::header::CONTENT_TYPE,
                "text/plain; charset=utf-8",
            )],
            locale.to_string(),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Which bodies are worth compressing, as a value rather than inline in [`router`] so the tests
/// can put the real predicate behind a stub handler — otherwise it could only be exercised
/// against a chain, and every assertion about it would skip on a box with no game install.
///
/// `DefaultPredicate` already contributes the size floor and skips `image/*`; naming
/// `NotForContentType::IMAGES` again keeps the BLP half of [`content_type`]'s contract visible
/// where the decision is made, and `audio/mpeg` is the one family it does not cover.
fn compressible() -> impl Predicate {
    DefaultPredicate::new()
        .and(NotForContentType::IMAGES)
        .and(NotForContentType::const_new("audio/mpeg"))
}

/// The `Content-Type` for a chain name — chosen for what it tells the compression predicate,
/// not for the browser, which never looks (the wasm `Chain` reads bytes and parses them by
/// signature).
///
/// The chain hands us *inflated* bytes: `Chain::read` has already undone the MPQ's own zlib, so
/// a DBC or an ADT arrives here as the raw structured form and compresses like one. Two families
/// do not, and they are most of the volume:
///
/// - **BLP** — the texels are DXT blocks (or a palette + indices). Already compressed; brotli
///   spends CPU to add bytes. `image/x-blp` puts them in the family `DefaultPredicate` skips.
/// - **MP3** — likewise, and audio is *not* in that default skip set, so it needs naming.
///
/// Anything else falls through to `application/octet-stream` and gets compressed. That is the
/// right default: the unlisted formats are DBC, ADT, WDT, WDL, M2, WMO and the FrameXML `.lua`
/// / `.xml` / `.toc` text, and every one of them is structured and redundant. A `.wav` lands
/// here too, deliberately — vanilla's are uncompressed PCM.
fn content_type(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or_default();
    if ext.eq_ignore_ascii_case("blp") {
        "image/x-blp"
    } else if ext.eq_ignore_ascii_case("mp3") {
        "audio/mpeg"
    } else {
        "application/octet-stream"
    }
}

/// A chain-read failure that means "this path doesn't exist in the composite" — the two shapes
/// `Chain::read` produces for a missing file (never mounted at all, or tombstoned by a patch) —
/// as opposed to a real I/O fault reading a corrupt archive, which is a 500, not a 404.
fn is_missing(err: &anyhow::Error) -> bool {
    let msg = err.to_string();
    msg.contains("not in patch chain") || msg.contains("deleted from patch chain")
}

/// The `Cache-Control` of a chain file asked for with `query` while the install is `version`.
/// `?v=<version>` names these exact bytes, so a shared cache may keep them under that URL; any
/// other `v=` must not be kept, or a new install's bytes would land under an old version's URL.
/// With no `v=`, the bytes are for the player's browser alone.
fn file_cache_control(query: Option<&str>, version: Option<&str>) -> &'static str {
    let asked = query.and_then(|q| q.split('&').find_map(|pair| pair.strip_prefix("v=")));
    match asked {
        None => "private, max-age=31536000, immutable",
        Some(v) if Some(v) == version => "public, max-age=31536000, immutable",
        Some(_) => "no-store",
    }
}

async fn file(method: Method, uri: Uri, State(state): State<DataState>) -> Response {
    let raw = uri.path().strip_prefix("/data/").unwrap_or("");
    let decoded = percent_encoding::percent_decode_str(raw).decode_utf8_lossy();
    let name = decoded.replace('/', "\\");
    // Before the move into `spawn_blocking` — the classification is pure string math on the
    // name and yields a `&'static str`, so it costs nothing to take it out of the task's way.
    let content_type = content_type(&name);
    let cache_control = file_cache_control(uri.query(), state.version.as_deref());

    let chain = Arc::clone(&state.chain);
    // `Chain::read` does synchronous file I/O (through benilla-mpq's blocking reads); running it
    // on the async runtime thread would stall every other in-flight request behind one disk seek.
    let read = tokio::task::spawn_blocking(move || chain.read(&name)).await;
    match read {
        Ok(Ok(bytes)) => {
            let body = if method == Method::HEAD {
                Vec::new()
            } else {
                bytes
            };
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, content_type),
                    (header::CACHE_CONTROL, cache_control),
                ],
                body,
            )
                .into_response()
        }
        Ok(Err(e)) if is_missing(&e) => StatusCode::NOT_FOUND.into_response(),
        Ok(Err(e)) => {
            tracing::error!(error = %e, "chain read failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
        Err(join_err) => {
            tracing::error!(error = %join_err, "chain read task panicked");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `GET /data/__index` — every name [`Chain::list`] can enumerate, for the wasm `Chain::list`
/// (Lane A) to mirror the native directory walk it can't do in a browser.
async fn index(State(state): State<DataState>) -> Response {
    let chain = Arc::clone(&state.chain);
    let listed = state
        .index
        .get(move || {
            let entries = chain.list()?;
            let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
            Ok(serde_json::to_vec(&names)?)
        })
        .await;
    match listed {
        Ok(bytes) => (
            [
                (header::CACHE_CONTROL, "private, max-age=86400"),
                (header::CONTENT_TYPE, "application/json"),
            ],
            bytes,
        )
            .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "chain index initialization failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{compressible, content_type, data_version, file_cache_control};

    /// Only `v=` as a whole parameter counts, wherever it sits in the query, and a host that
    /// names no version can never vouch for one.
    #[test]
    fn the_cache_control_follows_the_asked_version() {
        const PUBLIC: &str = "public, max-age=31536000, immutable";
        const PRIVATE: &str = "private, max-age=31536000, immutable";
        let cc = |q, v| file_cache_control(q, v);
        assert_eq!(cc(Some("v=abc"), Some("abc")), PUBLIC);
        assert_eq!(cc(Some("x=1&v=abc"), Some("abc")), PUBLIC);
        assert_eq!(cc(Some("v=old"), Some("abc")), "no-store");
        assert_eq!(cc(Some("v="), Some("abc")), "no-store");
        assert_eq!(cc(Some("v=abc"), None), "no-store");
        assert_eq!(cc(None, Some("abc")), PRIVATE);
        assert_eq!(cc(Some("nv=abc"), Some("abc")), PRIVATE);
    }

    /// The version names the archives, not the folder's other files, and moves when one changes.
    #[test]
    fn the_data_version_tracks_the_archives() {
        let dir = std::env::temp_dir().join(format!("wenilla-host-version-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("enUS")).unwrap();
        std::fs::write(dir.join("common.MPQ"), b"a").unwrap();
        std::fs::write(dir.join("enUS").join("locale-enUS.MPQ"), b"b").unwrap();
        let v1 = data_version(&dir).unwrap();
        assert_eq!(v1.len(), 16);
        assert_eq!(data_version(&dir).unwrap(), v1, "stable");
        std::fs::write(dir.join("notes.txt"), b"not an archive").unwrap();
        assert_eq!(data_version(&dir).unwrap(), v1, "other files do not count");
        std::fs::write(dir.join("patch-4.MPQ"), b"c").unwrap();
        let v2 = data_version(&dir).unwrap();
        assert_ne!(v2, v1, "a new patch");
        std::fs::write(dir.join("patch-4.MPQ"), b"cc").unwrap();
        assert_ne!(data_version(&dir).unwrap(), v2, "a replaced patch");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[tokio::test]
    async fn index_cache_coalesces_requests_and_is_scoped_to_mount() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let cache = Arc::new(super::IndexCache::default());
        let builds = Arc::new(AtomicUsize::new(0));
        let mut requests = Vec::new();
        for _ in 0..32 {
            let cache = cache.clone();
            let builds = builds.clone();
            requests.push(tokio::spawn(async move {
                cache
                    .get(move || {
                        builds.fetch_add(1, Ordering::SeqCst);
                        Ok(br#"["Interface\\FrameXML\\UI.lua"]"#.to_vec())
                    })
                    .await
                    .unwrap()
            }));
        }
        let mut first: Option<super::Bytes> = None;
        for request in requests {
            let bytes = request.await.unwrap();
            if let Some(ref prior) = first {
                assert_eq!(&bytes, prior);
                assert_eq!(bytes.as_ptr(), prior.as_ptr());
            } else {
                first = Some(bytes);
            }
        }
        assert_eq!(builds.load(Ordering::SeqCst), 1);
        let new_mount = super::IndexCache::default();
        let bytes = new_mount.get(|| Ok(b"[]".to_vec())).await.unwrap();
        assert_eq!(&bytes[..], b"[]");
    }

    #[tokio::test]
    async fn index_cache_retains_initialization_when_first_request_is_canceled() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let cache = Arc::new(super::IndexCache::default());
        let builds = Arc::new(AtomicUsize::new(0));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let first_cache = cache.clone();
        let first_builds = builds.clone();
        let first = tokio::spawn(async move {
            first_cache
                .get(move || {
                    first_builds.fetch_add(1, Ordering::SeqCst);
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(br#"["first"]"#.to_vec())
                })
                .await
        });
        // Cancel only after the blocking work has definitely started and cannot be canceled.
        started_rx.await.unwrap();
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        let later_builds = builds.clone();
        let later_cache = cache.clone();
        let later = tokio::spawn(async move {
            later_cache
                .get(move || {
                    later_builds.fetch_add(1, Ordering::SeqCst);
                    Ok(br#"["duplicate"]"#.to_vec())
                })
                .await
                .unwrap()
        });
        release_tx.send(()).unwrap();
        let bytes = later.await.unwrap();
        assert_eq!(&bytes[..], br#"["first"]"#);
        assert_eq!(builds.load(Ordering::SeqCst), 1);
        let warm = cache
            .get(|| panic!("warm cache must not rebuild"))
            .await
            .unwrap();
        assert_eq!(bytes.as_ptr(), warm.as_ptr());
    }

    #[tokio::test]
    async fn index_cache_retries_failed_initialization() {
        let cache = super::IndexCache::default();
        assert!(cache
            .get(|| anyhow::bail!("temporary read failure"))
            .await
            .is_err());
        assert_eq!(&cache.get(|| Ok(b"[]".to_vec())).await.unwrap()[..], b"[]");
    }

    /// Run one GET through the real predicate behind a handler that answers with `content_type`
    /// and `len` bytes, and report what `Content-Encoding` came back. `aaaa...` is maximally
    /// compressible on purpose: the question under test is whether the layer *tried*, not how
    /// well brotli did.
    async fn encoding_for(content_type: &'static str, len: usize) -> Option<String> {
        let app = axum::Router::new()
            .route(
                "/x",
                axum::routing::get(move || async move {
                    (
                        [(super::header::CONTENT_TYPE, content_type)],
                        vec![b'a'; len],
                    )
                }),
            )
            .layer(super::CompressionLayer::new().compress_when(compressible()));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/x")
                    .header("accept-encoding", "br")
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("router response");
        response
            .headers()
            .get(super::header::CONTENT_ENCODING)
            .map(|v| v.to_str().expect("ascii encoding").to_owned())
    }

    /// The pairing that makes the whole scheme work: a DBC/ADT/Lua body compresses, and the two
    /// families [`content_type`] diverts do not. Asserted through the real predicate, so a change
    /// to either half has to keep this true.
    #[tokio::test]
    async fn the_predicate_compresses_only_what_is_worth_compressing() {
        assert_eq!(
            encoding_for("application/octet-stream", 4096)
                .await
                .as_deref(),
            Some("br")
        );
        assert_eq!(encoding_for("image/x-blp", 4096).await, None);
        assert_eq!(encoding_for("audio/mpeg", 4096).await, None);
    }

    /// `DefaultPredicate`'s size floor still applies. Worth pinning: it is why a `HEAD`, whose
    /// body this route deliberately empties, never comes back claiming an encoding.
    #[tokio::test]
    async fn tiny_bodies_are_left_alone() {
        assert_eq!(encoding_for("application/octet-stream", 0).await, None);
    }

    /// The two families that must *not* be compressed, and the fall-through that must be. These
    /// are assertions about the compression predicate as much as about the strings: `image/x-blp`
    /// is only correct because `DefaultPredicate` skips `image/*`, and `audio/mpeg` is only
    /// correct because the layer in [`super::router`] names it.
    #[test]
    fn already_compressed_families_get_a_skipped_content_type() {
        assert_eq!(
            content_type("Interface\\Glues\\Common\\Glue-Panel-Button-Up.blp"),
            "image/x-blp"
        );
        assert_eq!(
            content_type("Sound\\Music\\CityMusic\\Stormwind\\1.mp3"),
            "audio/mpeg"
        );
    }

    /// Chain names arrive in whatever case the archive stored them in — the client's own reads
    /// mix `Interface\...` and `interface\...` in one session (see `web/world-manifest.json`),
    /// so a case-sensitive match would silently compress half the textures in the world.
    #[test]
    fn the_extension_match_is_case_insensitive() {
        assert_eq!(content_type("World\\Textures\\FOO.BLP"), "image/x-blp");
        assert_eq!(content_type("Sound\\ambience\\Forest.Mp3"), "audio/mpeg");
    }

    /// Everything else compresses, including the two edge shapes a `rsplit('.')` classifier can
    /// trip on: a name with no extension at all, and one whose only dot is in a directory.
    #[test]
    fn everything_else_is_compressible() {
        assert_eq!(
            content_type("DBFilesClient\\AreaTable.dbc"),
            "application/octet-stream"
        );
        assert_eq!(
            content_type("Interface\\FrameXML\\ContainerFrame.lua"),
            "application/octet-stream"
        );
        assert_eq!(
            content_type("World\\Maps\\Azeroth\\Azeroth_32_48.adt"),
            "application/octet-stream"
        );
        assert_eq!(content_type("Readme"), "application/octet-stream");
        assert_eq!(content_type("some.dir\\file"), "application/octet-stream");
    }
}
