//! Request-scoped policy: auth, cache headers, and the remote-hit counter.
//!
//! [`auth_layer`] is the authentication boundary — it decides whether a request
//! carries a valid device cookie, and (when a gallery password is set) whether
//! that device has proved it recently enough. It says nothing about *what* the
//! device may then do; that is `api`'s allowlist.
//!
//! A failed password check answers 401 with `WWW-Authenticate: LV-Password`,
//! which the frontend transport recognises, prompts for, and retries — so the
//! challenge never surfaces to calling code. A 401 *without* that header means
//! the cookie itself is bad, and the client redirects to pairing. The two cases
//! must stay distinguishable or a password prompt and a re-pair become the same
//! event to the client.
//!
//! [`track_remote_hits`] counts requests from non-loopback peers. That counter
//! is the only dependable evidence that the port is actually reachable through
//! the host firewall, which is what the settings UI reports.

use axum::{
    extract::{ConnectInfo, State},
    http::{header::CACHE_CONTROL, HeaderMap, HeaderValue, StatusCode},
    middleware::Next,
    response::Response,
};
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

use super::config::AuthMode;
use super::devices;
use super::server::ServerState;

/// Look up the device cookie, preferring this gallery's own name
/// (`lv_device_<suffix>`, see [`devices::cookie_name`]) and falling back to the
/// bare `lv_device` a device paired before per-gallery scoping still holds.
///
/// The `bool` is true when only the legacy name matched; callers answer that by
/// re-issuing the same value under the gallery's name with
/// [`device_cookie_header`], so nobody has to pair again. The legacy cookie is
/// deliberately *not* cleared — it is the one entry two galleries share, so
/// expiring it here would un-pair whichever gallery has not yet re-issued.
pub fn device_cookie(headers: &HeaderMap, name: &str) -> Option<(String, bool)> {
    if let Some(value) = cookie_value(headers, name) {
        return Some((value, false));
    }
    // A cookie named `lv_device_ab12` cannot match the `lv_device=` prefix, so
    // the fallback never shadows a gallery-scoped cookie.
    cookie_value(headers, devices::DEVICE_COOKIE_BASE).map(|value| (value, true))
}

/// `Set-Cookie` for a device cookie. One year, `HttpOnly`, `SameSite=Strict`,
/// and `Secure` whenever the connection is TLS so the value never travels in
/// the clear to the same host on a plaintext port.
pub fn device_cookie_header(name: &str, value: &str, tls: bool) -> String {
    format!(
        "{name}={value}; Path=/; Max-Age=31536000; SameSite=Strict; HttpOnly{}",
        if tls { "; Secure" } else { "" }
    )
}

/// Record every request from a non-loopback peer. A non-zero counter is proof
/// that an external device reached the server, which is the only dependable
/// "the port is open" signal we can produce from the host side.
pub async fn track_remote_hits(
    State(state): State<ServerState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    if !peer.ip().is_loopback() {
        state.remote_hits.fetch_add(1, Ordering::Relaxed);
    }
    next.run(request).await
}

/// Readiness gate: until a gallery is open (`AppState::current_gallery` set),
/// every route except those in [`GATE_EXEMPT`] answers 503 with a tiny
/// self-refreshing "starting" page. The headless server binds its listener
/// before the (potentially minutes-long) gallery scan, so early requests get a
/// sign of life instead of a refused connection; the page reloads itself and
/// flips to the app the moment the scan finishes.
///
/// The exemptions serve content that doesn't depend on a gallery at all, and
/// that a client may specifically need *while* one is opening: `/healthz` is
/// the liveness probe, and `/cert` hands out the TLS certificate — a browser
/// trying to establish trust shouldn't be told to come back after the scan.
const GATE_EXEMPT: [&str; 2] = ["/healthz", "/cert"];

pub async fn gallery_gate(
    State(state): State<ServerState>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let exempt = GATE_EXEMPT.contains(&request.uri().path());
    if !exempt && state.app.current_gallery.read().await.is_none() {
        return starting_response();
    }
    next.run(request).await
}

fn starting_response() -> Response {
    const BODY: &str = "<!doctype html><html><head><meta charset=\"utf-8\">\
        <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
        <meta http-equiv=\"refresh\" content=\"3\">\
        <title>LightView &mdash; starting</title></head>\
        <body style=\"font-family:system-ui;display:grid;place-items:center;height:100vh;margin:0\">\
        <p>LightView is starting &mdash; indexing the gallery&hellip; \
        This page retries automatically.</p></body></html>";
    let mut resp = Response::new(axum::body::Body::from(BODY));
    *resp.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
    resp.headers_mut()
        .insert("Retry-After", HeaderValue::from_static("3"));
    resp.headers_mut().insert(
        "Content-Type",
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    resp
}

/// Cache policy for the statically-served SPA.
///
/// Build assets carry a content hash in their filename (`main-<hash>.js`), so a
/// given URL is immutable — cache it for a year. The app shell, by contrast,
/// must be revalidated on every load: with no policy, a phone's home-screen
/// web-app keeps a stale `index.html` that still points at an old bundle and
/// never picks up new builds (the exact failure that motivated this). `no-cache`
/// lets the browser keep a copy but forces an `If-Modified-Since` revalidation,
/// which is a cheap 304 when nothing changed.
pub async fn static_cache_control(request: axum::extract::Request, next: Next) -> Response {
    let immutable = request.uri().path().starts_with("/assets/");
    let mut response = next.run(request).await;
    let value = if immutable {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static(value));
    response
}

/// Per-device cookie auth. Pass-through when `AuthMode::None`.
///
/// Flow:
///   1. Lock the open gallery's cache.db. None → 503 (gallery closed). The
///      lock comes first because the cookie's *name* is per-gallery and lives
///      in that DB.
///   2. Read this gallery's cookie, or the legacy bare `lv_device`. Missing
///      → 401.
///   3. Verify cookie → device. Failure → 401.
///   4. If a gallery password is set and the device has been silent for
///      longer than the inactivity window, return 401 with
///      `WWW-Authenticate: LV-Password` so the client knows to prompt.
///   5. Touch `last_seen` and pass, re-issuing the cookie under this
///      gallery's name if it arrived under the legacy one.
pub async fn auth_layer(
    State(state): State<ServerState>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: Next,
) -> Result<Response, StatusCode> {
    if matches!(state.config.auth, AuthMode::None) {
        return Ok(next.run(request).await);
    }

    // Hold the cache_db lock only long enough to do the verification + state
    // updates. SHA-256 verify is fast enough to run inline; we don't want to
    // re-lock for the touch.
    let (needs_password_challenge, reissue) = {
        let guard = state.app.cache_db.lock().await;
        let Some(db) = guard.as_ref() else {
            // No gallery open — there's no per-gallery DB to authenticate
            // against. Tell the client to retry later.
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        };
        let conn = db.conn();

        let name = devices::cookie_name(conn);
        let Some((cookie, via_legacy)) = device_cookie(&headers, &name) else {
            return Err(StatusCode::UNAUTHORIZED);
        };

        let device = match devices::verify_cookie(conn, &cookie) {
            Some(d) => d,
            None => return Err(StatusCode::UNAUTHORIZED),
        };

        let password_set = devices::get_password_hash(conn)
            .ok()
            .flatten()
            .is_some();
        let inactivity = devices::get_inactivity_secs(conn).unwrap_or(0);
        let now = now_secs();
        let stale = password_set && (now - device.last_auth_at) > inactivity;

        if !stale {
            // Bump last_seen on every authenticated request. Cheap write.
            let _ = devices::touch_device(conn, &device.id);
        }

        let reissue =
            via_legacy.then(|| device_cookie_header(&name, &cookie, state.config.tls));
        (stale, reissue)
    };

    if needs_password_challenge {
        return Ok(password_challenge_response());
    }

    let mut response = next.run(request).await;
    // Append rather than insert: the inner handler may already be setting
    // cookies of its own, and this must not replace them.
    if let Some(header) = reissue
        && let Ok(v) = header.parse()
    {
        response.headers_mut().append("set-cookie", v);
    }
    Ok(response)
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get("cookie")?.to_str().ok()?;
    let prefix = format!("{}=", name);
    raw.split(';').find_map(|part| {
        let trimmed = part.trim();
        trimmed.strip_prefix(&prefix).map(|v| v.to_string())
    })
}

fn password_challenge_response() -> Response {
    let mut resp = Response::new(axum::body::Body::from("password required"));
    *resp.status_mut() = StatusCode::UNAUTHORIZED;
    // Custom scheme so the browser's basic-auth dialog doesn't pop up — the
    // SPA listens for this header and shows its own modal.
    if let Ok(v) = "LV-Password".parse() {
        resp.headers_mut().insert("WWW-Authenticate", v);
    }
    resp
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
