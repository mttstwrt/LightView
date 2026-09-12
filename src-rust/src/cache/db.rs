//! The derived cache: schema, connections, and the maintenance that spans every
//! table.
//!
//! **There are no migrations.** One `format_version` integer in `gallery_meta`;
//! if it does not match the build's, the file is deleted and re-indexed. The
//! database is fully derived from the photos and their companions, so
//! migrating it would be permanent code that runs once — and the migration list
//! it replaces had already drifted from its own derived version constant.
//!
//! The consequence, stated because it is the cost: a schema mistake here is not
//! a patch later, it is a version bump that re-thumbnails every library. Two
//! things make that survivable rather than merely cheap to say — `date_added`
//! and `last_viewed` are mirrored into the companion (see
//! [`crate::companion::schema::CoreMeta`]), so the bump loses time and nothing
//! else.
//!
//! **Every path-keyed table is swept together.** [`path_keyed_tables`] is the
//! single source of truth, derived from [`ThumbTier::ALL`], and a test asserts
//! it matches every table in the schema that actually has a `path` column. The
//! failure that prevents is a multi-megabyte blob keyed to a path nothing can
//! reach again. There is no `not_duplicates` table, so nothing sits outside the
//! sweep.
//!
//! **Connections.** One writer behind a `tokio::Mutex`, because
//! `rusqlite::Connection` is `Send` but not `Sync`, and a read-only pool for
//! the thumbnail serve path. The writer is held **for statements only** — never
//! across filesystem I/O, an image decode or encode, a subprocess, or a loop
//! whose length scales with the library. Every one of those was a real hold in
//! the code this replaces, and each blocked every thumbnail the grid was
//! waiting on.

use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::cache::pool::{apply_read_pragmas, ReadPool};
use crate::cache::tiers::ThumbTier;
use crate::path::RelPath;
use crate::util::lock::DirLock;

/// Bumping this deletes and rebuilds every cache. There is no other mechanism.
///
/// 2 adds `media_meta.exif_read`. The rebuild is the point rather than a cost:
/// version 1 decided whether to read a file's header by asking whether anything
/// was known about it yet, and a thumbnail answers yes — so every file the grid
/// had drawn before its header was read was excluded from the EXIF pass
/// permanently. Those rows cannot be repaired in place, because nothing
/// distinguishes "probed, found nothing" from "never probed"; that is the
/// distinction the new column exists to record.
pub const FORMAT_VERSION: i64 = 2;

/// How many read connections, at most.
const READ_POOL_MAX: usize = 6;
/// Writer page cache, in KB. Measured, and carried over.
const WRITER_CACHE_KB: i64 = 64_000;
/// Per read connection, in KB.
const READER_CACHE_KB: i64 = 8_000;

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("this gallery is already open in another process")]
    AlreadyOpen,
    #[error("bad path in cache: {0}")]
    Path(#[from] crate::path::PathError),
}

/// Every table keyed by a media file's gallery-relative `path` column, tiers
/// included.
///
/// Derived from [`ThumbTier::ALL`] so a new rung is picked up automatically by
/// every path-keyed maintenance operation instead of each one repeating the
/// set — which is how tiers previously got left behind as orphaned rows.
pub fn path_keyed_tables() -> impl Iterator<Item = &'static str> {
    ["media_meta", "tag_index", "index_state"]
        .into_iter()
        .chain(ThumbTier::ALL.into_iter().map(|t| t.table()))
}

/// The whole schema. Created once; never altered.
///
/// Indexes are part of it rather than an optimization added afterwards,
/// because the query language is shaped by them: **a field is filterable only
/// if it is indexed.** Naming them here is what stops them being discovered by
/// a slow gallery.
fn schema_sql() -> String {
    let mut sql = String::from(
        "
    CREATE TABLE IF NOT EXISTS gallery_meta (
        key     TEXT PRIMARY KEY,
        value   TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS media_meta (
        path         TEXT PRIMARY KEY,
        media_type   TEXT NOT NULL,
        file_size    INTEGER NOT NULL,
        mtime        INTEGER NOT NULL,
        date_taken   INTEGER,
        date_added   INTEGER,
        last_viewed  INTEGER,
        last_rated   INTEGER,
        rating       INTEGER,
        width        INTEGER,
        height       INTEGER,
        duration     REAL,
        gps_lat      REAL,
        gps_lon      REAL,
        -- Whether this file's metadata header has been read, which is **not**
        -- the same as whether it had anything in it. A photo with no GPS and a
        -- screenshot with no EXIF block at all both leave every column above
        -- NULL, so any gate phrased over those columns re-reads them on every
        -- open, forever. This is the only honest spelling of already-looked.
        exif_read    INTEGER NOT NULL DEFAULT 0,
        color_label  TEXT,
        -- ~25 bytes, and it lives here rather than on a tier so the items
        -- query never walks a thumbnail row's overflow pages to reach it.
        thumbhash    BLOB
    );
    CREATE INDEX IF NOT EXISTS idx_meta_date_taken  ON media_meta(date_taken DESC);
    -- Partial, so it holds only the rows still owing a header read: empty on a
    -- warm gallery, which is what makes the backfill's candidate query free
    -- rather than a full scan of the library on every open.
    CREATE INDEX IF NOT EXISTS idx_meta_unprobed     ON media_meta(exif_read) WHERE exif_read = 0;
    CREATE INDEX IF NOT EXISTS idx_meta_date_added  ON media_meta(date_added DESC);
    CREATE INDEX IF NOT EXISTS idx_meta_last_viewed ON media_meta(last_viewed DESC);
    CREATE INDEX IF NOT EXISTS idx_meta_rating      ON media_meta(rating);
    CREATE INDEX IF NOT EXISTS idx_meta_color       ON media_meta(color_label);
    CREATE INDEX IF NOT EXISTS idx_meta_type        ON media_meta(media_type);
    CREATE INDEX IF NOT EXISTS idx_meta_width       ON media_meta(width);
    CREATE INDEX IF NOT EXISTS idx_meta_height      ON media_meta(height);
    CREATE INDEX IF NOT EXISTS idx_meta_size        ON media_meta(file_size DESC);

    CREATE TABLE IF NOT EXISTS tag_index (
        path        TEXT NOT NULL,
        namespace   TEXT NOT NULL,
        tag         TEXT NOT NULL,
        PRIMARY KEY (path, namespace, tag)
    );
    -- Serves the filter's EXISTS subqueries, autocomplete's refresh aggregate,
    -- and the duplicate finder's set-membership scan. The primary key's
    -- leftmost column is `path`, which is what the sweep needs.
    CREATE INDEX IF NOT EXISTS idx_tag_ns ON tag_index(namespace, tag);

    CREATE TABLE IF NOT EXISTS index_state (
        path                  TEXT PRIMARY KEY,
        -- Nanoseconds and size, not whole seconds. A gate that truncates to
        -- seconds and compares for equality was tolerable for one pass at open
        -- and is not once the sweep runs concurrently with an hours-long
        -- stream of writes from another machine: a companion read at T.2 and
        -- rewritten at T.6 has the same second, is skipped forever, and both
        -- caches confidently disagree with the durable file in opposite
        -- directions. NFSv3+ and SMB2 both carry sub-second mtimes.
        companion_mtime_nanos INTEGER NOT NULL,
        companion_size        INTEGER NOT NULL
    );
",
    );

    for tier in ThumbTier::ALL {
        sql.push_str(&format!(
            "
    CREATE TABLE IF NOT EXISTS {table} (
        path        TEXT PRIMARY KEY,
        bytes       BLOB NOT NULL,
        -- Stored rather than computed with length(), so the budget's SUM and
        -- the eviction scan never touch a blob's overflow pages.
        byte_len    INTEGER NOT NULL,
        accessed_at INTEGER NOT NULL{extra}
    );
",
            table = tier.table(),
            extra = if tier == ThumbTier::J {
                ",
        -- 64-bit dHash of this tier's decoded pixels. NULL means \"not hashed\",
        -- never \"hashed to zero\" — a genuinely flat image legitimately hashes
        -- to 0, and conflating the two makes `WHERE phash IS NOT NULL` pass
        -- every row.
        phash       INTEGER"
            } else {
                ""
            }
        ));
        if tier.bounded() {
            // Covering: the eviction window function orders by accessed_at and
            // sums byte_len, so it reads the index and nothing else.
            sql.push_str(&format!(
                "    CREATE INDEX IF NOT EXISTS idx_{table}_accessed ON {table}(accessed_at, byte_len);\n",
                table = tier.table()
            ));
        }
    }
    sql
}

/// An open gallery's derived cache.
///
/// Holding one implies holding the gallery's `flock`, which is what makes "one
/// writer" true rather than assumed.
pub struct CacheDb {
    dir: PathBuf,
    writer: tokio::sync::Mutex<Connection>,
    readers: ReadPool,
    _lock: DirLock,
}

impl CacheDb {
    /// Open (or create) the cache in `dir`, taking the gallery lock.
    ///
    /// Returns [`CacheError::AlreadyOpen`] when another process holds it — not
    /// a failure to report as one: a second `lightview <dir>` reads the live
    /// URL from `instance.json` and opens a browser there.
    pub fn open_at(dir: &Path) -> Result<Self, CacheError> {
        std::fs::create_dir_all(dir)?;
        let lock = DirLock::try_acquire(&dir.join("lock"))?.ok_or(CacheError::AlreadyOpen)?;

        let db_path = dir.join("cache.db");
        let mut conn = open_writer(&db_path)?;

        if stored_format_version(&conn)? != Some(FORMAT_VERSION) {
            // No migration path exists and none will. Drop the connection
            // first: unlinking a file SQLite still has open is silent on
            // Linux, and the process would keep writing to the dead inode.
            drop(conn);
            remove_database(&db_path)?;
            conn = open_writer(&db_path)?;
        }

        conn.execute_batch(&schema_sql())?;
        conn.execute(
            "INSERT OR REPLACE INTO gallery_meta (key, value) VALUES ('format_version', ?1)",
            [FORMAT_VERSION.to_string()],
        )?;

        let readers = ReadPool::open(&db_path, READ_POOL_MAX, READER_CACHE_KB)?;
        touch_last_opened(dir)?;

        Ok(Self {
            dir: dir.to_path_buf(),
            writer: tokio::sync::Mutex::new(conn),
            readers,
            _lock: lock,
        })
    }

    /// This gallery's cache directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Take the writer. **Hold it for statements only.**
    pub async fn writer(&self) -> tokio::sync::MutexGuard<'_, Connection> {
        self.writer.lock().await
    }

    /// Take the writer outside an async runtime — the CLI's one-shot verbs and
    /// tests. Panics if called from inside a runtime, which is the right
    /// failure: doing this on a worker thread is the hold the design forbids.
    pub fn writer_blocking(&self) -> tokio::sync::MutexGuard<'_, Connection> {
        self.writer.blocking_lock()
    }

    /// Take a read-only connection from the pool.
    pub async fn read(&self) -> crate::cache::pool::PooledConn<'_> {
        self.readers.get().await
    }

    /// Fold the WAL back into the main file. Worth doing at a natural pause;
    /// never on the hot path.
    pub fn checkpoint(conn: &Connection) -> Result<(), CacheError> {
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(())
    }
}

/// Remove every row keyed to one path, across every path-keyed table.
pub fn forget_path(conn: &Connection, path: &RelPath) -> Result<(), CacheError> {
    let tx = conn.unchecked_transaction()?;
    for table in path_keyed_tables() {
        tx.execute(
            &format!("DELETE FROM {table} WHERE path = ?1"),
            [path.as_str()],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Remove every row for paths the scan did not find.
///
/// **The caller must have a scan it trusts.** This is the operation that made
/// two ordinary events destroy the cache: a NAS not yet mounted at boot leaves
/// an empty mountpoint that scans to zero, and a transient `EIO` mid-walk
/// scanned partially — and the walk returned `Ok` for both. The provider now
/// propagates walk errors, and this refuses the second case outright.
pub fn prune_missing(conn: &Connection, present: &[RelPath]) -> Result<usize, CacheError> {
    let existing: i64 = conn.query_row("SELECT COUNT(*) FROM media_meta", [], |r| r.get(0))?;
    if present.is_empty() && existing > 0 {
        // A gallery that had files and now scans to zero is a mount that is not
        // there, not a library someone emptied by hand. Refusing costs a stale
        // row; accepting costs `date_added` and `last_viewed` for everything.
        log::warn!(
            "refusing to prune {existing} rows: the scan found no files at all, \
             which is what an unmounted share looks like"
        );
        return Ok(0);
    }

    let tx = conn.unchecked_transaction()?;
    tx.execute_batch("CREATE TEMP TABLE IF NOT EXISTS scan_present (path TEXT PRIMARY KEY)")?;
    tx.execute("DELETE FROM scan_present", [])?;
    {
        let mut stmt = tx.prepare("INSERT OR IGNORE INTO scan_present (path) VALUES (?1)")?;
        for p in present {
            stmt.execute([p.as_str()])?;
        }
    }
    let mut removed = 0;
    for table in path_keyed_tables() {
        removed += tx.execute(
            &format!("DELETE FROM {table} WHERE path NOT IN (SELECT path FROM scan_present)"),
            [],
        )?;
    }
    tx.execute("DELETE FROM scan_present", [])?;
    tx.commit()?;
    Ok(removed)
}

/// Read a `gallery_meta` value.
pub fn meta_get(conn: &Connection, key: &str) -> Result<Option<String>, CacheError> {
    let mut stmt = conn.prepare_cached("SELECT value FROM gallery_meta WHERE key = ?1")?;
    let mut rows = stmt.query([key])?;
    match rows.next()? {
        Some(row) => Ok(Some(row.get(0)?)),
        None => Ok(None),
    }
}

/// Write a `gallery_meta` value.
pub fn meta_set(conn: &Connection, key: &str, value: &str) -> Result<(), CacheError> {
    conn.prepare_cached("INSERT OR REPLACE INTO gallery_meta (key, value) VALUES (?1, ?2)")?
        .execute([key, value])?;
    Ok(())
}

fn open_writer(path: &Path) -> Result<Connection, CacheError> {
    let conn = Connection::open(path)?;
    // Carried over because they were measured. `synchronous=NORMAL` in
    // particular: without it the writer inherits SQLite's default FULL and
    // fsyncs the WAL on every commit, which on a batched index pass over a
    // large library is a startup regression nobody would trace back to an
    // omission from a list.
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;",
    )?;
    apply_read_pragmas(&conn, WRITER_CACHE_KB)?;
    Ok(conn)
}

fn stored_format_version(conn: &Connection) -> Result<Option<i64>, CacheError> {
    // A fresh file has no tables at all, which is a missing version, not an
    // error.
    let exists: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='gallery_meta'",
            [],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if !exists {
        return Ok(None);
    }
    Ok(meta_get(conn, "format_version")?.and_then(|v| v.parse().ok()))
}

fn remove_database(path: &Path) -> std::io::Result<()> {
    for suffix in ["", "-wal", "-shm"] {
        let p = PathBuf::from(format!("{}{}", path.display(), suffix));
        match std::fs::remove_file(&p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Stamp the cross-gallery LRU key.
///
/// An explicit file, **not** `cache.db`'s mtime: under WAL, writes land in
/// `cache.db-wal` and the main file's mtime moves only on checkpoint, so an
/// mtime key measures *least recently written* — and a fully-warmed gallery
/// opened daily and never written to looks colder than the throwaway folder
/// touched once. It would evict exactly the wrong thing.
pub fn touch_last_opened(dir: &Path) -> std::io::Result<()> {
    std::fs::write(dir.join("last_opened"), chrono::Utc::now().to_rfc3339())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn open(dir: &Path) -> CacheDb {
        CacheDb::open_at(dir).unwrap()
    }

    /// Insert one row keyed to `path` into every path-keyed table.
    fn seed(conn: &Connection, path: &str) {
        conn.execute(
            "INSERT INTO media_meta (path, media_type, file_size, mtime) VALUES (?1, 'image', 1, 1)",
            [path],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tag_index (path, namespace, tag) VALUES (?1, 'user', 't')",
            [path],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO index_state (path, companion_mtime_nanos, companion_size) VALUES (?1, 1, 1)",
            [path],
        )
        .unwrap();
        for tier in ThumbTier::ALL {
            conn.execute(
                &format!(
                    "INSERT INTO {} (path, bytes, byte_len, accessed_at) VALUES (?1, x'00', 1, 1)",
                    tier.table()
                ),
                [path],
            )
            .unwrap();
        }
    }

    fn rows_for(conn: &Connection, path: &str) -> Vec<(&'static str, i64)> {
        path_keyed_tables()
            .map(|t| {
                let n: i64 = conn
                    .query_row(
                        &format!("SELECT COUNT(*) FROM {t} WHERE path = ?1"),
                        [path],
                        |r| r.get(0),
                    )
                    .unwrap();
                (t, n)
            })
            .collect()
    }

    /// The stronger half of the sweep test: the list cannot fall out of step
    /// with the schema, because a table with a `path` column that is not in it
    /// fails here rather than becoming an orphaned multi-megabyte blob.
    #[test]
    fn every_table_with_a_path_column_is_in_the_sweep() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(dir.path());
        let conn = db.writer_blocking();

        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();

        let mut with_path = BTreeSet::new();
        for t in &tables {
            let has: bool = conn
                .prepare(&format!("SELECT 1 FROM pragma_table_info('{t}') WHERE name = 'path'"))
                .unwrap()
                .query_row([], |_| Ok(true))
                .unwrap_or(false);
            if has {
                with_path.insert(t.clone());
            }
        }

        let swept: BTreeSet<String> = path_keyed_tables().map(String::from).collect();
        assert_eq!(with_path, swept, "a path-keyed table is outside the sweep");
    }

    #[test]
    fn forgetting_a_path_clears_every_path_keyed_table() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(dir.path());
        let conn = db.writer_blocking();
        seed(&conn, "2026/a.jpg");
        seed(&conn, "2026/b.jpg");

        forget_path(&conn, &RelPath::new("2026/a.jpg").unwrap()).unwrap();

        for (table, n) in rows_for(&conn, "2026/a.jpg") {
            assert_eq!(n, 0, "{table} kept a row for a forgotten path");
        }
        for (table, n) in rows_for(&conn, "2026/b.jpg") {
            assert_eq!(n, 1, "{table} lost an unrelated row");
        }
    }

    #[test]
    fn pruning_removes_what_the_scan_did_not_find() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(dir.path());
        let conn = db.writer_blocking();
        seed(&conn, "a.jpg");
        seed(&conn, "b.jpg");

        prune_missing(&conn, &[RelPath::new("a.jpg").unwrap()]).unwrap();
        for (table, n) in rows_for(&conn, "b.jpg") {
            assert_eq!(n, 0, "{table} kept a row for a vanished file");
        }
        for (table, n) in rows_for(&conn, "a.jpg") {
            assert_eq!(n, 1, "{table} pruned a file that is still there");
        }
    }

    #[test]
    fn an_empty_scan_of_a_populated_gallery_is_refused() {
        // The NAS-not-mounted case. The mountpoint exists and is empty, so the
        // scan is a perfectly good `Ok(vec![])` — and acting on it destroys
        // `date_added` and `last_viewed` for the whole library.
        let dir = tempfile::tempdir().unwrap();
        let db = open(dir.path());
        let conn = db.writer_blocking();
        seed(&conn, "a.jpg");

        assert_eq!(prune_missing(&conn, &[]).unwrap(), 0);
        for (table, n) in rows_for(&conn, "a.jpg") {
            assert_eq!(n, 1, "{table} was pruned by an empty scan");
        }
    }

    #[test]
    fn a_format_version_bump_deletes_and_rebuilds() {
        let dir = tempfile::tempdir().unwrap();
        {
            let db = open(dir.path());
            let conn = db.writer_blocking();
            seed(&conn, "a.jpg");
            meta_set(&conn, "format_version", "0").unwrap();
        }
        let db = open(dir.path());
        let conn = db.writer_blocking();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM media_meta", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "a stale cache survived a format bump");
        // Against the constant, not a literal: a bump should not need this
        // test edited, or the edit becomes the place the bump is forgotten.
        assert_eq!(
            meta_get(&conn, "format_version").unwrap().as_deref(),
            Some(FORMAT_VERSION.to_string().as_str())
        );
    }

    #[test]
    fn a_second_open_is_refused_while_the_first_lives() {
        let dir = tempfile::tempdir().unwrap();
        let first = open(dir.path());
        assert!(matches!(
            CacheDb::open_at(dir.path()),
            Err(CacheError::AlreadyOpen)
        ));
        drop(first);
        assert!(CacheDb::open_at(dir.path()).is_ok());
    }

    #[test]
    fn opening_stamps_the_lru_key() {
        let dir = tempfile::tempdir().unwrap();
        let _db = open(dir.path());
        assert!(dir.path().join("last_opened").exists());
    }
}
