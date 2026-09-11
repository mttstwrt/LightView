//! Opening a gallery, and keeping it in step with the filesystem.
//!
//! # Ingest: how a file becomes a grid cell
//!
//! The middle of that path — between "the file lands" and "the client is told"
//! — used to be described as *"once a file lands, the ordinary fs-watcher
//! ingests it"*, and the code it referred to split in a way that guaranteed the
//! policy would be lost: [`crate::util::fs_watch`] is a 60-line transport whose
//! own doc says the caller decides what a burst of events means, and the 190
//! lines that decide lived in a file being rewritten. So the policy is carried
//! here deliberately, item by item:
//!
//! - **A quiet-period debounce, not a throttle.** The timer is reset by every
//!   event, so a phone uploading two hundred photos back to back produces no
//!   rows and no SSE until 500 ms after the last one lands.
//! - **Three skip filters, in this order**: the `settings.toml` hot-reload
//!   branch runs **before** the `.lightview` skip, or changing a display
//!   preference stops reaching the running process; then `.lightview` itself;
//!   then the media-extension filter, which is the only reason an upload's own
//!   `.lv-upload-*.tmp` is invisible.
//! - **Only `Create` and `Modify(Name(To))` count as additions.**
//!   `Modify(Data)` is ignored for media — see "a path is immutable" below —
//!   and watched for companions, which is the whole point of that branch.
//! - **Armed on the canonical root.** Database paths are relative to the
//!   canonical root; a watcher armed on the user-supplied one fails
//!   `strip_prefix` on every event, which looks exactly like "not in this
//!   gallery". `lightview --serve ~/photos` where that is a symlink would
//!   upload fine, thumbnail fine, and never show the file until a restart.
//! - **`notify`'s own errors are surfaced**, because inotify watch-limit
//!   exhaustion and queue overflow arrive through the same channel and make the
//!   watcher go *partially* deaf with no log line. On a root that disappears
//!   the process exits non-zero naming it: a serving process can do nothing
//!   useful without its gallery, and a systemd unit restarts it when the mount
//!   returns.
//! - **A newly ingested file gets its companion indexed in the same breath.**
//!   Otherwise a batch arriving *with* its sidecars over `rsync` or Samba — the
//!   headline deployment — appears with no tags, no rating and no colour label
//!   until a restart, and any edit made in that state overwrites a companion
//!   the index never read.
//! - **The watcher has a companion branch**, replacing a `continue`. It is the
//!   primary path by which a `lightview tag` run on the desktop reaches the
//!   phone: `smbd` writes are ordinary local writes on the server, so they fire
//!   its `inotify` exactly as a local edit would.
//!
//! **A path is immutable within a gallery session.** Nothing updates a row
//! whose file was replaced, the tier lookup has no mtime predicate, and the
//! ETag is a hash of the cached bytes — so a phone would revalidate, get a 304,
//! and re-stamp its freshness window. Dedupe guarantees a new upload is a new
//! path; replacing a file in place on the host is outside what this supports,
//! and saying so is cheaper than putting `mtime` in every tier lookup.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use notify::event::{EventKind, ModifyKind, RenameMode};

use crate::cache::{db, index, meta};
use crate::companion::reader::{self, CompanionLocation};
use crate::companion::schema::{CompanionFile, MediaType, PluginTagEntry};
use crate::companion::writer::{modify_companion, Outcome};
use crate::path::{RelPath, Root};
use crate::provider::local::LocalProvider;
use crate::server::events::Event;
use crate::services::settings::GallerySettings;
use crate::state::Gallery;

/// How often the watcher drains the transport.
const POLL: Duration = Duration::from_millis(300);
/// Quiet period before a burst is flushed. Reset by every event.
const DEBOUNCE: Duration = Duration::from_millis(500);

#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error(transparent)]
    Cache(#[from] crate::cache::db::CacheError),
    #[error(transparent)]
    Path(#[from] crate::path::PathError),
    #[error(transparent)]
    Provider(#[from] crate::provider::ProviderError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

/// Scan the gallery and bring `media_meta` in step with it.
///
/// The prune only happens on a scan that completed: the provider propagates
/// walk errors now, and an empty result for a gallery that had rows is refused
/// outright. Both of those are the unmounted-NAS case, which destroys
/// `date_added` and `last_viewed` for the whole library.
pub async fn scan_and_index(gallery: &Gallery) -> Result<usize, OpenError> {
    let root = gallery.root.clone();
    let files = tokio::task::spawn_blocking(move || {
        LocalProvider::new(root).list_dir_recursive()
    })
    .await
    .map_err(|e| OpenError::Io(std::io::Error::other(e.to_string())))??;

    let scanned: Vec<meta::ScannedFile> = files
        .iter()
        .map(|f| meta::ScannedFile {
            path: f.path.clone(),
            media_type: media_type_str(&f.path),
            file_size: f.size as i64,
            mtime: f.mtime as i64,
        })
        .collect();
    let present: Vec<RelPath> = scanned.iter().map(|s| s.path.clone()).collect();

    let conn = gallery.db.writer().await;
    meta::insert_scanned(&conn, &scanned)?;
    db::prune_missing(&conn, &present)?;
    Ok(present.len())
}

/// The open-time enrichment pass, in the order the design requires.
///
/// EXIF first, then geocoding, then the companion index — so the sidecars the
/// geocoder writes are picked up by the same sweep rather than waiting for the
/// next open.
///
/// **Every phase reads files and writes companions outside the writer lock**,
/// taking it only to commit batched statements. The pass this replaces held the
/// writer across the whole walk — a `walkdir`, a file read per changed
/// companion, a rayon read-modify-write over every geotagged file, and a
/// checkpoint — during which the grid could not warm a single thumbnail.
pub async fn enrich_and_index(gallery: &Gallery) -> Result<(), OpenError> {
    backfill_exif(gallery).await?;
    backfill_locations(gallery).await?;
    reindex_companions(gallery).await?;
    gallery.refresh_autocomplete().await;
    Ok(())
}

/// Read capture time and GPS out of headers for files nothing is known about.
///
/// The gate is "nothing has been learned about this file yet" rather than
/// "`gps_lat` is NULL", because a `NULL` never becomes non-`NULL` for a photo
/// that has no GPS — so the narrower gate would re-read every such header on
/// every open, forever. Any decode sets `width`, so once the backfill has
/// warmed a gallery this pass is a no-op.
async fn backfill_exif(gallery: &Gallery) -> Result<(), OpenError> {
    let candidates: Vec<RelPath> = {
        let conn = gallery.db.read().await;
        let mut stmt = conn.prepare(
            "SELECT path FROM media_meta
             WHERE date_taken IS NULL AND gps_lat IS NULL AND width IS NULL",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(RelPath::new(&row?)?);
        }
        out
    };
    if candidates.is_empty() {
        return Ok(());
    }

    let root = gallery.root.clone();
    let probed = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        for path in candidates {
            let Ok(absolute) = root.resolve(&path) else {
                continue;
            };
            let exif = crate::pipeline::exif::read(absolute.as_path());
            out.push((
                path,
                meta::ProbedMedia {
                    date_taken: exif.date_taken,
                    location: exif.location,
                    ..Default::default()
                },
            ));
        }
        out
    })
    .await
    .map_err(|e| OpenError::Io(std::io::Error::other(e.to_string())))?;

    let conn = gallery.db.writer().await;
    let tx = conn.unchecked_transaction()?;
    for (path, m) in &probed {
        meta::set_probed(&conn, path, m)?;
    }
    tx.commit()?;
    Ok(())
}

/// Turn coordinates into place names, once per file per gazetteer version.
///
/// **The skip is gated on the durable side.** The obvious test — "does this
/// path have any `plugin.location` rows?" — is a query against `tag_index`,
/// which is derived, so on a fresh or wiped cache it is empty and every
/// geotagged file is re-tagged whatever a version stamp says. That is a derived
/// wipe triggering durable writes: it rewrites every geotagged sidecar's mtime,
/// which every other machine's index sweep then has to look at. Reading the
/// companion's own recorded version makes the pass a genuine no-op on a
/// rebuilt cache, which is what makes "nothing is lost but time" true rather
/// than "nothing but time, and every sidecar's mtime".
async fn backfill_locations(gallery: &Gallery) -> Result<(), OpenError> {
    let geotagged: Vec<(RelPath, f64, f64)> = {
        let conn = gallery.db.read().await;
        let mut stmt = conn.prepare(
            "SELECT path, gps_lat, gps_lon FROM media_meta WHERE gps_lat IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?, r.get::<_, f64>(2)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (p, lat, lon) = row?;
            out.push((RelPath::new(&p)?, lat, lon));
        }
        out
    };
    if geotagged.is_empty() {
        return Ok(());
    }

    let root = gallery.root.clone();
    let written = tokio::task::spawn_blocking(move || {
        let mut written = Vec::new();
        for (path, lat, lon) in geotagged {
            let Ok(absolute) = root.resolve(&path) else {
                continue;
            };
            let version = crate::geocode::TAGGER_VERSION;

            let outcome = modify_companion(
                absolute.as_path(),
                media_type_of(&path),
                |companion: &mut CompanionFile| {
                    // The durable-side gate: this file's own record of which
                    // gazetteer wrote its place names.
                    if companion
                        .tags
                        .plugins
                        .get("location")
                        .is_some_and(|b| b.version == version)
                    {
                        return Outcome::Leave(false);
                    }
                    match crate::geocode::lookup(lat, lon) {
                        Some(place) => {
                            companion.tags.plugins.insert(
                                "location".to_string(),
                                PluginTagEntry {
                                    version: version.to_string(),
                                    tags: place.tags(),
                                    ..Default::default()
                                },
                            );
                            Outcome::Write(true)
                        }
                        // Past the 100 km ceiling nothing is emitted — but the
                        // bucket is still stamped, so the next open does not
                        // resolve the same coordinate again.
                        None => {
                            companion.tags.plugins.insert(
                                "location".to_string(),
                                PluginTagEntry {
                                    version: version.to_string(),
                                    tags: Vec::new(),
                                    ..Default::default()
                                },
                            );
                            Outcome::Write(true)
                        }
                    }
                },
            );
            match outcome {
                Ok(true) => written.push(path),
                Ok(false) => {}
                Err(e) => log::warn!("could not write place names for {path}: {e}"),
            }
        }
        written
    })
    .await
    .map_err(|e| OpenError::Io(std::io::Error::other(e.to_string())))?;

    if !written.is_empty() {
        log::info!("geocoded {} files", written.len());
    }
    Ok(())
}

/// Re-index every companion whose `(mtime_nanos, size)` has moved.
///
/// **Two phases, and that split is the point.** The scan phase walks, stats and
/// reads with no database handle at all; the commit phase takes the writer once
/// and runs batched statements. Held as one pass, this blocked every thumbnail
/// the grid was waiting on — tolerable once per open, and not once it runs
/// every hour beside an hours-long stream of writes from another machine.
pub async fn reindex_companions(gallery: &Gallery) -> Result<usize, OpenError> {
    let known = {
        let conn = gallery.db.read().await;
        index::load_state(&conn)?
    };
    let paths = {
        let conn = gallery.db.read().await;
        meta::all_paths(&conn)?
    };

    let root = gallery.root.clone();
    let scanned = tokio::task::spawn_blocking(move || {
        let mut changed = Vec::new();
        for path in paths {
            let Ok(absolute) = root.resolve(&path) else {
                continue;
            };
            let companion_file =
                reader::companion_path(absolute.as_path(), CompanionLocation::LightviewFolder);
            let Ok(metadata) = std::fs::metadata(&companion_file) else {
                continue;
            };
            let state = index::IndexState::of(&metadata);
            if known.get(path.as_str()) == Some(&state) {
                continue;
            }
            match reader::read_companion(absolute.as_path()) {
                Ok(Some(companion)) => changed.push((path, companion, state)),
                Ok(None) => {}
                // A companion that will not parse is logged, never silently
                // skipped: the failure it usually means is a truncated write
                // over a network mount, and swallowing it re-reads and
                // re-fails that file on every pass forever.
                Err(e) => log::warn!("could not parse the companion for {path}: {e}"),
            }
        }
        changed
    })
    .await
    .map_err(|e| OpenError::Io(std::io::Error::other(e.to_string())))?;

    if scanned.is_empty() {
        return Ok(0);
    }

    let count = scanned.len();
    let mut owed = Vec::new();
    {
        let conn = gallery.db.writer().await;
        let tx = conn.unchecked_transaction()?;
        for (path, companion, state) in &scanned {
            index::reindex_file(&conn, path, companion)?;
            index::set_state(&conn, path, *state)?;
            let mirror = meta::mirror_companion(&conn, path, companion)?;
            if mirror.missing_date_added.is_some() || mirror.missing_last_viewed.is_some() {
                owed.push((path.clone(), mirror));
            }
        }
        tx.commit()?;
    }

    // The other direction of the mirror: fields the database knows and the
    // companion does not. Written back outside the lock, because each one takes
    // the companion's own lock.
    complete_companions(gallery, owed).await;
    Ok(count)
}

/// How often the companion sweep runs.
///
/// The watcher is the primary path and this is the backstop, so an hour is
/// about how long a tag written somewhere `inotify` cannot see may take to
/// appear. That case is real: the watcher sees writes arriving at the server's
/// own disk — `smbd` is an ordinary local process, and inotify watches inodes —
/// but a *client-side* mount is invisible to it, and so is a gallery on a
/// filesystem that does not report events at all.
const COMPANION_SWEEP: std::time::Duration = std::time::Duration::from_secs(3600);

/// Re-index companions on a wall clock, for as long as the gallery is open.
///
/// **Deliberately not folded into the idle worker**, which skips its units
/// whenever somebody is touching the grid. That is right for thumbnail
/// backfill, which competes for the same pool the user is waiting on, and wrong
/// here: a companion `lightview tag` wrote over the share has to appear whether
/// or not anyone is looking, and a busy gallery is exactly when somebody is.
/// The sweep's own two-phase split is what keeps it from blocking the grid —
/// it walks, stats and reads with no database handle at all.
pub fn spawn_companion_sweep(gallery: Arc<Gallery>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(COMPANION_SWEEP).await;
            match reindex_companions(&gallery).await {
                Ok(0) => {}
                Ok(n) => {
                    log::info!("companion sweep re-indexed {n} file(s)");
                    gallery.refresh_autocomplete().await;
                    // `tags`, not `items`: the vocabulary moved and so did what
                    // a tag filter matches, but no file appeared or vanished.
                    gallery.events.send(Event::TagsIndexed);
                }
                Err(e) => log::warn!("companion sweep failed: {e}"),
            }
        }
    })
}

/// Write `date_added` / `last_viewed` back into sidecars that lack them.
async fn complete_companions(gallery: &Gallery, owed: Vec<(RelPath, meta::MirrorResult)>) {
    if owed.is_empty() {
        return;
    }
    let root = gallery.root.clone();
    let _ = tokio::task::spawn_blocking(move || {
        for (path, mirror) in owed {
            let Ok(absolute) = root.resolve(&path) else {
                continue;
            };
            let added = mirror.missing_date_added.map(meta::to_rfc3339);
            let viewed = mirror.missing_last_viewed.map(meta::to_rfc3339);
            let _ = modify_companion(absolute.as_path(), media_type_of(&path), |companion| {
                let mut core = companion.meta.core.take().unwrap_or_default();
                if core.date_added.is_none() {
                    core.date_added = added.clone();
                }
                if core.last_viewed.is_none() {
                    core.last_viewed = viewed.clone();
                }
                companion.meta.core = Some(core);
                Outcome::Write(())
            });
        }
    })
    .await;
}

/// Index one file's companion, for the watcher's add and companion branches.
pub async fn index_one(gallery: &Gallery, path: &RelPath) -> Result<(), OpenError> {
    let absolute = gallery.root.resolve(path)?;
    let read = tokio::task::spawn_blocking(move || {
        let companion = reader::read_companion(absolute.as_path());
        let state = std::fs::metadata(reader::companion_path(
            absolute.as_path(),
            CompanionLocation::LightviewFolder,
        ))
        .ok()
        .map(|m| index::IndexState::of(&m));
        (companion, state)
    })
    .await
    .map_err(|e| OpenError::Io(std::io::Error::other(e.to_string())))?;

    let (Ok(Some(companion)), Some(state)) = read else {
        return Ok(());
    };
    let conn = gallery.db.writer().await;
    index::reindex_file(&conn, path, &companion)?;
    index::set_state(&conn, path, state)?;
    meta::mirror_companion(&conn, path, &companion)?;
    Ok(())
}

/// Check that this process can replace companions another machine wrote.
///
/// **A UID question the design cannot answer and must not assume.** A companion
/// the desktop writes over the share is created on disk by `smbd` as whatever
/// user the share maps the client to; a companion the server writes is created
/// as the container's user. Each side then has to *replace* the other's work:
/// rename over a companion, open `.lock` for writing to take the `fcntl` lock,
/// and on the `cifs` fallback path unlink a target. Rename and unlink need
/// write permission on the **directory**; the write lock needs it on the **lock
/// file**. With Samba's default `create mask = 0744` and two different UIDs
/// every one of those fails — in both directions, silently on the desktop and
/// as a logged error on the server — for every directory the other side touched
/// first. A phone could not rate a photo the desktop had tagged.
///
/// Two configurations pass, and which one applies is a fact about `smb.conf`
/// rather than about this design, so `--serve` checks rather than trusting
/// either. Cheap, once, and it turns a silent write failure discovered weeks
/// later into a message at the moment the deployment is being set up.
///
/// The desktop side needs no probe: its first `lightview tag` run fails loudly
/// on the first companion it cannot replace, which is the same information a
/// run later.
pub fn probe_write_access(root: &Root) -> Result<(), String> {
    let lightview = root.as_path().join(".lightview");
    std::fs::create_dir_all(&lightview)
        .map_err(|e| refusal(&lightview, &format!("could not be created: {e}")))?;
    probe_directory(&lightview)?;

    // One existing companions/ directory this process does not own is the
    // interesting case; a tree where every directory is ours proves nothing
    // about the other writer.
    let Some(foreign) = foreign_companions_dir(root) else {
        return Ok(());
    };
    probe_directory(&foreign)?;

    // And the lock file specifically, which needs write permission on the file
    // rather than on the directory.
    let lock = foreign.join(".lock");
    if lock.exists() {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock)
            .map_err(|e| refusal(&lock, &format!("could not be opened for writing: {e}")))?;
    }
    Ok(())
}

/// Create and remove a file in `dir`, which is what rename and unlink need.
fn probe_directory(dir: &Path) -> Result<(), String> {
    let probe = dir.join(format!(".lv-probe-{}", uuid::Uuid::new_v4()));
    std::fs::write(&probe, b"")
        .map_err(|e| refusal(dir, &format!("is not writable by this process: {e}")))?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// The first `companions/` directory owned by somebody else.
fn foreign_companions_dir(root: &Root) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let me = unsafe { libc::geteuid() };
    for entry in walkdir::WalkDir::new(root.as_path())
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_dir() && e.file_name() == "companions")
    {
        if entry.metadata().map(|m| m.uid()).ok() != Some(me) {
            return Some(entry.path().to_path_buf());
        }
    }
    None
}

/// The refusal message: what, who owns it, and the two lines that fix it.
fn refusal(path: &Path, problem: &str) -> String {
    use std::os::unix::fs::MetadataExt;
    let owner = std::fs::metadata(path)
        .map(|m| format!("uid {}", m.uid()))
        .unwrap_or_else(|_| "an unknown user".to_string());
    let me = unsafe { libc::geteuid() };

    format!(
        "Refusing to serve: {} {problem}\n\
         \n\
         It is owned by {owner}; this process runs as uid {me}. Both writers have to \n\
         be able to replace each other's companion files, or a phone will not be able \n\
         to rate a photo the desktop tagged — silently on the desktop, and as a logged \n\
         error here.\n\
         \n\
         Two Samba configurations fix it:\n\
         \n\
         1. Connect as the account this process runs as, so there is one UID by \n\
            construction. Add `force user = <that account>` to the share if it has \n\
            more than one login.\n\
         2. For a share that must stay multi-user: `force group`, `create mask = 0664` \n\
            and `directory mask = 0775` on the share, and a matching umask here.",
        path.display()
    )
}

/// Why the watcher stopped. Both are fatal for a serving process.
#[derive(Debug)]
pub enum WatcherExit {
    /// The gallery root went away — an unmount, a delete.
    RootVanished(String),
    /// `notify` itself failed in a way we cannot recover from.
    WatcherFailed(String),
}

/// Arm the watcher.
///
/// Fails only if `notify` itself cannot start; the caller exits non-zero.
pub fn arm(gallery: &Gallery) -> Result<crate::util::fs_watch::FsWatcher, WatcherExit> {
    crate::util::fs_watch::FsWatcher::new(gallery.root.as_path(), true)
        .map_err(|e| WatcherExit::WatcherFailed(e.to_string()))
}

/// Run the watcher loop over an already-armed watcher.
///
/// Arming is separate from running because **the readiness gate must not open
/// until the watcher is listening**. A file arriving between "the gallery is
/// set" and "the watcher is armed" is in neither the completed scan nor the
/// watcher, and nothing ever notices it; with the two steps fused, that window
/// is the entire initial scan.
pub async fn watch(
    gallery: Arc<Gallery>,
    watcher: crate::util::fs_watch::FsWatcher,
) -> WatcherExit {
    let mut added: HashSet<RelPath> = HashSet::new();
    let mut removed: HashSet<RelPath> = HashSet::new();
    let mut companions: HashSet<RelPath> = HashSet::new();
    let mut last_event: Option<tokio::time::Instant> = None;

    loop {
        tokio::time::sleep(POLL).await;

        for result in watcher.poll() {
            let event = match result {
                Ok(e) => e,
                Err(e) => {
                    // inotify watch-limit exhaustion and queue overflow arrive
                    // here, and both make the watcher partially deaf. Loud,
                    // always: silence is what makes "some subtrees stopped
                    // ingesting" undiagnosable.
                    log::error!("filesystem watcher error: {e}");
                    continue;
                }
            };

            for path in &event.paths {
                match classify(&gallery, path, &event.kind) {
                    Classified::Settings => {
                        reload_settings(&gallery).await;
                    }
                    Classified::Ignored => {}
                    Classified::MediaAdded(rel) => {
                        removed.remove(&rel);
                        added.insert(rel);
                        last_event = Some(tokio::time::Instant::now());
                    }
                    Classified::MediaRemoved(rel) => {
                        added.remove(&rel);
                        removed.insert(rel);
                        last_event = Some(tokio::time::Instant::now());
                    }
                    Classified::Companion(rel) => {
                        companions.insert(rel);
                        last_event = Some(tokio::time::Instant::now());
                    }
                }
            }
        }

        if !gallery.root.as_path().exists() {
            return WatcherExit::RootVanished(gallery.root.as_path().display().to_string());
        }

        let quiet = last_event.is_some_and(|t| t.elapsed() >= DEBOUNCE);
        if !quiet || (added.is_empty() && removed.is_empty() && companions.is_empty()) {
            continue;
        }
        last_event = None;

        let batch_added: Vec<RelPath> = added.drain().collect();
        let batch_removed: Vec<RelPath> = removed.drain().collect();
        let batch_companions: Vec<RelPath> = companions.drain().collect();
        flush(&gallery, batch_added, batch_removed, batch_companions).await;
    }
}

/// What one event path is.
enum Classified {
    Settings,
    MediaAdded(RelPath),
    MediaRemoved(RelPath),
    Companion(RelPath),
    Ignored,
}

/// The three skip filters, in the order that matters.
fn classify(gallery: &Gallery, path: &Path, kind: &EventKind) -> Classified {
    let is_write = matches!(kind, EventKind::Create(_) | EventKind::Modify(_));

    // 1. `settings.toml` lives inside `.lightview` but must hot-reload on a
    //    hand edit, so it runs *before* the blanket skip below.
    if path.ends_with("settings.toml") && path.to_string_lossy().contains(".lightview") {
        return if is_write {
            Classified::Settings
        } else {
            Classified::Ignored
        };
    }

    // 2. A companion is inside `.lightview` too, and it is the primary path by
    //    which a tag run on another machine reaches this one. Only a *rewritten*
    //    companion needs `Modify(Data)` watched; media files keep ignoring it.
    let name = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
    if name.ends_with(crate::companion::schema::COMPANION_EXTENSION) {
        if !is_write {
            return Classified::Ignored;
        }
        return match media_for_companion(gallery, path) {
            Some(rel) => Classified::Companion(rel),
            None => Classified::Ignored,
        };
    }

    // 3. Everything else inside `.lightview` — the trash, the cache of old
    //    builds, the lock files — is ours and is not news.
    if path
        .components()
        .any(|c| c.as_os_str() == std::ffi::OsStr::new(".lightview"))
    {
        return Classified::Ignored;
    }

    // 4. The media-extension filter, which is the only reason an upload's own
    //    `.lv-upload-*.tmp` is invisible to the watcher.
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if MediaType::from_extension(ext).is_none() {
        return Classified::Ignored;
    }

    let Ok(rel) = gallery.root.relativize(path) else {
        // Never a silent continue: on a watcher armed on the wrong root this
        // fires for every event in the gallery and looks like nothing
        // happening at all.
        log::warn!("watcher saw a path outside the gallery: {}", path.display());
        return Classified::Ignored;
    };

    match kind {
        EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
            Classified::MediaAdded(rel)
        }
        EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
            Classified::MediaRemoved(rel)
        }
        _ => Classified::Ignored,
    }
}

/// `<dir>/.lightview/companions/<name>.lightview.json` → `<dir>/<name>`.
fn media_for_companion(gallery: &Gallery, companion: &Path) -> Option<RelPath> {
    let name = companion.file_name()?.to_string_lossy().to_string();
    let media_name =
        name.strip_suffix(crate::companion::schema::COMPANION_EXTENSION)?;
    // …/companions/<file> → …/companions → …/.lightview → the media directory
    let media_dir = companion.parent()?.parent()?.parent()?;
    gallery.root.relativize(&media_dir.join(media_name)).ok()
}

/// Apply one debounced batch and tell everyone.
async fn flush(
    gallery: &Gallery,
    added: Vec<RelPath>,
    removed: Vec<RelPath>,
    companions: Vec<RelPath>,
) {
    if !added.is_empty() {
        let root = gallery.root.clone();
        let to_scan = added.clone();
        let scanned = tokio::task::spawn_blocking(move || {
            to_scan
                .into_iter()
                .filter_map(|path| {
                    let absolute = root.resolve(&path).ok()?;
                    let metadata = std::fs::metadata(absolute.as_path()).ok()?;
                    Some(meta::ScannedFile {
                        media_type: media_type_str(&path),
                        file_size: metadata.len() as i64,
                        mtime: metadata
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0),
                        path,
                    })
                })
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();

        {
            let conn = gallery.db.writer().await;
            if let Err(e) = meta::insert_scanned(&conn, &scanned) {
                log::warn!("could not record newly added files: {e}");
            }
        }
        // In the same breath, so a batch arriving with its sidecars is not
        // tagless until a restart.
        for file in &scanned {
            if let Err(e) = index_one(gallery, &file.path).await {
                log::warn!("could not index the companion for {}: {e}", file.path);
            }
        }
    }

    if !removed.is_empty() {
        let conn = gallery.db.writer().await;
        for path in &removed {
            if let Err(e) = db::forget_path(&conn, path) {
                log::warn!("could not forget {path}: {e}");
            }
        }
    }

    let touched_tags = !companions.is_empty() || !added.is_empty();
    for path in &companions {
        if let Err(e) = index_one(gallery, path).await {
            log::warn!("could not re-index the companion for {path}: {e}");
        }
    }

    if !added.is_empty() || !removed.is_empty() {
        // What changed, not everything: one phone upload used to cost every
        // connected client a full-library payload, with the active filter
        // silently dropped on the way.
        gallery.events.send(Event::FsChanged { added, removed });
    }
    if touched_tags {
        gallery.refresh_autocomplete().await;
    }
}

/// Re-read `.lightview/settings.toml` after a hand edit.
async fn reload_settings(gallery: &Gallery) {
    let next = GallerySettings::load(gallery.root.as_path());
    if next != gallery.settings() {
        gallery.set_settings(next);
        gallery.events.send(Event::Resync {
            domains: vec![crate::server::events::Domain::Items],
        });
    }
}

fn media_type_of(path: &RelPath) -> MediaType {
    Path::new(path.as_str())
        .extension()
        .and_then(|e| e.to_str())
        .and_then(MediaType::from_extension)
        .unwrap_or(MediaType::Image)
}

fn media_type_str(path: &RelPath) -> &'static str {
    match media_type_of(path) {
        MediaType::Image => "image",
        MediaType::Video => "video",
        MediaType::Gif => "gif",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_companion_path_maps_back_to_its_media() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("2026/january/.lightview/companions")).unwrap();
        let gallery = test_gallery(d.path());

        let companion = d
            .path()
            .join("2026/january/.lightview/companions/sunset.jpg.lightview.json");
        assert_eq!(
            media_for_companion(&gallery, &companion)
                .map(|p| p.as_str().to_string())
                .as_deref(),
            Some("2026/january/sunset.jpg")
        );
    }

    #[test]
    fn the_settings_branch_runs_before_the_lightview_skip() {
        // Reversed, editing a display preference stops reaching the running
        // process — and the failure is silent.
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".lightview")).unwrap();
        let gallery = test_gallery(d.path());
        let settings = d.path().join(".lightview/settings.toml");

        assert!(matches!(
            classify(&gallery, &settings, &EventKind::Modify(ModifyKind::Any)),
            Classified::Settings
        ));
    }

    #[test]
    fn only_creates_and_rename_targets_count_as_additions() {
        let d = tempfile::tempdir().unwrap();
        let gallery = test_gallery(d.path());
        let media = d.path().join("a.jpg");

        assert!(matches!(
            classify(&gallery, &media, &EventKind::Create(notify::event::CreateKind::File)),
            Classified::MediaAdded(_)
        ));
        assert!(matches!(
            classify(
                &gallery,
                &media,
                &EventKind::Modify(ModifyKind::Name(RenameMode::To))
            ),
            Classified::MediaAdded(_)
        ));
        // A path is immutable within a session: a data write to a media file is
        // not an addition and not an update.
        assert!(matches!(
            classify(&gallery, &media, &EventKind::Modify(ModifyKind::Data(
                notify::event::DataChange::Content
            ))),
            Classified::Ignored
        ));
    }

    #[test]
    fn a_rewritten_companion_is_news_even_though_a_rewritten_photo_is_not() {
        // The primary path by which a `lightview tag` run on another machine
        // reaches this one.
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".lightview/companions")).unwrap();
        let gallery = test_gallery(d.path());
        let companion = d.path().join(".lightview/companions/a.jpg.lightview.json");

        assert!(matches!(
            classify(
                &gallery,
                &companion,
                &EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Content))
            ),
            Classified::Companion(_)
        ));
    }

    #[test]
    fn the_trash_and_an_upload_temp_file_are_both_invisible() {
        let d = tempfile::tempdir().unwrap();
        let gallery = test_gallery(d.path());
        let create = EventKind::Create(notify::event::CreateKind::File);

        for ignored in [
            d.path().join(".lightview/trash/123_0/a.jpg"),
            d.path().join(".lv-upload-1234-abcd.tmp"),
            d.path().join("notes.txt"),
        ] {
            assert!(
                matches!(classify(&gallery, &ignored, &create), Classified::Ignored),
                "{} was not ignored",
                ignored.display()
            );
        }
    }

    fn test_gallery(root: &Path) -> Gallery {
        use crate::autocomplete::engine::AutocompleteEngine;
        use crate::cache::db::CacheDb;
        use crate::pipeline::serve::ThumbService;
        use crate::server::events::Events;

        let cache_dir = tempfile::tempdir().unwrap().keep();
        let root = Root::open(root).unwrap();
        let db = Arc::new(CacheDb::open_at(&cache_dir).unwrap());
        let pool = Arc::new(rayon::ThreadPoolBuilder::new().num_threads(1).build().unwrap());
        let thumbs = Arc::new(ThumbService::new(db.clone(), root.clone(), pool, 1 << 20));
        Gallery {
            root,
            db,
            thumbs,
            events: Arc::new(Events::new()),
            autocomplete: Arc::new(AutocompleteEngine::new()),
            settings: std::sync::RwLock::new(GallerySettings::default()),
            cache_dir,
        }
    }
}
