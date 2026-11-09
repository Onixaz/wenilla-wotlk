//! One site per locale folder, at that folder's lowercase name: `/enus/`, `/eses/`, … Each site
//! is the whole page and its data routes, and sees its own prefix stripped from every path.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::uri::Uri;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::Router;

struct Sites {
    /// Each site's lowercase prefix and its router.
    sites: Vec<(String, Router)>,
    /// The prefix `/` redirects to.
    default: String,
}

/// Serve `sites`, each `(locale folder, router)`, with `/` redirecting to `default`'s.
pub fn router(sites: Vec<(String, Router)>, default: &str) -> Router {
    let sites = sites
        .into_iter()
        .map(|(folder, site)| (folder.to_ascii_lowercase(), site))
        .collect();
    let default = default.to_ascii_lowercase();
    Router::new()
        .fallback(dispatch)
        .with_state(Arc::new(Sites { sites, default }))
}

/// The first path segment and everything after it, its leading `/` included.
fn split_path(path: &str) -> (&str, &str) {
    let tail = path.strip_prefix('/').unwrap_or(path);
    tail.find('/')
        .map_or((tail, ""), |slash| tail.split_at(slash))
}

/// `path`, followed by `uri`'s query when it has one.
fn with_query(path: &str, uri: &Uri) -> String {
    match uri.query() {
        Some(query) => format!("{path}?{query}"),
        None => path.to_string(),
    }
}

async fn dispatch(State(sites): State<Arc<Sites>>, mut req: Request) -> Response {
    let uri = req.uri().clone();
    let (segment, rest) = split_path(uri.path());
    if segment.is_empty() {
        let target = with_query(&format!("/{}/", sites.default), &uri);
        return Redirect::temporary(&target).into_response();
    }
    let Some((prefix, site)) = sites
        .sites
        .iter()
        .find(|(prefix, _)| prefix.eq_ignore_ascii_case(segment))
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // One URL per language, and the trailing slash the page's relative paths resolve against.
    if segment != prefix || rest.is_empty() {
        let rest = if rest.is_empty() { "/" } else { rest };
        let target = with_query(&format!("/{prefix}{rest}"), &uri);
        return Redirect::permanent(&target).into_response();
    }
    let Ok(stripped) = with_query(rest, &uri).parse::<Uri>() else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    *req.uri_mut() = stripped;
    match tower::Service::call(&mut site.clone(), req).await {
        Ok(response) => response,
        Err(never) => match never {},
    }
}
