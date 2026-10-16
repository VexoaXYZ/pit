//! Serves the dashboard, which is compiled into the binary.

use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../../packages/web/dist"]
struct Assets;

pub async fn static_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path.starts_with("api/") {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    if let Some(file) = Assets::get(path).filter(|_| !path.is_empty()) {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        let cache = if path.starts_with("assets/") {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        };
        return (
            [
                (header::CONTENT_TYPE, mime.as_ref()),
                (header::CACHE_CONTROL, cache),
            ],
            file.data,
        )
            .into_response();
    }
    // Single-page app: every other route renders index.html.
    match Assets::get("index.html") {
        Some(file) => ([(header::CONTENT_TYPE, "text/html; charset=utf-8"), (header::CACHE_CONTROL, "no-cache")], file.data).into_response(),
        None => (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            "<!doctype html><title>Pit</title><p>Pit is running. The dashboard was not built into this binary \
             (run <code>bun run build</code> in <code>packages/web</code> and rebuild). The API is at <code>/api</code>.</p>",
        )
            .into_response(),
    }
}
