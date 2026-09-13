//! Writing a file so that a reader sees the old bytes or the new ones, and so
//! that "the write returned Ok" means the bytes are on the disk.
//!
//! Those are two different claims and the obvious implementation makes only the
//! first. `std::fs::write` is create + `write_all`, and the `close()` happens in
//! `Drop`, which cannot report — so on NFS, which defers `ENOSPC`, `EDQUOT`,
//! `ESTALE` and `EIO` to flush-at-close, the write returns `Ok`, the rename
//! succeeds, and the file on the far end is truncated. The companion file is
//! the only durable data in this system; resting it on a guarantee the code
//! does not provide is not a small gap.
//!
//! So: temp file in the *target directory* (same filesystem, therefore an
//! atomic rename) → `write_all` → `sync_all` on the file → rename → `sync_all`
//! on the parent directory. And the temp file is removed on **every** error
//! path, not only a failed rename: otherwise an `ENOSPC` mid-write leaks a
//! dotfile into the one tree the design tells the user is safe to `grep`,
//! `rsync` and back up.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

/// Write `bytes` to `path` atomically and durably.
///
/// The caller is responsible for whatever lock the file needs — this function
/// does not take one, because for a companion the lock has to span the read as
/// well as the write.
pub fn write_durable(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    fs::create_dir_all(parent)?;

    // Unique within the directory, dot-prefixed so no scan or watcher sees it,
    // and with no media extension so the upload filters ignore it too.
    let temp = parent.join(format!(".lightview-tmp-{}", uuid::Uuid::new_v4()));

    let result = (|| -> io::Result<()> {
        let mut f = File::create(&temp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&temp, path)?;
        // The rename itself needs to survive a power cut, and that is a
        // property of the directory, not of the file.
        sync_dir(parent)
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Rename `from` to `to`, failing with `EEXIST` if `to` already exists.
///
/// The check-then-rename shape it replaces (`!candidate.exists()` followed by
/// an unconditional rename) loses its race: two phones uploading `IMG_0001.jpg`
/// at once both find the name free and the second silently clobbers the first —
/// the exact thing the dedupe loop exists to prevent.
pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let from_c = std::ffi::CString::new(from.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL"))?;
    let to_c = std::ffi::CString::new(to.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL"))?;

    // SAFETY: both pointers are NUL-terminated and live for the call.
    let rc = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from_c.as_ptr(),
            libc::AT_FDCWD,
            to_c.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// `fsync` a directory, so a rename into it is durable.
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    // Opening a directory read-only and fsyncing it is the portable-on-Linux
    // way to make a rename durable; there is no `File::open` flag for it.
    OpenOptions::new().read(true).open(dir)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_durable_replaces_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub").join("file.json");
        write_durable(&p, b"first").unwrap();
        write_durable(&p, b"second").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"second");

        let leftovers: Vec<_> = fs::read_dir(p.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with(".lightview-tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "leaked temp files: {leftovers:?}");
    }

    #[test]
    fn write_durable_cleans_up_when_the_rename_cannot_land() {
        // A directory standing where the file should go makes the rename fail
        // after the temp file has been written — the path that used to leak.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("occupied");
        fs::create_dir(&p).unwrap();
        fs::write(p.join("child"), b"x").unwrap();

        assert!(write_durable(&p, b"bytes").is_err());
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with(".lightview-tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "leaked temp files: {leftovers:?}");
    }

    #[test]
    fn rename_noreplace_refuses_an_occupied_destination() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        fs::write(&a, b"a").unwrap();
        fs::write(&b, b"b").unwrap();

        assert_eq!(
            rename_noreplace(&a, &b).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(&b).unwrap(), b"b");

        let c = dir.path().join("c");
        rename_noreplace(&a, &c).unwrap();
        assert_eq!(fs::read(&c).unwrap(), b"a");
    }
}
