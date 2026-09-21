//! The SPA, compiled into the binary.
//!
//! A deployment is one executable: `rust-embed` folds `dist/` in at compile
//! time. The folder must therefore exist when the crate is first compiled,
//! which is why `dist/.gitkeep` is committed and `.gitignore` excludes
//! `dist/*` rather than `dist/`. A build without `npm run build` compiles fine
//! and serves a 404 at `/` — a loud runtime failure instead of a confusing
//! build failure, which is the trade that retires the "run npm first"
//! exception.
//!
//! In a debug build `rust-embed` reads from disk, so a frontend rebuild takes
//! effect without recompiling the Rust.

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../dist"]
struct Assets;

/// Serve one embedded file, or the SPA shell.
///
/// Unknown paths fall through to `index.html` so client-side routing works on a
/// hard refresh — but only for paths that look like routes. A missing asset
/// returning HTML would make a broken `<script src>` fail as a syntax error
/// rather than a 404, which is a genuinely confusing way to debug a bad build.
pub async fn serve(uri: axum::http::Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let candidate = if path.is_empty() { "index.html" } else { path };

    if let Some(file) = Assets::get(candidate) {
        let mime = file.metadata.mimetype().to_string();
        return respond(candidate, &mime, file.data.into_owned());
    }
    if candidate.contains('.') {
        // It named a file extension, so it wanted a file.
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    match Assets::get("index.html") {
        Some(shell) => {
            let mime = shell.metadata.mimetype().to_string();
            respond("index.html", &mime, shell.data.into_owned())
        }
        None => (
            StatusCode::NOT_FOUND,
            "The SPA was not built into this binary. Run `npm run build` and rebuild.",
        )
            .into_response(),
    }
}

fn respond(path: &str, mime: &str, body: Vec<u8>) -> Response {
    let mut response = (StatusCode::OK, body).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(mime)
            .unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    // The shell must never be cached: a stale one points at hashed asset names
    // that no longer exist, which presents as a blank page after a deploy.
    let cache = if path == "index.html" {
        "no-cache"
    } else {
        "public, max-age=31536000, immutable"
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    response
}
