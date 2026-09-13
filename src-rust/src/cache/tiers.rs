//! The four cached thumbnail tiers, and the byte budget on the two large ones.
//!
//! **A tier is a cached edge.** One family, one encoder, one render path: every
//! cached thumbnail is `generate_for_path_fit(path, edge)` at one of four
//! edges, all aspect-preserving, all WebP. Four rungs cost a table and a row in
//! `path_keyed_tables()`, not a concept — what would cost a concept is a tier
//! derived from another tier, a second encoder, or a GPU fast path.
//!
//! | Tier | Edge | Bounded | Also carries |
//! |------|------|---------|--------------|
//! | `js` | 128  | no      | panel thumbnails |
//! | `j`  | 512  | no      | `phash`, the grid's base rung |
//! | `jm` | 1280 | LRU     | the viewer's progressive underlay |
//! | `jh` | 2560 | LRU     | high zoom |
//!
//! `js` exists because three panels — the tag manager, the duplicates panel and
//! the merge dialog — render up to 120 thumbnails in an 88px grid. At 512px
//! that is ~84 MB of decoded bitmaps instead of ~7.9 MB, roughly 34× the pixels
//! the screen can show, on the exact axis that gets tabs killed on iOS. Its
//! rows are ~4 KB, so the tier is unbounded and the idle worker warms it
//! alongside `j` for a few megabytes per gallery.
//!
//! Those panels also address *arbitrary*, typically old files, while the idle
//! backfill warms newest-first — so each panel open could otherwise fire up to
//! 120 simultaneous generate-on-miss requests at precisely the part of the
//! library the backfill reaches last.

use rusqlite::Connection;

use crate::cache::db::CacheError;
use crate::path::RelPath;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThumbTier {
    Js,
    J,
    Jm,
    Jh,
}

impl ThumbTier {
    /// Every tier, in ascending edge order. The single source of truth — the
    /// path-keyed sweep and the schema both derive from it, so adding a rung
    /// cannot leave orphaned rows behind.
    pub const ALL: [ThumbTier; 4] = [ThumbTier::Js, ThumbTier::J, ThumbTier::Jm, ThumbTier::Jh];

    /// The URL segment: `/thumb/{segment}/{path}`.
    pub fn segment(self) -> &'static str {
        match self {
            ThumbTier::Js => "js",
            ThumbTier::J => "j",
            ThumbTier::Jm => "jm",
            ThumbTier::Jh => "jh",
        }
    }

    /// Longest edge in pixels, aspect preserved.
    pub fn edge(self) -> u32 {
        match self {
            ThumbTier::Js => 128,
            ThumbTier::J => 512,
            ThumbTier::Jm => 1280,
            ThumbTier::Jh => 2560,
        }
    }

    pub fn table(self) -> &'static str {
        match self {
            ThumbTier::Js => "thumbs_js",
            ThumbTier::J => "thumbs_j",
            ThumbTier::Jm => "thumbs_jm",
            ThumbTier::Jh => "thumbs_jh",
        }
    }

    /// Whether the tier is LRU byte-budgeted. The two small ones are not: `js`
    /// rows are ~4 KB and `j` is the rung everything else is derived from.
    pub fn bounded(self) -> bool {
        matches!(self, ThumbTier::Jm | ThumbTier::Jh)
    }

    pub fn from_segment(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.segment() == s)
    }

    /// The smallest tier at least `edge` across, or `None` when nothing is big
    /// enough and the caller must decode from source.
    ///
    /// **Round up, never down.** A model handed a smaller image than it trained
    /// on has lost information it cannot recover; a model handed a larger one
    /// downsizes internally. The same rule serves the grid, where rounding down
    /// would show a visibly soft cell.
    pub fn smallest_at_least(edge: u32) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.edge() >= edge)
    }
}

/// A cached tier row.
pub struct TierBytes {
    pub bytes: Vec<u8>,
}

/// Read a tier row, if it is cached.
///
/// Does not touch `accessed_at`: the read path holds a read-only connection, so
/// access marks buffer in memory and are drained immediately before an eviction
/// pass. Draining *after* eviction would evict exactly what the user is looking
/// at.
pub fn get(conn: &Connection, tier: ThumbTier, path: &RelPath) -> Result<Option<TierBytes>, CacheError> {
    let sql = format!("SELECT bytes FROM {} WHERE path = ?1", tier.table());
    let mut stmt = conn.prepare_cached(&sql)?;
    let mut rows = stmt.query([path.as_str()])?;
    match rows.next()? {
        Some(row) => Ok(Some(TierBytes { bytes: row.get(0)? })),
        None => Ok(None),
    }
}

/// Write a tier row.
///
/// `accessed_at` is seeded to *now* rather than left at a column default.
/// Leaving it at the default marks every freshly written row maximally cold, so
/// the next eviction pass deletes exactly what was just generated — a cache
/// that spends its whole life re-generating the same thumbnails.
pub fn put(
    conn: &Connection,
    tier: ThumbTier,
    path: &RelPath,
    bytes: &[u8],
) -> Result<(), CacheError> {
    let sql = format!(
        "INSERT INTO {} (path, bytes, byte_len, accessed_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(path) DO UPDATE SET
           bytes = excluded.bytes,
           byte_len = excluded.byte_len,
           accessed_at = excluded.accessed_at",
        tier.table()
    );
    conn.prepare_cached(&sql)?.execute(rusqlite::params![
        path.as_str(),
        bytes,
        bytes.len() as i64,
        now(),
    ])?;
    Ok(())
}

/// Stamp `accessed_at` for rows the read path served, drained from the
/// in-memory buffer immediately before an eviction pass.
pub fn touch(conn: &Connection, tier: ThumbTier, paths: &[RelPath]) -> Result<(), CacheError> {
    if paths.is_empty() {
        return Ok(());
    }
    let sql = format!("UPDATE {} SET accessed_at = ?1 WHERE path = ?2", tier.table());
    let tx = conn.unchecked_transaction()?;
    {
        let mut stmt = tx.prepare_cached(&sql)?;
        let stamp = now();
        for p in paths {
            stmt.execute(rusqlite::params![stamp, p.as_str()])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Total bytes held by one tier.
///
/// Reads `byte_len` rather than `length(bytes)` so the sum never touches a
/// blob's overflow pages; the `(accessed_at, byte_len)` index makes it and the
/// eviction scan below covering queries.
pub fn total_bytes(conn: &Connection, tier: ThumbTier) -> Result<i64, CacheError> {
    let sql = format!("SELECT COALESCE(SUM(byte_len), 0) FROM {}", tier.table());
    Ok(conn.query_row(&sql, [], |r| r.get(0))?)
}

/// The hysteresis multiplier: evict only once a tier is this far past budget,
/// and then trim all the way back to it.
///
/// Evicting at exactly the budget would run a delete pass on essentially every
/// write once the cache is warm. 1.25× turns that into an occasional pass.
pub const EVICT_AT: f64 = 1.25;

/// Evict warmest-first down to `budget_bytes`, if the tier is past `1.25 ×`
/// budget.
///
/// Returns the number of rows removed. Both write paths call this, not only the
/// batch one — a budget enforced on one of two writers is not a budget.
pub fn enforce_budget(
    conn: &Connection,
    tier: ThumbTier,
    budget_bytes: i64,
) -> Result<usize, CacheError> {
    if !tier.bounded() {
        return Ok(0);
    }
    let total = total_bytes(conn, tier)?;
    if (total as f64) <= budget_bytes as f64 * EVICT_AT {
        return Ok(0);
    }

    // Accumulate warmest-first and delete everything past the budget. The
    // window function does the running total in SQLite rather than pulling
    // every path into Rust.
    let sql = format!(
        "DELETE FROM {table} WHERE path IN (
             SELECT path FROM (
                 SELECT path,
                        SUM(byte_len) OVER (
                            ORDER BY accessed_at DESC, path DESC
                            ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
                        ) AS running
                 FROM {table}
             ) WHERE running > ?1
         )",
        table = tier.table()
    );
    Ok(conn.execute(&sql, [budget_bytes])?)
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::db::CacheDb;

    #[test]
    fn plugin_input_rounds_up_to_a_tier_edge() {
        assert_eq!(ThumbTier::smallest_at_least(1), Some(ThumbTier::Js));
        assert_eq!(ThumbTier::smallest_at_least(128), Some(ThumbTier::Js));
        assert_eq!(ThumbTier::smallest_at_least(129), Some(ThumbTier::J));
        assert_eq!(ThumbTier::smallest_at_least(512), Some(ThumbTier::J));
        // The bundled taggers declare 512 for exactly this reason: 1024 rounds
        // up to `jm`, which is a full generation per image on the server.
        assert_eq!(ThumbTier::smallest_at_least(1024), Some(ThumbTier::Jm));
        assert_eq!(ThumbTier::smallest_at_least(2560), Some(ThumbTier::Jh));
        assert_eq!(ThumbTier::smallest_at_least(4096), None);
    }

    #[test]
    fn segments_round_trip() {
        for t in ThumbTier::ALL {
            assert_eq!(ThumbTier::from_segment(t.segment()), Some(t));
        }
        assert_eq!(ThumbTier::from_segment("m"), None);
    }

    #[test]
    fn a_freshly_written_row_is_not_the_coldest_row() {
        // Seeding `accessed_at` to now is what stops an eviction pass deleting
        // exactly what was just generated.
        let dir = tempfile::tempdir().unwrap();
        let db = CacheDb::open_at(dir.path()).unwrap();
        let conn = db.writer_blocking();
        let p = RelPath::new("a.jpg").unwrap();
        put(&conn, ThumbTier::Jm, &p, &[0u8; 16]).unwrap();
        let accessed: i64 = conn
            .query_row("SELECT accessed_at FROM thumbs_jm WHERE path = 'a.jpg'", [], |r| r.get(0))
            .unwrap();
        assert!(accessed > 0);
        assert!((accessed - now()).abs() <= 2);
    }

    #[test]
    fn eviction_waits_for_hysteresis_then_trims_all_the_way_back() {
        let dir = tempfile::tempdir().unwrap();
        let db = CacheDb::open_at(dir.path()).unwrap();
        let conn = db.writer_blocking();

        // Ten rows of 100 bytes; budget 500. Warmest are the later paths,
        // because `accessed_at` ties break on path DESC.
        for i in 0..10 {
            let p = RelPath::new(&format!("{i:02}.jpg")).unwrap();
            put(&conn, ThumbTier::Jm, &p, &[0u8; 100]).unwrap();
        }
        assert_eq!(total_bytes(&conn, ThumbTier::Jm).unwrap(), 1000);

        // Under 1.25× budget: nothing happens, which is the whole point of the
        // hysteresis — otherwise a warm cache runs a delete pass per write.
        assert_eq!(enforce_budget(&conn, ThumbTier::Jm, 900).unwrap(), 0);

        // Past it: trim back to the budget itself, not to 1.25× of it.
        let removed = enforce_budget(&conn, ThumbTier::Jm, 500).unwrap();
        assert_eq!(removed, 5);
        assert_eq!(total_bytes(&conn, ThumbTier::Jm).unwrap(), 500);
    }

    #[test]
    fn unbounded_tiers_are_never_evicted() {
        let dir = tempfile::tempdir().unwrap();
        let db = CacheDb::open_at(dir.path()).unwrap();
        let conn = db.writer_blocking();
        for i in 0..10 {
            let p = RelPath::new(&format!("{i}.jpg")).unwrap();
            put(&conn, ThumbTier::J, &p, &[0u8; 100]).unwrap();
        }
        assert_eq!(enforce_budget(&conn, ThumbTier::J, 1).unwrap(), 0);
        assert_eq!(total_bytes(&conn, ThumbTier::J).unwrap(), 1000);
    }

    #[test]
    fn touch_moves_a_row_to_the_warm_end() {
        let dir = tempfile::tempdir().unwrap();
        let db = CacheDb::open_at(dir.path()).unwrap();
        let conn = db.writer_blocking();
        for i in 0..4 {
            let p = RelPath::new(&format!("{i}.jpg")).unwrap();
            put(&conn, ThumbTier::Jm, &p, &[0u8; 100]).unwrap();
            conn.execute(
                "UPDATE thumbs_jm SET accessed_at = ?1 WHERE path = ?2",
                rusqlite::params![1000 + i as i64, format!("{i}.jpg")],
            )
            .unwrap();
        }
        // "0.jpg" is coldest; touching it should save it from a trim to 200.
        touch(&conn, ThumbTier::Jm, &[RelPath::new("0.jpg").unwrap()]).unwrap();
        enforce_budget(&conn, ThumbTier::Jm, 200).unwrap();

        let survived: Vec<String> = conn
            .prepare("SELECT path FROM thumbs_jm ORDER BY path")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(survived.contains(&"0.jpg".to_string()), "touched row evicted: {survived:?}");
    }
}
