//! LightView — a local media gallery that also serves itself over the LAN.
//!
//! Three layers, and nothing points upward:
//!
//! ```text
//!   pure libraries   filter · sort · autocomplete · geocode · companion · path
//!                    util · hardware · file_clipboard
//!         ↑          (take a connection or a struct; know nothing above them)
//!   services         cache · pipeline · plugin · provider
//!                    services::{media, gallery, tags, files, duplicates,
//!                               trash, settings}
//!                    state::Gallery — the open gallery the services share
//!         ↑          (take state or pieces of it; no HTTP types)
//!   adapter          server (routes + one command table) · cli
//!                    state::AppState — what the listener adds to the gallery
//! ```
//!
//! The rule that keeps the layering honest: `cache/` must not learn what a
//! route is, and the pure libraries must not learn what application state is.
//! A module that needs to know who called it is in the wrong layer.
//!
//! One edge points upward today, and it is a known seam rather than a pattern
//! to copy: `services::{gallery, tags}` publish [`server::events::Event`], and
//! [`state::Gallery`] holds the channel, so the event vocabulary lives in the
//! adapter while the services are what speak it.
//!
//! **A process has one gallery and one listener, both fixed at startup** — see
//! [`cli`] for the modes and [`server::listen`] for why the listener's trust is
//! the process's trust. **Everything durable lives in the gallery and
//! everything derived lives outside it** — see [`util::paths`].

pub mod autocomplete;
pub mod cache;
pub mod cli;
pub mod companion;
pub mod file_clipboard;
pub mod filter;
pub mod geocode;
pub mod hardware;
pub mod path;
pub mod pipeline;
pub mod plugin;
pub mod provider;
pub mod server;
pub mod services;
pub mod state;
pub mod sort;
pub mod util;
