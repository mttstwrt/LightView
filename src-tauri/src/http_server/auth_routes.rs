//! Unauthenticated endpoints used to bootstrap a device's auth state:
//!
//!   - `POST /pair/redeem` — exchange a pairing code (QR token or PIN) for
//!     this gallery's `lv_device_<suffix>` cookie.
//!   - `POST /auth/password` — clear the inactivity challenge for an already
//!     paired device by proving the gallery password.
//!   - `GET  /auth/status`  — let the SPA discover whether it's authenticated
//!     and whether a password challenge is pending, without provoking 401s.
//!
//! All other state-changing operations (revoking devices, setting the
//! password, generating new pairing codes) happen via Tauri commands on the
//! desktop, never over HTTP — a remote browser is never trusted to manage
//! pairings.

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

use super::devices::{self, AuthError};
use super::middleware::{device_cookie, device_cookie_header};
use super::server::ServerState;

#[derive(Deserialize)]
pub struct RedeemRequest {
    pub code: String,
    #[serde(default)]
    pub device_name: String,
}

#[derive(Serialize)]
pub struct RedeemResponse {
    pub device_id: String,
    pub device_name: String,
}

/// `POST /pair/redeem` — consume a pairing code and set this gallery's device
/// cookie (`lv_device_<suffix>`; see `devices::cookie_name`).
pub async fn redeem(
    State(state): State<ServerState>,
    Json(req): Json<RedeemRequest>,
) -> Response {
    let result = {
        let guard = state.app.cache_db.lock().await;
        let Some(db) = guard.as_ref() else {
            return (StatusCode::SERVICE_UNAVAILABLE, "no gallery open").into_response();
        };
        let conn = db.conn();
        devices::redeem_pairing(conn, req.code.trim(), &req.device_name)
            .map(|redeemed| (redeemed, devices::cookie_name(conn)))
    };

    match result {
        Ok((redeemed, cookie_name)) => {
            let cookie_header = device_cookie_header(
                &cookie_name,
                &redeemed.cookie_value,
                state.config.tls,
            );
            let body = Json(RedeemResponse {
                device_id: redeemed.device.id.clone(),
                device_name: redeemed.device.name.clone(),
            });
            let mut resp = body.into_response();
            if let Ok(v) = cookie_header.parse() {
                resp.headers_mut().insert("set-cookie", v);
            }
            resp
        }
        Err(AuthError::InvalidPairing) => {
            (StatusCode::NOT_FOUND, "invalid pairing code").into_response()
        }
        Err(AuthError::PairingConsumed) => {
            (StatusCode::CONFLICT, "pairing code already used").into_response()
        }
        Err(AuthError::PairingExpired) => {
            (StatusCode::GONE, "pairing code expired").into_response()
        }
        Err(e) => {
            log::error!("redeem_pairing failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct PasswordRequest {
    pub password: String,
}

/// `POST /auth/password` — accept the gallery password and clear the
/// inactivity challenge for the requesting device. Requires the device
/// cookie but not a successful auth-layer pass (the cookie may exist while
/// the device is in the "stale" state).
pub async fn submit_password(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(req): Json<PasswordRequest>,
) -> Response {
    let guard = state.app.cache_db.lock().await;
    let Some(db) = guard.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no gallery open").into_response();
    };
    let conn = db.conn();

    let name = devices::cookie_name(conn);
    let Some((cookie, _)) = device_cookie(&headers, &name) else {
        return (StatusCode::UNAUTHORIZED, "no device cookie").into_response();
    };

    let device = match devices::verify_cookie(conn, &cookie) {
        Some(d) => d,
        None => return (StatusCode::UNAUTHORIZED, "invalid device").into_response(),
    };

    match devices::verify_password(conn, &req.password) {
        Ok(true) => {
            let _ = devices::mark_authenticated(conn, &device.id);
            (StatusCode::OK, "ok").into_response()
        }
        Ok(false) => (StatusCode::FORBIDDEN, "wrong password").into_response(),
        Err(e) => {
            log::error!("verify_password failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

#[derive(Serialize)]
pub struct AuthStatus {
    /// Has a valid device cookie.
    pub paired: bool,
    /// A gallery password is configured and this device is past the
    /// inactivity window. The SPA should show the password prompt.
    pub password_required: bool,
    /// True if any gallery password is set. Useful for showing the right
    /// affordance on the pair page.
    pub password_enabled: bool,
}

/// `GET /auth/status` — let the SPA decide what UI to show without
/// provoking 401s on every load.
pub async fn status(State(state): State<ServerState>, headers: HeaderMap) -> Response {
    let guard = state.app.cache_db.lock().await;
    let Some(db) = guard.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no gallery open").into_response();
    };
    let conn = db.conn();

    let name = devices::cookie_name(conn);
    let cookie = device_cookie(&headers, &name).map(|(value, _)| value);

    let password_enabled = devices::get_password_hash(conn)
        .ok()
        .flatten()
        .is_some();

    let (paired, password_required) = match cookie.as_deref().and_then(|c| devices::verify_cookie(conn, c)) {
        Some(device) => {
            let inactivity = devices::get_inactivity_secs(conn).unwrap_or(0);
            let stale = password_enabled && (now_secs() - device.last_auth_at) > inactivity;
            (true, stale)
        }
        None => (false, false),
    };

    Json(AuthStatus {
        paired,
        password_required,
        password_enabled,
    })
    .into_response()
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
