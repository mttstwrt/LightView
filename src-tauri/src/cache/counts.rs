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

    /// Get the top N most popular tags, optionally filtered by namespace.
    pub fn top_tags(
        &self,
        namespace: Option<&str>,
        limit: usize,
    ) -> Result<Vec<TagCount>, CacheError> {
        let (sql, params): (&str, Vec<Box<dyn rusqlite::types::ToSql>>) = match namespace {
            Some(ns) => (
                "SELECT namespace, tag, count FROM tag_counts WHERE namespace = ?1 ORDER BY count DESC LIMIT ?2",
                vec![Box::new(ns.to_string()), Box::new(limit as u32)],
            ),
            None => (
                "SELECT namespace, tag, count FROM tag_counts ORDER BY count DESC LIMIT ?1",
                vec![Box::new(limit as u32)],
            ),
        };
        let mut stmt = self.conn().prepare(sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| {
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
}
