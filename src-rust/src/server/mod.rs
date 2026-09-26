//! The adapter: routes, one command table, and the trust level each entry
//! requires.
//!
//! **The server owns who may ask; the services own what happens.** There is
//! one dispatch: every command from every client is an entry in [`commands`],
//! and each entry is a trust check and a call into a service, carrying its
//! minimum trust as a field so no second list has to agree with it. Where each
//! request goes:
//!
//! ```text
//! POST /api/invoke ──→ commands ──→ services ──→ cache (SQLite)
//!                                            └─→ companion (sidecars on disk)
//! GET  /thumb/{tier}/{path} ──→ pipeline ──→ cache, generating on a miss
//! GET  /media/{path} ─────────→ pipeline ──→ the original file, Range/206
//! GET  /api/events ───────────→ events: one broadcast channel, relayed as SSE
//! POST /api/upload ───────────→ upload: staged write, rename, the watcher ingests
//! ```
//!
//! [`routes`] has the full surface and the trust of each route; [`listen`]
//! has why that trust is fixed by the bind.

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
