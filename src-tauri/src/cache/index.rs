use crate::cache::db::{CacheDb, CacheError};
use crate::companion::schema::CompanionFile;

impl CacheDb {
    /// Re-index all tags for a single media file from its companion data.
    /// Deletes existing tags for the path and inserts fresh ones.
    pub fn reindex_tags_for_file(
        &self,
        path: &str,
        companion: &CompanionFile,
    ) -> Result<(), CacheError> {
        // Delete existing tags for this path
        self.conn().execute(
            "DELETE FROM tag_index WHERE path = ?1",
            rusqlite::params![path],
        )?;

        // Insert all tags
        let mut stmt = self.conn().prepare_cached(
            "INSERT OR IGNORE INTO tag_index (path, namespace, tag) VALUES (?1, ?2, ?3)",
        )?;

        for (namespace, tag) in companion.all_tags() {
            stmt.execute(rusqlite::params![path, namespace, tag])?;
        }

        // Update index_state with the current companion mtime
        // (caller is responsible for passing the correct mtime)

        Ok(())
    }

    /// Update the index_state for a file to record when it was last indexed.
    pub fn set_index_state(&self, path: &str, companion_mtime: u64) -> Result<(), CacheError> {
        self.conn().execute(
            "INSERT OR REPLACE INTO index_state (path, companion_mtime) VALUES (?1, ?2)",
            rusqlite::params![path, companion_mtime],
        )?;
        Ok(())
    }

    /// Load the entire `index_state` table into a map for in-memory mtime
    /// checks. Avoids one `SELECT` round-trip per companion during a full
    /// gallery scan.
    pub fn load_index_state(
        &self,
    ) -> Result<std::collections::HashMap<String, u64>, CacheError> {
        let mut stmt = self
            .conn()
            .prepare("SELECT path, companion_mtime FROM index_state")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
        })?;
        let mut map = std::collections::HashMap::new();
        for row in rows {
            let (path, mtime) = row?;
            map.insert(path, mtime);
        }
        Ok(map)
    }

    /// Check if a companion file needs re-indexing (mtime changed).
    pub fn needs_reindex(&self, path: &str, current_mtime: u64) -> Result<bool, CacheError> {
        let mut stmt = self
            .conn()
            .prepare_cached("SELECT companion_mtime FROM index_state WHERE path = ?1")?;
        let mut rows = stmt.query(rusqlite::params![path])?;
        match rows.next()? {
            Some(row) => {
                let cached: u64 = row.get(0)?;
                Ok(cached != current_mtime)
            }
            None => Ok(true), // never indexed
        }
    }

    /// Query paths matching a single tag in a namespace.
    pub fn query_tag(&self, namespace: &str, tag: &str) -> Result<Vec<String>, CacheError> {
        let mut stmt = self.conn().prepare_cached(
            "SELECT path FROM tag_index WHERE namespace = ?1 AND tag = ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![namespace, tag], |row| {
            row.get::<_, String>(0)
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// Query paths matching ANY tag in a namespace.
    pub fn query_namespace(&self, namespace: &str) -> Result<Vec<String>, CacheError> {
        let mut stmt = self.conn().prepare_cached(
            "SELECT DISTINCT path FROM tag_index WHERE namespace = ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![namespace], |row| {
            row.get::<_, String>(0)
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// Get all tags for a specific file, grouped by namespace.
    pub fn get_tags_for_file(
        &self,
        path: &str,
    ) -> Result<Vec<(String, String)>, CacheError> {
        let mut stmt = self.conn().prepare_cached(
            "SELECT namespace, tag FROM tag_index WHERE path = ?1 ORDER BY namespace, tag",
        )?;
        let rows = stmt.query_map(rusqlite::params![path], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// Delete all tag index entries (for full rebuild).
    pub fn clear_tag_index(&self) -> Result<(), CacheError> {
        self.conn().execute("DELETE FROM tag_index", [])?;
        self.conn().execute("DELETE FROM index_state", [])?;
        Ok(())
    }
}
