use crate::cache::db::{CacheDb, CacheError};
use std::collections::{HashMap, HashSet};

/// Maximum number of bound parameters per IN-list query. SQLite's default
/// variable limit is far higher, but 900 keeps us safely under the old
/// 999-variable floor.
const IN_CHUNK: usize = 900;

/// Build "?,?,?" with `n` placeholders.
fn placeholders(n: usize) -> String {
    let mut s = String::with_capacity(n * 2);
    for i in 0..n {
        if i > 0 {
            s.push(',');
        }
        s.push('?');
    }
    s
}

/// A cached thumbnail record.
pub struct CachedThumbnail {
    pub path: String,
    pub media_type: String,
    pub mtime: u64,
    pub width: u32,
    pub height: u32,
    pub thumbnail: Vec<u8>,
    pub format: String,
    pub resize_filter: String,
}

/// LOD tier for a thumbnail. Determines which SQL table a thumbnail is
/// read from / written to. Frontend picks the tier from the current
/// gallery cell size; see docs/thumbnailStreamingResearch.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThumbTier {
    /// 128x128 — dense grid (cellSize <= 160). `thumbnails_micro`.
    Micro,
    /// ~512x512 — standard grid (160 < cellSize <= 400). `thumbnails`.
    Standard,
    /// 1024x1024 — large grid (cellSize > 400), lazy-generated. `thumbnails_large`.
    Large,
    /// ~1600 px longest edge — viewer first-paint, lazy-generated. `thumbnails_preview`.
    Preview,
    /// ~512 px longest edge, **aspect-preserving** (no square crop) — justified
    /// gallery view, lazy-generated. `thumbnails_justified`.
    Justified,
    /// ~1280 px longest edge, **aspect-preserving** (no square crop) — justified
    /// gallery view at *mid* zoom, where a cell is far smaller than the 2560 px
    /// `JustifiedHigh` image would be, so serving `jh` there is a big over-decode.
    /// Lazy-generated for visible cells only. `thumbnails_justified_mid`.
    JustifiedMid,
    /// ~2560 px longest edge, **aspect-preserving** (no square crop) — justified
    /// gallery view when zoomed in *high*, lazy-generated for visible cells only.
    /// `thumbnails_justified_high`.
    JustifiedHigh,
}

impl ThumbTier {
    /// Every tier, in storage order. The single source of truth for "which
    /// thumbnail tables exist" — path-keyed maintenance (delete/rename/rebase/
    /// prune) iterates this instead of repeating the table list, which used to
    /// drift and leave orphaned rows in whichever tier a site had forgotten.
    pub const ALL: [ThumbTier; 7] = [
        Self::Standard,
        Self::Micro,
        Self::Large,
        Self::Preview,
        Self::Justified,
        Self::JustifiedMid,
        Self::JustifiedHigh,
    ];

    /// Human-readable tier name, for the per-file tier breakdown in the UI.
    pub fn label(self) -> &'static str {
        match self {
            Self::Micro => "Micro",
            Self::Standard => "Standard",
            Self::Large => "Large",
            Self::Preview => "Preview",
            Self::Justified => "Justified",
            Self::JustifiedMid => "Justified Mid",
            Self::JustifiedHigh => "Justified High",
        }
    }

    /// Whether this tier stores an **aspect-preserving** (fit, non-cropping)
    /// thumbnail. The justified tiers do; the square-cropped grid tiers don't.
    pub fn is_fit(self) -> bool {
        matches!(
            self,
            Self::Justified | Self::JustifiedMid | Self::JustifiedHigh
        )
    }

    /// Encoded format this tier is stored in. The big tiers use lossy WebP —
    /// ~30 % smaller at equivalent quality, which matters a lot at 1024–2560 px.
    pub fn format(self) -> crate::pipeline::thumbnailer::ThumbFormat {
        use crate::pipeline::thumbnailer::ThumbFormat;
        match self {
            Self::Large | Self::Preview => ThumbFormat::Webp,
            _ if self.is_fit() => ThumbFormat::Webp,
            _ => ThumbFormat::Jpeg,
        }
    }

    /// Map a URL-path segment ("s", "m", "l", "p") to a tier.
    pub fn from_segment(s: &str) -> Option<Self> {
        match s {
            "s" | "micro" => Some(Self::Micro),
            "m" | "standard" | "" => Some(Self::Standard),
            "l" | "large" => Some(Self::Large),
            "p" | "preview" => Some(Self::Preview),
            "j" | "justified" => Some(Self::Justified),
            "jm" | "justified_mid" => Some(Self::JustifiedMid),
            "jh" | "justified_high" => Some(Self::JustifiedHigh),
            _ => None,
        }
    }

    /// Short URL segment for this tier.
    pub fn as_segment(self) -> &'static str {
        match self {
            Self::Micro => "s",
            Self::Standard => "m",
            Self::Large => "l",
            Self::Preview => "p",
            Self::Justified => "j",
            Self::JustifiedMid => "jm",
            Self::JustifiedHigh => "jh",
        }
    }

    /// SQL table storing this tier.
    pub fn table(self) -> &'static str {
        match self {
            Self::Micro => "thumbnails_micro",
            Self::Standard => "thumbnails",
            Self::Large => "thumbnails_large",
            Self::Preview => "thumbnails_preview",
            Self::Justified => "thumbnails_justified",
            Self::JustifiedMid => "thumbnails_justified_mid",
            Self::JustifiedHigh => "thumbnails_justified_high",
        }
    }

    /// Static `SELECT thumbnail, format ... WHERE path = ?1` for this tier.
    /// Returned as `&'static str` so the hot serve path (called on every
    /// thumbnail request) avoids a per-call `format!` allocation and
    /// `prepare_cached` keys on a stable string.
    pub fn select_blob_sql(self) -> &'static str {
        match self {
            Self::Micro => "SELECT thumbnail, format FROM thumbnails_micro WHERE path = ?1",
            Self::Standard => "SELECT thumbnail, format FROM thumbnails WHERE path = ?1",
            Self::Large => "SELECT thumbnail, format FROM thumbnails_large WHERE path = ?1",
            Self::Preview => "SELECT thumbnail, format FROM thumbnails_preview WHERE path = ?1",
            Self::Justified => "SELECT thumbnail, format FROM thumbnails_justified WHERE path = ?1",
            Self::JustifiedMid => {
                "SELECT thumbnail, format FROM thumbnails_justified_mid WHERE path = ?1"
            }
            Self::JustifiedHigh => {
                "SELECT thumbnail, format FROM thumbnails_justified_high WHERE path = ?1"
            }
        }
    }

    /// Static `SELECT 1 ... WHERE path = ?1` existence check for this tier.
    pub fn exists_sql(self) -> &'static str {
        match self {
            Self::Micro => "SELECT 1 FROM thumbnails_micro WHERE path = ?1",
            Self::Standard => "SELECT 1 FROM thumbnails WHERE path = ?1",
            Self::Large => "SELECT 1 FROM thumbnails_large WHERE path = ?1",
            Self::Preview => "SELECT 1 FROM thumbnails_preview WHERE path = ?1",
            Self::Justified => "SELECT 1 FROM thumbnails_justified WHERE path = ?1",
            Self::JustifiedMid => "SELECT 1 FROM thumbnails_justified_mid WHERE path = ?1",
            Self::JustifiedHigh => "SELECT 1 FROM thumbnails_justified_high WHERE path = ?1",
        }
    }

    /// Target dimensions (square) for this tier.
    pub fn target_size(self) -> u32 {
        match self {
            Self::Micro => 128,
            Self::Standard => 512,
            Self::Large => 1024,
            Self::Preview => 1600,
            Self::Justified => 512,
            // Mid justified tier: ~half the linear size of `JustifiedHigh`, so a
            // quarter the decode/transfer cost. Mid zoom shows cells far smaller
            // than 2560 px, so this is the right resolution there and avoids the
            // jh over-decode.
            Self::JustifiedMid => 1280,
            // High justified tier is used both as a fallback for large/non-native
            // images (the small, common ones are served as originals) and to give
            // wide images enough pixels on their long edge. 2560 keeps wide cells
            // sharp at high zoom without approaching full-res file sizes.
            Self::JustifiedHigh => 2560,
        }
    }
}

/// Insert or replace a standard-tier thumbnail row. Free function over a raw
/// connection so both `CacheDb` methods and open transactions
/// (`Transaction` derefs to `Connection`) share one statement.
pub fn write_standard_row(
    conn: &rusqlite::Connection,
    path: &str,
    media_type: &str,
    mtime: u64,
    width: u32,
    height: u32,
    thumbnail: &[u8],
    format: &str,
    resize_filter: &str,
) -> Result<(), CacheError> {
    conn.execute(
        "INSERT OR REPLACE INTO thumbnails (path, media_type, mtime, width, height, thumbnail, format, resize_filter)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![path, media_type, mtime, width, height, thumbnail, format, resize_filter],
    )?;
    Ok(())
}

/// Insert or replace a row in a non-standard tier table (micro/large/preview).
/// The standard tier has an extra `resize_filter` column — use
/// [`write_standard_row`] for it.
pub fn write_tier_row(
    conn: &rusqlite::Connection,
    tier: ThumbTier,
    path: &str,
    media_type: &str,
    mtime: u64,
    width: u32,
    height: u32,
    thumbnail: &[u8],
    format: &str,
) -> Result<(), CacheError> {
    debug_assert!(!matches!(tier, ThumbTier::Standard));
    // LRU-capped tiers carry `accessed_at`; seed it to now on write. Leaving it
    // at the column default (0) would mark every freshly generated row as
    // maximally cold, so the next eviction would delete exactly the rows just
    // generated for the current viewport.
    let sql = if is_lru_capped(tier) {
        format!(
            "INSERT OR REPLACE INTO {} (path, media_type, mtime, width, height, thumbnail, format, accessed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, strftime('%s','now'))",
            tier.table()
        )
    } else {
        format!(
            "INSERT OR REPLACE INTO {} (path, media_type, mtime, width, height, thumbnail, format)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            tier.table()
        )
    };
    conn.execute(
        &sql,
        rusqlite::params![path, media_type, mtime, width, height, thumbnail, format],
    )?;
    Ok(())
}

/// Whether a tier is bounded by the byte-budgeted LRU (and therefore carries an
/// `accessed_at` column). Only the two zoomed-in justified tiers are: they're
/// generated for whatever you view zoomed in, so they'd otherwise grow without
/// limit, and their rows are big enough that the bound has to be real.
pub fn is_lru_capped(tier: ThumbTier) -> bool {
    matches!(tier, ThumbTier::JustifiedMid | ThumbTier::JustifiedHigh)
}

/// Mark `paths` as accessed now, so the LRU eviction treats them as warm.
///
/// Serving a thumbnail can't do this inline: the read path runs on the
/// read-only connection pool. Instead the serve path records hits in memory and
/// this flushes them from a writer, which is also the only moment the value
/// matters (immediately before an eviction pass).
pub fn touch_accessed(
    conn: &rusqlite::Connection,
    tier: ThumbTier,
    paths: &[String],
) -> Result<(), CacheError> {
    if !is_lru_capped(tier) || paths.is_empty() {
        return Ok(());
    }
    let sql = format!(
        "UPDATE {} SET accessed_at = strftime('%s','now') WHERE path = ?1",
        tier.table()
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    for path in paths {
        stmt.execute(rusqlite::params![path])?;
    }
    Ok(())
}

/// Fraction over budget a tier must climb before an eviction pass runs. The
/// pass then trims all the way back to the budget, so evictions are occasional
/// and proportional instead of firing on every write.
///
/// Without this the two write paths fought: on-demand serves grew the table
/// while never evicting, then a single batch write snapped it back to the cap
/// in one `DELETE` — a multi-second stall holding the cache mutex, which
/// dropped a large slice of the working set at once.
const EVICT_HIGH_WATER: f64 = 1.25;

/// Current total size of a tier's stored blobs, in bytes.
pub fn tier_bytes(conn: &rusqlite::Connection, tier: ThumbTier) -> Result<u64, CacheError> {
    let sql = format!("SELECT COALESCE(SUM(LENGTH(thumbnail)), 0) FROM {}", tier.table());
    let bytes: i64 = conn.prepare_cached(&sql)?.query_row([], |row| row.get(0))?;
    Ok(bytes.max(0) as u64)
}

/// Evict a tier's coldest rows until its stored blobs fit `budget_bytes`,
/// keeping the most-recently-accessed ones. Returns the evicted paths.
///
/// Byte-budgeted rather than row-capped because row size varies by more than an
/// order of magnitude across these tiers (a 2560 px WebP of a flat sky versus a
/// dense forest), so a fixed row count maps to a wildly variable footprint. The
/// running-total window function keeps exactly the warm prefix that fits.
///
/// No-ops until the tier exceeds [`EVICT_HIGH_WATER`] × budget — see that
/// constant for why the hysteresis matters.
pub fn evict_tier_lru(
    conn: &rusqlite::Connection,
    tier: ThumbTier,
    budget_bytes: u64,
) -> Result<Vec<String>, CacheError> {
    debug_assert!(is_lru_capped(tier));
    let used = tier_bytes(conn, tier)?;
    if used as f64 <= budget_bytes as f64 * EVICT_HIGH_WATER {
        return Ok(Vec::new());
    }

    // Walk rows warmest-first, accumulating their sizes; everything past the
    // point where the running total exceeds the budget is cold enough to drop.
    // `rowid DESC` breaks ties so rows touched in the same second (the
    // timestamp's resolution) evict newest-last rather than arbitrarily.
    let sql = format!(
        "DELETE FROM {table} WHERE path IN (
            SELECT path FROM (
                SELECT path,
                       SUM(LENGTH(thumbnail)) OVER (
                           ORDER BY accessed_at DESC, rowid DESC
                           ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
                       ) AS running
                FROM {table}
            ) WHERE running > ?1
        ) RETURNING path",
        table = tier.table()
    );
    let mut stmt = conn.prepare(&sql)?;
    let evicted: Vec<String> = stmt
        .query_map(rusqlite::params![budget_bytes], |row| row.get::<_, String>(0))?
        .filter_map(Result::ok)
        .collect();

    if !evicted.is_empty() {
        log::debug!(
            "{} tier eviction: {} rows dropped ({} MiB used vs {} MiB budget)",
            tier.as_segment(),
            evicted.len(),
            used / (1024 * 1024),
            budget_bytes / (1024 * 1024),
        );
    }
    Ok(evicted)
}

impl CacheDb {
    /// Get a cached thumbnail blob.
    pub fn get_thumbnail(&self, path: &str) -> Result<Option<CachedThumbnail>, CacheError> {
        let mut stmt = self.conn().prepare_cached(
            "SELECT path, media_type, mtime, width, height, thumbnail, format, resize_filter FROM thumbnails WHERE path = ?1",
        )?;
        let mut rows = stmt.query(rusqlite::params![path])?;
        match rows.next()? {
            Some(row) => Ok(Some(CachedThumbnail {
                path: row.get(0)?,
                media_type: row.get(1)?,
                mtime: row.get(2)?,
                width: row.get(3)?,
                height: row.get(4)?,
                thumbnail: row.get(5)?,
                format: row.get(6)?,
                resize_filter: row.get(7)?,
            })),
            None => Ok(None),
        }
    }

    /// Insert or update a thumbnail in the cache.
    pub fn upsert_thumbnail(
        &self,
        path: &str,
        media_type: &str,
        mtime: u64,
        width: u32,
        height: u32,
        thumbnail: &[u8],
        format: &str,
        resize_filter: &str,
    ) -> Result<(), CacheError> {
        write_standard_row(
            self.conn(),
            path,
            media_type,
            mtime,
            width,
            height,
            thumbnail,
            format,
            resize_filter,
        )
    }

    /// Bulk metadata lookup for the standard tier. Returns
    /// `path → (width, height, format, has_micro_row)`; paths without a
    /// cached thumbnail are absent from the map. One IN-list query per 900
    /// paths instead of two point queries per path.
    pub fn get_thumbnail_info_batch(
        &self,
        paths: &[String],
    ) -> Result<HashMap<String, (u32, u32, String, bool)>, CacheError> {
        let mut out = HashMap::with_capacity(paths.len());
        for chunk in paths.chunks(IN_CHUNK) {
            let sql = format!(
                "SELECT t.path, t.width, t.height, t.format,
                        EXISTS(SELECT 1 FROM thumbnails_micro m WHERE m.path = t.path)
                 FROM thumbnails t WHERE t.path IN ({})",
                placeholders(chunk.len())
            );
            let mut stmt = self.conn().prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(chunk), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    (
                        row.get::<_, u32>(1)?,
                        row.get::<_, u32>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, bool>(4)?,
                    ),
                ))
            })?;
            for row in rows {
                let (path, info) = row?;
                out.insert(path, info);
            }
        }
        Ok(out)
    }

    /// Get lightweight metadata about a cached thumbnail (no blob read).
    pub fn get_thumbnail_info(
        &self,
        path: &str,
    ) -> Result<Option<(u32, u32, u64, String, String)>, CacheError> {
        let mut stmt = self.conn().prepare_cached(
            "SELECT width, height, LENGTH(thumbnail), format, resize_filter FROM thumbnails WHERE path = ?1",
        )?;
        let mut rows = stmt.query(rusqlite::params![path])?;
        match rows.next()? {
            Some(row) => Ok(Some((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))),
            None => Ok(None),
        }
    }

    /// Delete all thumbnails (for full rebuild).
    pub fn clear_thumbnails(&self) -> Result<usize, CacheError> {
        let mut count = 0usize;
        for tier in ThumbTier::ALL {
            count += self
                .conn()
                .execute(&format!("DELETE FROM {}", tier.table()), [])
                .unwrap_or(0);
        }
        Ok(count)
    }

    // -------------------------------------------------------------------
    // ThumbHash (P1) — ~25-byte placeholder blob on the `thumbnails` row.
    // -------------------------------------------------------------------

    /// Fetch a single thumbhash blob by path. None if not cached yet.
    pub fn get_thumbhash(&self, path: &str) -> Result<Option<Vec<u8>>, CacheError> {
        let mut stmt = self
            .conn()
            .prepare_cached("SELECT thumbhash FROM thumbnails WHERE path = ?1")?;
        let mut rows = stmt.query(rusqlite::params![path])?;
        match rows.next()? {
            Some(row) => {
                let blob: Option<Vec<u8>> = row.get(0)?;
                Ok(blob)
            }
            None => Ok(None),
        }
    }

    /// Bulk-fetch thumbhashes for a list of paths. Returned in the same
    /// order as `paths`; missing/null entries become `None`.
    pub fn get_thumbhashes(&self, paths: &[String]) -> Result<Vec<Option<Vec<u8>>>, CacheError> {
        let mut map: HashMap<String, Vec<u8>> = HashMap::new();
        for chunk in paths.chunks(IN_CHUNK) {
            let sql = format!(
                "SELECT path, thumbhash FROM thumbnails WHERE path IN ({})",
                placeholders(chunk.len())
            );
            let mut stmt = self.conn().prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(chunk), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<Vec<u8>>>(1)?))
            })?;
            for row in rows {
                let (path, hash) = row?;
                if let Some(hash) = hash {
                    map.insert(path, hash);
                }
            }
        }
        Ok(paths.iter().map(|p| map.remove(p)).collect())
    }

    // -------------------------------------------------------------------
    // Tiered thumbnails (P2/P3/P5) — micro / large / preview tables.
    // The Standard tier still lives in `thumbnails`; use the original
    // methods above for that case.
    // -------------------------------------------------------------------

    /// Get metadata for all cached thumbnail tiers for a given path.
    /// Returns (tier_name, width, height, size_bytes, format, resize_filter_or_none) per tier.
    pub fn get_all_tier_info(
        &self,
        path: &str,
    ) -> Result<Vec<(String, u32, u32, u64, String, Option<String>)>, CacheError> {
        let mut results = Vec::new();

        // Standard tier — has resize_filter column
        {
            let mut stmt = self.conn().prepare_cached(
                "SELECT width, height, LENGTH(thumbnail), format, resize_filter FROM thumbnails WHERE path = ?1",
            )?;
            let mut rows = stmt.query(rusqlite::params![path])?;
            if let Some(row) = rows.next()? {
                results.push((
                    "Standard".to_string(),
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    Some(row.get::<_, String>(4)?),
                ));
            }
        }

        // Other tiers — no resize_filter column
        for tier in ThumbTier::ALL.into_iter().filter(|t| *t != ThumbTier::Standard) {
            let sql = format!(
                "SELECT width, height, LENGTH(thumbnail), format FROM {} WHERE path = ?1",
                tier.table()
            );
            let mut stmt = self.conn().prepare_cached(&sql)?;
            let mut rows = stmt.query(rusqlite::params![path])?;
            if let Some(row) = rows.next()? {
                results.push((
                    tier.label().to_string(),
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    None,
                ));
            }
        }

        Ok(results)
    }

    /// Check if a non-standard-tier thumbnail exists for this path.
    pub fn tier_is_cached(&self, tier: ThumbTier, path: &str) -> Result<bool, CacheError> {
        let mut stmt = self.conn().prepare_cached(tier.exists_sql())?;
        let mut rows = stmt.query(rusqlite::params![path])?;
        Ok(rows.next()?.is_some())
    }

    /// Which of `paths` already have a row in `tier`'s table. One IN-list
    /// query per 900 paths instead of a point query per path.
    pub fn tier_cached_set(
        &self,
        tier: ThumbTier,
        paths: &[String],
    ) -> Result<HashSet<String>, CacheError> {
        let mut out = HashSet::with_capacity(paths.len());
        for chunk in paths.chunks(IN_CHUNK) {
            let sql = format!(
                "SELECT path FROM {} WHERE path IN ({})",
                tier.table(),
                placeholders(chunk.len())
            );
            let mut stmt = self.conn().prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(chunk), |row| {
                row.get::<_, String>(0)
            })?;
            for row in rows {
                out.insert(row?);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod eviction_tests {
    use super::*;
    use crate::cache::db::CacheDb;

    const JH: ThumbTier = ThumbTier::JustifiedHigh;

    fn open_db() -> (tempfile::TempDir, CacheDb) {
        let tmp = tempfile::tempdir().unwrap();
        let db = CacheDb::open(tmp.path()).unwrap();
        (tmp, db)
    }

    /// Insert a row whose blob is exactly `bytes` long.
    fn insert(conn: &rusqlite::Connection, path: &str, bytes: usize) {
        write_tier_row(conn, JH, path, "image", 0, 100, 100, &vec![0u8; bytes], "webp").unwrap();
    }

    fn set_accessed(conn: &rusqlite::Connection, path: &str, at: i64) {
        conn.execute(
            "UPDATE thumbnails_justified_high SET accessed_at = ?1 WHERE path = ?2",
            rusqlite::params![at, path],
        )
        .unwrap();
    }

    fn paths(conn: &rusqlite::Connection) -> Vec<String> {
        conn.prepare("SELECT path FROM thumbnails_justified_high ORDER BY path")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    /// The hysteresis is the whole point of the rewrite: without it the two
    /// write paths alternated between growing the table and snapping it back in
    /// one huge DELETE.
    #[test]
    fn does_not_evict_until_past_the_high_water_mark() {
        let (_tmp, db) = open_db();
        for i in 0..10 {
            insert(db.conn(), &format!("/a/{i}.jpg"), 100);
        }
        // 1000 bytes used against a 900-byte budget is over budget but under
        // the 1.25× high-water mark.
        let evicted = evict_tier_lru(db.conn(), JH, 900).unwrap();
        assert!(evicted.is_empty(), "should tolerate a small overshoot");
        assert_eq!(paths(db.conn()).len(), 10);
    }

    #[test]
    fn evicts_down_to_budget_once_triggered() {
        let (_tmp, db) = open_db();
        for i in 0..10 {
            insert(db.conn(), &format!("/a/{i}.jpg"), 100);
        }
        let evicted = evict_tier_lru(db.conn(), JH, 500).unwrap();
        assert_eq!(evicted.len(), 5);
        assert_eq!(tier_bytes(db.conn(), JH).unwrap(), 500);
    }

    /// The regression the LRU column exists for: under the old rowid FIFO, rows
    /// inserted first were dropped first even if they were the ones being
    /// actively viewed.
    #[test]
    fn keeps_recently_accessed_rows_over_recently_inserted_ones() {
        let (_tmp, db) = open_db();
        for i in 0..10 {
            insert(db.conn(), &format!("/a/{i}.jpg"), 100);
        }
        // The two oldest-inserted rows are the warmest; the rest are cold.
        for i in 0..10 {
            set_accessed(db.conn(), &format!("/a/{i}.jpg"), if i < 2 { 9_000 } else { 1_000 });
        }
        let evicted = evict_tier_lru(db.conn(), JH, 200).unwrap();
        assert_eq!(evicted.len(), 8);
        assert_eq!(paths(db.conn()), vec!["/a/0.jpg", "/a/1.jpg"]);
    }

    /// A freshly generated row must not read as maximally cold, or the next
    /// eviction deletes exactly what was just generated for the viewport.
    #[test]
    fn newly_written_rows_are_warm() {
        let (_tmp, db) = open_db();
        insert(db.conn(), "/old.jpg", 100);
        set_accessed(db.conn(), "/old.jpg", 1_000);
        insert(db.conn(), "/new.jpg", 100);

        let evicted = evict_tier_lru(db.conn(), JH, 100).unwrap();
        assert_eq!(evicted, vec!["/old.jpg".to_string()]);
        assert_eq!(paths(db.conn()), vec!["/new.jpg"]);
    }

    /// Byte budgets, not row counts: one huge row must not survive just because
    /// it is a single row.
    #[test]
    fn budget_is_measured_in_bytes_not_rows() {
        let (_tmp, db) = open_db();
        insert(db.conn(), "/huge.jpg", 10_000);
        set_accessed(db.conn(), "/huge.jpg", 1_000);
        for i in 0..3 {
            insert(db.conn(), &format!("/small{i}.jpg"), 100);
            set_accessed(db.conn(), &format!("/small{i}.jpg"), 9_000);
        }
        let evicted = evict_tier_lru(db.conn(), JH, 1_000).unwrap();
        assert_eq!(evicted, vec!["/huge.jpg".to_string()]);
        assert_eq!(tier_bytes(db.conn(), JH).unwrap(), 300);
    }

    #[test]
    fn touch_accessed_marks_rows_warm() {
        let (_tmp, db) = open_db();
        insert(db.conn(), "/a.jpg", 100);
        insert(db.conn(), "/b.jpg", 100);
        set_accessed(db.conn(), "/a.jpg", 1_000);
        set_accessed(db.conn(), "/b.jpg", 1_000);

        touch_accessed(db.conn(), JH, &["/a.jpg".to_string()]).unwrap();

        let evicted = evict_tier_lru(db.conn(), JH, 100).unwrap();
        assert_eq!(evicted, vec!["/b.jpg".to_string()]);
    }

    /// Uncapped tiers have no `accessed_at` column; the helpers must be inert
    /// for them rather than producing a SQL error.
    #[test]
    fn uncapped_tiers_are_left_alone() {
        let (_tmp, db) = open_db();
        assert!(!is_lru_capped(ThumbTier::Justified));
        touch_accessed(db.conn(), ThumbTier::Justified, &["/a.jpg".to_string()]).unwrap();
    }
}
