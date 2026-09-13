//! Locating and parsing a companion file.
//!
//! **One location, read and write:** `<dir>/.lightview/companions/`, per
//! directory, beside the media. Per directory rather than one tree at the
//! gallery root, because that is where they already are — a root-level layout
//! would orphan every companion outside the top directory on first open.
//!
//! The two-location resolution this used to do is gone. `LightviewFolder` has
//! been the default since this module entered the tree, so **no build in this
//! repository's history ever wrote an alongside sidecar**: the fallback was
//! defensive code for a condition the application cannot produce, and it cost
//! an entry on a permanent exception ledger. What deleting it costs, stated
//! plainly: a `photo.jpg.lightview.json` placed by hand or by another tool is
//! ignored. It is not destroyed — writes go to `.lightview/companions/`, so a
//! stray file simply sits there.
//!
//! The alongside *form* survives as a path constructor, because a trash entry
//! stores the companion next to the media inside it, uniformly, whichever
//! location it came from.
//!
//! [`crate::companion::migration::migrate`] runs on every parse, which is what
//! makes adding a schema version a change to one function rather than an audit
//! of every caller.

use std::path::{Path, PathBuf};

use crate::companion::migration::migrate;
use crate::companion::schema::{CompanionFile, COMPANION_EXTENSION, CURRENT_SCHEMA_VERSION};
use crate::util::lock::FileLock;

#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Unsupported schema version: {0} (current: {1})")]
    UnsupportedVersion(u32, u32),
}

/// Which of the two path shapes a companion takes.
///
/// Not a setting — the `companion_location` preference is deleted. It had a UI
/// control and never reached a write, since every path used the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompanionLocation {
    /// `<dir>/.lightview/companions/<name>.lightview.json` — where companions
    /// live.
    #[default]
    LightviewFolder,
    /// `<media>.lightview.json` — the form used inside a trash entry, where the
    /// companion travels beside the file it belongs to.
    Alongside,
}

/// The companion path for a media file in one of the two shapes.
pub fn companion_path(media_path: &Path, location: CompanionLocation) -> PathBuf {
    match location {
        CompanionLocation::Alongside => {
            let mut p = media_path.as_os_str().to_owned();
            p.push(COMPANION_EXTENSION);
            PathBuf::from(p)
        }
        CompanionLocation::LightviewFolder => {
            companions_dir(media_path).join(companion_file_name(media_path))
        }
    }
}

/// The `.lightview/companions/` directory beside a media file.
pub fn companions_dir(media_path: &Path) -> PathBuf {
    media_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(".lightview")
        .join("companions")
}

/// The lock file guarding every companion in one directory.
///
/// A separate file, never a companion itself: a write ends in a rename that
/// replaces the companion's inode, so a lock on the old inode covers nothing
/// after the swap. One per directory, because companions are per directory
/// already and the contention being serialized is between two machines.
pub fn lock_path(media_path: &Path) -> PathBuf {
    companions_dir(media_path).join(".lock")
}

fn companion_file_name(media_path: &Path) -> std::ffi::OsString {
    media_path
        .file_name()
        .map(|n| {
            let mut s = n.to_owned();
            s.push(COMPANION_EXTENSION);
            s
        })
        .unwrap_or_else(|| std::ffi::OsString::from("unknown.lightview.json"))
}

/// Read a companion if one exists. No lock — for display reads and the index
/// sweep, where the rule is that **absence is never a deletion**.
///
/// Over `cifs` a rename that replaces an existing target unlinks it first, so a
/// reader that does not take the lock can observe the companion absent for an
/// instant. Writers are serialized, so this reaches only lock-free readers, and
/// they must never translate a missing companion into removing index rows. Rows
/// go away when the *media* file goes away.
pub fn read_companion(media_path: &Path) -> Result<Option<CompanionFile>, ReadError> {
    let path = companion_path(media_path, CompanionLocation::LightviewFolder);
    match std::fs::read_to_string(&path) {
        Ok(contents) => parse_companion(&contents).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ReadError::Io(e)),
    }
}

/// Read a companion **under the directory lock**.
///
/// This is what a decision about durable state is made from. A tagger's skip
/// predicate resolved against the derived index is a *plan*; resolved against
/// the file under the lock it is an answer, and the difference is whether the
/// design is correct for one tagging machine or for any number of them.
///
/// Takes and releases the lock itself, so it never nests with
/// [`crate::companion::writer::modify_companion`], which takes the same one.
pub fn read_companion_locked(media_path: &Path) -> Result<Option<CompanionFile>, ReadError> {
    let _guard = FileLock::acquire(&lock_path(media_path))?;
    read_companion(media_path)
}

/// Parse companion JSON, validating the schema version and running migrations.
pub fn parse_companion(json: &str) -> Result<CompanionFile, ReadError> {
    let companion: CompanionFile = serde_json::from_str(json)?;

    if companion.schema_version > CURRENT_SCHEMA_VERSION {
        return Err(ReadError::UnsupportedVersion(
            companion.schema_version,
            CURRENT_SCHEMA_VERSION,
        ));
    }

    Ok(migrate(companion))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn companion_path_alongside() {
        let p = companion_path(Path::new("/photos/sunset.jpg"), CompanionLocation::Alongside);
        assert_eq!(p, PathBuf::from("/photos/sunset.jpg.lightview.json"));
    }

    #[test]
    fn companion_path_is_per_directory() {
        let p = companion_path(
            Path::new("/photos/2026/january/sunset.jpg"),
            CompanionLocation::LightviewFolder,
        );
        assert_eq!(
            p,
            PathBuf::from(
                "/photos/2026/january/.lightview/companions/sunset.jpg.lightview.json"
            )
        );
    }

    #[test]
    fn lock_is_a_sibling_of_the_companions_not_one_of_them() {
        assert_eq!(
            lock_path(Path::new("/photos/a.jpg")),
            PathBuf::from("/photos/.lightview/companions/.lock")
        );
    }

    #[test]
    fn missing_companion_reads_as_none_not_an_error() {
        let d = tempfile::tempdir().unwrap();
        let media = d.path().join("a.jpg");
        std::fs::write(&media, b"x").unwrap();
        assert!(read_companion(&media).unwrap().is_none());
    }

    #[test]
    fn a_future_schema_version_is_refused() {
        let json = r#"{ "schema_version": 999, "file": "t.jpg" }"#;
        assert!(matches!(
            parse_companion(json),
            Err(ReadError::UnsupportedVersion(999, _))
        ));
    }
}
