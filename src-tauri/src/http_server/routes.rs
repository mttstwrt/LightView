use axum::{
    body::Body,
    extract::Path,
    http::{header, HeaderMap, HeaderValue, Response, StatusCode},
    response::IntoResponse,
};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
use tokio_util::io::ReaderStream;

// 64 KiB read chunks. Small enough to keep the WebKitGTK reader fed without
// stalling on large allocations; large enough to amortize syscall overhead.
const STREAM_CHUNK: usize = 64 * 1024;

/// `GET /media/{*rel}` — serves a full-resolution media file with Range
/// support. The captured `rel` is the absolute filesystem path with the
/// leading `/` stripped (the frontend strips it before encoding so axum's
/// matchit router accepts the URL). HEIC/HEIF are transcoded to JPEG to
/// match the behavior of the `lightview://media/` custom protocol.
pub async fn media(
    Path(rel): Path<String>,
    headers: HeaderMap,
) -> Response<Body> {
    if rel.is_empty() {
        return error(StatusCode::BAD_REQUEST);
    }
    // Axum already percent-decodes path captures.
    let path = format!("/{}", rel);

    let ext = std::path::Path::new(&path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    if ext == "heic" || ext == "heif" {
        return serve_heic(&path).await;
    }

    let mime = mime_for(&ext);
    serve_file(&path, mime, &headers).await
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
