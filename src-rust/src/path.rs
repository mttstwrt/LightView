//! Path confinement as a type, not as a call.
//!
//! The dangerous paths in this system arrive as **command arguments**, not as
//! route captures, and the checks that guarded them were lexical or absent:
//! `strip_prefix` preserves `..` and returns `Ok`, so a "confined" trash move
//! deposited host files inside the gallery where the media route served them;
//! tag writes compared against nothing at all and would create a JSON file
//! anywhere on the host with attacker-chosen contents; and a plugin name was
//! `join`ed onto the plugin directory, where an absolute argument discards the
//! base entirely.
//!
//! So there are two types and the compiler decides which check ran.
//!
//! - [`RelPath`] is a wire path whose every component has been verified to be
//!   `Component::Normal` — no `..`, no root, no prefix. It is what the database
//!   is keyed on, and constructing one costs no syscall.
//! - [`GalleryPath`] is an absolute path that has been canonicalized and
//!   compared against a [`Root`] captured at open, and it is **the only type
//!   any function that opens a file accepts**.
//!
//! [`Root::resolve`] is the single place the canonicalize happens. A handler
//! that answers from the database holds a `RelPath` and never pays for a
//! `realpath` walk — which on a NAS mount is round trips rather than syscalls,
//! once per component, and the current thumbnail route pays it on every cell of
//! every scroll. A handler that is about to touch the filesystem cannot compile
//! without a `GalleryPath` and cannot obtain one without the check.
//!
//! This is fewer concepts, not more: scattered `path_in_gallery` calls collapse
//! into one constructor, and "sources confined always" becomes a property the
//! compiler enforces rather than a sentence in a doc comment.
//!
//! **Destinations are the deliberate exception, and the only one.** A copy or
//! move *destination* was never confined and never can be — that is the entire
//! content of the `Owner` trust level. Sources confined always; destinations
//! confined never; one trust level deciding who may name a destination.

use std::path::{Component, Path, PathBuf};

/// Why a path was refused.
///
/// Callers map every variant to **404, not 403**: a 403 confirms the existence
/// of files the caller has no business knowing about.
#[derive(Debug, thiserror::Error)]
pub enum PathError {
    /// A component was `..`, a root, a prefix, or empty.
    #[error("not a relative path within the gallery: {0}")]
    NotRelative(String),
    /// The resolved path landed outside the confinement root.
    #[error("path escapes the gallery root: {0}")]
    Escapes(String),
    /// The path, or the deepest existing part of it, could not be resolved.
    #[error("path could not be resolved: {0}")]
    Unresolvable(String),
}

/// A gallery-relative path, every component verified `Component::Normal`.
///
/// Stored and compared with forward slashes regardless of platform, because it
/// is a database key and a wire value before it is ever a filesystem path.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct RelPath(String);

impl RelPath {
    /// Validate a wire path.
    ///
    /// Every `/`-separated segment must be a real name: not empty, not `.`,
    /// not `..`. That rejects absolute paths (a leading `/` is an empty first
    /// segment), trailing and doubled slashes, and traversal — and it means
    /// **exactly one spelling reaches the database**, which matters because
    /// this is a primary key. `a/./b` and `a//b` name the same file as `a/b`
    /// and are refused rather than quietly rewritten, so there is no
    /// normalization step that could turn a rejected path into an accepted one.
    ///
    /// The component walk afterwards is belt and braces: it is what catches a
    /// platform-specific root or prefix that the segment rule does not know
    /// about.
    pub fn new(raw: &str) -> Result<Self, PathError> {
        if raw.is_empty() {
            return Err(PathError::NotRelative(raw.to_string()));
        }
        for segment in raw.split('/') {
            if segment.is_empty()
                || segment == "."
                || segment == ".."
                || segment.contains('\0')
            {
                return Err(PathError::NotRelative(raw.to_string()));
            }
        }
        if Path::new(raw)
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(PathError::NotRelative(raw.to_string()));
        }
        Ok(Self(raw.to_string()))
    }

    /// The canonical wire and database form.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The final component. Always present — a `RelPath` has at least one.
    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }

    /// Everything before the final component, if there is any.
    pub fn parent(&self) -> Option<RelPath> {
        self.0.rfind('/').map(|i| RelPath(self.0[..i].to_string()))
    }

    /// Extend by one already-trusted component.
    pub fn join(&self, name: &str) -> Result<Self, PathError> {
        RelPath::new(&format!("{}/{}", self.0, name))
    }

    /// Join onto an arbitrary base directory **without** confinement.
    ///
    /// Lexically safe — every component is `Normal`, so the result cannot climb
    /// out of `base` — but it performs no canonicalize, so it must not be used
    /// to open a file. It exists for the places that build a path to hand to
    /// [`Root::resolve`], and for the trash, whose entry directories are
    /// composed before they exist.
    pub fn to_path_under(&self, base: &Path) -> PathBuf {
        let mut p = base.to_path_buf();
        for part in self.0.split('/') {
            p.push(part);
        }
        p
    }
}

impl std::fmt::Display for RelPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for RelPath {
    /// Deserializing validates. A `RelPath` that exists is a `RelPath` that
    /// passed the check, including one that arrived in a request body.
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        RelPath::new(&raw).map_err(serde::de::Error::custom)
    }
}

/// A confinement root: an absolute, canonical directory.
///
/// The gallery root is one. The trash root is another — `.lightview/trash/` is
/// *inside* the gallery, so confining a restore against the gallery root would
/// pass a path that has escaped the trash. Same type, different instance, one
/// mechanism.
#[derive(Debug, Clone)]
pub struct Root {
    canonical: PathBuf,
}

impl Root {
    /// Canonicalize a directory once, at open.
    ///
    /// Everything downstream — the database's relative keys, the watcher's
    /// `strip_prefix`, the cache key — is derived from this one value, so they
    /// cannot disagree about what "the same gallery" is.
    pub fn open(dir: &Path) -> Result<Self, PathError> {
        let canonical = dir
            .canonicalize()
            .map_err(|_| PathError::Unresolvable(dir.display().to_string()))?;
        Ok(Self { canonical })
    }

    /// The canonical directory itself.
    pub fn as_path(&self) -> &Path {
        &self.canonical
    }

    /// A root nested inside this one, for a subtree with its own confinement —
    /// the trash. Fails if the subtree does not exist yet.
    pub fn nested(&self, rel: &RelPath) -> Result<Root, PathError> {
        Root::open(&rel.to_path_under(&self.canonical))
    }

    /// **The one place a path becomes openable.**
    ///
    /// Canonicalizes as deep as the filesystem goes and appends whatever does
    /// not exist yet, then requires the result to be inside this root.
    /// Resolving the existing prefix is what catches a symlink pointing out of
    /// the gallery; requiring every remaining component to be `Normal` — which
    /// [`RelPath`] already guarantees — is what stops the appended tail from
    /// climbing back out.
    ///
    /// The comparison is `Path::starts_with`, which is component-wise, so
    /// `/g/photos-secret` does not match `/g/photos`.
    pub fn resolve(&self, rel: &RelPath) -> Result<GalleryPath, PathError> {
        let target = rel.to_path_under(&self.canonical);

        // Walk up to the deepest ancestor that exists, canonicalize it, then
        // put back what was trimmed. A path that exists in full canonicalizes
        // in full, which is the common case and the strongest check.
        let mut trimmed: Vec<&std::ffi::OsStr> = Vec::new();
        let mut probe: &Path = &target;
        let resolved = loop {
            match probe.canonicalize() {
                Ok(p) => break p,
                Err(_) => {
                    let name = probe
                        .file_name()
                        .ok_or_else(|| PathError::Unresolvable(rel.0.clone()))?;
                    trimmed.push(name);
                    probe = probe
                        .parent()
                        .ok_or_else(|| PathError::Unresolvable(rel.0.clone()))?;
                }
            }
        };

        let mut full = resolved;
        for name in trimmed.iter().rev() {
            full.push(name);
        }

        if !full.starts_with(&self.canonical) {
            return Err(PathError::Escapes(rel.0.clone()));
        }
        Ok(GalleryPath(full))
    }

    /// The inverse: an absolute path the filesystem handed us becomes a
    /// database key, or is rejected as not belonging to this gallery.
    ///
    /// Used by the watcher, whose events carry absolute paths. A failure here
    /// is a `log::warn`, never a silent `continue` — on a watcher armed on the
    /// wrong root it fires for every event in the gallery and looks exactly
    /// like "nothing is happening".
    pub fn relativize(&self, abs: &Path) -> Result<RelPath, PathError> {
        let rest = abs
            .strip_prefix(&self.canonical)
            .map_err(|_| PathError::Escapes(abs.display().to_string()))?;
        RelPath::new(&rest.to_string_lossy())
    }
}

/// An absolute path that has been canonicalized and confined.
///
/// There is no constructor but [`Root::resolve`], and no way to build one from
/// a string. That is the point: a function taking a `GalleryPath` cannot be
/// called with an unchecked path, and a function taking a [`RelPath`] cannot
/// open a file with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GalleryPath(PathBuf);

impl GalleryPath {
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for GalleryPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relpath_rejects_traversal_and_absolutes() {
        for bad in [
            "../etc/passwd",
            "/etc/passwd",
            "a/../../b",
            "..",
            ".",
            "",
            "/",
        ] {
            assert!(
                RelPath::new(bad).is_err(),
                "{bad:?} should not be a RelPath"
            );
        }
    }

    #[test]
    fn relpath_accepts_ordinary_paths_in_exactly_one_spelling() {
        assert_eq!(RelPath::new("a/b/c.jpg").unwrap().as_str(), "a/b/c.jpg");
        // Redundant spellings of the same file are refused, not rewritten:
        // this value is a primary key, so one file must have one key, and a
        // silent rewrite is how a rejected input becomes an accepted one.
        for redundant in ["a/./b", "a//b", "a/b/", "./a"] {
            assert!(
                RelPath::new(redundant).is_err(),
                "{redundant:?} should not be a RelPath"
            );
        }
        assert_eq!(RelPath::new("photo.jpg").unwrap().file_name(), "photo.jpg");
        assert_eq!(
            RelPath::new("a/b/c.jpg").unwrap().parent().unwrap().as_str(),
            "a/b"
        );
        assert!(RelPath::new("photo.jpg").unwrap().parent().is_none());
    }

    #[test]
    fn strip_prefix_alone_would_have_passed_this() {
        // The check being replaced: Path::new("/g/../../etc/passwd")
        // .strip_prefix("/g") returns Ok("../../etc/passwd").
        let escaped = Path::new("/g/../../etc/passwd");
        assert!(escaped.strip_prefix("/g").is_ok());
        assert!(RelPath::new("../../etc/passwd").is_err());
    }

    #[test]
    fn resolve_confines_and_allows_a_not_yet_existing_leaf() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/a.jpg"), b"x").unwrap();
        let root = Root::open(dir.path()).unwrap();

        let existing = root.resolve(&RelPath::new("sub/a.jpg").unwrap()).unwrap();
        assert!(existing.as_path().ends_with("sub/a.jpg"));

        // An upload destination does not exist yet and must still resolve.
        let fresh = root.resolve(&RelPath::new("sub/new.jpg").unwrap()).unwrap();
        assert!(fresh.as_path().ends_with("sub/new.jpg"));
        assert!(fresh.as_path().starts_with(root.as_path()));
    }

    #[test]
    fn resolve_refuses_a_symlink_out_of_the_root() {
        let outer = tempfile::tempdir().unwrap();
        let gallery = outer.path().join("gallery");
        let secret = outer.path().join("secret");
        std::fs::create_dir_all(&gallery).unwrap();
        std::fs::create_dir_all(&secret).unwrap();
        std::fs::write(secret.join("k.txt"), b"s").unwrap();
        std::os::unix::fs::symlink(&secret, gallery.join("link")).unwrap();

        let root = Root::open(&gallery).unwrap();
        let err = root.resolve(&RelPath::new("link/k.txt").unwrap());
        assert!(matches!(err, Err(PathError::Escapes(_))));
    }

    #[test]
    fn sibling_prefix_is_not_inside_the_root() {
        // Component-wise comparison, not string prefix: /g/photos-secret must
        // not match /g/photos.
        let outer = tempfile::tempdir().unwrap();
        let photos = outer.path().join("photos");
        let secret = outer.path().join("photos-secret");
        std::fs::create_dir_all(&photos).unwrap();
        std::fs::create_dir_all(&secret).unwrap();
        std::os::unix::fs::symlink(&secret, photos.join("s")).unwrap();

        let root = Root::open(&photos).unwrap();
        assert!(matches!(
            root.resolve(&RelPath::new("s/x").unwrap()),
            Err(PathError::Escapes(_))
        ));
    }

    #[test]
    fn relativize_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("y/m")).unwrap();
        let root = Root::open(dir.path()).unwrap();
        let abs = root.as_path().join("y/m/p.jpg");
        assert_eq!(root.relativize(&abs).unwrap().as_str(), "y/m/p.jpg");
        assert!(root.relativize(Path::new("/elsewhere/p.jpg")).is_err());
    }
}
