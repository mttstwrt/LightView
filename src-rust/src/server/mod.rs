//! The adapter: routes, one command table, and the trust level each entry
//! requires.
//!
//! There is one dispatch. The `*_impl` convention that kept a Tauri command
//! registry and an HTTP dispatch in step disappears with the second adapter,
//! and with it the 78-command registration and the 46-arm allowlist that had to
//! agree with it by hand.

pub mod auth;
pub mod commands;
pub mod config;
pub mod devices;
pub mod events;
pub mod listen;
pub mod routes;
pub mod tls;
pub mod upload;
pub mod web_assets;
