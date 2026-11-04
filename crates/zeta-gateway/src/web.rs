//! The browser client, embedded in the binary: one file to deploy, no Node.js.

use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::HeaderName;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use crate::api::AppState;

const INDEX: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const APP_CSS: &str = include_str!("../web/app.css");
const SW_JS: &str = include_str!("../web/sw.js");
const MANIFEST: &str = include_str!("../web/manifest.webmanifest");
const ICON: &str = include_str!("../web/icon.svg");

/// Only our own scripts and styles run. Pictures, video and audio come through
/// the gateway's media proxy (`data:` is for inline sample pictures), so the page
/// never contacts another host.
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; \
    img-src 'self' data:; media-src 'self'; connect-src 'self'; manifest-src 'self'; \
    worker-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'";

fn asset(content_type: &'static str, body: &'static str) -> Response {
    (
        [(CONTENT_TYPE, content_type), (CACHE_CONTROL, "no-cache")],
        body,
    )
        .into_response()
}

async fn index() -> Response {
    (
        [
            (CONTENT_TYPE, "text/html; charset=utf-8"),
            (CACHE_CONTROL, "no-cache"),
            (HeaderName::from_static("content-security-policy"), CSP),
            // Links open other sites: do not tell them where the gateway lives.
            (HeaderName::from_static("referrer-policy"), "no-referrer"),
            (HeaderName::from_static("x-content-type-options"), "nosniff"),
        ],
        INDEX,
    )
        .into_response()
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(index))
        .route(
            "/app.js",
            get(|| async { asset("text/javascript; charset=utf-8", APP_JS) }),
        )
        .route(
            "/app.css",
            get(|| async { asset("text/css; charset=utf-8", APP_CSS) }),
        )
        .route(
            "/sw.js",
            get(|| async { asset("text/javascript; charset=utf-8", SW_JS) }),
        )
        .route(
            "/manifest.webmanifest",
            get(|| async { asset("application/manifest+json", MANIFEST) }),
        )
        .route("/icon.svg", get(|| async { asset("image/svg+xml", ICON) }))
}
