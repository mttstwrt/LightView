use axum::{
    body::Body,
    extract::{Multipart, Path, Query, State},
    http::{header, HeaderMap, HeaderValue, Response, StatusCode},
    response::IntoResponse,
    Json,
};
use chrono::Datelike;
use serde::{Deserialize, Serialize};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
use tokio_util::io::ReaderStream;

use super::server::ServerState;
use super::uploads;
use crate::cache::thumbnails::ThumbTier;
use crate::companion::schema::MediaType;
use crate::thumb_serve::{self, ThumbOutcome, ThumbhashOutcome};

// 64 KiB read chunks. Small enough to keep the WebKitGTK reader fed without
// stalling on large allocations; large enough to amortize syscall overhead.
const STREAM_CHUNK: usize = 64 * 1024;

// Upper bound on the `?fit=` longest-edge resize target. Past this the justified
// grid already uses the `jh` tier (2560px), so a larger fit request would just
// out-decode it for no benefit; clamp defensively in case one slips through.
const FIT_MAX_EDGE: u32 = 4096;

/// Query string for `GET /media`. `fit=<px>` requests an aspect-preserving
/// resize to that longest edge (native raster stills only) instead of the full
/// file — so the webview decodes a cell-sized image off its main thread.
#[derive(Debug, Deserialize)]
pub struct MediaQuery {
    fit: Option<u32>,
}

/// Image extensions the resize pipeline (`image` crate) decodes directly, so a
/// `?fit=` request is cheap. HEIC/AVIF/RAW need the libheif/transcode paths and
/// are served whole instead.
fn is_fit_resizable(ext: &str) -> bool {
    matches!(ext, "jpg" | "jpeg" | "png" | "webp")
}

/// `GET /media/{*rel}` — serves a full-resolution media file with Range
/// support. The captured `rel` is the absolute filesystem path with the
/// leading `/` stripped (the frontend strips it before encoding so axum's
/// matchit router accepts the URL). HEIC/HEIF are transcoded to JPEG to
/// match the behavior of the `lightview://media/` custom protocol.
pub async fn media(
    State(state): State<ServerState>,
    Path(rel): Path<String>,
    Query(query): Query<MediaQuery>,
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

    // `?fit=<edge>` serves an aspect-preserving resize of a native raster still
    // to the cell's longest edge: the justified grid uses this in place of the
    // full original so the webview decodes a cell-sized image off its main
    // thread (and transfers far fewer bytes — matters for remote providers).
    if let Some(edge) = query.fit {
        if edge > 0 && is_fit_resizable(&ext) {
            return serve_fit_image(&state, &path, edge).await;
        }
    }

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
    serve_image(&path, mime, &headers).await
}

/// RFC 7231 HTTP-date for a filesystem mtime.
fn http_date(t: std::time::SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(t)
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string()
}

/// Reads a whole image into memory and serves it as one complete, cacheable
/// body — no Range, no chunked streaming. This is what lets WebKitGTK decode an
/// animated GIF once and loop it from cache at the correct frame rate.
///
/// Sent with `no-cache` + `Last-Modified` so the webview revalidates with
/// If-Modified-Since: an unchanged file answers 304 with no body transfer,
/// while an externally edited file is picked up immediately.
async fn serve_image(path: &str, mime: &'static str, headers: &HeaderMap) -> Response<Body> {
    let mtime = tokio::fs::metadata(path)
        .await
        .ok()
        .and_then(|m| m.modified().ok());

    if let (Some(mtime), Some(ims)) = (
        mtime,
        headers
            .get(header::IF_MODIFIED_SINCE)
            .and_then(|v| v.to_str().ok()),
    ) {
        if let Ok(since) = chrono::DateTime::parse_from_rfc2822(ims) {
            let mtime_secs = chrono::DateTime::<chrono::Utc>::from(mtime).timestamp();
            if mtime_secs <= since.timestamp() {
                return Response::builder()
                    .status(StatusCode::NOT_MODIFIED)
                    .header(header::CACHE_CONTROL, "no-cache")
                    .header(header::LAST_MODIFIED, http_date(mtime))
                    .body(Body::empty())
                    .unwrap();
            }
        }
    }

    let data = match tokio::fs::read(path).await {
        Ok(d) => d,
        Err(e) => {
            log::error!("Failed to read image file {}: {}", path, e);
            return error(StatusCode::NOT_FOUND);
        }
    };
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, HeaderValue::from_static(mime))
        .header(header::CONTENT_LENGTH, data.len().to_string())
        .header(header::CACHE_CONTROL, "no-cache");
    if let Some(mtime) = mtime {
        builder = builder.header(header::LAST_MODIFIED, http_date(mtime));
    }
    builder.body(Body::from(data)).unwrap()
}

/// Serves an aspect-preserving resize of a native raster still to `edge`
/// (longest-edge) pixels, encoded WebP. The decode + downscale runs on the
/// shared thumb pool — the same bounded rayon pool the thumbnail tiers use — so
/// it never upscales (see `fit_dims`) and never oversubscribes the CPU during a
/// scroll burst. Cached like the thumbnail route (`max-age`, ?fit= makes the URL
/// content-stable), so the webview holds the resized bytes without a disk cache
/// on our side. Falls back to the full file if the resize fails.
async fn serve_fit_image(state: &ServerState, path: &str, edge: u32) -> Response<Body> {
    let target = edge.clamp(16, FIT_MAX_EDGE);
    let src = path.to_string();
    let pool = state.app.thumb_pool.clone();
    let (tx, rx) = tokio::sync::oneshot::channel();
    pool.spawn(move || {
        let filter = crate::pipeline::thumbnailer::filter_for_size(target);
        let res = crate::pipeline::thumbnailer::generate_for_path_fit(
            std::path::Path::new(&src),
            filter,
            crate::pipeline::thumbnailer::ThumbFormat::Webp,
            target,
        );
        let _ = tx.send(res);
    });

    match rx.await {
        Ok(Ok(thumb)) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "image/webp")
            .header(header::CONTENT_LENGTH, thumb.data.len().to_string())
            // Same caching contract as the thumbnail route: the URL is keyed by
            // path + fit edge, so within a session its bytes don't change; the
            // max-age bounds cross-session staleness if the source is edited.
            .header(header::CACHE_CONTROL, "public, max-age=3600")
            .body(Body::from(thumb.data))
            .unwrap(),
        // Resize failed (decode error, dropped task) — fall back to the whole
        // file so the cell still shows something rather than breaking.
        other => {
            if let Ok(Err(e)) = other {
                log::warn!("fit resize failed for {path}: {e}");
            }
            let mime = mime_for(
                &std::path::Path::new(path)
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_lowercase(),
            );
            serve_image(path, mime, &HeaderMap::new()).await
        }
    }
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
                // The sprite sheet is multi-MB and GifCanvas remounts on
                // every hover/autoplay toggle — let the webview cache it.
                // Server-side it's keyed by (path, tier, mtime), so a
                // changed GIF produces fresh bytes after at most max-age.
                .header(header::CACHE_CONTROL, "public, max-age=3600")
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
        // Cacheable: mirrors the lightview:// protocol handler — the frontend
        // cache-busts with ?v= whenever bytes change.
        ThumbOutcome::Hit { data, format } => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, thumb_serve::thumb_mime(&format))
            .header(header::CACHE_CONTROL, "public, max-age=3600")
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
/// The root is canonicalized once at gallery open; the candidate is
/// canonicalized per request so `..` traversal and symlinks that escape the
/// root are rejected. False when no gallery is open or the path is missing.
async fn path_in_gallery(state: &ServerState, path: &str) -> bool {
    let root = {
        let guard = state.app.canonical_gallery_root.read().await;
        match guard.as_ref() {
            Some(r) => r.clone(),
            None => return false,
        }
    };
    let Ok(candidate) = tokio::fs::canonicalize(path).await else {
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

// ─────────────────────────────────────────────────────────────
// Uploads
// ─────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct UploadResult {
    /// Files written, with their gallery-relative destination paths.
    uploaded: Vec<UploadedItem>,
    /// Files refused, each with a human-readable reason.
    rejected: Vec<RejectedItem>,
}

#[derive(Serialize)]
struct UploadedItem {
    original: String,
    stored: String,
}

#[derive(Serialize)]
struct RejectedItem {
    original: String,
    reason: String,
}

/// `POST /api/upload` — accept one or more media files (multipart/form-data)
/// from a paired device and file them under `Uploads/` in the open gallery,
/// foldered by the host's configured scheme.
///
/// This is the only route that *writes* files on behalf of a device. It sits
/// behind the auth layer (paired cookie + optional password) like every other
/// data route; the additional `upload.enabled` gate lets a host turn it off
/// per gallery without dropping remote browsing.
///
/// Field contract: an optional `album` text field (used only by the
/// `year_album` scheme) **must precede** the file parts so it applies to them;
/// all other parts are treated as files. The frontend honors this ordering.
///
/// The written files are picked up by the gallery's recursive fs-watcher, so
/// no explicit indexing happens here.
pub async fn upload(
    State(state): State<ServerState>,
    mut multipart: Multipart,
) -> Response<Body> {
    // The canonical gallery root (resolved at open) gates the whole request
    // and anchors the path-confinement check.
    let root = {
        let guard = state.app.canonical_gallery_root.read().await;
        match guard.as_ref() {
            Some(r) => r.clone(),
            None => return error(StatusCode::SERVICE_UNAVAILABLE),
        }
    };

    // Read the per-gallery upload config (enabled + scheme).
    let scheme = {
        let guard = state.app.cache_db.lock().await;
        let Some(db) = guard.as_ref() else {
            return error(StatusCode::SERVICE_UNAVAILABLE);
        };
        match uploads::get_config(db.conn()) {
            Ok(cfg) if cfg.enabled => cfg.scheme,
            Ok(_) => return error(StatusCode::FORBIDDEN),
            Err(e) => {
                log::error!("upload: failed to read config: {e}");
                return error(StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
    };

    let mut album: Option<String> = None;
    let mut uploaded = Vec::new();
    let mut rejected = Vec::new();

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => {
                log::warn!("upload: malformed multipart: {e}");
                return error(StatusCode::BAD_REQUEST);
            }
        };

        // A part with no filename is a plain form field; the only one we read
        // is `album`. Everything with a filename is treated as a file.
        let Some(raw_name) = field.file_name().map(|s| s.to_string()) else {
            if field.name() == Some("album") {
                album = field.text().await.ok().filter(|s| !s.trim().is_empty());
            }
            continue;
        };

        let ext = std::path::Path::new(&raw_name)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        let media_type = MediaType::from_extension(&ext);
        if media_type.is_none() {
            rejected.push(RejectedItem {
                original: raw_name,
                reason: "unsupported file type".to_string(),
            });
            continue;
        }

        let bytes = match field.bytes().await {
            Ok(b) => b,
            Err(e) => {
                rejected.push(RejectedItem {
                    original: raw_name,
                    reason: format!("read failed: {e}"),
                });
                continue;
            }
        };

        // Capture time from EXIF for stills (videos rarely carry it). Drives
        // both the year/month folder and the file's mtime — which the gallery
        // indexes as `date_taken`, so an old photo sorts by when it was taken.
        let capture = match media_type {
            Some(MediaType::Image) | Some(MediaType::Gif) => {
                crate::pipeline::exif::capture_datetime_from_bytes(&bytes)
            }
            _ => None,
        };
        let (year, month) = capture
            .map(|dt| (dt.year(), dt.month()))
            .unwrap_or_else(now_year_month);
        let mtime = capture.and_then(naive_to_system_time);

        let safe_name = uploads::sanitize_component(&raw_name)
            .unwrap_or_else(|| fallback_name(&ext));

        let rel_dir = uploads::relative_dir(scheme, year, month, album.as_deref());
        let dir = root.join(&rel_dir);

        if !uploads::confine(&root, &dir) {
            log::error!("upload: refusing out-of-gallery destination {dir:?}");
            rejected.push(RejectedItem {
                original: raw_name,
                reason: "invalid destination".to_string(),
            });
            continue;
        }

        if let Err(e) = tokio::fs::create_dir_all(&dir).await {
            log::error!("upload: create_dir_all {dir:?} failed: {e}");
            rejected.push(RejectedItem {
                original: raw_name,
                reason: "could not create folder".to_string(),
            });
            continue;
        }

        let dest = uploads::dedupe_path(&dir, &safe_name);
        match write_atomic(&dir, &dest, &bytes, mtime).await {
            Ok(()) => {
                let stored = dest
                    .strip_prefix(&root)
                    .unwrap_or(&dest)
                    .to_string_lossy()
                    .into_owned();
                uploaded.push(UploadedItem {
                    original: raw_name,
                    stored,
                });
            }
            Err(e) => {
                log::error!("upload: write {dest:?} failed: {e}");
                rejected.push(RejectedItem {
                    original: raw_name,
                    reason: "write failed".to_string(),
                });
            }
        }
    }

    Json(UploadResult { uploaded, rejected }).into_response()
}

/// Write `bytes` to `dest` via a temp file in the same directory plus a rename,
/// so a reader (or the fs-watcher) never observes a half-written file. When
/// `mtime` is set, it's applied to the temp file *before* the rename so the
/// final file already carries the capture time when the watcher first sees it —
/// otherwise the indexer could record the upload time as `date_taken`.
async fn write_atomic(
    dir: &std::path::Path,
    dest: &std::path::Path,
    bytes: &[u8],
    mtime: Option<std::time::SystemTime>,
) -> std::io::Result<()> {
    let tmp = dir.join(format!(".lv-upload-{}.tmp", uuid_like()));
    tokio::fs::write(&tmp, bytes).await?;

    if let Some(t) = mtime {
        let tmp2 = tmp.clone();
        // set_modified needs a write-opened std File; do it off the async pool.
        let res = tokio::task::spawn_blocking(move || {
            std::fs::OpenOptions::new()
                .write(true)
                .open(&tmp2)
                .and_then(|f| f.set_modified(t))
        })
        .await;
        // Best-effort: a failed mtime stamp shouldn't abort the upload.
        if let Ok(Err(e)) = res {
            log::warn!("upload: failed to set capture mtime on {tmp:?}: {e}");
        }
    }

    match tokio::fs::rename(&tmp, dest).await {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            Err(e)
        }
    }
}

fn now_year_month() -> (i32, u32) {
    let now = chrono::Local::now();
    (now.year(), now.month())
}

/// Convert an EXIF naive (timezone-less) capture time to a `SystemTime`,
/// interpreting it as the host's local time — the same basis the rest of the
/// app uses for file mtimes. `None` for ambiguous/pre-epoch values.
fn naive_to_system_time(dt: chrono::NaiveDateTime) -> Option<std::time::SystemTime> {
    use chrono::TimeZone;
    let ts = chrono::Local.from_local_datetime(&dt).single()?.timestamp();
    if ts < 0 {
        return None;
    }
    Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(ts as u64))
}

fn fallback_name(ext: &str) -> String {
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
    if ext.is_empty() {
        format!("upload-{ts}")
    } else {
        format!("upload-{ts}.{ext}")
    }
}

/// Cheap unique-ish suffix for temp files — nanosecond clock plus a random
/// component. Doesn't need to be a real UUID; it only has to avoid collisions
/// between concurrent writes in the same directory.
fn uuid_like() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let rand: u32 = rand::random();
    format!("{nanos:x}-{rand:x}")
}
