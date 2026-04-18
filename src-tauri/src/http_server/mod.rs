//! Local HTTP server for serving media to the webview.
//!
//! WebKitGTK (the Linux webview backend) refuses to play `<video>` from any
//! non-http(s) URI scheme, including Tauri's custom protocols. This module
//! runs a tiny axum server on 127.0.0.1 so the webview can fetch media over
//! a real `http://` URL with Range support.
//!
//! The design is deliberately extensible: the `HttpConfig` struct gates
//! bind address and auth mode. Today we hard-code loopback + no-auth. When
//! remote access lands, flipping `bind` to `0.0.0.0` and setting
//! `AuthMode::BearerToken(_)` is the whole change.

pub mod config;
pub mod middleware;
pub mod routes;
pub mod server;

pub use config::{AuthMode, HttpConfig};
pub use server::start;
