//! The whole HTTP surface, which is small enough to read in one sitting.
//!
//! | Route | Trust |
//! |---|---|
//! | `POST /api/invoke` | per command |
//! | `GET /api/events` | `Device` — SSE, one channel, typed events |
//! | `GET /thumb/{tier}/{*rel}` | `Device` — `js` · `j` · `jm` · `jh` |
//! | `GET /media/{*rel}` | `Device` — Range/206, HEIC transcode, `?fit=` |
//! | `POST /api/upload` | `Device` |
//! | `GET /api/dirs?path=` | **`Owner`** — the picker |
//! | `GET /healthz` · `GET /cert` | bootstrap |
//! | `POST /pair/redeem` · `POST /auth/launch` · `POST /auth/password` · `GET /auth/status` | bootstrap |
//!
//! **Every route above the bootstrap group answers 503 until the initial scan
//! has completed and the watcher is armed.** A file arriving in that window is
//! in neither, and nothing ever notices it.
//!
//! The bootstrap group is unauthenticated by necessity — there would otherwise
//! be no way past the auth layer the first time — and it is a route group, not
//! a trust level. No *command* is reachable unauthenticated.
//!
//! **Paths on the wire are gallery-relative.** The encoding rule travels with
//! them: percent-encode each segment independently and leave `/` literal, because
//! axum decodes captures but rejects paths containing raw encoded slashes. A
//! single `encodeURIComponent` over the whole path 404s every file in a
//! subdirectory.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path as UrlPath, Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::cache::tiers::ThumbTier;
use crate::path::RelPath;
use crate::server::auth::{self, Trust, SESSION_COOKIE};
use crate::server::commands::{self, CommandError};
use crate::server::devices::{self, DEVICE_COOKIE};
use crate::server::events;
use crate::server::upload::{self, StagedUpload};
use crate::state::AppState;

/// How long a client may cache a thumbnail before revalidating.
///
/// Revalidation is cheap — an `ETag` round trip is a few hundred bytes — and it
/// is what lets a phone returning after expiry refresh its grid without
/// re-downloading. There is no service worker and no second cache: the
/// browser's own HTTP cache does this job correctly, with no code and no
/// ceiling to get wrong.
const THUMB_MAX_AGE: u32 = 604_800;

/// Build the router.
pub fn router(state: Arc<AppState>) -> Router {
    let bootstrap = Router::new()
        .route("/healthz", get(healthz))
        .route("/cert", get(cert))
        .route("/auth/status", get(auth_status))
        .route("/auth/launch", post(auth_launch))
        .route("/auth/password", post(auth_password))
        .route("/pair/redeem", post(pair_redeem));

    let guarded = Router::new()
        .route("/api/invoke", post(invoke))
        .route("/api/events", get(sse))
        .route("/api/upload", post(upload_route))
        .route("/api/dirs", get(dirs))
        .route("/thumb/{tier}/{*rel}", get(thumb))
        .route("/media/{*rel}", get(media))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            guard_layer,
        ));

    Router::new()
        .merge(bootstrap)
        .merge(guarded)
        .fallback(crate::server::web_assets::serve)
        .with_state(state)
}

// ---------------------------------------------------------------------------
// The guard: readiness, then origin, then authentication.
// ---------------------------------------------------------------------------

async fn guard_layer(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    if !state.is_ready() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "the gallery is still opening",
        )
            .into_response();
    }

    if !origin_ok(&state, &request) {
        // The endpoint is not secret — it is in the SPA the attacker's page can
        // read — so this says what happened rather than pretending to be a
        // missing route.
        return (StatusCode::FORBIDDEN, "cross-origin request refused").into_response();
    }

    match authenticate(&state, request.headers()).await {
        Auth::Ok => next.run(request).await,
        Auth::NotAuthenticated => not_authenticated(&state),
        Auth::PasswordRequired => (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "LV-Password")],
            "password required",
        )
            .into_response(),
    }
}

enum Auth {
    Ok,
    /// No usable credential. On `--serve` the client goes to pairing; on
    /// loopback there *is* no pairing flow, so the client shows "this session
    /// has ended".
    NotAuthenticated,
    /// Paired, but the inactivity window elapsed and a password is configured.
    PasswordRequired,
}

/// A 401 with no `WWW-Authenticate` means "your credential is missing or
/// revoked".
///
/// On a served bind the client redirects to pairing. **On loopback it must
/// not**: there is no pairing flow to redirect to, and a browser cannot read
/// `instance.json` to find the new URL — that is precisely the filesystem
/// access the whole trust model exists to withhold. So the body says the
/// session has ended and that starting LightView again opens a new tab, and
/// the client treats it as a dead end.
fn not_authenticated(state: &AppState) -> Response {
    let body = match state.trust {
        Trust::Owner => {
            "This LightView session has ended. Start LightView again; it will open a new tab."
        }
        Trust::Device => "not paired",
    };
    (StatusCode::UNAUTHORIZED, body).into_response()
}

/// The `Origin` / `Sec-Fetch-Site` rule, applied to state-changing requests.
///
/// A `GET` is exempt because a cross-origin `GET` cannot be made to do anything
/// a plain `<img src>` could not already do, and because requiring the header
/// on reads would break the `curl` recipe for no gain.
fn origin_ok(state: &AppState, request: &Request) -> bool {
    if request.method() == axum::http::Method::GET {
        return true;
    }
    let header_str = |name: header::HeaderName| {
        request
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    match state.trust {
        // A loopback bind has one origin and can name it byte for byte.
        Trust::Owner => auth::origin_allowed(&state.origin, header_str(header::ORIGIN).as_deref()),
        // A `0.0.0.0` bind has no single origin to name, which is why CORS was
        // `Any` before. `Sec-Fetch-Site` is browser-populated and unforgeable
        // by a page.
        Trust::Device => auth::fetch_site_allowed(
            header_str(header::HeaderName::from_static("sec-fetch-site")).as_deref(),
        ),
    }
}

async fn authenticate(state: &AppState, headers: &HeaderMap) -> Auth {
    match state.trust {
        Trust::Owner => {
            let Some(launch) = state.launch.as_ref() else {
                return Auth::NotAuthenticated;
            };
            match cookie(headers, SESSION_COOKIE) {
                Some(value) if launch.verify(&value) => Auth::Ok,
                _ => Auth::NotAuthenticated,
            }
        }
        Trust::Device => {
            let Some(devices) = state.devices.as_ref() else {
                return Auth::NotAuthenticated;
            };
            let Some(value) = cookie(headers, DEVICE_COOKIE) else {
                return Auth::NotAuthenticated;
            };

            // Through the read pool: auth runs on every thumbnail request and
            // must never queue behind a pairing write.
            let device = {
                let conn = devices.read().await;
                devices::verify_cookie(&conn, &value)
            };
            let Some(device) = device else {
                return Auth::NotAuthenticated;
            };

            if state.config.has_password()
                && auth::challenge_due(device.last_auth_at, state.config.inactivity_secs())
            {
                return Auth::PasswordRequired;
            }

            // Rate-limited: an unconditional touch would take the writer
            // hundreds of times per scroll.
            let needs_touch = {
                let conn = devices.read().await;
                devices::needs_last_seen_touch(&conn, &device.id)
            };
            if needs_touch {
                let conn = devices.writer().await;
                let _ = devices::touch_last_seen(&conn, &device.id);
            }
            Auth::Ok
        }
    }
}

fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';')
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
}

// ---------------------------------------------------------------------------
// Bootstrap routes
// ---------------------------------------------------------------------------

async fn healthz() -> &'static str {
    "ok"
}

/// The certificate, unauthenticated.
///
/// It leaks nothing — every handshake hands out the same certificate — and it
/// must be reachable *before* the browser trusts the connection enough to pair.
async fn cert(State(state): State<Arc<AppState>>) -> Response {
    match crate::server::tls::persisted_cert_pem(&state.dirs) {
        Some(pem) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "application/x-pem-file"),
                (
                    header::CONTENT_DISPOSITION,
                    "attachment; filename=\"lightview.pem\"",
                ),
            ],
            pem,
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "no certificate").into_response(),
    }
}

/// What a client needs to know before it has any credential.
async fn auth_status(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({
        "trust": state.trust,
        "password_required": state.config.has_password(),
        // A loopback bind has no pairing flow, so a client that finds itself
        // unauthenticated there must not try to start one.
        "pairing": state.trust == Trust::Device,
    }))
}

#[derive(Deserialize)]
struct LaunchBody {
    token: String,
}

/// Exchange the launch token for the session cookie, rotating the token.
async fn auth_launch(
    State(state): State<Arc<AppState>>,
    Json(body): Json<LaunchBody>,
) -> Response {
    let Some(launch) = state.launch.as_ref() else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let Some(session) = launch.redeem(&body.token) else {
        // Single use means single use.
        return (StatusCode::UNAUTHORIZED, "invalid or spent token").into_response();
    };

    // Rewrite instance.json so a second launch finds the *current* token.
    let url = format!("{}/?t={}", state.origin, launch.token());
    if let Err(e) = auth::Instance::write(&state.gallery.cache_dir, &url) {
        log::warn!("could not update instance.json: {e}");
    }

    // No `Secure`: loopback is plaintext. That is exactly why the bind is a
    // random 127.x.x.x — the address, not the flag, is what stops this cookie
    // reaching every other local port.
    let cookie = format!("{SESSION_COOKIE}={session}; HttpOnly; SameSite=Strict; Path=/");
    (
        StatusCode::OK,
        [(header::SET_COOKIE, cookie)],
        Json(json!({ "ok": true })),
    )
        .into_response()
}

#[derive(Deserialize)]
struct PasswordBody {
    password: String,
}

/// Clear the inactivity challenge for an already-paired device.
async fn auth_password(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<PasswordBody>,
) -> Response {
    let (Some(devices), Some(value)) = (
        state.devices.as_ref(),
        cookie(&headers, DEVICE_COOKIE),
    ) else {
        return (StatusCode::UNAUTHORIZED, "not paired").into_response();
    };
    let device = {
        let conn = devices.read().await;
        devices::verify_cookie(&conn, &value)
    };
    let Some(device) = device else {
        return (StatusCode::UNAUTHORIZED, "not paired").into_response();
    };
    if !auth::verify_password(&state.config.password_hash, &body.password) {
        return (StatusCode::UNAUTHORIZED, "wrong password").into_response();
    }
    let conn = devices.writer().await;
    let _ = devices::mark_authenticated(&conn, &device.id);
    (StatusCode::OK, Json(json!({ "ok": true }))).into_response()
}

#[derive(Deserialize)]
struct PairBody {
    code: String,
    #[serde(default = "default_device_name")]
    name: String,
}

fn default_device_name() -> String {
    "device".to_string()
}

async fn pair_redeem(State(state): State<Arc<AppState>>, Json(body): Json<PairBody>) -> Response {
    let Some(devices) = state.devices.as_ref() else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let conn = devices.writer().await;
    match devices::redeem(&conn, &body.code, &body.name) {
        Ok(pairing) => {
            // `Secure` here, because a served bind is always HTTPS.
            let cookie = format!(
                "{DEVICE_COOKIE}={}; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=31536000",
                pairing.cookie_value
            );
            (
                StatusCode::OK,
                [(header::SET_COOKIE, cookie)],
                Json(json!({ "ok": true })),
            )
                .into_response()
        }
        // One answer for every failure shape, so an attacker cannot tell a
        // wrong guess from an expired code.
        Err(_) => (StatusCode::UNAUTHORIZED, "invalid code").into_response(),
    }
}

// ---------------------------------------------------------------------------
// The command table
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct Invoke {
    command: String,
    #[serde(default)]
    args: Value,
}

async fn invoke(State(state): State<Arc<AppState>>, Json(body): Json<Invoke>) -> Response {
    match commands::dispatch(&state, &body.command, body.args).await {
        Ok(value) => (StatusCode::OK, Json(value)).into_response(),
        Err(CommandError::UnknownCommand(name)) => {
            (StatusCode::NOT_FOUND, Json(json!({ "error": format!("no such command: {name}") })))
                .into_response()
        }
        // 403: the command table is not secret, and a client already knows its
        // own trust level from `get_capabilities`.
        Err(CommandError::Forbidden) => (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "this command requires a local session" })),
        )
            .into_response(),
        Err(CommandError::BadArguments(e)) => {
            (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response()
        }
        Err(CommandError::Failed(e)) => {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))).into_response()
        }
    }
}

/// The directory picker. **`Owner` only.**
///
/// A route of its own because the picker is not part of the grid's command
/// vocabulary — but it **dispatches through the command table**, so the trust
/// decision is made in exactly one place. A second `require(Owner)` here would
/// be a second thing to keep in step, which is the failure the single table
/// exists to prevent.
async fn dirs(
    State(state): State<Arc<AppState>>,
    Query(query): Query<DirsQuery>,
) -> Response {
    invoke(
        State(state),
        Json(Invoke {
            command: "list_dirs".to_string(),
            args: json!({ "path": query.path }),
        }),
    )
    .await
}

#[derive(Deserialize)]
struct DirsQuery {
    /// Absent means "the gallery root", the same as the command it dispatches
    /// to — so `curl .../api/dirs` with no query is a working first call.
    path: Option<std::path::PathBuf>,
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

async fn sse(State(state): State<Arc<AppState>>) -> Sse<impl futures::Stream<Item = Result<SseEvent, std::convert::Infallible>>> {
    let mut rx = state.gallery.events.subscribe();
    let stream = async_stream::stream! {
        while let Some(event) = events::next_for_client(&mut rx).await {
            if let Ok(data) = serde_json::to_string(&event) {
                yield Ok(SseEvent::default().data(data));
            }
        }
    };
    // A keep-alive comment every fifteen seconds: a phone's radio and every
    // intermediary between it and the server will drop an idle connection, and
    // a silent drop is what turns "reconnect and re-fetch" into "sit on a
    // confidently wrong grid".
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

// ---------------------------------------------------------------------------
// Bytes
// ---------------------------------------------------------------------------

async fn thumb(
    State(state): State<Arc<AppState>>,
    UrlPath((tier, rel)): UrlPath<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (Some(tier), Ok(path)) = (ThumbTier::from_segment(&tier), RelPath::new(&rel)) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };

    match state
        .gallery
        .thumbs
        .get_or_generate(tier, &path, true)
        .await
    {
        crate::pipeline::serve::Outcome::Hit(bytes) => {
            let etag = weak_etag(&bytes);
            if headers
                .get(header::IF_NONE_MATCH)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v == etag)
            {
                return StatusCode::NOT_MODIFIED.into_response();
            }
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, "image/webp".to_string()),
                    (header::ETAG, etag),
                    (
                        header::CACHE_CONTROL,
                        format!("private, max-age={THUMB_MAX_AGE}"),
                    ),
                ],
                bytes,
            )
                .into_response()
        }
        crate::pipeline::serve::Outcome::Miss => {
            (StatusCode::NOT_FOUND, "not found").into_response()
        }
    }
}

#[derive(Deserialize)]
struct MediaQuery {
    /// An aspect-preserving resize of a still, through the same render path and
    /// the same coalescer the tiers use.
    #[serde(default)]
    fit: Option<u32>,
}

async fn media(
    State(state): State<Arc<AppState>>,
    UrlPath(rel): UrlPath<String>,
    Query(query): Query<MediaQuery>,
    headers: HeaderMap,
) -> Response {
    let Ok(path) = RelPath::new(&rel) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    // The one canonicalize on this route, in the branch that opens a file.
    let Ok(resolved) = state.gallery.root.resolve(&path) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };

    let extension = std::path::Path::new(path.as_str())
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    // `?fit=` applies to stills only. GIF and video fall back to the whole
    // file, because a still frame is not what either of them is for.
    if let Some(edge) = query.fit
        && matches!(extension.as_str(), "jpg" | "jpeg" | "png" | "webp")
    {
        {
            let file = resolved.clone();
            let generated = tokio::task::spawn_blocking(move || {
                crate::pipeline::thumbnailer::generate_for_path_fit(
                    file.as_path(),
                    crate::pipeline::thumbnailer::filter_for_size(edge),
                    edge,
                )
            })
            .await;
            return match generated {
                Ok(Ok(result)) => (
                    StatusCode::OK,
                    [
                        (header::CONTENT_TYPE, "image/webp".to_string()),
                        (
                            header::CACHE_CONTROL,
                            format!("private, max-age={THUMB_MAX_AGE}"),
                        ),
                    ],
                    result.data,
                )
                    .into_response(),
                _ => (StatusCode::NOT_FOUND, "not found").into_response(),
            };
        }
    }

    // **HEIC is transcoded on the serve path, not only in the decoder.** No
    // browser renders HEIC, so a full-resolution request for one returns JPEG,
    // through the bounded transcode cache keyed on (path, mtime). Wiring only
    // the thumbnail decoder ships a build where HEIC thumbnails work and the
    // viewer is blank — a failure that survives to production because the grid
    // looks correct.
    if matches!(extension.as_str(), "heic" | "heif") {
        let file = resolved.clone();
        let transcoded = tokio::task::spawn_blocking(move || {
            crate::pipeline::heic_cache::get_or_transcode(file.as_path())
        })
        .await;
        return match transcoded {
            Ok(Ok(jpeg)) => (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "image/jpeg")],
                jpeg,
            )
                .into_response(),
            _ => (StatusCode::NOT_FOUND, "not found").into_response(),
        };
    }

    serve_file(resolved.as_path(), &extension, &headers).await
}

/// Serve a whole file, or a byte range.
///
/// **Range support is not optional**: `<video>` scrubbing does not work in any
/// browser without real `206` responses, and the ported viewer assumes it. The
/// range is streamed rather than buffered — the implementation this replaces
/// allocated `len - N` bytes per seek.
async fn serve_file(
    path: &std::path::Path,
    extension: &str,
    headers: &HeaderMap,
) -> Response {
    let Ok(file) = tokio::fs::File::open(path).await else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let Ok(metadata) = file.metadata().await else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let len = metadata.len();
    let mime = mime_for(extension);

    let Some(range_header) = headers.get(header::RANGE).and_then(|v| v.to_str().ok()) else {
        let stream = tokio_util::io::ReaderStream::new(file);
        return (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, mime.to_string()),
                (header::ACCEPT_RANGES, "bytes".to_string()),
                (header::CONTENT_LENGTH, len.to_string()),
            ],
            Body::from_stream(stream),
        )
            .into_response();
    };

    let Ok(ranges) = http_range::HttpRange::parse(range_header, len) else {
        return (
            StatusCode::RANGE_NOT_SATISFIABLE,
            [(header::CONTENT_RANGE, format!("bytes */{len}"))],
            "range not satisfiable",
        )
            .into_response();
    };
    // One range only. Multipart byte ranges are legal and no browser's media
    // element asks for them.
    let Some(range) = ranges.first() else {
        return (StatusCode::RANGE_NOT_SATISFIABLE, "range not satisfiable").into_response();
    };

    let mut file = file;
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    if file
        .seek(std::io::SeekFrom::Start(range.start))
        .await
        .is_err()
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, "seek failed").into_response();
    }
    // `AsyncReadExt::take`, so the stream stops at the end of the range rather
    // than running to the end of the file.
    let stream = tokio_util::io::ReaderStream::new(file.take(range.length));
    let end = range.start + range.length - 1;
    (
        StatusCode::PARTIAL_CONTENT,
        [
            (header::CONTENT_TYPE, mime.to_string()),
            (header::ACCEPT_RANGES, "bytes".to_string()),
            (header::CONTENT_LENGTH, range.length.to_string()),
            (
                header::CONTENT_RANGE,
                format!("bytes {}-{end}/{len}", range.start),
            ),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}

fn mime_for(extension: &str) -> &'static str {
    match extension {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "avif" => "image/avif",
        "heic" | "heif" => "image/heic",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "avi" => "video/x-msvideo",
        _ => "application/octet-stream",
    }
}

/// A weak ETag over the served bytes.
///
/// Weak because it identifies the *representation* rather than the byte stream,
/// and because a path is immutable within a gallery session — so a matching tag
/// genuinely means "you already have this".
fn weak_etag(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    format!("W/\"{:x}\"", digest)
}

// ---------------------------------------------------------------------------
// Upload
// ---------------------------------------------------------------------------

async fn upload_route(State(state): State<Arc<AppState>>, mut multipart: axum::extract::Multipart) -> Response {
    if !state.config.uploads_enabled {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    let directory = match upload::upload_dir(&state.gallery.root, &state.config.upload_dir) {
        Ok(d) => d,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    let mut landed = Vec::new();
    let mut parts = 0usize;

    while let Ok(Some(mut field)) = multipart.next_field().await {
        parts += 1;
        if parts > upload::MAX_PARTS {
            return (StatusCode::PAYLOAD_TOO_LARGE, "too many files").into_response();
        }
        let Some(raw_name) = field.file_name().map(str::to_string) else {
            continue;
        };
        let name = match upload::sanitize_name(&raw_name) {
            Ok(n) => n,
            Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
        };

        let mut staged = match StagedUpload::create(directory.as_path()) {
            Ok(s) => s,
            Err(e) => return (StatusCode::INSUFFICIENT_STORAGE, e.to_string()).into_response(),
        };
        // Streamed, so a 4 GB clip never sits in RAM. The staged file cleans
        // itself up on every early return below.
        while let Ok(Some(chunk)) = field.chunk().await {
            if let Err(e) = staged.write(&chunk) {
                return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
            }
        }
        match staged.commit(directory.as_path(), &name, None) {
            Ok(path) => landed.push(path.display().to_string()),
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        }
    }

    // No separate indexing path: the ordinary fs-watcher ingests what landed.
    (StatusCode::OK, Json(json!({ "uploaded": landed }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn a_cookie_is_read_out_of_a_crowded_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("other=1; lv_device=abc.def; theme=dark"),
        );
        assert_eq!(cookie(&headers, DEVICE_COOKIE).as_deref(), Some("abc.def"));
        assert_eq!(cookie(&headers, SESSION_COOKIE), None);
    }

    #[test]
    fn a_cookie_whose_name_is_a_prefix_is_not_matched() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("lv_device_old=nope; lv_device=real"),
        );
        assert_eq!(cookie(&headers, DEVICE_COOKIE).as_deref(), Some("real"));
    }

    #[test]
    fn the_etag_changes_with_the_bytes() {
        assert_ne!(weak_etag(b"a"), weak_etag(b"b"));
        assert_eq!(weak_etag(b"a"), weak_etag(b"a"));
        assert!(weak_etag(b"a").starts_with("W/\""));
    }

    #[test]
    fn every_media_extension_the_scan_accepts_has_a_mime_type() {
        // A file the gallery indexes but the route serves as
        // application/octet-stream is a viewer that silently shows nothing.
        for ext in [
            "jpg", "jpeg", "png", "webp", "bmp", "tiff", "tif", "heic", "heif", "avif", "gif",
            "mp4", "mov", "mkv", "webm", "m4v", "avi",
        ] {
            assert_ne!(
                mime_for(ext),
                "application/octet-stream",
                "{ext} has no MIME type"
            );
        }
    }
}
