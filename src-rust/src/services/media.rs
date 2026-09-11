//! Answering "what is in this gallery" and "what does this file look like".
//!
//! **One query for the grid.** `get_items` takes the sort, the filter and the
//! grouping together and returns the whole payload: the filter compiles into
//! the `WHERE` clause of the same statement the sort orders, and grouping is an
//! in-memory pass over the result.
//!
//! That replaces a two-step shape in which a filter command returned a
//! `Vec<String>` of paths, the *client* handed them straight back, and
//! `json_each` re-expanded them. At 20k matches that is roughly a megabyte of
//! path strings up and a multi-megabyte payload down, per debounced keystroke,
//! to a phone — bought in exchange for not re-running one indexed SQL scan when
//! only the sort changed.

use serde::{Deserialize, Serialize};

use crate::cache::tiers::{self, ThumbTier};
use crate::filter::parser::parse_filter;
use crate::path::RelPath;
use crate::sort::grouper::{self, GroupBy, GroupHeader};
use crate::sort::sorter::{self, SortField, SortOrder, SortSpec, SortedItem};
use crate::state::Gallery;

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Cache(#[from] crate::cache::db::CacheError),
    #[error(transparent)]
    Path(#[from] crate::path::PathError),
    #[error("bad filter: {0}")]
    Filter(String),
}

/// What the grid asks for.
///
/// `Default` matches what the `serde` defaults produce for an empty request —
/// the whole gallery, newest first, ungrouped — so a caller that only wants a
/// filter (`lightview tag --filter`) need not restate a sort it does not care
/// about, and cannot drift from the wire defaults by restating it differently.
#[derive(Debug, Clone, Deserialize)]
pub struct ItemsRequest {
    #[serde(default = "default_sort")]
    pub sort: SortField,
    #[serde(default = "default_order")]
    pub order: SortOrder,
    #[serde(default)]
    pub sub_sort: Option<SortField>,
    #[serde(default)]
    pub sub_order: Option<SortOrder>,
    /// Filter query text. Empty means the whole gallery.
    #[serde(default)]
    pub filter: String,
    #[serde(default = "default_group")]
    pub group_by: GroupBy,
}

impl Default for ItemsRequest {
    fn default() -> Self {
        Self {
            sort: default_sort(),
            order: default_order(),
            sub_sort: None,
            sub_order: None,
            filter: String::new(),
            group_by: default_group(),
        }
    }
}

fn default_sort() -> SortField {
    SortField::Date
}
fn default_order() -> SortOrder {
    SortOrder::Desc
}
fn default_group() -> GroupBy {
    GroupBy::None
}

/// Everything the grid needs for first paint.
#[derive(Debug, Serialize)]
pub struct Items {
    pub items: Vec<SortedItem>,
    pub groups: Vec<GroupHeader>,
}

/// Run the one query.
pub async fn get_items(gallery: &Gallery, request: &ItemsRequest) -> Result<Items, MediaError> {
    let spec = SortSpec {
        field: request.sort,
        order: request.order,
        sub_field: request.sub_sort,
        sub_order: request.sub_order,
    };

    let mut params: Vec<String> = Vec::new();
    let where_sql = if request.filter.trim().is_empty() {
        None
    } else {
        let expr = parse_filter(&request.filter).map_err(|e| MediaError::Filter(e.to_string()))?;
        Some(crate::filter::evaluator::to_sql(&expr, &mut params))
    };

    let sql = sorter::items_sql(&spec, where_sql.as_deref());
    let conn = gallery.db.read().await;
    let mut stmt = conn.prepare(&sql)?;
    let bound: Vec<&dyn rusqlite::ToSql> =
        params.iter().map(|p| p as &dyn rusqlite::ToSql).collect();

    let mut items = Vec::new();
    let rows = stmt.query_map(bound.as_slice(), sorter::map_row)?;
    for row in rows {
        items.push(row?);
    }

    let groups = grouper::compute_groups(&items, &request.group_by);
    Ok(Items { items, groups })
}

/// Everything the info panel shows about one file.
#[derive(Debug, Serialize)]
pub struct MediaMeta {
    pub path: RelPath,
    pub media_type: String,
    pub file_size: i64,
    pub date_taken: Option<i64>,
    pub date_added: Option<i64>,
    pub last_viewed: Option<i64>,
    pub rating: Option<u8>,
    pub color_label: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration: Option<f64>,
    pub gps: Option<(f64, f64)>,
    pub tags: Vec<(String, String)>,
    pub notes: Option<String>,
}

/// Read one file's row plus its tags and notes.
pub async fn get_media_meta(
    gallery: &Gallery,
    path: &RelPath,
) -> Result<Option<MediaMeta>, MediaError> {
    let conn = gallery.db.read().await;
    let row = conn.query_row(
        "SELECT media_type, file_size, date_taken, date_added, last_viewed, rating,
                color_label, width, height, duration, gps_lat, gps_lon
         FROM media_meta WHERE path = ?1",
        [path.as_str()],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<i64>>(2)?,
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, Option<i64>>(4)?,
                r.get::<_, Option<u8>>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, Option<u32>>(7)?,
                r.get::<_, Option<u32>>(8)?,
                r.get::<_, Option<f64>>(9)?,
                r.get::<_, Option<f64>>(10)?,
                r.get::<_, Option<f64>>(11)?,
            ))
        },
    );
    let Ok(row) = row else {
        return Ok(None);
    };
    let tags = crate::cache::index::tags_for_file(&conn, path)?;
    drop(conn);

    // Notes live only in the companion — there is no `notes:` filter term, so
    // there is no column, because a field is filterable only if it is indexed.
    let absolute = gallery.root.resolve(path)?;
    let notes = tokio::task::spawn_blocking(move || {
        crate::companion::reader::read_companion(absolute.as_path())
            .ok()
            .flatten()
            .and_then(|c| c.meta.core.and_then(|m| m.notes))
    })
    .await
    .unwrap_or(None);

    Ok(Some(MediaMeta {
        path: path.clone(),
        media_type: row.0,
        file_size: row.1,
        date_taken: row.2,
        date_added: row.3,
        last_viewed: row.4,
        rating: row.5,
        color_label: row.6,
        width: row.7,
        height: row.8,
        duration: row.9,
        gps: match (row.10, row.11) {
            (Some(lat), Some(lon)) => Some((lat, lon)),
            _ => None,
        },
        tags,
        notes,
    }))
}

/// Every copy in a duplicate group, for the merge dialog to resolve.
///
/// Deliberately the same [`MediaMeta`] the info panel reads rather than a
/// second shape: a merge candidate is a row plus its tags and notes, which is
/// exactly that. What it adds is one round trip for the whole group instead of
/// one per copy.
///
/// There is no separate "EXIF location" beside the companion's. The companion's
/// coordinates are mirrored over the indexed ones at index time, so a file has
/// one effective location and the dialog chooses between the distinct locations
/// *across copies* — which is the choice a person was actually making.
pub async fn merge_candidates(
    gallery: &Gallery,
    paths: &[RelPath],
) -> Result<Vec<MediaMeta>, MediaError> {
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        if let Some(meta) = get_media_meta(gallery, path).await? {
            out.push(meta);
        }
    }
    Ok(out)
}

/// Which tiers are cached for one file, and how big each is.
#[derive(Debug, Serialize)]
pub struct TierPresence {
    pub tier: ThumbTier,
    pub edge: u32,
    pub bytes: Option<i64>,
}

pub async fn get_tiers(
    gallery: &Gallery,
    path: &RelPath,
) -> Result<Vec<TierPresence>, MediaError> {
    let conn = gallery.db.read().await;
    let mut out = Vec::new();
    for tier in ThumbTier::ALL {
        let sql = format!("SELECT byte_len FROM {} WHERE path = ?1", tier.table());
        let bytes: Option<i64> = conn
            .prepare_cached(&sql)?
            .query_row([path.as_str()], |r| r.get(0))
            .ok();
        out.push(TierPresence {
            tier,
            edge: tier.edge(),
            bytes,
        });
    }
    Ok(out)
}

/// Drop every cached tier for a file so the next request regenerates it.
///
/// The `phash` and the ThumbHash go with the `j` row, which is the lifetime
/// they should have: they describe the thumbnail, not the file.
pub async fn regenerate(gallery: &Gallery, paths: &[RelPath]) -> Result<(), MediaError> {
    let conn = gallery.db.writer().await;
    let tx = conn.unchecked_transaction()?;
    for path in paths {
        for tier in ThumbTier::ALL {
            tx.execute(
                &format!("DELETE FROM {} WHERE path = ?1", tier.table()),
                [path.as_str()],
            )?;
        }
        tx.execute(
            "UPDATE media_meta SET thumbhash = NULL WHERE path = ?1",
            [path.as_str()],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Warm a tier for a list of paths, without blocking on the result.
///
/// Speculative work lands on the same bounded pool as visible cells, so the
/// caller gates it rather than this function isolating it — there is no second
/// pool to escape to.
pub async fn precache(gallery: &Gallery, tier: ThumbTier, paths: &[RelPath]) {
    for path in paths {
        // Not user-driven: a precache must not convince the idle worker that
        // somebody is looking.
        gallery.thumbs.get_or_generate(tier, path, false).await;
    }
}

/// Suggest tags for a prefix.
pub async fn autocomplete(
    gallery: &Gallery,
    query: &str,
    namespace: Option<&str>,
    limit: usize,
) -> Vec<crate::autocomplete::engine::TagSuggestion> {
    gallery.autocomplete.query(query, namespace, limit).await
}

/// Total bytes each tier holds, for the thumbnails pane.
pub async fn tier_totals(gallery: &Gallery) -> Result<Vec<(ThumbTier, i64)>, MediaError> {
    let conn = gallery.db.read().await;
    let mut out = Vec::new();
    for tier in ThumbTier::ALL {
        out.push((tier, tiers::total_bytes(&conn, tier)?));
    }
    Ok(out)
}
