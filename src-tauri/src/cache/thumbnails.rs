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

impl CacheDb {
    /// Check if a thumbnail is cached and still valid (mtime matches).
    pub fn thumbnail_is_valid(&self, path: &str, current_mtime: u64) -> Result<bool, CacheError> {
        let mut stmt = self
            .conn()
            .prepare_cached("SELECT mtime FROM thumbnails WHERE path = ?1")?;
        let mut rows = stmt.query(rusqlite::params![path])?;
        match rows.next()? {
            Some(row) => {
                let cached_mtime: u64 = row.get(0)?;
                Ok(cached_mtime == current_mtime)
            }
            None => Ok(false),
        }
    }

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
        self.conn().execute(
            "INSERT OR REPLACE INTO thumbnails (path, media_type, mtime, width, height, thumbnail, format, resize_filter)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![path, media_type, mtime, width, height, thumbnail, format, resize_filter],
        )?;
        Ok(())
    }

    /// Get all cached thumbnail paths (for checking what needs regeneration).
    pub fn all_thumbnail_paths(&self) -> Result<Vec<(String, u64)>, CacheError> {
        let mut stmt = self
            .conn()
            .prepare("SELECT path, mtime FROM thumbnails")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
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
        let count = self.conn().execute("DELETE FROM thumbnails", [])?;
        Ok(count)
    }
}
