//! The cache directory as a whole: what it holds, how big it is allowed to
//! get, and which gallery gives way first.
//!
//! **Two budgets, two names, because they are not the same mechanism.** The
//! *tier budget* bounds `jm` and `jh` inside one gallery's `cache.db` and is
//! enforced automatically on both write paths (see
//! [`crate::cache::tiers::enforce_budget`]). The *cache-directory ceiling*
//! bounds the total bytes under `galleries/` across every gallery, and is
//! enforced here.
//!
//! **The ceiling is enforced at gallery open, not only by a command.** Leaving
//! it to `lightview cache --prune` means the first usage pattern the design
//! exists to serve — two hundred images processed once and never reopened —
//! leaves a permanent cache that nothing reclaims, in a directory advertised as
//! safe *because* it is budgeted. A hundred such folders is a hundred orphan
//! caches. Open is the right moment: one `read_dir` and a sort, at exactly the
//! point a stale cache is provably unused.
//!
//! **The LRU key is each gallery's `last_opened` file**, never `cache.db`'s
//! mtime. Under WAL, writes land in `cache.db-wal` and the main file's mtime
//! moves only on checkpoint — so an mtime key measures *least recently
//! written*, and a fully-warmed gallery opened daily and never written to looks
//! colder than the throwaway folder touched once. It would evict exactly the
//! wrong thing.
//!
//! **Eviction takes each gallery's `flock` non-blocking and skips what it
//! cannot get.** Unlinking a `cache.db` that a running process holds open is
//! completely silent on Linux: that process keeps writing to the unlinked inode
//! and the work is discarded at exit. Pruning to reclaim space would throw away
//! the thumbnails a running `--serve` generated all afternoon.

use std::path::{Path, PathBuf};

use crate::util::lock::DirLock;

/// One gallery's cache directory, as the pruner sees it.
#[derive(Debug)]
pub struct GalleryCache {
    pub dir: PathBuf,
    pub bytes: u64,
    /// Unix seconds from the `last_opened` file; 0 when it is missing, which
    /// makes an un-stamped directory the coldest thing there is.
    pub last_opened: i64,
}

/// Measure every gallery cache under `galleries/`, newest first.
pub fn survey(galleries_dir: &Path) -> std::io::Result<Vec<GalleryCache>> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(galleries_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let dir = entry.path();
        out.push(GalleryCache {
            bytes: dir_bytes(&dir)?,
            last_opened: read_last_opened(&dir),
            dir,
        });
    }
    out.sort_by(|a, b| b.last_opened.cmp(&a.last_opened));
    Ok(out)
}

/// Total bytes under `galleries/`.
pub fn total_bytes(galleries_dir: &Path) -> std::io::Result<u64> {
    Ok(survey(galleries_dir)?.iter().map(|g| g.bytes).sum())
}

/// What one eviction pass did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PruneReport {
    pub removed: Vec<PathBuf>,
    pub skipped_in_use: Vec<PathBuf>,
    pub freed_bytes: u64,
}

/// Evict least-recently-opened galleries until the total is within `ceiling`.
///
/// `keep` is the gallery being opened right now, which must never be a
/// candidate however cold it looks — it is about to become the warmest thing
/// there is, and its lock is held by this very process.
pub fn prune_to_ceiling(
    galleries_dir: &Path,
    ceiling_bytes: u64,
    keep: Option<&Path>,
) -> std::io::Result<PruneReport> {
    let mut report = PruneReport::default();
    let caches = survey(galleries_dir)?;
    let mut total: u64 = caches.iter().map(|g| g.bytes).sum();
    if total <= ceiling_bytes {
        return Ok(report);
    }

    // Coldest first.
    for cache in caches.into_iter().rev() {
        if total <= ceiling_bytes {
            break;
        }
        if keep.is_some_and(|k| k == cache.dir) {
            continue;
        }
        // Non-blocking: a gallery someone is using keeps its cache.
        let lock = match DirLock::try_acquire(&cache.dir.join("lock")) {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                report.skipped_in_use.push(cache.dir);
                continue;
            }
            Err(e) => return Err(e),
        };
        std::fs::remove_dir_all(&cache.dir)?;
        drop(lock);
        total = total.saturating_sub(cache.bytes);
        report.freed_bytes += cache.bytes;
        report.removed.push(cache.dir);
    }
    Ok(report)
}

fn dir_bytes(dir: &Path) -> std::io::Result<u64> {
    let mut total = 0;
    for entry in walkdir::WalkDir::new(dir).into_iter().flatten() {
        if entry.file_type().is_file() {
            total += entry.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    Ok(total)
}

fn read_last_opened(dir: &Path) -> i64 {
    std::fs::read_to_string(dir.join("last_opened"))
        .ok()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s.trim()).ok())
        .map(|d| d.timestamp())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plant(root: &Path, name: &str, bytes: usize, opened: &str) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("cache.db"), vec![0u8; bytes]).unwrap();
        std::fs::write(dir.join("last_opened"), opened).unwrap();
        dir
    }

    #[test]
    fn survey_orders_warmest_first_and_measures_bytes() {
        let root = tempfile::tempdir().unwrap();
        plant(root.path(), "cold", 100, "2020-01-01T00:00:00+00:00");
        plant(root.path(), "warm", 200, "2026-01-01T00:00:00+00:00");

        let s = survey(root.path()).unwrap();
        assert_eq!(s[0].dir.file_name().unwrap(), "warm");
        assert!(s[0].bytes >= 200);
        assert_eq!(total_bytes(root.path()).unwrap(), s.iter().map(|g| g.bytes).sum::<u64>());
    }

    #[test]
    fn the_least_recently_opened_gallery_gives_way_first() {
        let root = tempfile::tempdir().unwrap();
        let cold = plant(root.path(), "cold", 1000, "2020-01-01T00:00:00+00:00");
        let warm = plant(root.path(), "warm", 1000, "2026-01-01T00:00:00+00:00");

        let report = prune_to_ceiling(root.path(), 1500, None).unwrap();
        assert_eq!(report.removed, vec![cold.clone()]);
        assert!(!cold.exists());
        assert!(warm.exists());
    }

    #[test]
    fn a_gallery_someone_is_using_keeps_its_cache() {
        // Unlinking a cache.db a live process holds open is silent: it keeps
        // writing to the dead inode and loses the afternoon's work at exit.
        let root = tempfile::tempdir().unwrap();
        let busy = plant(root.path(), "busy", 1000, "2020-01-01T00:00:00+00:00");

        let _held = DirLock::try_acquire(&busy.join("lock")).unwrap().unwrap();
        let report = prune_to_ceiling(root.path(), 500, None).unwrap();

        assert!(report.removed.is_empty());
        assert_eq!(report.skipped_in_use, vec![busy.clone()]);
        assert!(busy.exists());
        // Still over the ceiling, and that is the honest outcome: a pinned
        // entry is not reclaimable, so the alternative is destroying a live
        // process's work to satisfy an arithmetic bound.
        assert!(total_bytes(root.path()).unwrap() > 500);
    }

    #[test]
    fn a_pinned_gallery_does_not_stop_the_pass() {
        // Skipping is not giving up: the next coldest reclaimable cache is
        // still a candidate. This is an ordinary LRU with in-use entries
        // pinned, and it is why a long-running `--serve` whose `last_opened`
        // grows stale is harmless — its lock is held, so its staleness never
        // gets a chance to matter.
        let root = tempfile::tempdir().unwrap();
        let busy = plant(root.path(), "busy", 1000, "2020-01-01T00:00:00+00:00");
        let warm = plant(root.path(), "warm", 1000, "2026-01-01T00:00:00+00:00");

        let _held = DirLock::try_acquire(&busy.join("lock")).unwrap().unwrap();
        let report = prune_to_ceiling(root.path(), 1500, None).unwrap();

        assert_eq!(report.skipped_in_use, vec![busy.clone()]);
        assert_eq!(report.removed, vec![warm.clone()]);
        assert!(busy.exists());
        assert!(!warm.exists());
    }

    #[test]
    fn the_gallery_being_opened_is_never_a_candidate() {
        let root = tempfile::tempdir().unwrap();
        let opening = plant(root.path(), "opening", 1000, "2020-01-01T00:00:00+00:00");
        let other = plant(root.path(), "other", 1000, "2026-01-01T00:00:00+00:00");

        let report = prune_to_ceiling(root.path(), 500, Some(&opening)).unwrap();
        assert!(opening.exists());
        assert_eq!(report.removed, vec![other]);
    }

    #[test]
    fn an_unstamped_directory_is_the_coldest_thing_there_is() {
        let root = tempfile::tempdir().unwrap();
        let stray = root.path().join("stray");
        std::fs::create_dir_all(&stray).unwrap();
        std::fs::write(stray.join("cache.db"), vec![0u8; 1000]).unwrap();
        plant(root.path(), "warm", 1000, "2026-01-01T00:00:00+00:00");

        let report = prune_to_ceiling(root.path(), 1500, None).unwrap();
        assert_eq!(report.removed, vec![stray]);
    }

    #[test]
    fn nothing_happens_below_the_ceiling() {
        let root = tempfile::tempdir().unwrap();
        plant(root.path(), "a", 100, "2026-01-01T00:00:00+00:00");
        assert_eq!(prune_to_ceiling(root.path(), 10_000, None).unwrap(), PruneReport::default());
    }
}
