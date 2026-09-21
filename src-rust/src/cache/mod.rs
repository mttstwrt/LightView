//! The derived cache — everything here is reconstructable from the photos and
//! their companion files, and none of it is durable.
//!
//! That is the property the whole module is organized around. It lives under
//! `$XDG_CACHE_HOME`, it is budgeted, it is deletable at any moment, and when
//! the build's `format_version` does not match the file's it *is* deleted
//! rather than migrated.
//!
//! - [`db`] — schema, connections, the path-keyed sweep, `format_version`.
//! - [`pool`] — read-only connections for the thumbnail serve path.
//! - [`meta`] — `media_meta`, and the companion fields mirrored into columns
//!   because the query language can only filter what is indexed.
//! - [`index`] — `tag_index` and the `index_state` skip gate.
//! - [`tiers`] — the four cached edges and the LRU byte budget on the two
//!   large ones.
//! - [`coalescer`] — one generator per `(path, tier)`, released on cancel.
//! - [`duplicates`] — dHash over the `j` tier, and set co-membership as the
//!   replacement for stored "not a duplicate" verdicts.
//! - [`store`] — the cache directory across galleries: its ceiling, and which
//!   gallery gives way first.
//!
//! **`cache/` must not learn what a route is.** It takes connections and plain
//! values; the services above it decide when to call and the adapter above
//! those decides who may.

pub mod coalescer;
pub mod db;
pub mod duplicates;
pub mod index;
pub mod meta;
pub mod pool;
pub mod store;
pub mod tiers;
