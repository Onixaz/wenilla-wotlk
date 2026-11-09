//! The locale prefixes: which URL reaches which site, and what each redirect keeps.

use axum::body::Body;
use axum::http::{header, Request, StatusCode, Uri};
use axum::Router;
use tower::ServiceExt;

/// A site that answers with its name and the URI it was handed.
fn site(name: &'static str) -> Router {
    Router::new().fallback(move |uri: Uri| async move { format!("{name} {uri}") })
}

fn app() -> Router {
    wenilla_host::locale_site::router(
        vec![("enUS".into(), site("en")), ("esES".into(), site("es"))],
        "enUS",
    )
}

async fn get(path: &str) -> (StatusCode, String, String) {
    let request = Request::get(path).body(Body::empty()).unwrap();
    let response = app().oneshot(request).await.unwrap();
    let status = response.status();
    let location = response
        .headers()
        .get(header::LOCATION)
        .map_or(String::new(), |l| l.to_str().unwrap().to_string());
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, location, String::from_utf8(body.to_vec()).unwrap())
}

#[tokio::test]
async fn a_prefix_reaches_its_own_site_with_the_prefix_stripped() {
    let (status, _, body) = get("/eses/data/a.blp?v=1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "es /data/a.blp?v=1");
    assert_eq!(get("/enus/").await.2, "en /");
}

#[tokio::test]
async fn the_root_redirects_to_the_default_and_keeps_the_query() {
    let (status, location, _) = get("/?user=a&char=b").await;
    assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(location, "/enus/?user=a&char=b");
}

#[tokio::test]
async fn any_spelling_redirects_to_the_one_url_of_that_locale() {
    for (from, to) in [
        ("/esES", "/eses/"),
        ("/eses", "/eses/"),
        ("/ESES/?user=a", "/eses/?user=a"),
        ("/esES/data/a.blp", "/eses/data/a.blp"),
    ] {
        let (status, location, _) = get(from).await;
        assert_eq!(status, StatusCode::PERMANENT_REDIRECT, "{from}");
        assert_eq!(location, to, "{from}");
    }
}

#[tokio::test]
async fn a_locale_the_host_does_not_have_is_not_found() {
    assert_eq!(get("/frfr/").await.0, StatusCode::NOT_FOUND);
    assert_eq!(get("/data/a.blp").await.0, StatusCode::NOT_FOUND);
}
