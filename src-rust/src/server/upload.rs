//! The one write channel from a device.
//!
//! Six things are load-bearing here, each the failure a line prevents.
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
//!    that file. What the mtime is stamped *with* is [`StagedUpload::commit`]'s
//!    rule: the sender's own date for the file, else its capture time.
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
//! 6. **A part is committed only after its last byte arrived.** A read error is
//!    not an end of file: treating it as one renames the bytes received so far
//!    into the gallery as a finished photo, which the thumbnailer then draws
//!    half of. The handler commits only after the part ends cleanly, and any
//!    error drops the [`StagedUpload`] instead — see [`crate::server::routes`].
//!
//! **A file's date travels beside it.** A browser knows a date for each file it
//! sends (`File.lastModified`) that is often written nowhere in the bytes: an
//! iPhone reports the Photos library date, which for an image saved from the
//! web exists only in that library. The client sends it as a
//! [`LAST_MODIFIED_FIELD`] text part immediately before the file part, and the
//! route hands it to the next file part it sees. FormData cannot put a header
//! on a part, which is why it is a part of its own.
//!
//! **Uploads are bounded by what can actually run out.** Each part streams to
//! the temp file, so no part sits in RAM; a date part is read only up to
//! [`LAST_MODIFIED_MAX_LEN`]; the part count is capped ([`MAX_PARTS`]); and
//! free space is checked when a part starts and again every
//! [`RECHECK_EVERY`] bytes while it streams, so the [`FREE_SPACE_MARGIN`] holds
//! for the cache and the sidecars that share the volume. There is deliberately
//! **no cap on the request body**: axum's default is 2 MiB for the whole
//! request, which cuts a phone photo off partway, and any fixed number is a
//! guess against a multi-gigabyte clip. [`crate::server::routes::router`] lifts
//! it for this route alone.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use filetime::FileTime;

use crate::companion::schema::MediaType;
use crate::path::{GalleryPath, RelPath, Root};

/// Parts per request, a file's date part counting as one: a hundred files with
/// their dates. A phone's share sheet sends a handful; a hundred is generous
/// and a thousand is an attack.
pub const MAX_PARTS: usize = 200;

/// The name of the text part that carries the next file's date, as integer
/// milliseconds since the Unix epoch — JavaScript's `File.lastModified`, sent
/// unconverted so the client does no arithmetic on it.
pub const LAST_MODIFIED_FIELD: &str = "last_modified";

/// Bytes read from a date part before it is refused. An `i64` is at most twenty
/// characters with its sign, and the body has no cap, so without this a date
/// part is the one part that would be read into RAM unbounded.
pub const LAST_MODIFIED_MAX_LEN: usize = 20;

/// Refuse when the destination filesystem has less than this free.
pub const FREE_SPACE_MARGIN: u64 = 512 * 1024 * 1024;

/// Bytes streamed between free-space re-checks. A check scans the mount table,
/// so it is made once per this many bytes rather than once per write.
pub const RECHECK_EVERY: u64 = 64 * 1024 * 1024;

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
    #[error("invalid last_modified field")]
    BadDate,
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

/// Parse a [`LAST_MODIFIED_FIELD`] part into the date to stamp, or `None` when
/// the date is one the sender cannot have meant.
///
/// Strict on the wire and lenient on the value. Text that is not an integer is
/// a broken client and is refused. A date after `now` is a wrong clock, and one
/// at or before the epoch is a placeholder for no date at all; both are dropped
/// rather than refused, so the file still lands and falls back to its capture
/// time. Stamping a future date would sort the file above everything,
/// permanently.
pub fn sender_date(text: &[u8], now: SystemTime) -> Result<Option<FileTime>, UploadError> {
    let millis: i64 = std::str::from_utf8(text)
        .ok()
        .and_then(|t| t.trim().parse().ok())
        .ok_or(UploadError::BadDate)?;
    let now_millis = now
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    if millis <= 0 || millis > now_millis {
        return Ok(None);
    }
    Ok(Some(FileTime::from_unix_time(
        millis.div_euclid(1000),
        (millis.rem_euclid(1000) * 1_000_000) as u32,
    )))
}

/// A temp file in the destination directory, cleaned up on every error path.
///
/// The `Drop` is what makes point 5 above true regardless of which `?` returns
/// first.
pub struct StagedUpload {
    temp: PathBuf,
    file: Option<std::fs::File>,
    committed: bool,
    /// Free space below which a write refuses. Always [`FREE_SPACE_MARGIN`]
    /// outside tests, which raise it to force a refusal.
    margin: u64,
    /// Bytes written since free space was last checked.
    unchecked: u64,
}

impl StagedUpload {
    /// Open a temp file beside where the upload will land, refusing a disk that
    /// is already below the margin.
    ///
    /// Dot-prefixed and with no media extension, which is the only reason the
    /// watcher and the scan do not see it.
    pub fn create(dir: &Path) -> Result<Self, UploadError> {
        if below_margin(dir, FREE_SPACE_MARGIN) {
            return Err(UploadError::NoSpace);
        }
        let temp = dir.join(format!(".lv-upload-{}.tmp", uuid::Uuid::new_v4()));
        let file = std::fs::File::create(&temp)?;
        Ok(Self {
            temp,
            file: Some(file),
            committed: false,
            margin: FREE_SPACE_MARGIN,
            unchecked: 0,
        })
    }

    /// Append a chunk, refusing once free space has fallen below the margin.
    ///
    /// Streaming, so a 4 GB video never sits in RAM — and so the margin has to
    /// be re-checked here: checking only when the part starts lets one large
    /// part run the volume to `ENOSPC` while the cache and the companion
    /// sidecars, the only durable data, share it. The check runs every
    /// [`RECHECK_EVERY`] bytes, after the write, so it sees the space this chunk
    /// took.
    pub fn write(&mut self, chunk: &[u8]) -> Result<(), UploadError> {
        if let Some(file) = self.file.as_mut() {
            file.write_all(chunk)?;
        }
        self.unchecked += chunk.len() as u64;
        if self.unchecked >= RECHECK_EVERY {
            self.unchecked = 0;
            if self
                .temp
                .parent()
                .is_some_and(|dir| below_margin(dir, self.margin))
            {
                return Err(UploadError::NoSpace);
            }
        }
        Ok(())
    }

    /// Flush, stamp the mtime, and rename into place.
    ///
    /// **The mtime is the sender's date when it sent one, else the capture time
    /// in the file's own EXIF, else the upload time.** The sender's date wins
    /// because it is the only one of the three that exists nowhere else: the
    /// capture time stays in the bytes, which are never altered, and the index
    /// reads it into `date_taken` — which the grid sorts by in preference to the
    /// mtime —
    /// whatever the mtime says. So stamping the sender's date keeps both, where
    /// stamping the capture time would keep one twice and drop the other. And a
    /// sender whose date is merely the moment it picked the file costs nothing:
    /// without a capture time the fallback would have been the upload time
    /// anyway.
    ///
    /// Stamped **before** the rename, or the indexer records the upload time
    /// and every uploaded file sorts as "today" forever.
    ///
    /// `sent` comes from the route, the only place that holds the request. A
    /// parameter that its one caller always passed as `None` once left this
    /// stamp described and not running, so a router test
    /// (`an_uploads_date_part_dates_the_next_file_and_only_that_one`) pins that
    /// the route passes it.
    pub fn commit(
        mut self,
        destination_dir: &Path,
        name: &str,
        sent: Option<FileTime>,
    ) -> Result<PathBuf, UploadError> {
        if let Some(mut file) = self.file.take() {
            file.flush()?;
            file.sync_all()?;
        }
        let stamp = sent.or_else(|| {
            crate::pipeline::exif::read(&self.temp)
                .date_taken
                .map(|taken| FileTime::from_unix_time(taken, 0))
        });
        if let Some(stamp) = stamp {
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

/// `name` with ` (n)` inserted before its extension — the name a colliding
/// upload tries next.
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

/// Whether the filesystem holding `dir` has less than `margin` free.
///
/// A mount [`free_bytes`] cannot find reads as *not* below: there is nothing to
/// compare, so on such a filesystem (some FUSE and network mounts) neither check
/// refuses anything.
fn below_margin(dir: &Path, margin: u64) -> bool {
    free_bytes(dir).is_some_and(|free| free < margin)
}

/// Free space on the filesystem holding `path`, by the longest mount point that
/// prefixes it; `None` if no mount matches.
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
        let landed = staged.commit(d.path(), "a.jpg", None).unwrap();

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

    #[test]
    fn an_upload_with_no_capture_time_keeps_the_upload_time() {
        let d = tempfile::tempdir().unwrap();
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let mut staged = StagedUpload::create(d.path()).unwrap();
        staged.write(b"no exif here at all").unwrap();
        let landed = staged.commit(d.path(), "a.jpg", None).unwrap();

        let mtime = filetime::FileTime::from_last_modification_time(
            &std::fs::metadata(&landed).unwrap(),
        );
        assert!(
            mtime.unix_seconds() >= before - 5,
            "a file with no capture time should keep the upload time"
        );
    }

    #[test]
    fn the_senders_date_is_stamped_over_the_capture_time() {
        // The capture time stays in the bytes and reaches `date_taken`; the
        // sender's date exists nowhere else, so it is the one the mtime keeps.
        let d = tempfile::tempdir().unwrap();
        let jpeg =
            crate::pipeline::exif::tests_support::jpeg_with_exif(Some("2020:09:13 12:26:40"), None);
        let sent = FileTime::from_unix_time(1_700_000_000, 250_000_000);
        let mut staged = StagedUpload::create(d.path()).unwrap();
        staged.write(&jpeg).unwrap();
        let landed = staged.commit(d.path(), "a.jpg", Some(sent)).unwrap();

        assert_eq!(std::fs::read(&landed).unwrap(), jpeg, "the bytes were altered");
        let mtime = FileTime::from_last_modification_time(&std::fs::metadata(&landed).unwrap());
        assert_eq!(mtime.unix_seconds(), 1_700_000_000);
    }

    #[test]
    fn a_file_with_no_capture_time_takes_the_senders_date() {
        // An image saved from the web: no EXIF date, and the Photos library
        // date is the only one there is.
        let d = tempfile::tempdir().unwrap();
        let sent = FileTime::from_unix_time(1_762_709_673, 0);
        let mut staged = StagedUpload::create(d.path()).unwrap();
        staged.write(b"a png with no exif").unwrap();
        let landed = staged.commit(d.path(), "saved.png", Some(sent)).unwrap();

        let mtime = FileTime::from_last_modification_time(&std::fs::metadata(&landed).unwrap());
        assert_eq!(mtime.unix_seconds(), 1_762_709_673);
    }

    #[test]
    fn a_senders_date_is_milliseconds_and_keeps_its_fraction() {
        let now = UNIX_EPOCH + std::time::Duration::from_secs(1_800_000_000);
        let date = sender_date(b"1762709673250", now).unwrap().unwrap();
        assert_eq!(date.unix_seconds(), 1_762_709_673);
        assert_eq!(date.nanoseconds(), 250_000_000);
    }

    #[test]
    fn a_date_the_sender_cannot_have_meant_is_dropped_not_refused() {
        // A wrong clock, or a browser with no answer: the file still lands and
        // falls back to its capture time.
        let now = UNIX_EPOCH + std::time::Duration::from_secs(1_800_000_000);
        assert!(sender_date(b"0", now).unwrap().is_none());
        assert!(sender_date(b"-5000", now).unwrap().is_none());
        assert!(sender_date(b"1800000001000", now).unwrap().is_none(), "a future date was kept");
    }

    #[test]
    fn a_date_part_that_is_not_an_integer_is_refused() {
        let now = SystemTime::now();
        for bad in [&b""[..], b"yesterday", b"1.5", b"\xff\xfe"] {
            assert!(matches!(sender_date(bad, now), Err(UploadError::BadDate)), "{bad:?}");
        }
    }

    #[test]
    fn a_disk_that_falls_below_the_margin_refuses_mid_stream() {
        let d = tempfile::tempdir().unwrap();
        if free_bytes(d.path()).is_none() {
            // No mount for the temp dir: the check has nothing to compare, and
            // the assertion below would fail for the wrong reason.
            eprintln!("skipped: no mount found for {}", d.path().display());
            return;
        }
        {
            let mut staged = StagedUpload::create(d.path()).unwrap();
            // Every disk is below this margin, so the first re-check refuses.
            staged.margin = u64::MAX;
            // A small write is under the re-check interval and is not checked.
            staged.write(&[0u8; 4096]).unwrap();
            assert!(matches!(
                staged.write(&vec![0u8; RECHECK_EVERY as usize]),
                Err(UploadError::NoSpace)
            ));
        }
        // And the refused part left nothing behind.
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 0);
    }

    #[test]
    fn two_uploads_of_one_name_both_survive() {
        // The race the check-then-rename shape lost: both find the name free,
        // and the second silently replaces the first.
        let d = tempfile::tempdir().unwrap();
        let mut first = StagedUpload::create(d.path()).unwrap();
        first.write(b"first").unwrap();
        first.commit(d.path(), "IMG_0001.jpg", None).unwrap();

        let mut second = StagedUpload::create(d.path()).unwrap();
        second.write(b"second").unwrap();
        let landed = second.commit(d.path(), "IMG_0001.jpg", None).unwrap();

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
