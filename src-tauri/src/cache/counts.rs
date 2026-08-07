//! `tag_counts`: per-`(namespace, tag)` frequencies, derived from
//! [`crate::cache::index`].
//!
//! Maintained two ways on purpose. A full rebuild runs at gallery open, where
//! one aggregate scan beats replaying every tag; individual edits then
//! increment and decrement, because a rebuild per keystroke in the tag editor
//! would scan the whole index to change one row. The autocomplete engine loads
//! from here rather than from `tag_index`, so a tag's popularity is already
//! summed by the time it is ranked.

use crate::cache::db::{CacheDb, CacheError};

/// A tag with its namespace and frequency count.
#[derive(Debug, Clone)]
pub struct TagCount {
    pub namespace: String,
    pub tag: String,
    pub count: u32,
}

impl CacheDb {
    /// Rebuild the entire tag_counts table from the tag_index table.
    /// Call after a full re-index or when counts may be out of sync.
    pub fn rebuild_tag_counts(&self) -> Result<(), CacheError> {
        self.conn().execute_batch(
            "
            DELETE FROM tag_counts;
            INSERT INTO tag_counts (namespace, tag, count)
            SELECT namespace, tag, COUNT(*) FROM tag_index
            GROUP BY namespace, tag;
            ",
        )?;
        Ok(())
    }

    /// Increment the count for a specific tag (used during incremental indexing).
    pub fn increment_tag_count(&self, namespace: &str, tag: &str) -> Result<(), CacheError> {
        self.conn().execute(
            "INSERT INTO tag_counts (namespace, tag, count) VALUES (?1, ?2, 1)
             ON CONFLICT(namespace, tag) DO UPDATE SET count = count + 1",
            rusqlite::params![namespace, tag],
        )?;
        Ok(())
    }

    /// Decrement the count for a specific tag. Removes the row if count reaches 0.
    pub fn decrement_tag_count(&self, namespace: &str, tag: &str) -> Result<(), CacheError> {
        self.conn().execute(
            "UPDATE tag_counts SET count = count - 1 WHERE namespace = ?1 AND tag = ?2",
            rusqlite::params![namespace, tag],
        )?;
        self.conn().execute(
            "DELETE FROM tag_counts WHERE namespace = ?1 AND tag = ?2 AND count <= 0",
            rusqlite::params![namespace, tag],
        )?;
        Ok(())
    }

    /// Get all tag counts (used to populate the in-memory autocomplete cache).
    pub fn query_all_tag_counts(&self) -> Result<Vec<TagCount>, CacheError> {
        let mut stmt = self.conn().prepare(
            "SELECT namespace, tag, count FROM tag_counts ORDER BY count DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(TagCount {
                namespace: row.get(0)?,
                tag: row.get(1)?,
                count: row.get::<_, u32>(2)?,
            })
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// List every tag in a namespace with the number of files carrying it.
    ///
    /// Reads `tag_index` directly rather than `tag_counts`: the counts table is
    /// maintained incrementally (increment/decrement per edit) and can drift,
    /// and the tag manager shows these numbers to the user right before they
    /// merge or delete something.
    pub fn tags_in_namespace_with_counts(
        &self,
        namespace: &str,
    ) -> Result<Vec<TagCount>, CacheError> {
        let mut stmt = self.conn().prepare(
            "SELECT tag, COUNT(*) FROM tag_index WHERE namespace = ?1
             GROUP BY tag ORDER BY COUNT(*) DESC, tag ASC",
        )?;
        let rows = stmt.query_map(rusqlite::params![namespace], |row| {
            Ok(TagCount {
                namespace: namespace.to_string(),
                tag: row.get(0)?,
                count: row.get::<_, u32>(1)?,
            })
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }
}
