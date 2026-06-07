use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, HeaderMap, HeaderValue, Response, StatusCode},
    response::IntoResponse,
};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
use tokio_util::io::ReaderStream;

use super::server::ServerState;
use crate::cache::thumbnails::ThumbTier;
use crate::thumb_serve::{self, ThumbOutcome, ThumbhashOutcome};

// 64 KiB read chunks. Small enough to keep the WebKitGTK reader fed without
// stalling on large allocations; large enough to amortize syscall overhead.
const STREAM_CHUNK: usize = 64 * 1024;

/// `GET /media/{*rel}` — serves a full-resolution media file with Range
/// support. The captured `rel` is the absolute filesystem path with the
/// leading `/` stripped (the frontend strips it before encoding so axum's
/// matchit router accepts the URL). HEIC/HEIF are transcoded to JPEG to
/// match the behavior of the `lightview://media/` custom protocol.
pub async fn media(
    State(state): State<ServerState>,
    Path(rel): Path<String>,
    headers: HeaderMap,
) -> Response<Body> {
    if rel.is_empty() {
        return error(StatusCode::BAD_REQUEST);
    }
    // Axum already percent-decodes path captures.
    let path = format!("/{}", rel);

    // Confine to the open gallery root — without this, a non-loopback bind
    // would expose every file on the host. Return 404 (not 403) so we don't
    // distinguish "outside gallery" from "missing".
    if !path_in_gallery(&state, &path).await {
        return error(StatusCode::NOT_FOUND);
    }

    let ext = std::path::Path::new(&path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    if ext == "heic" || ext == "heif" {
        return serve_heic(&path).await;
    }

    let mime = mime_for(&ext);
    // Range streaming exists so multi-hundred-MB videos don't get buffered into
    // memory. Images (incl. animated GIFs) are small and must arrive as one
    // complete body: WebKitGTK mishandles a chunked/Range-served animated GIF —
    // it re-pulls the source every loop (fast, inconsistent playback + a leaked
    // decode per pass) and a truncated chunk shows as a black frame. Buffer them
    // fully, like the thumbnail route, and serve a complete cacheable resource.
    if mime.starts_with("video/") {
        return serve_file(&path, mime, &headers).await;
    }
    serve_image(&path, mime).await
}

/// Reads a whole image into memory and serves it as one complete, cacheable
/// body — no Range, no chunked streaming. This is what lets WebKitGTK decode an
/// animated GIF once and loop it from cache at the correct frame rate.
async fn serve_image(path: &str, mime: &'static str) -> Response<Body> {
    let data = match tokio::fs::read(path).await {
        Ok(d) => d,
        Err(e) => {
            log::error!("Failed to read image file {}: {}", path, e);
            return error(StatusCode::NOT_FOUND);
        }
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, HeaderValue::from_static(mime))
        .header(header::CONTENT_LENGTH, data.len().to_string())
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from(data))
        .unwrap()
}

/// `GET /gif-atlas/{tier}/{*rel}` — serves a pre-rendered GIF frame atlas: a PNG
/// sprite sheet of every frame plus `X-Gif-*` metadata headers for canvas
/// playback. Generated on a miss and cached in SQLite (keyed by path+tier+mtime).
/// See `crate::gif_serve` for why GIFs are rendered this way on WebKitGTK.
pub async fn gif_atlas(
    State(state): State<ServerState>,
    Path((tier, rel)): Path<(String, String)>,
) -> Response<Body> {
    let Some(tier) = ThumbTier::from_segment(&tier) else {
        return error(StatusCode::BAD_REQUEST);
    };
    if rel.is_empty() {
        return error(StatusCode::BAD_REQUEST);
    }
    let path = format!("/{}", rel);
    if !path_in_gallery(&state, &path).await {
        return error(StatusCode::NOT_FOUND);
    }

    match crate::gif_serve::get_or_generate(&state.app, tier, path).await {
        Ok(atlas) => {
            let delays = atlas
                .delays
                .iter()
                .map(|d| d.to_string())
                .collect::<Vec<_>>()
                .join(",");
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "image/png")
                .header(header::CACHE_CONTROL, "no-cache")
                .header("X-Gif-Frame-Count", atlas.frame_count.to_string())
                .header("X-Gif-Frame-Width", atlas.frame_w.to_string())
                .header("X-Gif-Frame-Height", atlas.frame_h.to_string())
                .header("X-Gif-Cols", atlas.cols.to_string())
                .header("X-Gif-Delays", delays)
                .body(Body::from(atlas.png))
                .unwrap()
        }
        Err(e) => {
            log::warn!("gif atlas generation failed: {e}");
            error(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// `GET /thumb/{tier}/{*rel}` — serves a cached thumbnail, generating it on a
/// miss. `tier` is one of `s`/`m`/`l`/`p`; `rel` is the absolute path with the
/// leading `/` stripped (same convention as `/media`). Mirrors the
/// `lightview://thumb/<tier>/<path>` custom protocol for the web client.
pub async fn thumb(
    State(state): State<ServerState>,
    Path((tier, rel)): Path<(String, String)>,
) -> Response<Body> {
    let Some(tier) = ThumbTier::from_segment(&tier) else {
        return error(StatusCode::BAD_REQUEST);
    };
    if rel.is_empty() {
        return error(StatusCode::BAD_REQUEST);
    }
    let path = format!("/{}", rel);

    // Generate-on-miss would otherwise decode an arbitrary file on the host.
    if !path_in_gallery(&state, &path).await {
        return error(StatusCode::NOT_FOUND);
    }

    match thumb_serve::get_or_generate(&state.app, tier, path).await {
        ThumbOutcome::Hit { data, format } => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, thumb_serve::thumb_mime(&format))
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Body::from(data))
            .unwrap(),
        ThumbOutcome::NoGallery => error(StatusCode::SERVICE_UNAVAILABLE),
        ThumbOutcome::Miss => error(StatusCode::NOT_FOUND),
    }
}

/// `GET /thumbhash/{*rel}` — serves the decoded ThumbHash placeholder as a tiny
/// PNG. Mirrors the `lightview://thumbhash/<path>` custom protocol.
pub async fn thumbhash(
    State(state): State<ServerState>,
    Path(rel): Path<String>,
) -> Response<Body> {
    if rel.is_empty() {
        return error(StatusCode::BAD_REQUEST);
    }
    let path = format!("/{}", rel);

    match thumb_serve::render_thumbhash_png(&state.app, &path) {
        ThumbhashOutcome::Png(png) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "image/png")
            .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
            .body(Body::from(png))
            .unwrap(),
        ThumbhashOutcome::NoGallery => error(StatusCode::SERVICE_UNAVAILABLE),
        ThumbhashOutcome::Miss => error(StatusCode::NOT_FOUND),
        ThumbhashOutcome::Error => error(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn serve_heic(path: &str) -> Response<Body> {
    let src = path.to_string();
    let result = tokio::task::spawn_blocking(move || {
        let p = std::path::Path::new(&src);
        crate::pipeline::heic_cache::get_or_transcode(p).map_err(|e| e.to_string())
    })
    .await;

    match result {
        Ok(Ok(jpeg)) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "image/jpeg")
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Body::from(jpeg))
            .unwrap(),
        Ok(Err(e)) => {
            log::error!("HEIC transcode failed for {}: {}", path, e);
            error(StatusCode::INTERNAL_SERVER_ERROR)
        }
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

// Streams the file body to the client instead of buffering it into a Vec.
// Buffering the whole range was the root cause of WebKitGTK seek crashes
// and random restarts: a `Range: bytes=N-` from the <video> element would
// allocate `len - N` bytes and ship them as a single body, so a seek into
// a multi-hundred-MB clip stalled the connection long enough for the
// webview to abort and reissue the request — looking like a restart from
// 0 to the user.
async fn serve_file(path: &str, mime: &'static str, headers: &HeaderMap) -> Response<Body> {
    let mut file = match File::open(path).await {
        Ok(f) => f,
        Err(e) => {
            log::error!("Failed to open media file {}: {}", path, e);
            return error(StatusCode::NOT_FOUND);
        }
    };

    let len = match file.metadata().await {
        Ok(m) => m.len(),
        Err(e) => {
            log::error!("Failed to read metadata for {}: {}", path, e);
            return error(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };

    if let Some(range_header) = headers.get(header::RANGE).and_then(|v| v.to_str().ok()) {
        let ranges = match http_range::HttpRange::parse(range_header, len) {
            Ok(r) => r,
            Err(_) => {
                return Response::builder()
                    .status(StatusCode::RANGE_NOT_SATISFIABLE)
                    .header(header::CONTENT_RANGE, format!("bytes */{len}"))
                    .body(Body::empty())
                    .unwrap();
            }
        };

        if let Some(range) = ranges.first() {
            let start = range.start;
            let end = start + range.length - 1;

            if start >= len || end >= len {
                return Response::builder()
                    .status(StatusCode::RANGE_NOT_SATISFIABLE)
                    .header(header::CONTENT_RANGE, format!("bytes */{len}"))
                    .body(Body::empty())
                    .unwrap();
            }

            if let Err(e) = file.seek(SeekFrom::Start(start)).await {
                log::error!("Failed to seek to {} in {}: {}", start, path, e);
                return error(StatusCode::INTERNAL_SERVER_ERROR);
            }

            let nbytes = end + 1 - start;
            let limited = file.take(nbytes);
            let stream = ReaderStream::with_capacity(limited, STREAM_CHUNK);

            return Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_TYPE, HeaderValue::from_static(mime))
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{len}"))
                .header(header::CONTENT_LENGTH, nbytes.to_string())
                .header(header::CACHE_CONTROL, "no-cache")
                .body(Body::from_stream(stream))
                .unwrap();
        }
    }

    let stream = ReaderStream::with_capacity(file, STREAM_CHUNK);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, HeaderValue::from_static(mime))
        .header(header::CONTENT_LENGTH, len.to_string())
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// True if `path` resolves to a file inside the currently-open gallery root.
/// Canonicalizes both sides so `..` traversal and symlinks that escape the
/// root are rejected. False when no gallery is open or the path is missing.
async fn path_in_gallery(state: &ServerState, path: &str) -> bool {
    let root = {
        let guard = state.app.current_gallery.read().await;
        match guard.as_ref() {
            Some(r) => r.clone(),
            None => return false,
        }
    };
    let (Ok(root), Ok(candidate)) = (
        tokio::fs::canonicalize(&root).await,
        tokio::fs::canonicalize(path).await,
    ) else {
        return false;
    };
    candidate.starts_with(&root)
}

fn mime_for(ext: &str) -> &'static str {
    match ext {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "avif" => "image/avif",
        "tiff" | "tif" => "image/tiff",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        _ => "application/octet-stream",
    }
}

fn error(status: StatusCode) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(Body::empty())
        .unwrap()
}

pub async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}
