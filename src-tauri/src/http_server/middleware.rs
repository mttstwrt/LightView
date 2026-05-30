use axum::{
    extract::{ConnectInfo, State},
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::Response,
};
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

use super::config::AuthMode;
use super::devices;
use super::server::ServerState;

/// Cookie name carrying the per-device pairing cookie. The value is of the
/// form `<device_id>.<secret>` — see `devices::verify_cookie`.
pub const DEVICE_COOKIE: &str = "lv_device";

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

/// Per-device cookie auth. Pass-through when `AuthMode::None`.
///
/// Flow:
///   1. Read `lv_device` cookie. Missing → 401.
///   2. Lock the open gallery's cache.db. None → 503 (gallery closed).
///   3. Verify cookie → device. Failure → 401.
///   4. If a gallery password is set and the device has been silent for
///      longer than the inactivity window, return 401 with
///      `WWW-Authenticate: LV-Password` so the client knows to prompt.
///   5. Touch `last_seen` and pass.
pub async fn auth_layer(
    State(state): State<ServerState>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: Next,
) -> Result<Response, StatusCode> {
    if matches!(state.config.auth, AuthMode::None) {
        return Ok(next.run(request).await);
    }

    let cookie = match cookie_value(&headers, DEVICE_COOKIE) {
        Some(c) => c,
        None => return Err(StatusCode::UNAUTHORIZED),
    };

    // Hold the cache_db lock only long enough to do the verification + state
    // updates. SHA-256 verify is fast enough to run inline; we don't want to
    // re-lock for the touch.
    let needs_password_challenge = {
        let guard = state.app.cache_db.lock().await;
        let Some(db) = guard.as_ref() else {
            // No gallery open — there's no per-gallery DB to authenticate
            // against. Tell the client to retry later.
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        };
        let conn = db.conn();

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

        stale
    };

    if needs_password_challenge {
        return Ok(password_challenge_response());
    }

    Ok(next.run(request).await)
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
