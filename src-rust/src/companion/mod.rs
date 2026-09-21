//! The `.lightview.json` sidecar: the record of what the user has said about a
//! media file.
//!
//! This, not the database, is the source of truth for tags, ratings, notes, and
//! location. The cache indexes it for query speed and can be rebuilt from it.
//!
//! - [`schema`] — the on-disk shape, plus the `MediaType` vocabulary (media
//!   type is a companion field, so extension inference lives here).
//! - [`reader`] / [`writer`] — locating, parsing, and atomically replacing it.
//! - [`migration`] — version upgrade, called on every read.
//!
//! The tag namespaces are the load-bearing part of the format: `user` is never
//! written by anything but the user, and a plugin writes only under its own key
//! in `tags.plugins`, so re-running a tagger replaces that tagger's output and
//! touches nothing else.
//!
//! Note that `schema_version` here is unrelated to the cache's `SCHEMA_VERSION`.
//! This one describes a file that lives in the user's gallery and must be
//! readable by other installations; that one describes a local database that
//! can be thrown away. They move independently.

pub mod schema;
pub mod reader;
pub mod writer;
pub mod migration;
