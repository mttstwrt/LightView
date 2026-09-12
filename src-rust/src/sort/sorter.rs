//! Sort fields, and the one `SELECT ... ORDER BY` that serves the grid.
//!
//! One query returns everything the grid needs for first paint, including the
//! ~25-byte ThumbHash. Inlining that is what lets a client paint a recognisable
//! blurry grid before any thumbnail request goes out, instead of a round-trip
//! per cell.
//!
//! **There is no join.** The ThumbHash lives on `media_meta`, not on a
//! thumbnail tier, and the difference is whether a gallery opens instantly. On
//! a tier table the hash sits *after* a 20–40 KB blob in record order; SQLite
//! spills a blob that size to overflow pages, so reaching a column past it
//! walks that row's overflow chain — the join touched essentially every byte of
//! the thumbnail table to produce 25 bytes a row, on the most frequent
//! expensive query in the system. `phash` stays on the tier, because it is read
//! once per duplicate scan rather than per gallery open.
//!
//! **The filter compiles into this statement.** It used to be two steps: a
//! filter command returned a `Vec<String>` of paths, the *client* handed them
//! straight back, and `json_each` re-expanded them. At 20k matches that is
//! roughly a megabyte of paths up and a multi-megabyte payload down, per
//! debounced keystroke, to a phone — to save re-running one indexed SQL scan.
//! The `WHERE` fragment arrives here instead.
//!
//! Every column is qualified with the `m` alias even though there is nothing to
//! be ambiguous with today. The habit is what a join *reintroduced* once
//! already: `path` and `media_type` existed on both tables, and an unqualified
//! `ORDER BY` made SQLite reject the whole statement, so sorting by Name or
//! Media Type — or any sort using one as its tiebreaker — failed outright.

use serde::{Deserialize, Serialize};

use crate::path::RelPath;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortField {
    Date,
    Size,
    Name,
    Rating,
    MediaType,
    LastViewed,
    DateAdded,
    LastRated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortOrder {
    Asc,
    Desc,
}

impl SortOrder {
    fn as_sql(self) -> &'static str {
        match self {
            SortOrder::Asc => "ASC",
            SortOrder::Desc => "DESC",
        }
    }
}

/// A single item in the sorted results list.
#[derive(Debug, Clone, Serialize)]
pub struct SortedItem {
    pub path: RelPath,
    /// The date the grid orders and groups by: the capture time when the file
    /// has one, its modification time when it does not. **Not `date_taken`** —
    /// that field is the camera's `DateTimeOriginal` and is what the `date=`
    /// filters compile against; this one is never NULL. The name is the
    /// distinction: a screenshot has a date here and no capture time anywhere.
    pub date: Option<i64>,
    pub file_size: i64,
    pub media_type: String,
    pub rating: Option<u8>,
    /// Colour label, lowercased. Rides along here for the same reason `rating`
    /// does — the grid draws it per cell, so a per-item round-trip would be one
    /// request per visible thumbnail.
    pub color_label: Option<String>,
    pub last_viewed: Option<i64>,
    pub date_added: Option<i64>,
    pub last_rated: Option<i64>,
    /// Video duration in seconds, if known (probed lazily during thumbnailing).
    pub duration: Option<f64>,
    /// Source media dimensions, if indexed. Drive aspect-ratio layout without
    /// an extra round-trip.
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Base64 of the ~25-byte ThumbHash placeholder, when one has been
    /// computed.
    pub thumbhash: Option<String>,
}

/// What the grid orders, groups and labels a file by.
///
/// **Capture time when there is one, file mtime when there is not.** EXIF only
/// answers for photographs straight out of a camera; a screenshot, an export,
/// an image out of a messaging app and every video carry no
/// `DateTimeOriginal`, and ordering by `date_taken` alone swept all of them
/// into one undated heap at the end of the library.
///
/// The filters deliberately do **not** coalesce: `date=2024` means *taken* in
/// 2024, and answering it with files merely copied in 2024 would be a worse
/// wrong than the heap. Ordering has to total-order everything; filtering has
/// to mean what it says.
///
/// Assumes the `media_meta m` alias, which both call sites use. It is a
/// constant so the second one — the idle warmer, whose whole justification is
/// warming the order the grid presents — cannot drift from the first.
pub const SORT_DATE: &str = "COALESCE(m.date_taken, m.mtime)";

/// The column list, kept beside the row mapper because the mapper is
/// positional: inserting a column here without shifting every index there
/// silently moves every field one place along.
fn cols() -> String {
    format!(
        "m.path, {SORT_DATE}, m.file_size, m.media_type, m.rating, \
         m.color_label, m.last_viewed, m.date_added, m.last_rated, \
         m.duration, m.width, m.height, m.thumbhash"
    )
}

/// The `ORDER BY` expression for one field and direction.
fn order_expr(field: SortField, order: SortOrder) -> String {
    let o = order.as_sql();
    match field {
        // No `NULLS LAST`: `mtime` is NOT NULL, so the coalesced value never is.
        SortField::Date => format!("{SORT_DATE} {o}"),
        SortField::Size => format!("m.file_size {o}"),
        SortField::Name => format!("m.path {o}"),
        SortField::Rating => format!("m.rating {o} NULLS LAST"),
        SortField::MediaType => format!("m.media_type {o}"),
        SortField::LastViewed => format!("m.last_viewed {o} NULLS LAST"),
        SortField::DateAdded => format!("m.date_added {o} NULLS LAST"),
        SortField::LastRated => format!("m.last_rated {o} NULLS LAST"),
    }
}

/// What the caller asked the grid to show.
#[derive(Debug, Clone, Copy)]
pub struct SortSpec {
    pub field: SortField,
    pub order: SortOrder,
    pub sub_field: Option<SortField>,
    pub sub_order: Option<SortOrder>,
}

/// Build the statement. `where_sql` is the compiled filter fragment, or `None`
/// for the whole gallery; its bound values are supplied by the caller in the
/// same order the compiler pushed them.
pub fn items_sql(spec: &SortSpec, where_sql: Option<&str>) -> String {
    let mut order_clause = order_expr(spec.field, spec.order);
    if let Some(sub) = spec.sub_field {
        order_clause.push_str(", ");
        order_clause.push_str(&order_expr(sub, spec.sub_order.unwrap_or(SortOrder::Desc)));
    }
    let cols = cols();
    match where_sql {
        Some(w) => format!("SELECT {cols} FROM media_meta m WHERE {w} ORDER BY {order_clause}"),
        None => format!("SELECT {cols} FROM media_meta m ORDER BY {order_clause}"),
    }
}

/// Map one row of [`items_sql`].
///
/// A path that fails validation is a corrupt row rather than an input, so it is
/// surfaced as an error rather than skipped: silently dropping cells is how a
/// grid ends up confidently incomplete.
pub fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SortedItem> {
    use base64::Engine;
    let raw_path: String = row.get(0)?;
    let path = RelPath::new(&raw_path).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let thumbhash: Option<Vec<u8>> = row.get(12)?;
    Ok(SortedItem {
        path,
        date: row.get(1)?,
        file_size: row.get(2)?,
        media_type: row.get(3)?,
        rating: row.get(4)?,
        color_label: row.get(5)?,
        last_viewed: row.get(6)?,
        date_added: row.get(7)?,
        last_rated: row.get(8)?,
        duration: row.get(9)?,
        width: row.get(10)?,
        height: row.get(11)?,
        thumbhash: thumbhash.map(|h| base64::engine::general_purpose::STANDARD.encode(h)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) const ALL_FIELDS: [SortField; 8] = [
        SortField::Date,
        SortField::Size,
        SortField::Name,
        SortField::Rating,
        SortField::MediaType,
        SortField::LastViewed,
        SortField::DateAdded,
        SortField::LastRated,
    ];

    /// Every `media_meta` column the ordering names, so the test below can
    /// check each one is alias-qualified wherever it appears.
    const COLUMNS: [&str; 10] = [
        "path",
        "date_taken",
        "mtime",
        "file_size",
        "media_type",
        "rating",
        "color_label",
        "last_viewed",
        "date_added",
        "last_rated",
    ];

    #[test]
    fn every_column_in_the_order_by_is_alias_qualified() {
        // A bare column name must never appear. The failure it prevents is
        // "ambiguous column name" from a join a later change adds — which does
        // not degrade the sort, it rejects the statement.
        //
        // Checked per occurrence rather than by prefix: the date arm is a
        // `COALESCE` over two columns, so the expression no longer *starts*
        // with the alias and a prefix test would pass while leaving the second
        // column bare.
        for field in ALL_FIELDS {
            for order in [SortOrder::Asc, SortOrder::Desc] {
                let e = order_expr(field, order);
                for col in COLUMNS {
                    let mut from = 0;
                    while let Some(i) = e[from..].find(col) {
                        let at = from + i;
                        assert!(
                            at >= 2 && &e[at - 2..at] == "m.",
                            "unqualified `{col}` in order expression: {e}"
                        );
                        from = at + col.len();
                    }
                }
            }
        }
    }

    /// The grid must order by the same expression it selects and the idle
    /// warmer warms by, or the scrollbar's labels, the group headers and the
    /// warm-up order all disagree with the order on screen.
    #[test]
    fn the_date_sort_falls_back_to_the_file_time() {
        let spec = SortSpec {
            field: SortField::Date,
            order: SortOrder::Desc,
            sub_field: None,
            sub_order: None,
        };
        let sql = items_sql(&spec, None);
        assert!(sql.contains(&format!("ORDER BY {SORT_DATE} DESC")), "{sql}");
        assert!(sql.contains(SORT_DATE), "the selected date must be the sorted one");
        // `mtime` is NOT NULL, so the coalesced value never is and a null
        // ordering clause on this arm would be dead code pretending to matter.
        assert!(
            !order_expr(SortField::Date, SortOrder::Desc).contains("NULLS LAST"),
            "the coalesced date is never null"
        );
    }

    #[test]
    fn the_filter_lands_inside_the_statement() {
        let spec = SortSpec {
            field: SortField::Date,
            order: SortOrder::Desc,
            sub_field: None,
            sub_order: None,
        };
        let sql = items_sql(&spec, Some("m.rating >= ?1"));
        assert!(sql.contains("WHERE m.rating >= ?1"));
        assert!(sql.contains(&format!("ORDER BY {SORT_DATE} DESC")));
        // No join, and no path list round-tripping through the client.
        assert!(!sql.contains("JOIN"));
        assert!(!sql.contains("json_each"));
    }

    #[test]
    fn a_sub_sort_appends_a_second_term() {
        let spec = SortSpec {
            field: SortField::Rating,
            order: SortOrder::Desc,
            sub_field: Some(SortField::Name),
            sub_order: Some(SortOrder::Asc),
        };
        let sql = items_sql(&spec, None);
        assert!(sql.ends_with("ORDER BY m.rating DESC NULLS LAST, m.path ASC"));
    }
}
