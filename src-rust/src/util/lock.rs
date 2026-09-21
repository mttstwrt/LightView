//! The two advisory locks in the system, and why they are different syscalls.
//!
//! **`flock` for the derived cache directory.** One process per gallery is an
//! assumption `cache.db`'s single writer depends on, so it is enforced rather
//! than assumed — but only *locally*, because each machine has its own derived
//! cache and therefore its own private lock. `flock` is right here for a reason
//! that has nothing to do with the filesystem: the kernel releases it when the
//! descriptor closes, on exit, on `SIGKILL`, on a container being torn down. A
//! pidfile or a lock-by-file-existence would survive a crash and brick the
//! gallery until someone deleted a file they have never heard of. A leftover
//! lock *file* is not a leftover lock.
//!
//! **`fcntl` for the companion directory.** The mount is Samba. A `cifs` client
//! sends `fcntl` byte-range locks to the server as SMB locks, and `smbd` with
//! `posix locking = yes` — its default — takes the matching `fcntl` lock on the
//! server's own file. So a lock taken on the desktop and one taken by the
//! `--serve` process contend, **but only if both are `fcntl`**: on Linux
//! `flock` and `fcntl` locks do not see each other at all, so `flock` here
//! would be coherent on one machine and decorative across two — a phone's
//! rating silently lost to a plugin run that read the file a second earlier.
//!
//! **And specifically `F_OFD_SETLKW`, not `F_SETLKW`.** Classic POSIX record
//! locks are owned by the *process*, which breaks this design in two ways
//! inside a single `--serve`: two tasks taking the lock through different
//! descriptors do not contend at all, and closing *any* descriptor on the file
//! drops every lock the process holds on it — so one task finishing releases
//! another task's lock mid-write. Open file description locks are owned by the
//! open itself, so two opens in one process serialize exactly as two processes
//! do. This is why the calls below are raw `fcntl` rather than a safe wrapper:
//! neither `rustix` nor the standard library exposes the OFD commands.
//!
//! [`FileLock::acquire`] **blocks**, and on a share it can block on another
//! machine. Async callers must reach it through `spawn_blocking`.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;

/// An exclusive `flock` held for the life of the value.
///
/// Dropping it — or the process dying — releases it.
#[derive(Debug)]
pub struct DirLock {
    _file: File,
}

impl DirLock {
    /// Take the lock, or report that someone else holds it.
    ///
    /// `Ok(None)` means the lock is held elsewhere: a live process owns this
    /// gallery. That is not an error, it is the answer the caller asked for —
    /// a second `lightview <dir>` opens the first one's browser rather than
    /// refusing, and `lightview cache --prune` skips a gallery it cannot get
    /// rather than unlinking a `cache.db` a running process still writes to.
    pub fn try_acquire(path: &Path) -> io::Result<Option<Self>> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)?;

        // SAFETY: `flock` takes a raw descriptor and an operation; `file` owns
        // the descriptor and outlives the call.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            return Ok(Some(Self { _file: file }));
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EWOULDBLOCK) => Ok(None),
            _ => Err(err),
        }
    }
}

/// An exclusive open-file-description lock on a whole file, held for the life
/// of the value. Blocks until it is granted.
///
/// Used on `<dir>/.lightview/companions/.lock`: a separate lock file, never a
/// companion itself. A companion write ends in a rename that replaces the
/// inode, so a lock on the old inode covers nothing after the swap; a lock file
/// that is never replaced does. One per directory, because companions are per
/// directory already and the contention being serialized is between two
/// *machines*, not two files.
#[derive(Debug)]
pub struct FileLock {
    file: File,
}

impl FileLock {
    /// Create the lock file if needed and block until the lock is held.
    ///
    /// Requires write permission on the lock file, which is the half of the
    /// UID question `--serve`'s startup probe checks: rename and unlink need
    /// write permission on the *directory*, the lock needs it on the *file*,
    /// and with two UIDs and Samba's default `create mask = 0744` both fail.
    pub fn acquire(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)?;
        set_ofd_lock(&file, libc::F_WRLCK as libc::c_short)?;
        Ok(Self { file })
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // Closing the descriptor releases an OFD lock anyway; unlocking first
        // makes the release explicit and keeps the guard's lifetime honest if
        // the `File` is ever kept alive past the guard.
        let _ = set_ofd_lock(&self.file, libc::F_UNLCK as libc::c_short);
    }
}

/// `fcntl(fd, F_OFD_SETLKW, &flock{ l_type, whole file })`.
///
/// `l_len == 0` means "from `l_start` to the end of the file, as it grows",
/// which is the whole-file lock this wants. Retries on `EINTR`, since
/// `F_OFD_SETLKW` is interruptible and a signal is not a lock failure.
fn set_ofd_lock(file: &File, l_type: libc::c_short) -> io::Result<()> {
    let lock = libc::flock {
        l_type,
        l_whence: libc::SEEK_SET as libc::c_short,
        l_start: 0,
        l_len: 0,
        l_pid: 0,
    };
    loop {
        // SAFETY: `fcntl` reads a `struct flock` through the pointer for the
        // duration of the call; `lock` is a live local and `file` owns the
        // descriptor.
        let rc = unsafe {
            libc::fcntl(
                file.as_raw_fd(),
                libc::F_OFD_SETLKW,
                &lock as *const libc::flock,
            )
        };
        if rc == 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return Err(err);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_dir_lock_is_refused_while_the_first_lives() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");
        let first = DirLock::try_acquire(&path).unwrap();
        assert!(first.is_some());
        assert!(DirLock::try_acquire(&path).unwrap().is_none());
        drop(first);
        assert!(DirLock::try_acquire(&path).unwrap().is_some());
    }

    #[test]
    fn a_leftover_lock_file_is_not_a_leftover_lock() {
        // The whole reason for flock over a pidfile: a crash leaves the file,
        // and the file alone must not brick the gallery.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");
        drop(DirLock::try_acquire(&path).unwrap().unwrap());
        assert!(path.exists());
        assert!(DirLock::try_acquire(&path).unwrap().is_some());
    }

    #[test]
    fn two_opens_in_one_process_serialize() {
        // The test that catches `F_SETLKW`. Classic POSIX record locks are
        // owned by the process, so this second acquire would be granted
        // immediately — and inside `--serve` that is a phone's rating racing
        // the companion re-index with no lock between them at all.
        use std::sync::mpsc;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".lock");
        let held = FileLock::acquire(&path).unwrap();

        let (tx, rx) = mpsc::channel();
        let p = path.clone();
        let t = std::thread::spawn(move || {
            let _l = FileLock::acquire(&p).unwrap();
            let _ = tx.send(());
        });

        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(300))
                .is_err(),
            "second lock was granted while the first was held"
        );
        drop(held);
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .expect("second lock was never granted after release");
        t.join().unwrap();
    }

    #[test]
    fn an_unrelated_close_does_not_drop_a_held_lock() {
        // The other classic-lock failure: with `F_SETLKW`, closing *any*
        // descriptor on the file releases every lock the process holds on it,
        // so an unrelated open-and-close mid-write would silently unlock.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".lock");
        let held = FileLock::acquire(&path).unwrap();

        drop(File::open(&path).unwrap());

        let (tx, rx) = std::sync::mpsc::channel();
        let p = path.clone();
        let t = std::thread::spawn(move || {
            let _l = FileLock::acquire(&p).unwrap();
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(300))
                .is_err(),
            "an unrelated close released the held lock"
        );
        drop(held);
        rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        t.join().unwrap();
    }
}
