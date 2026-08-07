//! Ordering and grouping an already-filtered set of paths.
//!
//! - [`sorter`] — the sort vocabulary and the `ORDER BY` construction. The
//!   query it builds joins `thumbnails` for the inlined ThumbHash, which is why
//!   every column is qualified with the `m.` alias.
//! - [`grouper`] — group headers, computed in memory over the sorted list.
//! - [`timeline`] — the month-boundary index the scrollbar's date markers use.
//!
//! Grouping never reorders. It walks the sorted items and emits a header
//! wherever the group key changes, so a grouping that disagrees with the sort
//! field produces fragmented headers rather than a silently different order —
//! grouping describes the ordering, it does not impose one.

pub mod sorter;
pub mod grouper;
pub mod timeline;
