//! The per-gallery SQLite cache: schema, connections, and every SQL statement
//! in the process.
//!
//! One database file per gallery, at `<gallery>/.lightview/cache.db`, so the
//! cache travels with the data it describes — see
//! `docs/decisions/0001-one-cache-per-gallery.md`. It is rebuildable from the
//! media files plus their companion sidecars, but not free to delete: view
//! history, dedup decisions, and device pairings live only here.
//!
//! Split by concern rather than by table, since several concerns span tables:
//!
//! - [`db`] — connection setup, PRAGMAs, the migration ladder, and the
//!   cross-table maintenance operations (per-file delete, root rebase).
//! - [`thumbnails`] — the seven tier tables, the tier vocabulary
//!   (`ThumbTier::ALL` is the source of truth every sweep derives from), and
//!   the byte-budgeted LRU for the two zoom tiers.
//! - [`index`] / [`counts`] — the tag index rebuilt from companion files, and
//!   the per-tag frequency table that feeds autocomplete.
//! - [`duplicates`] — perceptual hashing and grouping.
//! - [`gif_atlas`] — cached GIF sprite sheets.
//! - [`coalescer`] — not storage at all; it dedupes concurrent *generation*
//!   of the same missing thumbnail, and lives here because the key it guards
//!   is a cache key.
//!
//! This module owns no policy about *when* to write. The single writer
//! connection is the serialization point for every write in the process, so
//! callers must do their expensive work — decode, encode, `ffprobe` — before
//! taking the lock. `docs/cache/README.md` states the invariants in full.

pub mod db;
pub mod thumbnails;
pub mod index;
pub mod counts;
pub mod gif_atlas;
pub mod duplicates;
pub mod coalescer;
