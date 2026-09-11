//! Ordering and grouping an already-filtered set of paths.
//!
//! - [`sorter`] — the sort vocabulary, the `ORDER BY` construction and the row
//!   mapper. Every column is qualified with the `m.` alias; there is no join to
//!   be ambiguous with today, and keeping the habit is what a later one would
//!   otherwise break.
//! - [`grouper`] — group headers, computed in memory over the sorted list.
//!
//! Grouping never reorders. It walks the sorted items and emits a header
//! wherever the group key changes, so a grouping that disagrees with the sort
//! field produces fragmented headers rather than a silently different order —
//! grouping describes the ordering, it does not impose one.

pub mod sorter;
pub mod grouper;
