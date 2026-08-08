//! Connection setup, the migration list, and the maintenance operations that
//! span every table.
//!
//! Two rules here are load-bearing and easy to break silently. First,
//! `SCHEMA_VERSION` is *derived* from `MIGRATIONS` rather than maintained
//! alongside it — they drifted once, and the drift skipped migrations. Second,
//! [`path_keyed_tables`] is the single source of truth for "which tables are
//! keyed by a media path", so every operation that removes or relocates a file
//! iterates it instead of listing tables itself.

use std::path::Path;

use crate::cache::thumbnails::ThumbTier;

/// Every table keyed by a media file's absolute `path` column, thumbnail tiers
/// included. One list, derived from [`ThumbTier::ALL`], so a new tier is picked
/// up automatically by every path-keyed maintenance operation (per-file delete,
/// gallery rebase, stale-row pruning) instead of each repeating the set — which
/// is how tiers previously got left behind as orphaned rows.
///
/// `not_duplicates` is deliberately absent: its paths live in `path_a`/`path_b`,
/// so callers handle it separately.
pub fn path_keyed_tables() -> impl Iterator<Item = &'static str> {
    ["media_meta", "tag_index", "index_state", "gif_atlas"]
        .into_iter()
        .chain(ThumbTier::ALL.into_iter().map(|t| t.table()))
}

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

/// Current schema version — the highest version in [`MIGRATIONS`].
///
/// Derived rather than hand-maintained: the two drifted once already (the
/// constant sat at 14 while a v15 migration existed), and the consumer at the
/// time used it as "this database is fully up to date" — which silently
/// *skipped* migrations. A `const fn` keeps them welded together at compile
/// time.
const SCHEMA_VERSION: u32 = latest_migration_version();

const fn latest_migration_version() -> u32 {
    // Base schema is v1; every entry in MIGRATIONS moves it forward from there.
    let mut latest = 1;
    let mut i = 0;
    while i < MIGRATIONS.len() {
        if MIGRATIONS[i].version > latest {
            latest = MIGRATIONS[i].version;
        }
        i += 1;
    }
    latest
}

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
    // v15: last-access timestamps for the two capped justified tiers, so
    // eviction can drop genuinely cold rows instead of the oldest-inserted
    // ones. Under the previous rowid FIFO, scrolling back over cells you were
    // just looking at could miss: they were *inserted* early, which is what
    // rowid order encodes. Backfilled to the current time so existing rows
    // aren't all treated as equally ancient on the first pass after upgrade.
    Migration {
        version: 15,
        sql: "
            ALTER TABLE thumbnails_justified_mid ADD COLUMN accessed_at INTEGER NOT NULL DEFAULT 0;
            ALTER TABLE thumbnails_justified_high ADD COLUMN accessed_at INTEGER NOT NULL DEFAULT 0;
            UPDATE thumbnails_justified_mid SET accessed_at = strftime('%s','now') WHERE accessed_at = 0;
            UPDATE thumbnails_justified_high SET accessed_at = strftime('%s','now') WHERE accessed_at = 0;
            CREATE INDEX IF NOT EXISTS idx_jm_accessed ON thumbnails_justified_mid(accessed_at);
            CREATE INDEX IF NOT EXISTS idx_jh_accessed ON thumbnails_justified_high(accessed_at);
        ",
    },
    // `file_hashes` was created by the base schema for duplicate detection and
    // never written to: perceptual hashes ended up on `thumbnails.phash`
    // instead, so they are discarded and recomputed with the thumbnail they
    // describe. The empty table is harmless but its name is not — it reads as
    // the home of dedup state, which is somewhere else entirely.
    Migration {
        version: 16,
        sql: "
            DROP TABLE IF EXISTS file_hashes;
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

/// Only the migration tests need this now — the version-0 branch no longer
/// probes the schema, it just applies the idempotent base and re-runs.
#[cfg(test)]
fn has_table(conn: &rusqlite::Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
        rusqlite::params![name],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0)
        > 0
}

/// Run all pending migrations to bring the database up to `SCHEMA_VERSION`.
fn run_migrations(conn: &rusqlite::Connection) -> Result<(), CacheError> {
    let mut version = get_schema_version(conn);

    // No stamp: either a brand-new file, or one predating the versioning
    // scheme. Both are handled the same way — apply the base schema (every
    // statement is `IF NOT EXISTS`, so it is a no-op on a populated database),
    // claim v1, and let the loop below re-run every migration.
    //
    // This replaced a ten-rung feature-detection ladder that tried to *guess*
    // an unstamped database's version. Guessing is all downside here: the loop
    // skips everything at or below the version it is handed, so an
    // over-estimate silently leaves tables uncreated — which is exactly the bug
    // decision 0003 was written about. Under-estimating costs one idempotent
    // re-run of migrations that mostly do nothing, once, on a database that
    // almost certainly does not exist.
    if version == 0 {
        conn.execute_batch(BASE_SCHEMA)?;
        version = 1;
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

    // Post-condition. The loop is driven by MIGRATIONS, so the only way to
    // land short is a database stamped *ahead* of this build (opened by a
    // newer LightView, then downgraded). Say so loudly: the tables a newer
    // version added are still there, but this build's queries were written
    // against an older shape, so anything odd afterwards starts here.
    if version != SCHEMA_VERSION {
        log::warn!(
            "Cache schema is v{version}, but this build expects v{SCHEMA_VERSION} — \
             the cache was likely written by a newer LightView"
        );
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
        // Keep temp B-trees (ORDER BY, IN-list materialization) in RAM rather
        // than spilling to a disk temp file.
        conn.execute_batch("PRAGMA temp_store=MEMORY;")?;
        // Memory-map the DB (256MB) so hot thumbnail-blob reads come straight
        // from a mapped page instead of read() syscalls + buffer copies.
        conn.execute_batch("PRAGMA mmap_size=268435456;")?;

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

    /// Drop every row keyed by a media path: metadata, tags, index state,
    /// hashes, dedup decisions, GIF atlases, and every thumbnail LOD tier.
    /// Used when a file leaves the gallery (trash, move out, fs-watch
    /// removal) so no row is left orphaned.
    pub fn remove_media_rows(&self, path: &str) {
        for table in path_keyed_tables() {
            let _ = self.conn.execute(
                &format!("DELETE FROM {table} WHERE path = ?1"),
                rusqlite::params![path],
            );
        }
        let _ = self.conn.execute(
            "DELETE FROM not_duplicates WHERE path_a = ?1 OR path_b = ?1",
            rusqlite::params![path],
        );
    }

    /// Rewrite the gallery-root prefix of every path-keyed row after a gallery
    /// directory moves (new mount point, renamed folder, copied to another
    /// machine). Carries thumbnails, ratings, view history, tag index, file
    /// hashes, and dedup decisions to the new root without regeneration.
    ///
    /// `OR REPLACE`: a bare row may already exist under the new root (the
    /// gallery was opened there before this rebase ran) — the relocated row
    /// wins, since it carries the accumulated history.
    ///
    /// Returns the number of rows rewritten across all tables.
    pub fn rebase_root(&self, old_root: &str, new_root: &str) -> Result<u64, CacheError> {
        let old_root = old_root.trim_end_matches('/');
        let new_root = new_root.trim_end_matches('/');
        if old_root == new_root || old_root.is_empty() {
            return Ok(0);
        }

        let tx = self.conn.unchecked_transaction()?;
        let mut rewritten = 0u64;
        // Match on `old_root || '/'` so a sibling like `/data/photos2` is never
        // rewritten when the old root is `/data/photos`.
        for table in path_keyed_tables() {
            rewritten += tx.execute(
                &format!(
                    "UPDATE OR REPLACE {table}
                     SET path = ?2 || substr(path, length(?1) + 1)
                     WHERE substr(path, 1, length(?1) + 1) = ?1 || '/'"
                ),
                rusqlite::params![old_root, new_root],
            )? as u64;
        }
        for col in ["path_a", "path_b"] {
            rewritten += tx.execute(
                &format!(
                    "UPDATE OR REPLACE not_duplicates
                     SET {col} = ?2 || substr({col}, length(?1) + 1)
                     WHERE substr({col}, 1, length(?1) + 1) = ?1 || '/'"
                ),
                rusqlite::params![old_root, new_root],
            )? as u64;
        }
        tx.commit()?;
        Ok(rewritten)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_db() -> (tempfile::TempDir, CacheDb) {
        let tmp = tempfile::tempdir().unwrap();
        let db = CacheDb::open(tmp.path()).unwrap();
        (tmp, db)
    }

    /// A freshly opened database must land on the newest migration, and the
    /// migration list must be strictly increasing — an out-of-order or
    /// duplicated version would be silently skipped by the `<= version`
    /// guard in `run_migrations`.
    #[test]
    fn fresh_db_reaches_the_latest_migration() {
        let (_tmp, db) = open_db();
        assert_eq!(get_schema_version(db.conn()), SCHEMA_VERSION);

        let mut prev = 1;
        for migration in MIGRATIONS {
            assert!(
                migration.version > prev,
                "migration v{} is not after v{prev}",
                migration.version
            );
            prev = migration.version;
        }
        assert_eq!(SCHEMA_VERSION, prev, "SCHEMA_VERSION must track MIGRATIONS");
    }

    /// Re-running every migration over an already-current database must be a
    /// no-op, since that is exactly what an under-reporting legacy detection
    /// (or a crash mid-migration) causes.
    #[test]
    fn migrations_are_idempotent() {
        let (_tmp, db) = open_db();
        set_schema_version(db.conn(), 1).unwrap();
        run_migrations(db.conn()).expect("re-running migrations must succeed");
        assert_eq!(get_schema_version(db.conn()), SCHEMA_VERSION);
        for tier in ThumbTier::ALL {
            assert!(has_table(db.conn(), tier.table()), "{} missing", tier.table());
        }
    }

    #[test]
    fn rebase_root_rewrites_all_path_tables() {
        let (_tmp, db) = open_db();
        db.conn()
            .execute_batch(
                "INSERT INTO media_meta (path, file_size, media_type, rating) VALUES
                     ('/old/root/a.jpg', 1, 'image', 4),
                     ('/old/root/sub/b.jpg', 1, 'image', 2),
                     ('/old/rootling/c.jpg', 1, 'image', 5);
                 INSERT INTO tag_index (path, namespace, tag) VALUES
                     ('/old/root/a.jpg', 'user', 'cat');
                 INSERT INTO not_duplicates (path_a, path_b) VALUES
                     ('/old/root/a.jpg', '/old/root/sub/b.jpg');
                 INSERT INTO thumbnails (path, media_type, mtime, width, height, thumbnail)
                     VALUES ('/old/root/a.jpg', 'image', 1, 8, 8, x'00');",
            )
            .unwrap();

        // 2 media_meta + 1 tag_index + 1 thumbnail + 2 not_duplicates columns
        let n = db.rebase_root("/old/root", "/new/spot").unwrap();
        assert_eq!(n, 6);

        let rating: u8 = db
            .conn()
            .query_row(
                "SELECT rating FROM media_meta WHERE path = '/new/spot/sub/b.jpg'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rating, 2);

        // Sibling directory sharing the prefix string must be untouched.
        let untouched: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM media_meta WHERE path = '/old/rootling/c.jpg'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(untouched, 1);

        let tag_path: String = db
            .conn()
            .query_row("SELECT path FROM tag_index WHERE tag = 'cat'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(tag_path, "/new/spot/a.jpg");

        let pair: (String, String) = db
            .conn()
            .query_row("SELECT path_a, path_b FROM not_duplicates", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(pair.0, "/new/spot/a.jpg");
        assert_eq!(pair.1, "/new/spot/sub/b.jpg");
    }

    #[test]
    fn rebase_root_replaces_bare_rows_under_new_root() {
        let (_tmp, db) = open_db();
        // The gallery was already opened at the new root once, so a bare row
        // (no rating) exists alongside the history-carrying old row.
        db.conn()
            .execute_batch(
                "INSERT INTO media_meta (path, file_size, media_type, rating) VALUES
                     ('/old/root/a.jpg', 1, 'image', 5),
                     ('/new/spot/a.jpg', 1, 'image', NULL);",
            )
            .unwrap();

        db.rebase_root("/old/root", "/new/spot").unwrap();

        let (count, rating): (i64, Option<u8>) = db
            .conn()
            .query_row(
                "SELECT COUNT(*), MAX(rating) FROM media_meta WHERE path = '/new/spot/a.jpg'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(rating, Some(5));
    }

    /// Every path-keyed table must be swept, thumbnail tiers included — the
    /// reason the table list is derived from `ThumbTier::ALL` rather than
    /// spelled out per call site. A tier missing from the sweep leaves a
    /// multi-megabyte blob keyed to a file that no longer exists.
    #[test]
    fn remove_media_rows_clears_every_path_keyed_table() {
        let (_tmp, db) = open_db();
        db.conn()
            .execute_batch(
                "INSERT INTO media_meta (path, file_size, media_type) VALUES ('/g/a.jpg', 1, 'image');
                 INSERT INTO tag_index (path, namespace, tag) VALUES ('/g/a.jpg', 'user', 'cat');
                 INSERT INTO index_state (path, companion_mtime) VALUES ('/g/a.jpg', 42);
                 INSERT INTO gif_atlas (path, tier, mtime, frame_count, frame_w, frame_h, cols, delays, atlas)
                     VALUES ('/g/a.jpg', 'm', 1, 1, 8, 8, 1, '100', x'00');
                 INSERT INTO not_duplicates (path_a, path_b) VALUES ('/g/a.jpg', '/g/b.jpg');
                 INSERT INTO not_duplicates (path_a, path_b) VALUES ('/g/b.jpg', '/g/c.jpg');",
            )
            .unwrap();
        // A cached thumbnail in every LOD tier.
        for tier in ThumbTier::ALL {
            db.conn()
                .execute(
                    &format!(
                        "INSERT INTO {} (path, media_type, mtime, width, height, thumbnail)
                         VALUES ('/g/a.jpg', 'image', 1, 8, 8, x'00')",
                        tier.table()
                    ),
                    [],
                )
                .unwrap();
        }

        db.remove_media_rows("/g/a.jpg");

        for table in path_keyed_tables() {
            let left: i64 = db
                .conn()
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE path = '/g/a.jpg'"),
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(left, 0, "{table} row not removed");
        }
        // The pair involving the removed file goes; the unrelated pair stays.
        let pairs: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM not_duplicates", [], |r| r.get(0))
            .unwrap();
        assert_eq!(pairs, 1);
    }

    #[test]
    fn rebase_root_noops_on_same_or_empty_root() {
        let (_tmp, db) = open_db();
        db.conn()
            .execute(
                "INSERT INTO media_meta (path, file_size, media_type) VALUES ('/g/a.jpg', 1, 'image')",
                [],
            )
            .unwrap();
        assert_eq!(db.rebase_root("/g", "/g").unwrap(), 0);
        assert_eq!(db.rebase_root("", "/x").unwrap(), 0);
        assert_eq!(db.rebase_root("/g/", "/g").unwrap(), 0); // trailing slash normalized
    }
}
