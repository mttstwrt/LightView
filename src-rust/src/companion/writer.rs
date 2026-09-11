//! Replacing a companion file: one read-modify-write, under one lock.
//!
//! **The lock is the point.** Both writers used to do a whole-file read →
//! mutate → serialize with no lock at all, so a rating set from the phone was
//! silently gone if the desktop's plugin run had read that companion a moment
//! earlier — and over a network mount "a moment" is the client's attribute
//! cache, one second on `cifs` by default. The losing write is not the older
//! one; it is whichever reader lost the race. So the whole operation happens
//! inside [`modify_companion`], and there is no public way to write a companion
//! without it.
//!
//! The lock is `fcntl` on a per-directory lock file, for reasons spelled out in
//! [`crate::util::lock`]: `flock` would be coherent on one machine and
//! decorative across the share.
//!
//! The write itself is atomic **and durable** — see
//! [`crate::util::fs_atomic::write_durable`]. Atomic alone was the old
//! behaviour and it is not enough for a file on a NAS.
//!
//! `modified` is stamped here rather than by the caller, so every write carries
//! an accurate timestamp regardless of which path produced it.

use std::path::Path;

use crate::companion::reader::{
    companion_path, lock_path, read_companion, CompanionLocation, ReadError,
};
use crate::companion::schema::{CompanionFile, MediaType};
use crate::util::fs_atomic::write_durable;
use crate::util::lock::FileLock;

#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("companion read failed: {0}")]
    Read(#[from] ReadError),
}

/// What a mutation decided, once it had seen the companion under the lock.
///
/// `Leave` exists because the authoritative skip check happens *here*: a
/// tagging run that finds a newer result already in the file must not overwrite
/// it, and it can only know that after taking the lock and reading. Returning
/// `Leave` is a successful outcome, not a failure.
pub enum Outcome<T> {
    Write(T),
    Leave(T),
}

impl<T> Outcome<T> {
    fn into_inner(self) -> (T, bool) {
        match self {
            Outcome::Write(v) => (v, true),
            Outcome::Leave(v) => (v, false),
        }
    }
}

/// Read, mutate and write a companion as a single locked operation.
///
/// Creates the companion if it does not exist. The closure sees the current
/// contents — the ones on disk right now, not the ones some earlier read
/// cached — and says whether its changes should be written.
///
/// This replaces every ad-hoc read-modify-write in the system. "Last writer
/// wins, no corruption, no merge" is then true per *operation*, which is the
/// claim that was always meant.
pub fn modify_companion<T>(
    media_path: &Path,
    media_type: MediaType,
    f: impl FnOnce(&mut CompanionFile) -> Outcome<T>,
) -> Result<T, WriteError> {
    let _guard = FileLock::acquire(&lock_path(media_path))?;

    let mut companion = match read_companion(media_path)? {
        Some(c) => c,
        None => {
            let filename = media_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown");
            CompanionFile::new(filename, media_type)
        }
    };

    let (value, should_write) = f(&mut companion).into_inner();
    if !should_write {
        return Ok(value);
    }

    companion.modified = chrono::Utc::now().to_rfc3339();
    let json = serde_json::to_string_pretty(&companion)?;
    write_durable(
        &companion_path(media_path, CompanionLocation::LightviewFolder),
        json.as_bytes(),
    )?;
    Ok(value)
}

/// Write a companion to an explicit path, with no lock and no read.
///
/// The one caller is the trash, which deposits a companion *alongside* the
/// media inside an entry directory it has just created and nobody else can
/// reach. Everything touching a live companion goes through
/// [`modify_companion`].
pub fn write_companion_to(path: &Path, companion: &CompanionFile) -> Result<(), WriteError> {
    let json = serde_json::to_string_pretty(companion)?;
    write_durable(path, json.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn media(dir: &Path) -> std::path::PathBuf {
        let p = dir.join("a.jpg");
        std::fs::write(&p, b"x").unwrap();
        p
    }

    #[test]
    fn creates_then_updates_in_place() {
        let d = tempfile::tempdir().unwrap();
        let m = media(d.path());

        modify_companion(&m, MediaType::Image, |c| {
            c.tags.user.push("vacation".into());
            Outcome::Write(())
        })
        .unwrap();

        modify_companion(&m, MediaType::Image, |c| {
            c.tags.set.push("kellys-comic".into());
            Outcome::Write(())
        })
        .unwrap();

        let read = read_companion(&m).unwrap().unwrap();
        assert_eq!(read.tags.user, vec!["vacation"]);
        assert_eq!(read.tags.set, vec!["kellys-comic"]);
    }

    #[test]
    fn leave_writes_nothing() {
        let d = tempfile::tempdir().unwrap();
        let m = media(d.path());
        modify_companion(&m, MediaType::Image, |c| {
            c.tags.user.push("kept".into());
            Outcome::Write(())
        })
        .unwrap();

        let before = std::fs::read_to_string(companion_path(&m, CompanionLocation::LightviewFolder))
            .unwrap();

        let decided = modify_companion(&m, MediaType::Image, |c| {
            c.tags.user.push("discarded".into());
            Outcome::Leave("skipped")
        })
        .unwrap();

        assert_eq!(decided, "skipped");
        let after = std::fs::read_to_string(companion_path(&m, CompanionLocation::LightviewFolder))
            .unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn the_closure_sees_disk_state_not_a_stale_copy() {
        // The race being closed: a reader that took its copy earlier must not
        // be able to write it back over a newer one.
        let d = tempfile::tempdir().unwrap();
        let m = media(d.path());

        modify_companion(&m, MediaType::Image, |c| {
            c.meta.core = Some(crate::companion::schema::CoreMeta {
                rating: Some(5),
                ..Default::default()
            });
            Outcome::Write(())
        })
        .unwrap();

        modify_companion(&m, MediaType::Image, |c| {
            assert_eq!(c.meta.core.as_ref().unwrap().rating, Some(5));
            c.tags.user.push("later".into());
            Outcome::Write(())
        })
        .unwrap();

        let read = read_companion(&m).unwrap().unwrap();
        assert_eq!(read.meta.core.unwrap().rating, Some(5));
        assert_eq!(read.tags.user, vec!["later"]);
    }

    #[test]
    fn an_unmodelled_key_survives_a_modification() {
        let d = tempfile::tempdir().unwrap();
        let m = media(d.path());
        let path = companion_path(&m, CompanionLocation::LightviewFolder);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"schema_version":1,"file":"a.jpg","file_hash":"","media_type":"image",
                "created":"","modified":"",
                "tags":{"user":[],"auto":["indoor"],"plugins":{}},
                "meta":{"core":null,"plugins":{}}}"#,
        )
        .unwrap();

        modify_companion(&m, MediaType::Image, |c| {
            c.tags.user.push("v".into());
            Outcome::Write(())
        })
        .unwrap();

        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["tags"]["auto"], serde_json::json!(["indoor"]));
        assert_eq!(raw["tags"]["user"], serde_json::json!(["v"]));
    }
}
