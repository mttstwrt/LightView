//! `.lightview/trash/<epoch_ms>_<seq>/<gallery-relative path>`.
//!
//! The first segment is the deletion time plus a sequence number and is the
//! uniqueness key; everything after it is the original path. **There is no
//! metadata file** — the path inside the entry *is* the provenance, so purge is
//! a `read_dir`, a numeric parse and a `remove_dir_all` with no file reads at
//! all.
//!
//! Two things a bare path mirror could not do, which is why the timestamp
//! segment exists: mtime cannot carry the deletion time (a rename preserves it,
//! and preserving it is the point — restoring a file with a rewritten mtime is
//! silent data loss), and relative paths are not unique over time (trash,
//! restore, edit, trash again).
//!
//! # The entry id is not a path, and that is a security requirement
//!
//! Making the client-visible id `<epoch_ms>/<relative path>` — a string
//! containing slashes — creates an arbitrary-file-move primitive at `Device`
//! trust, because it *forces* the removal of the digits-and-underscores check
//! whose own comment names the attack. The chain: upload an `x.jpg` whose bytes
//! are a plugin manifest (the extension allowlist checks the name, not the
//! content), move it to trash, restore it to
//! `<ts>/../../../../tmp/evil/manifest.json`, and run a plugin by that name.
//! Even without the last step it is write-anywhere as the server user:
//! `~/.config/autostart/`, `~/.bashrc`, a systemd user unit.
//!
//! So the id stays opaque, the original location travels in its own field, and
//! the destination is rebuilt from validated parts. [`RelPath`] does the
//! component check and [`Root`] does the confinement — **against the trash
//! root, not the gallery root**, because `.lightview/trash/` is *inside* the
//! gallery and a gallery-root check passes a path that has escaped the trash.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::companion::reader::{companion_path, CompanionLocation};
use crate::path::{PathError, RelPath, Root};

/// Where the trash lives, relative to the gallery root.
const TRASH_DIR: &str = ".lightview/trash";

#[derive(Debug, thiserror::Error)]
pub enum TrashError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Path(#[from] PathError),
    #[error("not a trash entry id: {0}")]
    BadEntryId(String),
    #[error("no such trash entry")]
    NoSuchEntry,
    #[error("something already occupies {0}")]
    DestinationOccupied(String),
}

/// One entry, as a client sees it.
#[derive(Debug, Clone, Serialize)]
pub struct TrashEntry {
    /// `<epoch_ms>_<seq>`. **Digits and underscores only**, so it needs no
    /// percent-encoding and cannot name a path.
    pub id: String,
    /// Where the file came from, gallery-relative. A separate field, never part
    /// of the id.
    pub relative_path: RelPath,
    pub file_name: String,
    /// Unix milliseconds, parsed from the id.
    pub deleted_at: i64,
    pub size: u64,
}

/// Accept only an entry directory name.
///
/// Anything else — separators, `..`, a leading dot — is refused, so a client
/// cannot escape the trash directory. This is the check a path-shaped id would
/// have forced the removal of.
fn valid_entry_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit() || b == b'_')
}

fn entry_id_rel(id: &str) -> Result<RelPath, TrashError> {
    if !valid_entry_id(id) {
        return Err(TrashError::BadEntryId(id.to_string()));
    }
    RelPath::new(id).map_err(|_| TrashError::BadEntryId(id.to_string()))
}

/// The trash directory for a gallery, created if needed.
pub fn trash_root(gallery: &Root) -> Result<Root, TrashError> {
    let dir = gallery.as_path().join(TRASH_DIR);
    std::fs::create_dir_all(&dir)?;
    Ok(Root::open(&dir)?)
}

/// Reserve a fresh entry directory.
///
/// **The uniqueness suffix is reserved by `create_dir` failing with
/// `AlreadyExists` and retrying — an atomic `mkdir`.** That is why it also
/// holds for two *machines* trashing over the share in the same millisecond,
/// with no per-machine component in the name. Dropping the suffix, as an
/// earlier design did, would merge two deletes landing in the same millisecond
/// into one directory and silently break "one delete is one directory".
fn create_entry_dir(trash: &Path) -> Result<(String, PathBuf), TrashError> {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    for seq in 0..10_000u32 {
        let name = format!("{ms}_{seq}");
        let dir = trash.join(&name);
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok((name, dir)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(TrashError::Io(e)),
        }
    }
    Err(TrashError::Io(std::io::Error::other(
        "could not reserve a trash entry directory",
    )))
}

/// Move files into one new trash entry. Returns its id.
///
/// **Media first, companion second.** Reversed, a crash between the two moves
/// leaves the photo in the gallery with its ratings and tags in the trash.
/// State the order, because either one looks arbitrary until you name the
/// failure.
pub fn move_to_trash(gallery: &Root, paths: &[RelPath]) -> Result<String, TrashError> {
    let trash = trash_root(gallery)?;
    let (id, entry_dir) = create_entry_dir(trash.as_path())?;

    for rel in paths {
        let source = gallery.resolve(rel)?;
        let dest = rel.to_path_under(&entry_dir);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        move_file(source.as_path(), &dest)?;

        // The companion travels *alongside* the media inside the entry,
        // uniformly, whichever location it came from.
        let companion_src = companion_path(source.as_path(), CompanionLocation::LightviewFolder);
        if companion_src.exists() {
            let companion_dest = companion_path(&dest, CompanionLocation::Alongside);
            move_file(&companion_src, &companion_dest)?;
        }
    }
    Ok(id)
}

/// List every entry, newest first.
pub fn list(gallery: &Root) -> Result<Vec<TrashEntry>, TrashError> {
    let trash = gallery.as_path().join(TRASH_DIR);
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&trash) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(TrashError::Io(e)),
    };

    for entry in entries {
        let entry = entry?;
        let id = entry.file_name().to_string_lossy().to_string();
        if !entry.file_type()?.is_dir() || !valid_entry_id(&id) {
            continue;
        }
        let Some(deleted_at) = deletion_ms(&id) else {
            continue;
        };
        for (rel, size) in walk_entry(&entry.path())? {
            out.push(TrashEntry {
                file_name: rel.file_name().to_string(),
                relative_path: rel,
                id: id.clone(),
                deleted_at,
                size,
            });
        }
    }
    out.sort_by(|a, b| b.deleted_at.cmp(&a.deleted_at));
    Ok(out)
}

/// Restore one file to where it came from.
///
/// Refuses if something already occupies the destination — restoring over a
/// file the user has since put there would be a silent overwrite, and the trash
/// is the one place in the system that exists to prevent losses.
pub fn restore(
    gallery: &Root,
    id: &str,
    relative_path: &RelPath,
) -> Result<(), TrashError> {
    let trash = trash_root(gallery)?;
    let entry_rel = entry_id_rel(id)?;

    // Confined against the *trash* root, and only after both halves have been
    // validated as ordinary path components.
    let inside = entry_rel.as_str().to_string() + "/" + relative_path.as_str();
    let source = trash.resolve(&RelPath::new(&inside)?)?;
    if !source.as_path().exists() {
        return Err(TrashError::NoSuchEntry);
    }

    let dest = gallery.resolve(relative_path)?;
    if dest.as_path().exists() {
        return Err(TrashError::DestinationOccupied(
            relative_path.as_str().to_string(),
        ));
    }
    if let Some(parent) = dest.as_path().parent() {
        std::fs::create_dir_all(parent)?;
    }
    move_file(source.as_path(), dest.as_path())?;

    // **The companion goes to the current write location**, not alongside the
    // media where the trash entry keeps it. A naive path-mirroring restore
    // drops it beside the photo — where nothing reads it any more, since the
    // alongside read fallback is deleted — so it *appears* to work and the next
    // metadata write forks a second sidecar.
    let companion_src = companion_path(source.as_path(), CompanionLocation::Alongside);
    if companion_src.exists() {
        let companion_dest =
            companion_path(dest.as_path(), CompanionLocation::LightviewFolder);
        if let Some(parent) = companion_dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        move_file(&companion_src, &companion_dest)?;
    }

    prune_empty_dirs(source.as_path(), trash.as_path());
    Ok(())
}

/// Delete one entry outright. `Owner` only.
pub fn purge_entry(gallery: &Root, id: &str) -> Result<(), TrashError> {
    let trash = trash_root(gallery)?;
    let dir = trash.resolve(&entry_id_rel(id)?)?;
    match std::fs::remove_dir_all(dir.as_path()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(TrashError::NoSuchEntry),
        Err(e) => Err(TrashError::Io(e)),
    }
}

/// Remove entries older than the retention window.
///
/// Reads the deletion time out of the directory *name* — no file reads, no
/// metadata file, and nothing to keep in step.
pub fn auto_purge(gallery: &Root, retention_secs: i64) -> Result<usize, TrashError> {
    if retention_secs <= 0 {
        return Ok(0);
    }
    purge_older_than(gallery, (now_secs() - retention_secs) * 1000)
}

/// Empty the trash: every entry, whatever its age. **`Owner` only** — this is
/// the one place a person can destroy what the retention window was still
/// holding for them.
pub fn purge_all(gallery: &Root) -> Result<usize, TrashError> {
    purge_older_than(gallery, i64::MAX)
}

fn purge_older_than(gallery: &Root, cutoff_ms: i64) -> Result<usize, TrashError> {
    let trash = gallery.as_path().join(TRASH_DIR);
    let mut removed = 0;

    let entries = match std::fs::read_dir(&trash) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(TrashError::Io(e)),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if !entry.file_type()?.is_dir() || !valid_entry_id(&name) {
            continue;
        }
        let Some(ms) = deletion_ms(&name) else {
            continue;
        };
        if ms < cutoff_ms {
            std::fs::remove_dir_all(entry.path())?;
            removed += 1;
        }
    }
    Ok(removed)
}

/// The deletion time encoded in an entry id.
fn deletion_ms(id: &str) -> Option<i64> {
    id.split('_').next()?.parse().ok()
}

/// Every file inside one entry, as `(relative path, size)`.
fn walk_entry(dir: &Path) -> Result<Vec<(RelPath, u64)>, TrashError> {
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(dir).into_iter().flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        // Companions travel with their media; they are not separate entries.
        if name.ends_with(crate::companion::schema::COMPANION_EXTENSION) {
            continue;
        }
        let Ok(rest) = entry.path().strip_prefix(dir) else {
            continue;
        };
        let Ok(rel) = RelPath::new(&rest.to_string_lossy()) else {
            continue;
        };
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        out.push((rel, size));
    }
    Ok(out)
}

/// Rename, falling back to copy-and-delete across filesystems.
fn move_file(from: &Path, to: &Path) -> Result<(), TrashError> {
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        // EXDEV: the trash is inside the gallery, so this should not happen —
        // but a bind mount or an overlay inside the tree makes it possible, and
        // failing a delete because of one would be a surprising loss.
        Err(_) => {
            std::fs::copy(from, to)?;
            std::fs::remove_file(from)?;
            Ok(())
        }
    }
}

/// Remove now-empty directories between a restored file and the entry root.
fn prune_empty_dirs(from: &Path, stop_at: &Path) {
    let mut dir = from.parent().map(Path::to_path_buf);
    while let Some(d) = dir {
        if !d.starts_with(stop_at) || d == stop_at {
            break;
        }
        if std::fs::remove_dir(&d).is_err() {
            break;
        }
        dir = d.parent().map(Path::to_path_buf);
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gallery() -> (tempfile::TempDir, Root) {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("2026/january")).unwrap();
        std::fs::write(d.path().join("2026/january/a.jpg"), b"photo").unwrap();
        let companions = d.path().join("2026/january/.lightview/companions");
        std::fs::create_dir_all(&companions).unwrap();
        std::fs::write(companions.join("a.jpg.lightview.json"), b"{}").unwrap();
        let root = Root::open(d.path()).unwrap();
        (d, root)
    }

    #[test]
    fn a_nested_path_round_trips() {
        let (_d, root) = gallery();
        let rel = RelPath::new("2026/january/a.jpg").unwrap();

        let id = move_to_trash(&root, std::slice::from_ref(&rel)).unwrap();
        assert!(!root.as_path().join("2026/january/a.jpg").exists());

        let listed = list(&root).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, id);
        assert_eq!(listed[0].relative_path, rel);
        assert_eq!(listed[0].file_name, "a.jpg");

        restore(&root, &id, &rel).unwrap();
        assert_eq!(
            std::fs::read(root.as_path().join("2026/january/a.jpg")).unwrap(),
            b"photo"
        );
    }

    #[test]
    fn the_companion_is_restored_to_the_write_location_not_alongside() {
        // A path-mirroring restore drops it beside the photo, where nothing
        // reads it — so it looks like it worked and the next metadata write
        // forks a second sidecar.
        let (_d, root) = gallery();
        let rel = RelPath::new("2026/january/a.jpg").unwrap();
        let id = move_to_trash(&root, std::slice::from_ref(&rel)).unwrap();
        restore(&root, &id, &rel).unwrap();

        let written = root
            .as_path()
            .join("2026/january/.lightview/companions/a.jpg.lightview.json");
        assert!(written.exists(), "companion not at the write location");
        assert!(
            !root.as_path().join("2026/january/a.jpg.lightview.json").exists(),
            "companion left beside the media"
        );
    }

    #[test]
    fn a_restore_onto_an_occupied_destination_is_refused() {
        let (_d, root) = gallery();
        let rel = RelPath::new("2026/january/a.jpg").unwrap();
        let id = move_to_trash(&root, std::slice::from_ref(&rel)).unwrap();
        std::fs::write(root.as_path().join("2026/january/a.jpg"), b"newer").unwrap();

        assert!(matches!(
            restore(&root, &id, &rel),
            Err(TrashError::DestinationOccupied(_))
        ));
        assert_eq!(
            std::fs::read(root.as_path().join("2026/january/a.jpg")).unwrap(),
            b"newer"
        );
    }

    #[test]
    fn an_entry_id_cannot_name_a_path() {
        // The arbitrary-file-move primitive, closed at the front door.
        let (_d, root) = gallery();
        let rel = RelPath::new("2026/january/a.jpg").unwrap();
        for hostile in [
            "../../../tmp/evil",
            "/tmp/evil",
            "123/../..",
            "abc",
            "12-34",
            "",
        ] {
            assert!(
                matches!(
                    restore(&root, hostile, &rel),
                    Err(TrashError::BadEntryId(_))
                ),
                "{hostile:?} was accepted as an entry id"
            );
        }
    }

    #[test]
    fn a_relative_path_cannot_escape_the_trash_either() {
        // Both halves are validated. `RelPath` refuses the traversal before any
        // join happens, which is why there is nothing for the confinement check
        // to catch here — and why the confinement is against the trash root
        // rather than the gallery root when it does run.
        assert!(RelPath::new("../../../../tmp/evil/manifest.json").is_err());
        assert!(RelPath::new("../a.jpg").is_err());
    }

    #[test]
    fn two_deletes_in_the_same_millisecond_are_two_entries() {
        // The atomic mkdir. Without the sequence suffix they merge into one
        // directory and "one delete is one directory" breaks silently — and the
        // same reservation is what makes this hold across two machines.
        let (_d, root) = gallery();
        std::fs::write(root.as_path().join("b.jpg"), b"b").unwrap();
        std::fs::write(root.as_path().join("c.jpg"), b"c").unwrap();

        let a = move_to_trash(&root, &[RelPath::new("b.jpg").unwrap()]).unwrap();
        let b = move_to_trash(&root, &[RelPath::new("c.jpg").unwrap()]).unwrap();
        assert_ne!(a, b);
        assert_eq!(list(&root).unwrap().len(), 2);
    }

    #[test]
    fn purge_by_age_reads_only_the_directory_name() {
        let (_d, root) = gallery();
        let trash = trash_root(&root).unwrap();

        // One entry from long ago, one from now — planted by name alone, which
        // is the whole point: no metadata file to keep in step.
        let old = trash.as_path().join("1000000000000_0");
        std::fs::create_dir_all(old.join("2026")).unwrap();
        std::fs::write(old.join("2026/old.jpg"), b"x").unwrap();
        let fresh_id = move_to_trash(&root, &[RelPath::new("2026/january/a.jpg").unwrap()]).unwrap();

        let removed = auto_purge(&root, 24 * 3600).unwrap();
        assert_eq!(removed, 1);
        assert!(!old.exists());
        assert!(trash.as_path().join(&fresh_id).exists());
    }

    #[test]
    fn a_zero_retention_purges_nothing() {
        // Guard against a misread config wiping the trash on the next open.
        let (_d, root) = gallery();
        move_to_trash(&root, &[RelPath::new("2026/january/a.jpg").unwrap()]).unwrap();
        assert_eq!(auto_purge(&root, 0).unwrap(), 0);
        assert_eq!(list(&root).unwrap().len(), 1);
    }

    #[test]
    fn emptying_the_trash_takes_entries_the_retention_window_still_holds() {
        let (_d, root) = gallery();
        move_to_trash(&root, &[RelPath::new("2026/january/a.jpg").unwrap()]).unwrap();

        // A generous window keeps it; emptying deliberately does not care —
        // that difference is the whole reason the command exists.
        assert_eq!(auto_purge(&root, 365 * 24 * 3600).unwrap(), 0);
        assert_eq!(purge_all(&root).unwrap(), 1);
        assert!(list(&root).unwrap().is_empty());
    }

    #[test]
    fn purging_one_entry_leaves_the_others() {
        let (_d, root) = gallery();
        std::fs::write(root.as_path().join("b.jpg"), b"b").unwrap();
        let keep = move_to_trash(&root, &[RelPath::new("2026/january/a.jpg").unwrap()]).unwrap();
        let drop = move_to_trash(&root, &[RelPath::new("b.jpg").unwrap()]).unwrap();

        purge_entry(&root, &drop).unwrap();
        let ids: Vec<String> = list(&root).unwrap().into_iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![keep]);
    }
}
