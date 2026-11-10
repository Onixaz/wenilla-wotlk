//! The `/data` routes over a `Data` folder with two locales: plain `/data/*` is the default
//! locale's chain, `/data/__l/{locale}/*` each locale's, and `/data/__locales` lists them. Runs on
//! any clone: the install is a handful of stored-file archives this file writes.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::Router;
use benilla_formats::Chain;
use tower::ServiceExt;
use wenilla_host::data::{data_version_for, router_locales, router_with, LocaleData};

/// A minimal MPQ writer: a version 1 header, every file stored (no compression), a `(listfile)`,
/// and the hash and block tables encrypted as the reader expects.
mod mpq {
    use std::path::Path;

    const fn table() -> [u32; 0x500] {
        let mut table = [0u32; 0x500];
        let mut seed: u32 = 0x0010_0001;
        let mut i1 = 0;
        while i1 < 0x100 {
            let mut i2 = 0;
            while i2 < 5 {
                seed = (seed.wrapping_mul(125).wrapping_add(3)) % 0x2A_AAAB;
                let t1 = (seed & 0xFFFF) << 0x10;
                seed = (seed.wrapping_mul(125).wrapping_add(3)) % 0x2A_AAAB;
                table[i1 + i2 * 0x100] = t1 | (seed & 0xFFFF);
                i2 += 1;
            }
            i1 += 1;
        }
        table
    }
    static TABLE: [u32; 0x500] = table();

    fn hash(name: &str, kind: u32) -> u32 {
        let (mut s1, mut s2): (u32, u32) = (0x7FED_7FED, 0xEEEE_EEEE);
        for &b in name.as_bytes() {
            let ch = u32::from(if b == b'/' {
                b'\\'
            } else {
                b.to_ascii_uppercase()
            });
            s1 = TABLE[(kind + ch) as usize] ^ s1.wrapping_add(s2);
            s2 = ch
                .wrapping_add(s1)
                .wrapping_add(s2)
                .wrapping_add(s2 << 5)
                .wrapping_add(3);
        }
        s1
    }

    fn encrypt(words: &mut [u32], mut key: u32) {
        let mut seed: u32 = 0xEEEE_EEEE;
        for v in words.iter_mut() {
            seed = seed.wrapping_add(TABLE[0x400 + (key & 0xFF) as usize]);
            let plain = *v;
            *v = plain ^ key.wrapping_add(seed);
            key = (!key << 0x15).wrapping_add(0x1111_1111) | (key >> 0x0B);
            seed = plain
                .wrapping_add(seed)
                .wrapping_add(seed << 5)
                .wrapping_add(3);
        }
    }

    pub fn write(path: &Path, files: &[(&str, &[u8])]) {
        let listfile = files
            .iter()
            .map(|(n, _)| *n)
            .collect::<Vec<_>>()
            .join("\r\n");
        let mut all: Vec<(&str, &[u8])> = files.to_vec();
        all.push(("(listfile)", listfile.as_bytes()));
        let slots = (all.len() * 2).next_power_of_two().max(16);
        let mut data = Vec::new();
        let mut blocks = Vec::new();
        let mut hashes = vec![[0xFFFF_FFFFu32; 4]; slots];
        for (i, (name, bytes)) in all.iter().enumerate() {
            let pos = 32 + data.len() as u32;
            data.extend_from_slice(bytes);
            let len = bytes.len() as u32;
            blocks.extend([pos, len, len, 0x8000_0000]);
            let mut slot = hash(name, 0) as usize & (slots - 1);
            while hashes[slot][3] != 0xFFFF_FFFF {
                slot = (slot + 1) % slots;
            }
            hashes[slot] = [hash(name, 0x100), hash(name, 0x200), 0, i as u32];
        }
        let mut hash_words: Vec<u32> = hashes.concat();
        encrypt(&mut hash_words, hash("(hash table)", 0x300));
        encrypt(&mut blocks, hash("(block table)", 0x300));
        let hash_pos = 32 + data.len() as u32;
        let block_pos = hash_pos + hash_words.len() as u32 * 4;
        let size = block_pos + blocks.len() as u32 * 4;
        let mut out = Vec::new();
        for w in [0x1A51_504D, 32, size] {
            out.extend_from_slice(&u32::to_le_bytes(w));
        }
        out.extend_from_slice(&0u16.to_le_bytes()); // format version 1
        out.extend_from_slice(&3u16.to_le_bytes()); // 4 KiB sectors
        for w in [hash_pos, block_pos, slots as u32, all.len() as u32] {
            out.extend_from_slice(&w.to_le_bytes());
        }
        out.extend_from_slice(&data);
        for w in hash_words.iter().chain(&blocks) {
            out.extend_from_slice(&w.to_le_bytes());
        }
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, out).unwrap();
    }
}

/// A `Data` folder: one shared archive, then enUS and esES, each with the two archives the
/// chain requires and a `Wow.ini` naming its locale. The shared archive also holds a file named
/// as the esES route spells one, so a locale URL that fell through to the wildcard would read it.
fn install(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("wenilla-host-locales-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    mpq::write(
        &dir.join("common.MPQ"),
        &[
            ("Shared.txt", b"shared"),
            ("__l\\esES\\Strings.lua", b"the wildcard"),
        ],
    );
    for (loc, lang, country, text) in [
        ("enUS", "en", "US", "Abandon"),
        ("esES", "es", "ES", "Abandonar"),
    ] {
        let ini = format!("[WoW Config]\r\nLanguage={lang}\r\nCountry={country}\r\n");
        mpq::write(
            &dir.join(loc).join(format!("locale-{loc}.MPQ")),
            &[
                ("Wow.ini", ini.as_bytes()),
                ("Strings.lua", text.as_bytes()),
            ],
        );
        mpq::write(
            &dir.join(loc).join(format!("speech-{loc}.MPQ")),
            &[("Speech.txt", loc.as_bytes())],
        );
    }
    dir
}

/// Every locale of `dir`, served as `wenilla-host` serves them.
fn app(dir: &Path, default: &str) -> Router {
    let folders = benilla_formats::data_locales(dir).unwrap();
    let locales = folders
        .iter()
        .zip(Chain::open_locales(dir, &folders))
        .map(|(folder, chain)| LocaleData {
            locale: folder.to_string(),
            chain: Arc::new(chain.unwrap()),
            version: data_version_for(dir, folder),
        })
        .collect();
    router_locales(locales, default)
}

async fn call(app: &Router, method: Method, uri: &str, body: &str) -> (StatusCode, String, String) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let cache = resp
        .headers()
        .get("cache-control")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, cache, String::from_utf8_lossy(&bytes).into_owned())
}

async fn get(app: &Router, uri: &str) -> (StatusCode, String) {
    let (status, _, body) = call(app, Method::GET, uri, "").await;
    (status, body)
}

#[test]
fn the_folder_lists_and_resolves_its_locales() {
    let dir = install("list");
    assert_eq!(
        benilla_formats::data_locales(&dir).unwrap(),
        ["enUS", "esES"]
    );
    assert_eq!(
        benilla_formats::resolve_data_locale(&dir, "enUS").unwrap(),
        "enUS"
    );
    assert_eq!(
        benilla_formats::resolve_data_locale(&dir, "esES").unwrap(),
        "esES"
    );
    // No frFR: the next existing folder in the client's table, wrapping.
    assert_eq!(
        benilla_formats::resolve_data_locale(&dir, "frFR").unwrap(),
        "enUS"
    );
    let chains = Chain::open_locales(&dir, &["enUS", "esES"]);
    let es = chains[1].as_ref().unwrap().locale();
    assert_eq!(es.folder, "esES");
    assert_eq!(es.script_name(), "esES", "the esES Wow.ini picks the slot");
    assert_eq!(es.existing, ["enUS", "esES"]);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Plain `/data/*` is the default locale; `/data/__l/{locale}/*` reads that locale's chain, the
/// shared archives included.
#[tokio::test]
async fn each_locale_reads_its_own_chain() {
    let dir = install("files");
    let app = app(&dir, "enUS");
    assert_eq!(
        get(&app, "/data/Strings.lua").await,
        (StatusCode::OK, "Abandon".into())
    );
    assert_eq!(
        get(&app, "/data/__l/enUS/Strings.lua").await,
        (StatusCode::OK, "Abandon".into())
    );
    assert_eq!(
        get(&app, "/data/__l/esES/Strings.lua").await,
        (StatusCode::OK, "Abandonar".into())
    );
    assert_eq!(
        get(&app, "/data/__l/esES/Shared.txt").await,
        (StatusCode::OK, "shared".into())
    );
    let (status, body) = get(&app, "/data/__l/esES/Wow.ini").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("Language=es") && body.contains("Country=ES"),
        "{body}"
    );
    let (status, _, body) = call(&app, Method::HEAD, "/data/__l/esES/Strings.lua", "").await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, ""));
    assert_eq!(
        get(&app, "/data/__l/esES/nope.x").await.0,
        StatusCode::NOT_FOUND
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// matchit picks the literal `__l` segment over `/data/{*name}`: the shared archive's file named
/// `__l\esES\Strings.lua` is reachable only by its encoded name.
#[tokio::test]
async fn a_locale_route_wins_over_the_wildcard() {
    let dir = install("matchit");
    let app = app(&dir, "enUS");
    assert_eq!(get(&app, "/data/__l/esES/Strings.lua").await.1, "Abandonar");
    assert_eq!(
        get(&app, "/data/__l%5CesES%5CStrings.lua").await,
        (StatusCode::OK, "the wildcard".into())
    );
    // Not a locale root: no locale and no trailing name.
    assert_eq!(get(&app, "/data/__l/esES").await.0, StatusCode::NOT_FOUND);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn an_unknown_locale_is_a_404_on_every_route() {
    let dir = install("unknown");
    let app = app(&dir, "enUS");
    for uri in [
        "/data/__l/frFR/Shared.txt",
        "/data/__l/frFR/__index",
        "/data/__l/frFR/__locale",
        "/data/__l/frFR/__version",
        "/data/__l/esmx/Strings.lua",
        "/data/__l/eses/Strings.lua",
    ] {
        assert_eq!(get(&app, uri).await.0, StatusCode::NOT_FOUND, "{uri}");
    }
    let (status, _, _) = call(&app, Method::POST, "/data/__l/frFR/__batch", "[]").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn the_locales_route_lists_them_in_table_order() {
    let dir = install("listing");
    let (status, cache, body) = call(&app(&dir, "enUS"), Method::GET, "/data/__locales", "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cache, "no-cache");
    assert_eq!(body, r#"{"default":"enUS","locales":["enUS","esES"]}"#);
    // `--locale esES`: plain `/data` is esES's chain.
    let es = app(&dir, "esES");
    assert_eq!(
        get(&es, "/data/__locales").await.1,
        r#"{"default":"esES","locales":["enUS","esES"]}"#
    );
    assert_eq!(get(&es, "/data/Strings.lua").await.1, "Abandonar");
    assert_eq!(get(&es, "/data/__locale").await.1, "esES");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// `__locale`, `__version`, `__index` and `__batch` answer per locale; plain `/data` answers the
/// default's, as it did before there was more than one.
#[tokio::test]
async fn the_host_routes_answer_per_locale() {
    let dir = install("routes");
    let app = app(&dir, "enUS");
    let en = data_version_for(&dir, "enUS").unwrap();
    let es = data_version_for(&dir, "esES").unwrap();
    assert_ne!(en, es);
    assert_eq!(get(&app, "/data/__locale").await.1, "enUS");
    assert_eq!(get(&app, "/data/__l/esES/__locale").await.1, "esES");
    assert_eq!(get(&app, "/data/__version").await.1, en);
    assert_eq!(get(&app, "/data/__l/enUS/__version").await.1, en);
    assert_eq!(get(&app, "/data/__l/esES/__version").await.1, es);

    let (status, index) = get(&app, "/data/__l/esES/__index").await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<String> = serde_json::from_str(&index).unwrap();
    assert!(names.iter().any(|n| n == "Strings.lua"), "{names:?}");
    assert!(names.iter().any(|n| n == "Shared.txt"), "{names:?}");

    let (status, _, frames) = call(
        &app,
        Method::POST,
        "/data/__l/esES/__batch",
        r#"["Strings.lua"]"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(frames.ends_with("Abandonar"), "{frames:?}");
    let (_, _, frames) = call(&app, Method::POST, "/data/__batch", r#"["Strings.lua"]"#).await;
    assert!(frames.ends_with("Abandon"), "{frames:?}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Each locale vouches only for its own version: a `?v=` naming the other locale's is not kept.
#[tokio::test]
async fn each_locale_keeps_its_own_cache_rule() {
    const PUBLIC: &str = "public, max-age=31536000, immutable";
    const PRIVATE: &str = "private, max-age=31536000, immutable";
    let dir = install("cache");
    let app = app(&dir, "enUS");
    let en = data_version_for(&dir, "enUS").unwrap();
    let es = data_version_for(&dir, "esES").unwrap();
    for (uri, expected) in [
        (format!("/data/Strings.lua?v={en}"), PUBLIC),
        (format!("/data/Strings.lua?v={es}"), "no-store"),
        ("/data/Strings.lua".to_string(), PRIVATE),
        (format!("/data/__l/esES/Strings.lua?v={es}"), PUBLIC),
        (format!("/data/__l/esES/Strings.lua?v={en}"), "no-store"),
        ("/data/__l/esES/Strings.lua".to_string(), PRIVATE),
        (format!("/data/__l/enUS/Strings.lua?v={en}"), PUBLIC),
    ] {
        let (status, cache, _) = call(&app, Method::GET, &uri, "").await;
        assert_eq!(
            (status, cache.as_str()),
            (StatusCode::OK, expected),
            "{uri}"
        );
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The one-chain router keeps its old answers: no locale means no `__locales`, and a locale is
/// listed alone and served under its root too.
#[tokio::test]
async fn the_one_chain_router_is_unchanged() {
    let dir = install("single");
    let chain = || Arc::new(Chain::open_locale(&dir, "enUS").unwrap());
    let bare = router_with(chain(), None, Some("v1".into()));
    assert_eq!(get(&bare, "/data/__locales").await.0, StatusCode::NOT_FOUND);
    assert_eq!(get(&bare, "/data/__locale").await.0, StatusCode::NOT_FOUND);
    assert_eq!(get(&bare, "/data/__version").await.1, "v1");
    assert_eq!(get(&bare, "/data/Strings.lua").await.1, "Abandon");
    assert_eq!(
        get(&bare, "/data/__l/enUS/Strings.lua").await.0,
        StatusCode::NOT_FOUND
    );

    let named = router_with(chain(), Some("enUS".into()), Some("v1".into()));
    assert_eq!(
        get(&named, "/data/__locales").await.1,
        r#"{"default":"enUS","locales":["enUS"]}"#
    );
    assert_eq!(get(&named, "/data/__l/enUS/Strings.lua").await.1, "Abandon");
    assert_eq!(
        get(&named, "/data/__l/esES/Strings.lua").await.0,
        StatusCode::NOT_FOUND
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
