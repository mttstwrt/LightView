use axum::{
    extract::{ConnectInfo, Query, State},
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::Response,
};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;

use super::config::AuthMode;
use super::server::ServerState;

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

/// Auth middleware. Pass-through when `AuthMode::None`. When a bearer token
/// is configured, checks for `Authorization: Bearer <token>` or `?token=<t>`.
pub async fn auth_layer(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
    request: axum::extract::Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let expected = match &state.config.auth {
        AuthMode::None => return Ok(next.run(request).await),
        AuthMode::BearerToken(t) => t,
    };

    // Accept the token from a bearer header, a `?token=` query param, or an
    // `lv_token` cookie. The cookie path is what the web client uses: it lets
    // `<img>`/`<video>`/fetch all authenticate on same-origin requests without
    // rewriting every URL.
    let supplied = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .or_else(|| params.get("token").map(|s| s.as_str()))
        .or_else(|| cookie_token(&headers));

    match supplied {
        Some(t) if constant_time_eq(t, expected) => Ok(next.run(request).await),
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

/// Extract the `lv_token` cookie value from the Cookie header, if present.
fn cookie_token(headers: &HeaderMap) -> Option<&str> {
    let cookie = headers.get("cookie")?.to_str().ok()?;
    cookie.split(';').find_map(|part| {
        part.trim().strip_prefix("lv_token=")
    })
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc = 0u8;
    for (x, y) in a.bytes().zip(b.bytes()) {
        acc |= x ^ y;
    }
    acc == 0
}
