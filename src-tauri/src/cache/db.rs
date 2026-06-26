use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

// ---------------------------------------------------------------------------
// Schema migrations
// ---------------------------------------------------------------------------

/// Current schema version. Bump this when adding a new migration.
const SCHEMA_VERSION: u32 = 13;

/// Base tables created on a fresh database (version 0 → 1).
const BASE_SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS gallery_meta (
        key     TEXT PRIMARY KEY,
        value   TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS thumbnails (
        path         TEXT PRIMARY KEY,
        media_type   TEXT NOT NULL,
        mtime        INTEGER NOT NULL,
        width        INTEGER NOT NULL,
        height       INTEGER NOT NULL,
        thumbnail    BLOB NOT NULL
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
";

struct Migration {
    version: u32,
    sql: &'static str,
}

/// Ordered list of migrations. Each entry brings the schema from
/// `version - 1` to `version`. Migrations must be idempotent (use
/// `IF NOT EXISTS`, `ADD COLUMN` with error-ignore, etc.) so that
/// a crash mid-migration can be retried safely.
const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 2,
        sql: "
            ALTER TABLE thumbnails ADD COLUMN format TEXT NOT NULL DEFAULT 'jpeg';
            ALTER TABLE thumbnails ADD COLUMN resize_filter TEXT NOT NULL DEFAULT 'nearest';
            ALTER TABLE thumbnails ADD COLUMN phash INTEGER;
        ",
    },
    Migration {
        version: 3,
        sql: "
            ALTER TABLE media_meta ADD COLUMN last_viewed INTEGER;
            ALTER TABLE media_meta ADD COLUMN date_added INTEGER;
            CREATE INDEX IF NOT EXISTS idx_meta_last_viewed ON media_meta(last_viewed DESC);
            CREATE INDEX IF NOT EXISTS idx_meta_date_added ON media_meta(date_added DESC);
        ",
    },
    Migration {
        version: 4,
        sql: "
            ALTER TABLE media_meta ADD COLUMN last_rated INTEGER;
            CREATE INDEX IF NOT EXISTS idx_meta_last_rated ON media_meta(last_rated DESC);
        ",
    },
    // v5: ThumbHash placeholder column on the existing thumbnails table.
    // Stores the ~25-byte ThumbHash encoding used for the skeleton fallback
    // while the full thumbnail is still loading.
    Migration {
        version: 5,
        sql: "
            ALTER TABLE thumbnails ADD COLUMN thumbhash BLOB;
        ",
    },
    // v6: Separate tier tables for the LOD pyramid.
    //   thumbnails_micro  — 128x128 (T1, cellSize <= 160)
    //   thumbnails_large  — 1024x1024 (T3, cellSize > 400, lazy-generated)
    // The original `thumbnails` table remains the standard 512x512 tier (T2).
    // Using separate tables avoids rewriting the existing PK and lets queries
    // on T2 stay untouched.
    Migration {
        version: 6,
        sql: "
            CREATE TABLE IF NOT EXISTS thumbnails_micro (
                path         TEXT PRIMARY KEY,
                media_type   TEXT NOT NULL,
                mtime        INTEGER NOT NULL,
                width        INTEGER NOT NULL,
                height       INTEGER NOT NULL,
                thumbnail    BLOB NOT NULL,
                format       TEXT NOT NULL DEFAULT 'jpeg'
            );
            CREATE TABLE IF NOT EXISTS thumbnails_large (
                path         TEXT PRIMARY KEY,
                media_type   TEXT NOT NULL,
                mtime        INTEGER NOT NULL,
                width        INTEGER NOT NULL,
                height       INTEGER NOT NULL,
                thumbnail    BLOB NOT NULL,
                format       TEXT NOT NULL DEFAULT 'jpeg'
            );
            CREATE TABLE IF NOT EXISTS thumbnails_preview (
                path         TEXT PRIMARY KEY,
                media_type   TEXT NOT NULL,
                mtime        INTEGER NOT NULL,
                width        INTEGER NOT NULL,
                height       INTEGER NOT NULL,
                thumbnail    BLOB NOT NULL,
                format       TEXT NOT NULL DEFAULT 'jpeg'
            );
        ",
    },
    // v7: GPS columns on media_meta + spatial index for bbox queries.
    // Populated lazily by the EXIF backfill pass after gallery open.
    Migration {
        version: 7,
        sql: "
            ALTER TABLE media_meta ADD COLUMN gps_lat REAL;
            ALTER TABLE media_meta ADD COLUMN gps_lon REAL;
            CREATE INDEX IF NOT EXISTS idx_meta_geo ON media_meta(gps_lat, gps_lon)
                WHERE gps_lat IS NOT NULL;
        ",
    },
    // v8: User-confirmed non-duplicate pairs. Pairs are stored with
    // path_a < path_b so order is canonical and lookups are simple.
    Migration {
        version: 8,
        sql: "
            CREATE TABLE IF NOT EXISTS not_duplicates (
                path_a  TEXT NOT NULL,
                path_b  TEXT NOT NULL,
                PRIMARY KEY (path_a, path_b)
            );
        ",
    },
    // v9: Remote-access auth — per-device cookies and one-time pairing codes.
    // `remote_devices.token_hash` is an argon2 hash of the random cookie value,
    // so a database leak alone never grants access.
    // `remote_pairing` holds short-lived enrollment codes (QR token or 6-digit
    // PIN); each row is consumed on first redemption.
    // Companion settings live in gallery_meta: `remote.password_hash`,
    // `remote.inactivity_secs`.
    Migration {
        version: 9,
        sql: "
            CREATE TABLE IF NOT EXISTS remote_devices (
                id            TEXT PRIMARY KEY,
                name          TEXT NOT NULL,
                token_hash    TEXT NOT NULL,
                created_at    INTEGER NOT NULL,
                last_seen     INTEGER NOT NULL,
                last_auth_at  INTEGER NOT NULL,
                revoked_at    INTEGER
            );

            CREATE TABLE IF NOT EXISTS remote_pairing (
                code         TEXT PRIMARY KEY,
                kind         TEXT NOT NULL,
                expires_at   INTEGER NOT NULL,
                consumed_at  INTEGER
            );
            CREATE INDEX IF NOT EXISTS idx_pairing_expires ON remote_pairing(expires_at);
        ",
    },
    // v10: Pre-rendered animated-GIF frame atlases. WebKitGTK 2.52 animates
    // `<img>` GIFs too fast and leaks decoded frames, so the frontend renders
    // GIFs on a canvas from a sprite sheet we build here. Keyed by (path, tier)
    // like thumbnails; `delays` is a comma-separated per-frame ms list.
    Migration {
        version: 10,
        sql: "
            CREATE TABLE IF NOT EXISTS gif_atlas (
                path         TEXT NOT NULL,
                tier         TEXT NOT NULL,
                mtime        INTEGER NOT NULL,
                frame_count  INTEGER NOT NULL,
                frame_w      INTEGER NOT NULL,
                frame_h      INTEGER NOT NULL,
                cols         INTEGER NOT NULL,
                delays       TEXT NOT NULL,
                atlas        BLOB NOT NULL,
                PRIMARY KEY (path, tier)
            );
        ",
    },
    // v11: Aspect-preserving tier for the justified gallery view. Unlike the
    // square tiers, rows here store the thumbnail's true (non-square) width and
    // height; the layout reads source dimensions from media_meta separately.
    Migration {
        version: 11,
        sql: "
            CREATE TABLE IF NOT EXISTS thumbnails_justified (
                path         TEXT PRIMARY KEY,
                media_type   TEXT NOT NULL,
                mtime        INTEGER NOT NULL,
                width        INTEGER NOT NULL,
                height       INTEGER NOT NULL,
                thumbnail    BLOB NOT NULL,
                format       TEXT NOT NULL DEFAULT 'webp'
            );
        ",
    },
    // v12: high-resolution justified tier (1600px longest edge), generated
    // for visible cells only when zoomed in. Same shape as thumbnails_justified.
    Migration {
        version: 12,
        sql: "
            CREATE TABLE IF NOT EXISTS thumbnails_justified_high (
                path         TEXT PRIMARY KEY,
                media_type   TEXT NOT NULL,
                mtime        INTEGER NOT NULL,
                width        INTEGER NOT NULL,
                height       INTEGER NOT NULL,
                thumbnail    BLOB NOT NULL,
                format       TEXT NOT NULL DEFAULT 'webp'
            );
        ",
    },
    // v13: the high justified tier target grew 1600 → 2560, so any rows cached
    // at the old size are stale. Clear them; they regenerate lazily at 2560.
    Migration {
        version: 13,
        sql: "DELETE FROM thumbnails_justified_high;",
    },
    // v14: intermediate justified tier (1280px longest edge) for mid zoom, so a
    // mid-detail cell stops decoding the 2560px high tier. Same shape as the
    // other justified tiers.
    Migration {
        version: 14,
        sql: "
            CREATE TABLE IF NOT EXISTS thumbnails_justified_mid (
                path         TEXT PRIMARY KEY,
                media_type   TEXT NOT NULL,
                mtime        INTEGER NOT NULL,
                width        INTEGER NOT NULL,
                height       INTEGER NOT NULL,
                thumbnail    BLOB NOT NULL,
                format       TEXT NOT NULL DEFAULT 'webp'
            );
        ",
    },
];

/// Read the current schema version from `gallery_meta`.
/// Returns 0 if the table doesn't exist or has no version entry.
fn get_schema_version(conn: &rusqlite::Connection) -> u32 {
    conn.query_row(
        "SELECT value FROM gallery_meta WHERE key = 'schema_version'",
        [],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .and_then(|v| v.parse::<u32>().ok())
    .unwrap_or(0)
}

fn set_schema_version(conn: &rusqlite::Connection, version: u32) -> Result<(), CacheError> {
    conn.execute(
        "INSERT OR REPLACE INTO gallery_meta (key, value) VALUES ('schema_version', ?1)",
        rusqlite::params![version.to_string()],
    )?;
    Ok(())
}

/// Detect if this is a pre-versioned database by checking for tables
/// that only exist after the base schema was applied.
fn detect_legacy_version(conn: &rusqlite::Connection) -> u32 {
    // If media_meta doesn't exist, this is a completely fresh DB.
    let has_media_meta: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='media_meta'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0)
        > 0;

    if !has_media_meta {
        return 0;
    }

    // media_meta exists — check if the v2 thumbnail columns are present.
    let has_thumb_format = conn
        .execute("SELECT format FROM thumbnails LIMIT 0", [])
        .is_ok();

    if !has_thumb_format {
        return 1; // base schema only
    }

    // v2 columns exist — check for v3 columns.
    let has_last_viewed = conn
        .execute("SELECT last_viewed FROM media_meta LIMIT 0", [])
        .is_ok();

    if !has_last_viewed {
        return 2;
    }

    // v3 columns exist — check for v4 columns.
    let has_last_rated = conn
        .execute("SELECT last_rated FROM media_meta LIMIT 0", [])
        .is_ok();

    if !has_last_rated {
        return 3;
    }

    // v4 columns exist — check for v5 (thumbhash).
    let has_thumbhash = conn
        .execute("SELECT thumbhash FROM thumbnails LIMIT 0", [])
        .is_ok();

    if !has_thumbhash {
        return 4;
    }

    // v5 present — check for v6 (tier tables).
    let has_micro_table: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='thumbnails_micro'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0)
        > 0;

    if !has_micro_table {
        return 5;
    }

    // v6 present — check for v7 (GPS columns).
    let has_gps = conn
        .execute("SELECT gps_lat FROM media_meta LIMIT 0", [])
        .is_ok();

    if !has_gps {
        return 6;
    }

    // v7 present — check for v8 (not_duplicates table).
    let has_not_duplicates: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='not_duplicates'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0)
        > 0;

    if !has_not_duplicates {
        return 7;
    }

    // v8 present — check for v9 (remote_devices table).
    let has_remote_devices: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='remote_devices'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0)
        > 0;

    if !has_remote_devices {
        return 8;
    }

    // v9 present — check for v10 (gif_atlas table).
    let has_gif_atlas: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='gif_atlas'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0)
        > 0;

    if !has_gif_atlas {
        return 9;
    }

    // Everything present — fully up to date.
    SCHEMA_VERSION
}

/// Run all pending migrations to bring the database up to `SCHEMA_VERSION`.
fn run_migrations(conn: &rusqlite::Connection) -> Result<(), CacheError> {
    let mut version = get_schema_version(conn);

    // Handle databases created before the migration system existed.
    if version == 0 {
        let detected = detect_legacy_version(conn);
        if detected == 0 {
            // Fresh database — create all base tables.
            conn.execute_batch(BASE_SCHEMA)?;
            version = 1;
        } else {
            // Existing database without a version stamp.
            version = detected;
        }
        set_schema_version(conn, version)?;
    }

    // Apply each migration whose version is greater than current.
    for migration in MIGRATIONS {
        if migration.version <= version {
            continue;
        }

        log::info!(
            "Running cache schema migration v{} → v{}",
            version,
            migration.version
        );

        // Execute each statement individually so that "duplicate column"
        // errors on partially-applied migrations don't abort the batch.
        for statement in migration.sql.split(';') {
            let trimmed = statement.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Err(e) = conn.execute_batch(trimmed) {
                // ALTER TABLE ADD COLUMN fails if the column already exists
                // (e.g. crash during a previous migration attempt). This is
                // expected and safe to ignore.
                let msg = e.to_string();
                if msg.contains("duplicate column") {
                    continue;
                }
                return Err(e.into());
            }
        }

        version = migration.version;
        set_schema_version(conn, version)?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// CacheDb
// ---------------------------------------------------------------------------

/// SQLite cache database for thumbnails, tag index, media metadata, and counts.
pub struct CacheDb {
    conn: rusqlite::Connection,
}

impl CacheDb {
    /// Open (or create) the cache database for a gallery.
    /// Creates the `.lightview/` directory if it doesn't exist.
    /// Runs any pending schema migrations automatically.
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

        // Ensure gallery_meta exists before anything else — migrations
        // need it to read/write the schema version.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS gallery_meta (
                key     TEXT PRIMARY KEY,
                value   TEXT NOT NULL
            );",
        )?;

        run_migrations(&conn)?;

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
