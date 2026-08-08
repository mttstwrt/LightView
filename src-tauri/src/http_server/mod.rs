//! The axum HTTP server: the LAN web interface, and the desktop's media server.
//!
//! Two instances of the same server run, configured differently, and keeping
//! them separate is deliberate:
//!
//! * **Loopback media server** (`HttpConfig::local_only`) — no auth, no static
//!   files, OS-assigned port. It exists solely because WebKitGTK refuses to
//!   play `<video>` from any non-`http(s)` scheme, including Tauri's custom
//!   protocols, so the desktop webview needs a real `http://` URL with Range
//!   support.
//! * **LAN server** (`HttpConfig::remote`) — binds `0.0.0.0` over TLS, serves
//!   the SPA, and gates everything behind per-device cookie auth.
//!
//! Sharing one instance would mean enabling remote access silently added an
//! auth layer to the desktop webview's own requests.
//!
//! Module map:
//!
//! - [`server`] — router assembly and the layering. Which routes sit behind
//!   [`middleware::auth_layer`] and which cannot (the pairing endpoints, the
//!   certificate download, the SPA shell) is decided there.
//! - [`api`] — `POST /api/invoke`, the allowlist of backend commands a paired
//!   device may call. This is the authorization boundary; auth is only
//!   authentication. See
//!   `docs/decisions/0005-remote-invoke-is-an-allowlist.md`.
//! - [`routes`] — media, thumbnails, GIF atlases, thumbhashes, uploads, and the
//!   SSE change stream.
//! - [`devices`] / [`auth_routes`] — pairing, cookie verification, and the
//!   optional gallery password.
//! - [`tls`] — the persisted self-signed certificate and its SANs.
//! - [`uploads`] — the one write channel a device has, confined three ways.
//!
//! Every route that resolves a filesystem path must call `path_in_gallery`
//! first. Without it a non-loopback bind exposes the whole host. It answers 404
//! rather than 403, so "outside the gallery" is indistinguishable from
//! "missing". Full contract in `docs/remote/README.md`.

pub mod api;
pub mod auth_routes;
pub mod config;
pub mod devices;
pub mod middleware;
pub mod routes;
pub mod server;
pub mod tls;
pub mod uploads;

pub use config::{AuthMode, HttpConfig};
pub use server::{start, RemoteAccess, RunningServer};
