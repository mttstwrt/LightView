//! The tag index: `tag_index` (path, namespace, tag) and the `index_state`
//! companion-mtime bookkeeping that lets re-indexing skip unchanged files.
//!
//! This table is pure derived state — it is rebuilt from companion files and
//! can be dropped and regenerated at any time. Companion files are the record
//! of intent; this is the shape that makes filtering a SQL query instead of a
//! directory walk.

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
        // One transaction for the delete plus every insert. Outside one, each
        // of those statements auto-commits, so re-indexing a file carrying
        // twenty tags cost twenty-one WAL commits — and the callers that
        // matter are loops: renaming a tag rewrites every file that carries
        // it, and `reindex_gallery` walks the whole library.
        //
        // `None` when a caller already has a transaction open on this
        // connection (nesting is an error, not a nested scope). The statements
        // then run inside the caller's, which is what that caller wanted; the
        // only cost is that this function no longer decides when they commit.
        let tx = self.conn().unchecked_transaction().ok();

        // Replace rather than merge: a companion is the whole truth for a
        // path, so a tag removed there must disappear here too.
        self.conn().execute(
            "DELETE FROM tag_index WHERE path = ?1",
            rusqlite::params![path],
        )?;

        {
            let mut stmt = self.conn().prepare_cached(
                "INSERT OR IGNORE INTO tag_index (path, namespace, tag) VALUES (?1, ?2, ?3)",
            )?;

            for (namespace, tag) in companion.all_tags() {
                stmt.execute(rusqlite::params![path, namespace, tag])?;
            }
        }

        if let Some(tx) = tx {
            tx.commit()?;
        }

        // `index_state` is deliberately *not* stamped here. The caller knows
        // the companion mtime it read, and stamping a different one would make
        // the next open skip a file that had in fact changed.
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
