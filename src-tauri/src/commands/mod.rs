//! Tauri command handlers — one of the two adapters onto the domain modules.
//!
//! Every command follows the same shape, and the shape is the point:
//!
//! ```ignore
//! #[tauri::command]
//! pub async fn foo(state: tauri::State<'_, AppState>, ..) -> Result<T, String> {
//!     foo_impl(&state, ..).await
//! }
//! pub async fn foo_impl(state: &AppState, ..) -> Result<T, String> { .. }
//! ```
//!
//! The wrapper exists only to unwrap `tauri::State`. All the behaviour lives in
//! `*_impl`, which takes a plain `&AppState` — and that is what
//! `http_server::api` dispatches to, so a web client and the desktop run
//! *identical* code rather than two implementations that drift. Adding logic to
//! the wrapper instead of the impl silently makes a feature desktop-only.
//!
//! Commands the web must never reach (file copy/move, plugin execution, render
//! config) are excluded by their absence from the `/api/invoke` allowlist, not
//! by anything here — see
//! `docs/decisions/0005-remote-invoke-is-an-allowlist.md`.
//!
//! Errors are `String` because that is what crosses the IPC boundary. Domain
//! error types stop at this layer.

pub mod gallery;
pub mod media;
pub mod tags;
pub mod filter;
pub mod geo;
pub mod autocomplete;
pub mod sort;
pub mod plugins;
pub mod settings;
pub mod viewer;
pub mod files;
pub mod trash;
pub mod duplicates;
