//! The built SPA, compiled into the executable.
//!
//! Serving `dist/` from disk meant every deployment — the Docker image, a
//! copied-out `lightview-headless` — carried a directory that had to stay in
//! step with the binary beside it, and a mismatch showed up as a blank page or
//! a stale bundle rather than as an error. The whole build is ~650 KB, so
//! embedding it costs nothing worth measuring and makes the binary
//! self-contained.
//!
//! `HttpConfig::web_root` remains as an override: when it is set, `server.rs`
//! serves that directory with `ServeDir` and this module is not used at all.
//! That is the dev path — point it at a live Vite output and skip the
//! recompile.
//!
//! In debug builds rust-embed does not actually embed: `Assets::get` reads
//! `dist/` off disk at the path recorded at compile time, so a rebuilt
//! frontend takes effect without touching Rust. Release builds carry the
//! bytes. Either way the folder must exist when the crate is compiled.
//!
//! Cache policy is *not* set here — [`super::middleware::static_cache_control`]
//! layers over this handler exactly as it does over `ServeDir`, so the two
//! paths cannot drift. What is here is the ETag, which that policy depends on:
//! the shell is served `no-cache`, meaning "revalidate every load", and without
//! a validator every load would be a full 200 instead of a 304.

use axum::{
    body::Body,
    http::{header, HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use rust_embed::RustEmbed;

// The path is resolved against this crate's manifest directory, so it is the
// same `dist/` that `tauri.conf.json`'s `frontendDist` names.
#[derive(RustEmbed)]
#[folder = "../dist"]
struct Assets;

/// Serve an embedded asset, falling back to `index.html` for anything that
/// isn't one. The fallback is what makes client-side routing work: `/pair` is
/// a route in the SPA, not a file, and must return the shell rather than 404.
pub async fn serve(uri: Uri, headers: HeaderMap) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    match Assets::get(path).or_else(|| Assets::get("index.html")) {
        Some(file) => {
            let etag = format!("\"{}\"", hex(&file.metadata.sha256_hash()));
            if headers
                .get(header::IF_NONE_MATCH)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v == etag)
            {
                return StatusCode::NOT_MODIFIED.into_response();
            }
            (
                [
                    (header::CONTENT_TYPE, file.metadata.mimetype().to_string()),
                    (header::ETAG, etag),
                ],
                Body::from(file.data.into_owned()),
            )
                .into_response()
        }
        // Only reachable if the crate was built against a dist/ with no
        // index.html, which is a broken frontend build rather than a bad URL.
        None => (StatusCode::NOT_FOUND, "web app not built").into_response(),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
