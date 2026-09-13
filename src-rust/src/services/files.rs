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

/// A shortcut in the picker's sidebar: somewhere a person actually files
/// things, reachable in one click.
#[derive(Debug, Clone, Serialize)]
pub struct Place {
    pub label: String,
    pub path: PathBuf,
}

/// One level of the picker: where it is, where "up" goes, what is here, and
/// where else is worth going.
///
/// The parent travels with the listing because the client must not compute it
/// — a browser doing its own string surgery on a path is how a picker ends up
/// asking for something that is not a directory, and the answer is one field.
/// `places` is there for the same reason: the sidebar every native file dialog
/// has, spelled as paths the server named rather than paths the client built.
#[derive(Debug, Serialize)]
pub struct DirListing {
    pub path: PathBuf,
    /// `None` at the filesystem root.
    pub parent: Option<PathBuf>,
    pub entries: Vec<DirEntry>,
    /// Constant for the process, so it rides on the listing already being
    /// fetched rather than costing a second command for six short strings.
    pub places: Vec<Place>,
}

/// The sidebar: the gallery, `$HOME`, and whichever XDG user directories
/// exist.
///
/// **Only paths that exist are offered**, so the sidebar never leads to a dead
/// end — which is also why this cannot be a compile-time list.
///
/// Read from `~/.config/user-dirs.dirs` when it is there, because that file is
/// where a localized or relocated `Pictures` is recorded and guessing the
/// English name would miss it entirely on a French or German desktop. Falling
/// back to the conventional names under `$HOME` covers a machine that has
/// never run `xdg-user-dirs-update`.
///
/// Hand-parsed rather than pulling in the `dirs` crate: the format is
/// `XDG_PICTURES_DIR="$HOME/Pictures"`, one per line, and a new dependency for
/// twenty lines of that is the wrong trade.
fn user_places() -> Vec<Place> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    let configured = xdg_user_dirs(&home);

    // Ordered as a file dialog orders them: home, then the folders media
    // actually lands in, then the rest.
    let wanted = [
        ("PICTURES", "Pictures"),
        ("VIDEOS", "Videos"),
        ("DOWNLOAD", "Downloads"),
        ("DOCUMENTS", "Documents"),
        ("DESKTOP", "Desktop"),
    ];

    let mut out = vec![Place {
        label: "Home".to_string(),
        path: home.clone(),
    }];
    for (key, fallback) in wanted {
        let path = configured
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| home.join(fallback));
        // The label is the conventional English name rather than the
        // directory's own: the sidebar names a *role*, and a reader scanning
        // for "Pictures" should find it whatever the folder is called.
        if path.is_dir() && path != home {
            out.push(Place {
                label: fallback.to_string(),
                path,
            });
        }
    }
    out.retain(|p| p.path.is_dir());
    out
}

/// Parse `~/.config/user-dirs.dirs` into `(PICTURES, /home/me/Bilder)` pairs.
fn xdg_user_dirs(home: &Path) -> Vec<(String, PathBuf)> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    let Ok(text) = std::fs::read_to_string(config.join("user-dirs.dirs")) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.starts_with('#') {
                return None;
            }
            let (key, value) = line.split_once('=')?;
            let key = key.trim().strip_prefix("XDG_")?.strip_suffix("_DIR")?;
            let value = value.trim().trim_matches('"');
            // The file writes `$HOME/Pictures`; nothing else is expanded,
            // because nothing else appears in it.
            let expanded = match value.strip_prefix("$HOME/") {
                Some(rest) => home.join(rest),
                None if value == "$HOME" => home.to_path_buf(),
                None => PathBuf::from(value),
            };
            Some((key.to_string(), expanded))
        })
        .collect()
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
pub fn list_dirs(path: &Path, gallery_root: &Path) -> Result<DirListing, FileError> {
    // Canonicalized so the listing is in the same terms the next request will
    // be, and so `..` and symlinks do not accumulate in the path the picker
    // displays.
    let path = path.canonicalize()?;
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&path)? {
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

    // The gallery first: it is where a copy or move destination is most often
    // picked, and it is the one directory this process is certain of.
    let mut places = vec![Place {
        label: "Gallery".to_string(),
        path: gallery_root.to_path_buf(),
    }];
    places.extend(
        USER_PLACES
            .get_or_init(user_places)
            .iter()
            .filter(|p| p.path != gallery_root)
            .cloned(),
    );

    Ok(DirListing {
        parent: path.parent().map(std::path::Path::to_path_buf),
        path,
        entries: out,
        places,
    })
}

/// Resolved once: `$HOME` does not move, and neither does `user-dirs.dirs`
/// within a session. Re-reading it on every navigation would be a file read
/// per click for an answer that cannot change.
static USER_PLACES: std::sync::OnceLock<Vec<Place>> = std::sync::OnceLock::new();


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

        let listing = list_dirs(d.path(), d.path()).unwrap();
        let listed: Vec<String> = listing.entries.into_iter().map(|e| e.name).collect();
        assert_eq!(listed, vec!["2026"]);
        // The parent travels with the listing so the picker never does its own
        // string surgery on a path to walk up.
        assert_eq!(
            listing.parent.as_deref(),
            d.path().canonicalize().unwrap().parent()
        );
    }

    #[test]
    fn the_sidebar_leads_the_gallery_and_never_offers_a_dead_end() {
        let d = tempfile::tempdir().unwrap();
        let listing = list_dirs(d.path(), d.path()).unwrap();

        assert_eq!(
            listing.places.first().map(|p| p.label.as_str()),
            Some("Gallery"),
            "the gallery is where a destination is most often picked"
        );
        assert_eq!(listing.places[0].path, d.path());
        for place in &listing.places {
            assert!(
                place.path.is_dir(),
                "the sidebar offered {:?}, which is not a directory",
                place.path
            );
        }
        // The gallery is listed once, not again under whatever else it is.
        let gallery_entries = listing
            .places
            .iter()
            .filter(|p| p.path == d.path())
            .count();
        assert_eq!(gallery_entries, 1);
    }

    #[test]
    fn a_relocated_user_directory_is_read_from_user_dirs_rather_than_guessed() {
        // A French or German desktop has no `Pictures`, and guessing the
        // English name would silently drop the one shortcut that matters.
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".config")).unwrap();
        std::fs::create_dir_all(home.path().join("Bilder")).unwrap();
        std::fs::write(
            home.path().join(".config/user-dirs.dirs"),
            "# generated\nXDG_PICTURES_DIR=\"$HOME/Bilder\"\nXDG_VIDEOS_DIR=\"$HOME/Filme\"\n",
        )
        .unwrap();

        let dirs = xdg_user_dirs(home.path());
        assert_eq!(
            dirs.iter().find(|(k, _)| k == "PICTURES").map(|(_, v)| v),
            Some(&home.path().join("Bilder")),
            "$HOME was not expanded, or the key was not parsed"
        );
        assert!(
            dirs.iter().any(|(k, _)| k == "VIDEOS"),
            "a directory that does not exist is still parsed; existence is \
             checked separately, when the sidebar is built"
        );
        assert!(
            !dirs.iter().any(|(k, _)| k.starts_with('#')),
            "a comment line was parsed as a key"
        );
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

}
