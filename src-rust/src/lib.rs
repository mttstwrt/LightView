//! LightView — a local media gallery that also serves itself over the LAN.
//!
//! Three layers, and nothing points upward:
//!
//! ```text
//!   pure libraries   filter · sort · autocomplete · geocode · companion · util
//!         ↑          (take a connection or a struct; know nothing above them)
//!   services         cache · pipeline · plugin · tagging
//!                    media · gallery · tags · files · duplicates · trash
//!         ↑          (take state or pieces of it; no HTTP types)
//!   adapter          server (routes + one command table) + cli
//! ```
//!
//! The rule that keeps the layering honest: `cache/` must not learn what a
//! route is, and the pure libraries must not learn what application state is.

pub mod autocomplete;
pub mod cache;
pub mod companion;
pub mod file_clipboard;
pub mod filter;
pub mod geocode;
pub mod hardware;
pub mod path;
pub mod pipeline;
pub mod provider;
pub mod sort;
pub mod util;
