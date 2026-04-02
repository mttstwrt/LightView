use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// SQLite cache database for thumbnails, tag index, media metadata, and counts.
pub struct CacheDb {
    conn: rusqlite::Connection,
}

impl CacheDb {
    /// Open (or create) the cache database for a gallery.
    /// Creates the `.lightview/` directory if it doesn't exist.
    pub fn open(gallery_path: &Path) -> Result<Self, CacheError> {
        let lightview_dir = gallery_path.join(".lightview");
        if !lightview_dir.exists() {
            std::fs::create_dir_all(&lightview_dir)?;
        }

        let db_path = lightview_dir.join("cache.db");
        let conn = rusqlite::Connection::open(&db_path)?;

        // Enable WAL mode for better concurrent read performance
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
        conn.execute_batch("PRAGMA synchronous=NORMAL;")?;
        conn.execute_batch("PRAGMA cache_size=-64000;")?; // 64MB cache

        // Create all tables
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS thumbnails (
                path         TEXT PRIMARY KEY,
                media_type   TEXT NOT NULL,
                mtime        INTEGER NOT NULL,
                width        INTEGER NOT NULL,
                height       INTEGER NOT NULL,
                thumbnail    BLOB NOT NULL,
                format       TEXT NOT NULL DEFAULT 'jpeg',
                resize_filter TEXT NOT NULL DEFAULT 'nearest'
            );

            CREATE TABLE IF NOT EXISTS tag_index (
                path        TEXT NOT NULL,
                namespace   TEXT NOT NULL,
                tag         TEXT NOT NULL,
                PRIMARY KEY (path, namespace, tag)
            );
            CREATE INDEX IF NOT EXISTS idx_tag_ns ON tag_index(namespace, tag);
            CREATE INDEX IF NOT EXISTS idx_tag_value ON tag_index(tag);

            CREATE TABLE IF NOT EXISTS tag_counts (
                namespace   TEXT NOT NULL,
                tag         TEXT NOT NULL,
                count       INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (namespace, tag)
            );
            CREATE INDEX IF NOT EXISTS idx_tag_counts_pop ON tag_counts(count DESC);

            CREATE TABLE IF NOT EXISTS file_hashes (
                path        TEXT PRIMARY KEY,
                hash        TEXT NOT NULL,
                mtime       INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_hash ON file_hashes(hash);

            CREATE TABLE IF NOT EXISTS index_state (
                path                TEXT PRIMARY KEY,
                companion_mtime     INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS media_meta (
                path          TEXT PRIMARY KEY,
                date_taken    INTEGER,
                file_size     INTEGER NOT NULL,
                media_type    TEXT NOT NULL,
                width         INTEGER,
                height        INTEGER,
                duration      REAL,
                rating        INTEGER
            );
            CREATE INDEX IF NOT EXISTS idx_meta_date ON media_meta(date_taken DESC);
            CREATE INDEX IF NOT EXISTS idx_meta_size ON media_meta(file_size DESC);
            CREATE INDEX IF NOT EXISTS idx_meta_type ON media_meta(media_type);

            CREATE TABLE IF NOT EXISTS gallery_meta (
                key     TEXT PRIMARY KEY,
                value   TEXT NOT NULL
            );
            ",
        )?;

        // Migrate: add format and resize_filter columns to existing thumbnails tables
        let _ = conn.execute_batch(
            "ALTER TABLE thumbnails ADD COLUMN format TEXT NOT NULL DEFAULT 'jpeg'",
        );
        let _ = conn.execute_batch(
            "ALTER TABLE thumbnails ADD COLUMN resize_filter TEXT NOT NULL DEFAULT 'nearest'",
        );
        // Migrate: add perceptual hash column for duplicate detection
        let _ = conn.execute_batch(
            "ALTER TABLE thumbnails ADD COLUMN phash INTEGER",
        );

        Ok(Self { conn })
    }

    /// Get a reference to the underlying connection (for module-specific queries).
    pub fn conn(&self) -> &rusqlite::Connection {
        &self.conn
    }

    /// Run a WAL checkpoint to consolidate the WAL file back into the main database.
    /// Uses TRUNCATE mode to reclaim disk space from the WAL file.
    pub fn checkpoint(&self) -> Result<(), CacheError> {
        self.conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(())
    }

    /// Begin a transaction for batch operations.
    pub fn transaction(&mut self) -> Result<rusqlite::Transaction<'_>, CacheError> {
        Ok(self.conn.transaction()?)
    }

    /// Store a gallery-level metadata value.
    pub fn set_gallery_meta(&self, key: &str, value: &str) -> Result<(), CacheError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO gallery_meta (key, value) VALUES (?1, ?2)",
            rusqlite::params![key, value],
        )?;
        Ok(())
    }

    /// Retrieve a gallery-level metadata value.
    pub fn get_gallery_meta(&self, key: &str) -> Result<Option<String>, CacheError> {
        let mut stmt = self
            .conn
            .prepare("SELECT value FROM gallery_meta WHERE key = ?1")?;
        let mut rows = stmt.query(rusqlite::params![key])?;
        match rows.next()? {
            Some(row) => Ok(Some(row.get(0)?)),
            None => Ok(None),
        }
    }
}
