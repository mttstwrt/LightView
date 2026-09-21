//! The one write channel from a device.
//!
//! Five things are load-bearing here, and every one of them was a real defect
//! in what this replaces.
//!
//! 1. **The filename is reduced to a basename and the extension must resolve to
//!    a known media type.** The allowlist admits no `.json`, `.svg` or `.html`,
//!    so a paired device cannot land a companion file, anything inside
//!    `.lightview/`, or a script-bearing type served from the gallery's own
//!    origin.
//! 2. **The destination directory is canonicalized *after* `create_dir_all` and
//!    compared against the root.** The check this replaces was lexical — its own
//!    doc comment said "not symlink-safe on its own" — and it checked only the
//!    directory, before creating it, never the final destination. Point
//!    `Uploads/` at a bigger disk with a symlink, an ordinary thing to do, and
//!    every upload landed outside the root while the check returned true.
//!    Nothing outside the root is indexed or served, so it failed *silently*.
//! 3. **A temp file in the destination directory, with the mtime stamped
//!    before the rename.** Without the temp file the watcher fires `Create` on
//!    an empty file and the `INSERT OR IGNORE` records `file_size = 0` and
//!    `date_taken = <upload time>` — and because it is `INSERT OR IGNORE`,
//!    nothing ever corrects them. The thumbnail self-heals; the metadata does
//!    not, so size sort, `size>=10mb` and date sort are permanently wrong for
//!    that file. The capture time is read from the staged file by `commit`
//!    itself: it used to be a parameter, and the one caller passed `None` on
//!    every upload, so this paragraph described something that was not
//!    happening.
//! 4. **`RENAME_NOREPLACE`.** The dedupe loop checked `!candidate.exists()` and
//!    then renamed unconditionally; between those two steps, two phones
//!    uploading `IMG_0001.jpg` clobber each other — the exact thing the loop's
//!    comment claims it prevents.
//! 5. **The temp file is removed on *every* error path.** Only a failed rename
//!    used to trigger cleanup, so an `ENOSPC` or a dropped connection left
//!    `.lv-upload-*.tmp` behind permanently — and it has no media extension, so
//!    neither the scan nor the watcher will ever see it. Invisible litter, in
//!    the one tree the design tells the user is safe to `grep` and `rsync`.
//!
//! And **uploads are bounded**, which they never were. Each part used to be
//! held in RAM in full, with unbounded parts per request and unbounded
//! concurrent requests: a paired phone could OOM the NAS with a handful of
//! parallel POSTs. Each part streams to the temp file, the part count is
//! capped, and a disk below the margin refuses.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::companion::schema::MediaType;
use crate::path::{GalleryPath, RelPath, Root};

/// Parts per request. A phone's share sheet sends a handful; a hundred is
/// generous and a thousand is an attack.
pub const MAX_PARTS: usize = 100;

/// Refuse when the destination filesystem has less than this free.
pub const FREE_SPACE_MARGIN: u64 = 512 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum UploadError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Path(#[from] crate::path::PathError),
    #[error("unsupported file type")]
    UnsupportedType,
    #[error("invalid file name")]
    BadName,
    #[error("too many files in one request")]
    TooManyParts,
    #[error("not enough free space")]
    NoSpace,
}

/// Reduce a client-supplied name to a safe basename with a known media
/// extension.
///
/// Fails closed on any divergence between the extension read from the raw name
/// and the one on the sanitized name — that divergence is how a crafted name
/// gets one type past the check and lands as another.
pub fn sanitize_name(raw: &str) -> Result<String, UploadError> {
    // Take the basename under both separator conventions, because a Windows
    // client will happily send `C:\Users\me\x.jpg`.
    let base = raw
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(raw)
        .trim()
        .trim_matches('.');
    if base.is_empty() || base == "." || base == ".." {
        return Err(UploadError::BadName);
    }

    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | '\0') {
                '_'
            } else {
                c
            }
        })
        .collect();

    let raw_ext = Path::new(base)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase());
    let clean_ext = Path::new(&cleaned)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase());
    if raw_ext != clean_ext {
        return Err(UploadError::BadName);
    }

    match clean_ext.as_deref().and_then(MediaType::from_extension) {
        Some(_) => Ok(cleaned),
        None => Err(UploadError::UnsupportedType),
    }
}

/// A temp file in the destination directory, cleaned up on every error path.
///
/// The `Drop` is what makes point 5 above true regardless of which `?` returns
/// first.
pub struct StagedUpload {
    temp: PathBuf,
    file: Option<std::fs::File>,
    committed: bool,
}

impl StagedUpload {
    /// Open a temp file beside where the upload will land.
    ///
    /// Dot-prefixed and with no media extension, which is the only reason the
    /// watcher and the scan do not see it.
    pub fn create(dir: &Path) -> Result<Self, UploadError> {
        if free_bytes(dir).is_some_and(|free| free < FREE_SPACE_MARGIN) {
            return Err(UploadError::NoSpace);
        }
        let temp = dir.join(format!(".lv-upload-{}.tmp", uuid::Uuid::new_v4()));
        let file = std::fs::File::create(&temp)?;
        Ok(Self {
            temp,
            file: Some(file),
            committed: false,
        })
    }

    /// Append a chunk. Streaming, so a 4 GB video never sits in RAM.
    pub fn write(&mut self, chunk: &[u8]) -> Result<(), UploadError> {
        if let Some(file) = self.file.as_mut() {
            file.write_all(chunk)?;
        }
        Ok(())
    }

    /// Flush, stamp the mtime from the photo's own capture time, and rename
    /// into place.
    ///
    /// The mtime is stamped **before** the rename, or the indexer records the
    /// upload time and every uploaded photo sorts as "today" forever.
    ///
    /// **The capture time is read here rather than passed in**, because this is
    /// the only place that has the flushed bytes and a path to read them from.
    /// The parameter it replaces was the bug: the one caller passed `None` on
    /// every upload, so nothing was ever stamped and the module's own doc
    /// comment described a fix that was not running. A photo with no EXIF date
    /// — and every video — keeps the upload time, which is the best available
    /// answer rather than a guess.
    pub fn commit(mut self, destination_dir: &Path, name: &str) -> Result<PathBuf, UploadError> {
        if let Some(mut file) = self.file.take() {
            file.flush()?;
            file.sync_all()?;
        }
        if let Some(taken) = crate::pipeline::exif::read(&self.temp).date_taken {
            let stamp = filetime::FileTime::from_unix_time(taken, 0);
            filetime::set_file_mtime(&self.temp, stamp)?;
        }

        // Dedupe, then rename with RENAME_NOREPLACE so the check and the rename
        // cannot be raced apart. On a collision we pick the next name and try
        // again rather than failing the upload.
        let mut candidate = destination_dir.join(name);
        for attempt in 0..1000 {
            match crate::util::fs_atomic::rename_noreplace(&self.temp, &candidate) {
                Ok(()) => {
                    self.committed = true;
                    crate::util::fs_atomic::sync_dir(destination_dir)?;
                    return Ok(candidate);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    candidate = destination_dir.join(dedupe_name(name, attempt + 2));
                }
                Err(e) => return Err(UploadError::Io(e)),
            }
        }
        Err(UploadError::Io(std::io::Error::other(
            "could not find a free name for the upload",
        )))
    }
}

impl Drop for StagedUpload {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.temp);
        }
    }
}

/// Resolve and create the upload directory, then confine it.
///
/// **After** `create_dir_all`, so a directory that did not exist yet is
/// canonicalized as it actually is rather than as it was asked for.
pub fn upload_dir(root: &Root, configured: &str) -> Result<GalleryPath, UploadError> {
    let rel = RelPath::new(configured)?;
    let dir = rel.to_path_under(root.as_path());
    std::fs::create_dir_all(&dir)?;
    Ok(root.resolve(&rel)?)
}

fn dedupe_name(name: &str, n: usize) -> String {
    let path = Path::new(name);
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{stem} ({n}).{ext}"),
        None => format!("{stem} ({n})"),
    }
}

fn free_bytes(path: &Path) -> Option<u64> {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    disks
        .list()
        .iter()
        .filter(|d| path.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map(|d| d.available_space())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_traversal_in_a_filename_becomes_a_basename() {
        assert_eq!(sanitize_name("../../etc/passwd.jpg").unwrap(), "passwd.jpg");
        assert_eq!(sanitize_name("/tmp/evil.png").unwrap(), "evil.png");
        assert_eq!(sanitize_name("C:\\Users\\me\\x.jpg").unwrap(), "x.jpg");
    }

    #[test]
    fn only_known_media_extensions_land() {
        // The allowlist is the upload check: no companion files, nothing inside
        // .lightview, and no script-bearing type from the gallery's origin.
        for hostile in [
            "a.lightview.json",
            "payload.svg",
            "page.html",
            "script.js",
            "manifest.json",
            "noextension",
        ] {
            assert!(
                sanitize_name(hostile).is_err(),
                "{hostile:?} was accepted"
            );
        }
        for ok in ["a.jpg", "B.JPEG", "clip.MOV", "anim.gif", "shot.heic"] {
            assert!(sanitize_name(ok).is_ok(), "{ok:?} was refused");
        }
    }

    #[test]
    fn a_name_whose_extension_changes_under_sanitizing_is_refused() {
        // The divergence is how a crafted name gets one type past the check and
        // lands as another.
        assert!(sanitize_name("evil.jpg\u{0}.html").is_err());
        assert!(sanitize_name("").is_err());
        assert!(sanitize_name("...").is_err());
    }

    #[test]
    fn a_staged_upload_leaves_nothing_behind_when_it_is_abandoned() {
        let d = tempfile::tempdir().unwrap();
        {
            let mut staged = StagedUpload::create(d.path()).unwrap();
            staged.write(b"partial").unwrap();
            // Dropped without commit — a dropped connection, an ENOSPC, a `?`.
        }
        let leftovers: Vec<_> = std::fs::read_dir(d.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert!(leftovers.is_empty(), "leaked: {leftovers:?}");
    }

    #[test]
    fn a_commit_renames_into_place_and_stamps_the_capture_time() {
        let d = tempfile::tempdir().unwrap();
        let jpeg =
            crate::pipeline::exif::tests_support::jpeg_with_exif(Some("2020:09:13 12:26:40"), None);
        let mut staged = StagedUpload::create(d.path()).unwrap();
        staged.write(&jpeg).unwrap();
        let landed = staged.commit(d.path(), "a.jpg").unwrap();

        assert_eq!(std::fs::read(&landed).unwrap(), jpeg);
        let mtime = filetime::FileTime::from_last_modification_time(
            &std::fs::metadata(&landed).unwrap(),
        );
        assert_eq!(
            mtime.unix_seconds(),
            1_600_000_000,
            "the uploaded file should carry its own capture time, not the upload time"
        );
        // And no temp file survived.
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 1);
    }

    /// The regression that shipped: `commit` took the capture time as a
    /// parameter and its one caller always passed `None`, so an upload filed
    /// under "today" forever and the module doc above described a fix that was
    /// not running.
    #[test]
    fn an_upload_with_no_capture_time_keeps_the_upload_time() {
        let d = tempfile::tempdir().unwrap();
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let mut staged = StagedUpload::create(d.path()).unwrap();
        staged.write(b"no exif here at all").unwrap();
        let landed = staged.commit(d.path(), "a.jpg").unwrap();

        let mtime = filetime::FileTime::from_last_modification_time(
            &std::fs::metadata(&landed).unwrap(),
        );
        assert!(
            mtime.unix_seconds() >= before - 5,
            "a file with no capture time should keep the upload time"
        );
    }

    #[test]
    fn two_uploads_of_one_name_both_survive() {
        // The race the check-then-rename shape lost: both find the name free,
        // and the second silently replaces the first.
        let d = tempfile::tempdir().unwrap();
        let mut first = StagedUpload::create(d.path()).unwrap();
        first.write(b"first").unwrap();
        first.commit(d.path(), "IMG_0001.jpg").unwrap();

        let mut second = StagedUpload::create(d.path()).unwrap();
        second.write(b"second").unwrap();
        let landed = second.commit(d.path(), "IMG_0001.jpg").unwrap();

        assert_eq!(landed.file_name().unwrap(), "IMG_0001 (2).jpg");
        assert_eq!(
            std::fs::read(d.path().join("IMG_0001.jpg")).unwrap(),
            b"first"
        );
        assert_eq!(std::fs::read(&landed).unwrap(), b"second");
    }

    #[test]
    fn the_upload_directory_is_confined_after_it_is_created() {
        let d = tempfile::tempdir().unwrap();
        let root = Root::open(d.path()).unwrap();
        let dir = upload_dir(&root, "Uploads").unwrap();
        assert!(dir.as_path().starts_with(root.as_path()));

        // A symlinked Uploads pointing outside the root is the ordinary mistake
        // this catches, and it used to fail silently.
        let outside = tempfile::tempdir().unwrap();
        let root2 = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root2.path().join("Elsewhere")).unwrap();
        let root2 = Root::open(root2.path()).unwrap();
        assert!(matches!(
            upload_dir(&root2, "Elsewhere"),
            Err(UploadError::Path(_))
        ));
    }
}
