//! Device-to-host photo uploads for the LAN web interface.
//!
//! The web client is otherwise read-only: it can browse and edit per-item
//! metadata, but never write files or read arbitrary host paths. Uploads are
//! the one exception — a deliberate, one-way channel that drops a phone's
//! photos into the open gallery. They never expand what a device can *read*.
//!
//! Files are organised into subfolders (year blocks by default) so the
//! gallery's top level doesn't fill with thousands of items. The scheme is
//! host-configured per gallery and stored in `gallery_meta` alongside the
//! remote password — see [`get_config`]/[`set_config`].
//!
//! Once a file lands under the gallery root, the recursive fs-watcher
//! (`commands::gallery::start_fs_watcher`) ingests it automatically: thumbnail,
//! index, UI refresh. This module's job ends at writing the bytes safely.
//!
//! Safety is enforced at three layers, since the filename comes from an
//! untrusted device: the name is reduced to its basename and traversal is
//! rejected ([`sanitize_component`]), the extension must be a known media type
//! (checked by the caller via `MediaType::from_extension`), and the resolved
//! destination is confirmed to sit inside the gallery root before any write
//! ([`confine`]).

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

/// Base directory under the gallery root that all uploads live in, keeping
/// device-pushed files out of the user's hand-curated top level. Not
/// host-configurable today; a single well-known folder is easy to reason about.
pub const UPLOAD_SUBDIR: &str = "Uploads";

const META_ENABLED: &str = "upload.enabled";
const META_SCHEME: &str = "upload.scheme";

/// How uploaded files are foldered under `Uploads/`. The capture date (EXIF,
/// falling back to upload time) drives the year/month segments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UploadScheme {
    /// `Uploads/2026/`
    Year,
    /// `Uploads/2026/06/`
    YearMonth,
    /// `Uploads/2026/<album>/` — album typed by the device, optional.
    YearAlbum,
    /// `Uploads/` — no date subfolders.
    Flat,
}

impl UploadScheme {
    pub fn as_str(&self) -> &'static str {
        match self {
            UploadScheme::Year => "year",
            UploadScheme::YearMonth => "year_month",
            UploadScheme::YearAlbum => "year_album",
            UploadScheme::Flat => "flat",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "year" => Some(UploadScheme::Year),
            "year_month" => Some(UploadScheme::YearMonth),
            "year_album" => Some(UploadScheme::YearAlbum),
            "flat" => Some(UploadScheme::Flat),
            _ => None,
        }
    }
}

/// Per-gallery upload settings, surfaced to both the desktop config UI and the
/// web client (so it knows whether to show the upload button and album field).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadConfig {
    pub enabled: bool,
    pub scheme: UploadScheme,
}

impl Default for UploadConfig {
    fn default() -> Self {
        // Default ON: once a host sets up remote access and pairs a device,
        // uploads work without a second opt-in. The host can still switch them
        // off per gallery.
        Self {
            enabled: true,
            scheme: UploadScheme::Year,
        }
    }
}

/// Read the upload config from `gallery_meta`, falling back to defaults for any
/// unset key (a gallery that predates this feature has neither row).
pub fn get_config(conn: &Connection) -> Result<UploadConfig, rusqlite::Error> {
    let default = UploadConfig::default();
    let enabled = meta(conn, META_ENABLED)?
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(default.enabled);
    let scheme = meta(conn, META_SCHEME)?
        .and_then(|v| UploadScheme::from_str(&v))
        .unwrap_or(default.scheme);
    Ok(UploadConfig { enabled, scheme })
}

/// Persist the upload config to `gallery_meta`.
pub fn set_config(conn: &Connection, cfg: &UploadConfig) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT OR REPLACE INTO gallery_meta (key, value) VALUES (?1, ?2)",
        params![META_ENABLED, if cfg.enabled { "1" } else { "0" }],
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO gallery_meta (key, value) VALUES (?1, ?2)",
        params![META_SCHEME, cfg.scheme.as_str()],
    )?;
    Ok(())
}

fn meta(conn: &Connection, key: &str) -> Result<Option<String>, rusqlite::Error> {
    conn.query_row(
        "SELECT value FROM gallery_meta WHERE key = ?1",
        params![key],
        |row| row.get(0),
    )
    .optional()
}

// ─────────────────────────────────────────────────────────────
// Path construction (untrusted input)
// ─────────────────────────────────────────────────────────────

/// Reduce an untrusted filename (or album name) to a single safe path
/// component: strip any directory parts, reject `.`/`..`/empty, and replace
/// characters that are awkward across filesystems. Returns `None` when nothing
/// usable remains, so the caller can substitute a generated name.
pub fn sanitize_component(raw: &str) -> Option<String> {
    // `file_name` discards everything up to and including the last separator,
    // so `../../etc/passwd` becomes `passwd` and `a/b` becomes `b`.
    let base = Path::new(raw.trim()).file_name()?.to_string_lossy();
    let cleaned: String = base
        .chars()
        .map(|c| match c {
            // Reserved on Windows / awkward in shells; collapse to underscore.
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').trim();
    if trimmed.is_empty() {
        return None;
    }
    // Cap length so a hostile client can't craft pathological names.
    Some(trimmed.chars().take(180).collect())
}

/// Build the gallery-relative directory for an upload under `Uploads/`,
/// according to `scheme`. Pure (no IO); the caller joins it onto the gallery
/// root and creates it.
pub fn relative_dir(
    scheme: UploadScheme,
    year: i32,
    month: u32,
    album: Option<&str>,
) -> PathBuf {
    let mut dir = PathBuf::from(UPLOAD_SUBDIR);
    match scheme {
        UploadScheme::Flat => {}
        UploadScheme::Year => {
            dir.push(year.to_string());
        }
        UploadScheme::YearMonth => {
            dir.push(year.to_string());
            dir.push(format!("{:02}", month));
        }
        UploadScheme::YearAlbum => {
            dir.push(year.to_string());
            // Album is already sanitized to one component by the caller; only
            // append when present so an empty album degrades to plain Year.
            if let Some(album) = album.and_then(sanitize_component) {
                dir.push(album);
            }
        }
    }
    dir
}

/// Append ` (n)` before the extension until the path doesn't exist, so two
/// phones uploading `IMG_0001.jpg` in the same minute don't clobber each other.
pub fn dedupe_path(dir: &Path, file_name: &str) -> PathBuf {
    let candidate = dir.join(file_name);
    if !candidate.exists() {
        return candidate;
    }
    let path = Path::new(file_name);
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = path.extension().map(|e| e.to_string_lossy().into_owned());
    for n in 1.. {
        let name = match &ext {
            Some(ext) => format!("{stem} ({n}).{ext}"),
            None => format!("{stem} ({n})"),
        };
        let candidate = dir.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!("counter exhausted")
}

/// Confirm `dest` lies inside `gallery_root` (defense in depth: even though we
/// build the path ourselves, a symlink under the gallery or a future bug
/// shouldn't be able to write outside it). Works lexically on a normalized
/// path so it doesn't require the file to exist yet.
pub fn confine(gallery_root: &Path, dest: &Path) -> bool {
    let root = normalize(gallery_root);
    let target = normalize(dest);
    target.starts_with(&root)
}

/// Lexically resolve `.`/`..` without touching the filesystem. Not symlink-safe
/// on its own; paired with [`confine`] plus our own controlled segments it's a
/// sufficient lexical guard.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn open_db() -> (TempDir, crate::cache::db::CacheDb) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = crate::cache::db::CacheDb::open(dir.path()).expect("open cache db");
        (dir, db)
    }

    #[test]
    fn config_round_trips_and_defaults() {
        let (_dir, db) = open_db();
        let conn = db.conn();
        // Unset → defaults (enabled, year).
        let cfg = get_config(conn).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.scheme, UploadScheme::Year);

        set_config(
            conn,
            &UploadConfig {
                enabled: false,
                scheme: UploadScheme::YearMonth,
            },
        )
        .unwrap();
        let cfg = get_config(conn).unwrap();
        assert!(!cfg.enabled);
        assert_eq!(cfg.scheme, UploadScheme::YearMonth);
    }

    #[test]
    fn sanitize_strips_traversal_and_dirs() {
        assert_eq!(sanitize_component("../../etc/passwd").as_deref(), Some("passwd"));
        assert_eq!(sanitize_component("a/b/c.jpg").as_deref(), Some("c.jpg"));
        assert_eq!(sanitize_component("IMG_0001.jpg").as_deref(), Some("IMG_0001.jpg"));
        assert_eq!(sanitize_component("evil:name?.png").as_deref(), Some("evil_name_.png"));
        assert_eq!(sanitize_component(".."), None);
        assert_eq!(sanitize_component("."), None);
        assert_eq!(sanitize_component("   "), None);
        assert_eq!(sanitize_component(""), None);
    }

    #[test]
    fn relative_dir_per_scheme() {
        assert_eq!(
            relative_dir(UploadScheme::Year, 2026, 6, None),
            PathBuf::from("Uploads/2026")
        );
        assert_eq!(
            relative_dir(UploadScheme::YearMonth, 2026, 6, None),
            PathBuf::from("Uploads/2026/06")
        );
        assert_eq!(
            relative_dir(UploadScheme::YearAlbum, 2026, 6, Some("Trip to Rome")),
            PathBuf::from("Uploads/2026/Trip to Rome")
        );
        // Album with traversal is reduced to its basename.
        assert_eq!(
            relative_dir(UploadScheme::YearAlbum, 2026, 6, Some("../secret")),
            PathBuf::from("Uploads/2026/secret")
        );
        // Empty album degrades to plain year.
        assert_eq!(
            relative_dir(UploadScheme::YearAlbum, 2026, 6, Some("  ")),
            PathBuf::from("Uploads/2026")
        );
        assert_eq!(
            relative_dir(UploadScheme::Flat, 2026, 6, None),
            PathBuf::from("Uploads")
        );
    }

    #[test]
    fn confine_accepts_inside_rejects_outside() {
        let root = Path::new("/galleries/photos");
        assert!(confine(root, Path::new("/galleries/photos/Uploads/2026/a.jpg")));
        assert!(!confine(root, Path::new("/galleries/other/a.jpg")));
        assert!(!confine(
            root,
            Path::new("/galleries/photos/Uploads/../../escape.jpg")
        ));
    }

    #[test]
    fn dedupe_avoids_collisions() {
        let dir = tempfile::tempdir().unwrap();
        let p1 = dedupe_path(dir.path(), "a.jpg");
        assert_eq!(p1.file_name().unwrap(), "a.jpg");
        std::fs::write(&p1, b"x").unwrap();
        let p2 = dedupe_path(dir.path(), "a.jpg");
        assert_eq!(p2.file_name().unwrap(), "a (1).jpg");
    }
}
