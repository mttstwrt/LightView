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
    /// ~1600 px longest edge, **aspect-preserving** (no square crop) — justified
    /// gallery view when zoomed in, lazy-generated for visible cells only.
    /// `thumbnails_justified_high`.
    JustifiedHigh,
}

impl ThumbTier {
    /// Map a URL-path segment ("s", "m", "l", "p") to a tier.
    pub fn from_segment(s: &str) -> Option<Self> {
        match s {
            "s" | "micro" => Some(Self::Micro),
            "m" | "standard" | "" => Some(Self::Standard),
            "l" | "large" => Some(Self::Large),
            "p" | "preview" => Some(Self::Preview),
            "j" | "justified" => Some(Self::Justified),
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
            Self::JustifiedHigh => "thumbnails_justified_high",
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
    let sql = format!(
        "INSERT OR REPLACE INTO {} (path, media_type, mtime, width, height, thumbnail, format)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        tier.table()
    );
    conn.execute(
        &sql,
        rusqlite::params![path, media_type, mtime, width, height, thumbnail, format],
    )?;
    Ok(())
}

/// Bound a tier table to its `max_rows` most-recently-inserted rows, deleting
/// the oldest beyond that. Ordering is by `rowid` (insertion order), so this is
/// FIFO eviction, not strict LRU — true LRU would need an access-time write on
/// every cache read, which we deliberately avoid on the hot serve path. Used to
/// cap the high justified tier, whose 2560 px WebP rows are generated for
/// whatever you view zoomed in and would otherwise grow without limit.
/// Returns the number of rows evicted.
pub fn evict_tier_fifo(
    conn: &rusqlite::Connection,
    tier: ThumbTier,
    max_rows: u32,
) -> Result<usize, CacheError> {
    debug_assert!(!matches!(tier, ThumbTier::Standard));
    // Keep the newest `max_rows` (highest rowids); delete everything older.
    // `LIMIT -1 OFFSET n` skips the n newest, yielding the rest (the oldest).
    let sql = format!(
        "DELETE FROM {} WHERE rowid IN (
            SELECT rowid FROM {} ORDER BY rowid DESC LIMIT -1 OFFSET ?1
        )",
        tier.table(),
        tier.table(),
    );
    let evicted = conn.execute(&sql, rusqlite::params![max_rows])?;
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
        let mut count = self.conn().execute("DELETE FROM thumbnails", [])?;
        count += self
            .conn()
            .execute("DELETE FROM thumbnails_micro", [])
            .unwrap_or(0);
        count += self
            .conn()
            .execute("DELETE FROM thumbnails_large", [])
            .unwrap_or(0);
        count += self
            .conn()
            .execute("DELETE FROM thumbnails_preview", [])
            .unwrap_or(0);
        count += self
            .conn()
            .execute("DELETE FROM thumbnails_justified", [])
            .unwrap_or(0);
        count += self
            .conn()
            .execute("DELETE FROM thumbnails_justified_high", [])
            .unwrap_or(0);
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
        for (label, tier) in [
            ("Micro", ThumbTier::Micro),
            ("Large", ThumbTier::Large),
            ("Preview", ThumbTier::Preview),
            ("Justified", ThumbTier::Justified),
            ("Justified High", ThumbTier::JustifiedHigh),
        ] {
            let sql = format!(
                "SELECT width, height, LENGTH(thumbnail), format FROM {} WHERE path = ?1",
                tier.table()
            );
            let mut stmt = self.conn().prepare_cached(&sql)?;
            let mut rows = stmt.query(rusqlite::params![path])?;
            if let Some(row) = rows.next()? {
                results.push((
                    label.to_string(),
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
        let sql = format!("SELECT 1 FROM {} WHERE path = ?1", tier.table());
        let mut stmt = self.conn().prepare_cached(&sql)?;
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
