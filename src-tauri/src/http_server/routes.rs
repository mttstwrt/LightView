use axum::{
    body::Body,
    extract::Path,
    http::{header, HeaderMap, HeaderValue, Response, StatusCode},
    response::IntoResponse,
};
use memmap2::Mmap;
use std::io::{Read, Seek, SeekFrom};

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
    tokio::task::spawn_blocking(move || serve_file(&path, mime, &headers))
        .await
        .unwrap_or_else(|_| error(StatusCode::INTERNAL_SERVER_ERROR))
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

fn serve_file(path: &str, mime: &'static str, headers: &HeaderMap) -> Response<Body> {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            log::error!("Failed to open media file {}: {}", path, e);
            return error(StatusCode::NOT_FOUND);
        }
    };

    let len = match file.metadata() {
        Ok(m) => m.len(),
        Err(e) => {
            log::error!("Failed to read metadata for {}: {}", path, e);
            return error(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };

    const MAX_CHUNK: u64 = 2 * 1024 * 1024;

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
            let mut end = start + range.length - 1;

            if start >= len || end >= len {
                return Response::builder()
                    .status(StatusCode::RANGE_NOT_SATISFIABLE)
                    .header(header::CONTENT_RANGE, format!("bytes */{len}"))
                    .body(Body::empty())
                    .unwrap();
            }

            end = start + (end - start).min(MAX_CHUNK - 1);
            let nbytes = (end + 1 - start) as usize;

            let mut buf = vec![0u8; nbytes];
            if let Err(e) = file
                .seek(SeekFrom::Start(start))
                .and_then(|_| file.read_exact(&mut buf))
            {
                log::error!("Failed to read range {}-{} of {}: {}", start, end, path, e);
                return error(StatusCode::INTERNAL_SERVER_ERROR);
            }

            return Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_TYPE, HeaderValue::from_static(mime))
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{len}"))
                .header(header::CONTENT_LENGTH, nbytes.to_string())
                .header(header::CACHE_CONTROL, "no-cache")
                .body(Body::from(buf))
                .unwrap();
        }
    }

    // No Range header — serve full body.
    // SAFETY: read-only mmap, file handle retained for the copy.
    match unsafe { Mmap::map(&file) } {
        Ok(mmap) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, HeaderValue::from_static(mime))
            .header(header::CONTENT_LENGTH, len.to_string())
            .header(header::ACCEPT_RANGES, "bytes")
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Body::from(mmap[..].to_vec()))
            .unwrap(),
        Err(e) => {
            log::error!("Failed to mmap media file {}: {}", path, e);
            error(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
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
