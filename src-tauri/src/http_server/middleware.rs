use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::Response,
};
use std::collections::HashMap;

use super::config::AuthMode;
use super::server::ServerState;

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

    let supplied = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .or_else(|| params.get("token").map(|s| s.as_str()));

    match supplied {
        Some(t) if constant_time_eq(t, expected) => Ok(next.run(request).await),
        _ => Err(StatusCode::UNAUTHORIZED),
    }
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
