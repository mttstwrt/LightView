//! `tag_index` and the `index_state` bookkeeping that lets re-indexing skip
//! unchanged companions.
//!
//! Pure derived state: rebuilt from companion files, droppable at any time.
//! Companions are the record of intent; this is the shape that makes filtering
//! a SQL query instead of a directory walk.
//!
//! There is no `tag_counts` table. That one was keyed `(namespace, tag)` rather
//! than by path, so it sat outside the path-keyed sweep, needed two maintenance
//! paths of its own, and was rebuilt per apply batch at a cost that scaled with
//! the library rather than the batch. [`tag_counts`] is the aggregate that
//! replaces it, run at the moments the autocomplete engine already refreshes.

use std::collections::HashMap;

use rusqlite::Connection;

use crate::autocomplete::engine::TagCount;
use crate::cache::db::CacheError;
use crate::companion::schema::CompanionFile;
use crate::path::RelPath;

/// What `index_state` remembers about a companion, so an unchanged one is
/// skipped.
///
/// `(mtime_nanos, size)` rather than whole seconds: see the schema comment in
/// [`crate::cache::db`] for the failure that truncation causes once the sweep
/// runs beside a stream of writes from another machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexState {
    pub mtime_nanos: i64,
    pub size: i64,
}

impl IndexState {
    /// Read it off a companion file's metadata.
    pub fn of(meta: &std::fs::Metadata) -> Self {
        let nanos = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        Self {
            mtime_nanos: nanos,
            size: meta.len() as i64,
        }
    }
}

/// Replace every tag row for one path from its companion.
///
/// Replace rather than merge: a companion is the whole truth for a path, so a
/// tag removed there must disappear here too.
///
/// `index_state` is deliberately **not** stamped here. The caller knows which
/// companion state it read, and stamping a different one would make the next
/// pass skip a file that had in fact changed in between.
pub fn reindex_file(
    conn: &Connection,
    path: &RelPath,
    companion: &CompanionFile,
) -> Result<(), CacheError> {
    // One transaction for the delete plus every insert. Outside one, each
    // statement auto-commits, so re-indexing a file carrying twenty tags cost
    // twenty-one WAL commits — and the callers that matter are loops: renaming
    // a tag rewrites every file that carries it.
    //
    // `None` when the caller already has a transaction open on this connection
    // (nesting is an error, not a nested scope). The statements then run inside
    // the caller's, which is what that caller wanted.
    let tx = conn.unchecked_transaction().ok();

    conn.prepare_cached("DELETE FROM tag_index WHERE path = ?1")?
        .execute([path.as_str()])?;
    {
        let mut stmt = conn.prepare_cached(
            "INSERT OR IGNORE INTO tag_index (path, namespace, tag) VALUES (?1, ?2, ?3)",
        )?;
        for (namespace, tag) in companion.all_tags() {
            stmt.execute(rusqlite::params![path.as_str(), namespace, tag])?;
        }
    }

    if let Some(tx) = tx {
        tx.commit()?;
    }
    Ok(())
}

/// Record which companion state the index reflects.
pub fn set_state(conn: &Connection, path: &RelPath, state: IndexState) -> Result<(), CacheError> {
    conn.prepare_cached(
        "INSERT OR REPLACE INTO index_state (path, companion_mtime_nanos, companion_size)
         VALUES (?1, ?2, ?3)",
    )?
    .execute(rusqlite::params![
        path.as_str(),
        state.mtime_nanos,
        state.size
    ])?;
    Ok(())
}

/// Load the whole table for in-memory comparison during a scan, rather than one
/// `SELECT` per companion.
pub fn load_state(conn: &Connection) -> Result<HashMap<String, IndexState>, CacheError> {
    let mut stmt =
        conn.prepare("SELECT path, companion_mtime_nanos, companion_size FROM index_state")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            IndexState {
                mtime_nanos: r.get(1)?,
                size: r.get(2)?,
            },
        ))
    })?;
    let mut map = HashMap::new();
    for row in rows {
        let (path, state) = row?;
        map.insert(path, state);
    }
    Ok(map)
}

/// The tag vocabulary with popularity, for the autocomplete engine.
///
/// One aggregate over an indexed table, at the moments the engine already
/// refreshes — which is the whole replacement for a second table.
pub fn tag_counts(conn: &Connection) -> Result<Vec<TagCount>, CacheError> {
    let mut stmt =
        conn.prepare("SELECT namespace, tag, COUNT(*) FROM tag_index GROUP BY 1, 2")?;
    let rows = stmt.query_map([], |r| {
        Ok(TagCount {
            namespace: r.get(0)?,
            tag: r.get(1)?,
            count: r.get::<_, i64>(2)? as u32,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Every `(path, set tag)` pair, loaded once before the duplicate finder's
/// all-pairs loop.
///
/// Two files sharing any `set::` tag are never offered as a duplicate pair, and
/// that check is the *common* case rather than a rare one: a forty-frame burst
/// is 780 near-matches, every one of them suppressed. It used to be a rare
/// check over dismissed pairs, which is why loading it in one query and
/// comparing interned ids — not paths — matters here more than it did there.
pub fn set_membership(conn: &Connection) -> Result<Vec<(String, String)>, CacheError> {
    let mut stmt = conn.prepare("SELECT path, tag FROM tag_index WHERE namespace = 'set'")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Every tag on one file, grouped by namespace.
pub fn tags_for_file(
    conn: &Connection,
    path: &RelPath,
) -> Result<Vec<(String, String)>, CacheError> {
    let mut stmt = conn.prepare_cached(
        "SELECT namespace, tag FROM tag_index WHERE path = ?1 ORDER BY namespace, tag",
    )?;
    let rows = stmt.query_map([path.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Every path carrying a given tag.
pub fn paths_with_tag(
    conn: &Connection,
    namespace: &str,
    tag: &str,
) -> Result<Vec<RelPath>, CacheError> {
    let mut stmt = conn
        .prepare_cached("SELECT path FROM tag_index WHERE namespace = ?1 AND tag = ?2")?;
    let rows = stmt.query_map([namespace, tag], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(RelPath::new(&row?)?);
    }
    Ok(out)
}

/// Drop the index entirely, for a full rebuild.
pub fn clear(conn: &Connection) -> Result<(), CacheError> {
    conn.execute_batch("DELETE FROM tag_index; DELETE FROM index_state;")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::db::CacheDb;
    use crate::companion::schema::{MediaType, PluginTagEntry};

    fn db(dir: &std::path::Path) -> CacheDb {
        let db = CacheDb::open_at(dir).unwrap();
        {
            let conn = db.writer_blocking();
            conn.execute(
                "INSERT INTO media_meta (path, media_type, file_size, mtime) VALUES ('a.jpg','image',1,1)",
                [],
            )
            .unwrap();
        }
        db
    }

    #[test]
    fn reindexing_replaces_rather_than_merges() {
        let dir = tempfile::tempdir().unwrap();
        let db = db(dir.path());
        let conn = db.writer_blocking();
        let path = RelPath::new("a.jpg").unwrap();

        let mut c = CompanionFile::new("a.jpg", MediaType::Image);
        c.tags.user = vec!["one".into(), "two".into()];
        reindex_file(&conn, &path, &c).unwrap();
        assert_eq!(tags_for_file(&conn, &path).unwrap().len(), 2);

        c.tags.user = vec!["one".into()];
        reindex_file(&conn, &path, &c).unwrap();
        let tags = tags_for_file(&conn, &path).unwrap();
        assert_eq!(tags, vec![("user".to_string(), "one".to_string())]);
    }

    #[test]
    fn auto_tags_in_an_old_sidecar_never_reach_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let db = db(dir.path());
        let conn = db.writer_blocking();
        let path = RelPath::new("a.jpg").unwrap();

        let c: CompanionFile = serde_json::from_str(
            r#"{"schema_version":1,"file":"a.jpg","file_hash":"","media_type":"image",
                "created":"","modified":"",
                "tags":{"user":["u"],"auto":["indoor"],"set":["s"],"plugins":{}},
                "meta":{"core":null,"plugins":{}}}"#,
        )
        .unwrap();
        reindex_file(&conn, &path, &c).unwrap();

        let namespaces: Vec<String> = tags_for_file(&conn, &path)
            .unwrap()
            .into_iter()
            .map(|(ns, _)| ns)
            .collect();
        assert!(namespaces.contains(&"user".to_string()));
        assert!(namespaces.contains(&"set".to_string()));
        assert!(!namespaces.contains(&"auto".to_string()));
    }

    #[test]
    fn tag_counts_aggregates_what_a_counts_table_used_to_hold() {
        let dir = tempfile::tempdir().unwrap();
        let db = db(dir.path());
        let conn = db.writer_blocking();
        conn.execute(
            "INSERT INTO media_meta (path, media_type, file_size, mtime) VALUES ('b.jpg','image',1,1)",
            [],
        )
        .unwrap();

        let mut a = CompanionFile::new("a.jpg", MediaType::Image);
        a.tags.user = vec!["beach".into()];
        a.tags.plugins.insert(
            "wd".into(),
            PluginTagEntry {
                version: "1.0.0".into(),
                tags: vec!["beach".into()],
                ..Default::default()
            },
        );
        let mut b = CompanionFile::new("b.jpg", MediaType::Image);
        b.tags.user = vec!["beach".into()];

        reindex_file(&conn, &RelPath::new("a.jpg").unwrap(), &a).unwrap();
        reindex_file(&conn, &RelPath::new("b.jpg").unwrap(), &b).unwrap();

        let mut counts = tag_counts(&conn).unwrap();
        counts.sort_by(|x, y| x.namespace.cmp(&y.namespace));
        assert_eq!(counts.len(), 2);
        assert_eq!(counts[0].namespace, "plugin.wd");
        assert_eq!(counts[0].count, 1);
        assert_eq!(counts[1].namespace, "user");
        assert_eq!(counts[1].count, 2);
    }

    #[test]
    fn index_state_distinguishes_two_writes_in_one_second() {
        let dir = tempfile::tempdir().unwrap();
        let db = db(dir.path());
        let conn = db.writer_blocking();
        let path = RelPath::new("a.jpg").unwrap();

        let at_two = IndexState { mtime_nanos: 1_700_000_000_200_000_000, size: 400 };
        let at_six = IndexState { mtime_nanos: 1_700_000_000_600_000_000, size: 400 };
        set_state(&conn, &path, at_two).unwrap();
        assert_ne!(load_state(&conn).unwrap()["a.jpg"], at_six);

        set_state(&conn, &path, at_six).unwrap();
        assert_eq!(load_state(&conn).unwrap()["a.jpg"], at_six);
    }
}
