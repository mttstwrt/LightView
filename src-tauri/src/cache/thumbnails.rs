use crate::cache::db::{CacheDb, CacheError};

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
}

impl ThumbTier {
    /// Map a URL-path segment ("s", "m", "l", "p") to a tier.
    pub fn from_segment(s: &str) -> Option<Self> {
        match s {
            "s" | "micro" => Some(Self::Micro),
            "m" | "standard" | "" => Some(Self::Standard),
            "l" | "large" => Some(Self::Large),
            "p" | "preview" => Some(Self::Preview),
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
        }
    }

    /// SQL table storing this tier.
    pub fn table(self) -> &'static str {
        match self {
            Self::Micro => "thumbnails_micro",
            Self::Standard => "thumbnails",
            Self::Large => "thumbnails_large",
            Self::Preview => "thumbnails_preview",
        }
    }

    /// Target dimensions (square) for this tier.
    pub fn target_size(self) -> u32 {
        match self {
            Self::Micro => 128,
            Self::Standard => 512,
            Self::Large => 1024,
            Self::Preview => 1600,
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
        let mut stmt = self
            .conn()
            .prepare_cached("SELECT thumbhash FROM thumbnails WHERE path = ?1")?;
        let mut out = Vec::with_capacity(paths.len());
        for path in paths {
            let mut rows = stmt.query(rusqlite::params![path])?;
            out.push(match rows.next()? {
                Some(row) => row.get::<_, Option<Vec<u8>>>(0)?,
                None => None,
            });
        }
        Ok(out)
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
}
