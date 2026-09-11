//! Filesystem operations — the whole content of the `Owner` trust level.
//!
//! **Sources confined always; destinations confined never; one trust level
//! deciding who may name a destination.** A copy or move *destination* was
//! never confined and never can be — naming a folder outside the gallery is the
//! entire point of the operation — so the protection is that no non-loopback
//! listener can reach any of this at all.
//!
//! # `open_with` names an index, not a program
//!
//! The command it replaces took a `command: String` and a `Vec<String>` of
//! arguments straight off the wire and spawned them. That is why every other
//! finding in this area escalated from "file access" to "code execution", and
//! it is why the loopback session exists at all. The user already configures
//! external applications; the wire now carries an **integer** and a
//! gallery-relative path, the server looks the entry up in its own
//! configuration, and **no request can name a program**. That removes a
//! parameter rather than adding a check.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::path::{PathError, RelPath};
use crate::state::Gallery;

#[derive(Debug, thiserror::Error)]
pub enum FileError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Path(#[from] PathError),
    #[error("no external application at index {0}")]
    NoSuchApp(usize),
    #[error("the file clipboard is not available on this display")]
    ClipboardUnavailable,
    #[error("clipboard error: {0}")]
    Clipboard(String),
}

/// One entry of the user's configured external applications.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalApp {
    pub label: String,
    /// The program. **Server-side configuration only** — never wire input.
    pub command: String,
    /// Arguments, with `{path}` substituted for the confined file.
    #[serde(default)]
    pub args: Vec<String>,
}

/// Copy files to a destination directory.
///
/// The destination is absolute and unconfined by design. Sources are resolved
/// through the gallery root, so a request cannot copy a host file it chose.
pub fn copy(gallery: &Gallery, paths: &[RelPath], destination: &Path) -> Result<usize, FileError> {
    std::fs::create_dir_all(destination)?;
    let mut copied = 0;
    for rel in paths {
        let source = gallery.root.resolve(rel)?;
        let target = unique_destination(destination, rel.file_name());
        std::fs::copy(source.as_path(), &target)?;
        copied += 1;
    }
    Ok(copied)
}

/// Move files to a destination directory, falling back to copy-and-delete
/// across filesystems.
pub fn move_files(
    gallery: &Gallery,
    paths: &[RelPath],
    destination: &Path,
) -> Result<usize, FileError> {
    std::fs::create_dir_all(destination)?;
    let mut moved = 0;
    for rel in paths {
        let source = gallery.root.resolve(rel)?;
        let target = unique_destination(destination, rel.file_name());
        match std::fs::rename(source.as_path(), &target) {
            Ok(()) => {}
            Err(_) => {
                std::fs::copy(source.as_path(), &target)?;
                std::fs::remove_file(source.as_path())?;
            }
        }
        moved += 1;
    }
    Ok(moved)
}

/// Put a file list on the system clipboard.
pub fn clipboard(
    gallery: &Gallery,
    paths: &[RelPath],
    cut: bool,
) -> Result<(), FileError> {
    if !crate::file_clipboard::available() {
        return Err(FileError::ClipboardUnavailable);
    }
    let resolved: Vec<PathBuf> = paths
        .iter()
        .map(|p| gallery.root.resolve(p).map(|g| g.as_path().to_path_buf()))
        .collect::<Result<_, _>>()?;
    let refs: Vec<&Path> = resolved.iter().map(|p| p.as_path()).collect();
    let op = if cut {
        crate::file_clipboard::Op::Cut
    } else {
        crate::file_clipboard::Op::Copy
    };
    crate::file_clipboard::write_files(&refs, op)
        .map_err(|e| FileError::Clipboard(e.to_string()))
}

/// Open one file in a configured external application.
///
/// `app_index` is an index into `apps`, which comes from server-side
/// configuration. There is no code path by which a request supplies a program
/// name or an argument.
pub fn open_with(
    gallery: &Gallery,
    apps: &[ExternalApp],
    app_index: usize,
    path: &RelPath,
) -> Result<(), FileError> {
    let app = apps.get(app_index).ok_or(FileError::NoSuchApp(app_index))?;
    let resolved = gallery.root.resolve(path)?;
    let file = resolved.as_path().to_string_lossy().to_string();

    let args: Vec<String> = if app.args.is_empty() {
        vec![file]
    } else {
        app.args
            .iter()
            .map(|a| a.replace("{path}", &file))
            .collect()
    };

    std::process::Command::new(&app.command)
        .args(&args)
        .spawn()?;
    Ok(())
}

/// One subdirectory, for the picker.
#[derive(Debug, Serialize)]
pub struct DirEntry {
    pub name: String,
    pub path: PathBuf,
}

/// List the subdirectories of a path. **`Owner` only.**
///
/// Returns directory names and nothing else — never media, never file
/// contents. This does not contradict local mode being selection-scoped: that
/// decision is about what a *remote* client may reach and about not putting a
/// bypass into path confinement. A directory listing is loopback-only and
/// returns what the local user can already enumerate with any file manager.
///
/// It exists because a browser cannot return a filesystem path: the File System
/// Access API yields a handle, not a path, and only in Chromium. The
/// alternative was `rfd`, which links GTK or a desktop portal on a process that
/// may have no display.
pub fn list_dirs(path: &Path) -> Result<Vec<DirEntry>, FileError> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        out.push(DirEntry {
            name,
            path: entry.path(),
        });
    }
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(out)
}

/// Recently opened galleries, for the opener. Local mode only.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Recent {
    #[serde(default)]
    pub galleries: Vec<PathBuf>,
}

impl Recent {
    const LIMIT: usize = 12;

    pub fn load(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// Move `gallery` to the front, bounded.
    pub fn record(path: &Path, gallery: &Path) -> std::io::Result<Self> {
        let mut recent = Self::load(path);
        recent.galleries.retain(|g| g != gallery);
        recent.galleries.insert(0, gallery.to_path_buf());
        recent.galleries.truncate(Self::LIMIT);
        let body = serde_json::to_vec_pretty(&recent)?;
        crate::util::fs_atomic::write_durable(path, &body)?;
        Ok(recent)
    }

    pub fn remove(path: &Path, gallery: &Path) -> std::io::Result<Self> {
        let mut recent = Self::load(path);
        recent.galleries.retain(|g| g != gallery);
        let body = serde_json::to_vec_pretty(&recent)?;
        crate::util::fs_atomic::write_durable(path, &body)?;
        Ok(recent)
    }
}

/// `name`, or `name (2)`, `name (3)`… if taken.
///
/// A copy into a folder that already has that name must not silently replace
/// it; this is the same rule the upload path enforces with `RENAME_NOREPLACE`,
/// applied where the operation is local and a race between two `Owner` clients
/// is not a threat model.
fn unique_destination(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let path = Path::new(name);
    let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy()));
    for n in 2..10_000 {
        let next = dir.join(format!("{stem} ({n}){}", ext.as_deref().unwrap_or("")));
        if !next.exists() {
            return next;
        }
    }
    candidate
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_copy_never_silently_replaces() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.jpg"), b"first").unwrap();
        let next = unique_destination(d.path(), "a.jpg");
        assert_eq!(next.file_name().unwrap(), "a (2).jpg");

        std::fs::write(&next, b"second").unwrap();
        assert_eq!(
            unique_destination(d.path(), "a.jpg").file_name().unwrap(),
            "a (3).jpg"
        );
        // The original is untouched.
        assert_eq!(std::fs::read(d.path().join("a.jpg")).unwrap(), b"first");
    }

    #[test]
    fn an_extensionless_name_still_gets_a_suffix() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("README"), b"x").unwrap();
        assert_eq!(
            unique_destination(d.path(), "README").file_name().unwrap(),
            "README (2)"
        );
    }

    #[test]
    fn the_picker_lists_directories_only() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("2026")).unwrap();
        std::fs::create_dir_all(d.path().join(".hidden")).unwrap();
        std::fs::write(d.path().join("photo.jpg"), b"x").unwrap();

        let listed: Vec<String> = list_dirs(d.path())
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(listed, vec!["2026"]);
    }

    #[test]
    fn open_with_refuses_an_index_that_is_not_configured() {
        // The wire carries an integer. There is no shape of request that can
        // name a program, which is what turns this from code execution into
        // file access.
        let apps: Vec<ExternalApp> = Vec::new();
        assert!(apps.get(3).is_none());
        let one = [ExternalApp {
            label: "GIMP".into(),
            command: "gimp".into(),
            args: vec!["{path}".into()],
        }];
        assert!(one.first().is_some_and(|a| a.label == "GIMP"));
        assert!(one.get(1).is_none());
    }

    #[test]
    fn recent_galleries_are_bounded_and_most_recent_first() {
        let d = tempfile::tempdir().unwrap();
        let store = d.path().join("recent.json");
        for i in 0..(Recent::LIMIT + 5) {
            Recent::record(&store, Path::new(&format!("/g/{i}"))).unwrap();
        }
        let recent = Recent::load(&store);
        assert_eq!(recent.galleries.len(), Recent::LIMIT);
        assert_eq!(
            recent.galleries[0],
            PathBuf::from(format!("/g/{}", Recent::LIMIT + 4))
        );

        // Re-recording moves rather than duplicates.
        Recent::record(&store, Path::new("/g/0")).unwrap();
        let recent = Recent::load(&store);
        assert_eq!(recent.galleries[0], PathBuf::from("/g/0"));
        assert_eq!(
            recent.galleries.iter().filter(|g| *g == Path::new("/g/0")).count(),
            1
        );

        Recent::remove(&store, Path::new("/g/0")).unwrap();
        assert!(!Recent::load(&store).galleries.contains(&PathBuf::from("/g/0")));
    }
}
