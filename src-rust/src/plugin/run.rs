//! Driving one plugin over one selection.
//!
//! This is the part a caller sees: a plugin name, a list of paths, and a
//! progress sink. Everything else — the subprocess, the pending window, the
//! temp directory, frame sampling, merging — is [`super::runner`] and
//! [`super::input`].
//!
//! **A run is resumable because writes are per-file and idempotent.**
//! Companions are written as results arrive, so an interrupted run — Ctrl-C, a
//! crash, a closed laptop — leaves the files it finished finished. Re-run it
//! and the already-tagged files are skipped. That is the whole recovery story,
//! and it is why there is no requeueing, no claim expiry and no partially
//! applied batch to reason about.
//!
//! **The skip predicate is "this version or higher", and it is checked twice.**
//! Once while planning, to decide what to send; and again under the companion
//! lock immediately before writing, because a run over twenty thousand files
//! takes hours and another process — `lightview tag` over the share, a phone
//! adding a tag — may have touched the file in between. The plan's answer is a
//! hint; the answer under the lock is the decision.
//!
//! Why *version or higher* rather than equality: a retrained model ships as a
//! version bump, and a gallery tagged by v1 must re-tag under v2 while one
//! tagged by v2 must not be dragged backwards by an older install.

use std::path::PathBuf;
use std::sync::Arc;

use crate::companion::schema::{MediaType, PluginTagEntry};
use crate::companion::writer::{modify_companion, Outcome, WriteError};
use crate::path::RelPath;
use crate::plugin::input::{self, MergedItem, Part, PartResult};
use crate::plugin::manifest::Installed;
use crate::plugin::runner::{RunError, Session};
use crate::state::Gallery;

/// How many finished items to index in one pass.
///
/// Writes are per-file and immediate; this batches only the *index* update,
/// which takes the single writer connection. One transaction per file would
/// take it thousands of times for a library-sized run.
const APPLY_BATCH: usize = 32;

/// What a finished run reports.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Report {
    pub tagged: usize,
    pub skipped: usize,
    pub failed: usize,
}

/// One item's plan: which parts to send, and where the answers go.
struct Item {
    path: RelPath,
    media_type: MediaType,
    /// Temp file names still outstanding for this item.
    outstanding: usize,
    results: Vec<PartResult>,
}

/// Run `plugin` over `paths`, reporting progress as `(done, total)`.
///
/// The progress closure is the only thing that differs between the UI's run and
/// `lightview tag`'s, which is why it is a closure: a trait with two
/// implementations would be a plugin point for a second consumer that does not
/// exist.
pub async fn run(
    gallery: &Arc<Gallery>,
    plugin: &Installed,
    paths: &[RelPath],
    mut progress: impl FnMut(usize, usize),
) -> Result<Report, RunError> {
    let mut report = Report::default();

    // Plan first, so the plugin is told a true `LIGHTVIEW_JOB_TOTAL` and a run
    // over an already-tagged gallery costs one companion read per file rather
    // than a subprocess that is started and immediately has nothing to do.
    let mut items = Vec::new();
    for path in paths {
        if already_tagged(gallery, plugin, path) {
            report.skipped += 1;
            continue;
        }
        items.push(Item {
            path: path.clone(),
            media_type: media_type_for(path),
            outstanding: 0,
            results: Vec::new(),
        });
    }
    let total = items.len();
    progress(0, total);
    if total == 0 {
        return Ok(report);
    }

    // The scratch directory the plugin reads from. Removed when this binding
    // drops, which is every path out of this function including a panic.
    //
    // Inside the gallery's **cache** directory rather than `/tmp`: a run holds
    // up to `MAX_PENDING` images at up to 2560px, and `/tmp` is a small tmpfs
    // on most systems. The cache directory is already budgeted, already known
    // writable, and on the same filesystem as the tiers these are derived
    // from.
    let scratch = tempfile::Builder::new()
        .prefix("plugin-")
        .tempdir_in(&gallery.cache_dir)?;

    let max_edge = plugin.manifest.input.max_edge;
    let frames = plugin.video_frames();
    let mut session = Session::start(plugin, total)?;

    // Parts are materialized as the window frees, not up front: each one costs
    // a decode and a file on disk, and the window is what bounds both.
    let mut next_item = 0usize;
    let mut pending_item: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut finished = 0usize;
    let mut applied: Vec<(RelPath, MergedItem)> = Vec::new();

    let result = 'run: loop {
        // Fill the window.
        while session.has_room() && next_item < total {
            let index = next_item;
            next_item += 1;
            let parts = match materialize(gallery, &items[index], scratch.path(), index, max_edge, frames).await {
                Ok(parts) if !parts.is_empty() => parts,
                // Nothing readable: the item costs itself and nothing else.
                _ => {
                    report.failed += 1;
                    finished += 1;
                    progress(finished, total);
                    continue;
                }
            };
            items[index].outstanding = parts.len();
            let mut send_failed = None;
            for part in &parts {
                pending_item.insert(part.name.clone(), index);
                if let Err(e) = session.send(part).await {
                    send_failed = Some(e);
                    break;
                }
            }
            // A write to a plugin that has exited is the end of the run, not
            // of this item: there is nothing left to send anything to.
            if let Some(e) = send_failed {
                break 'run Err(e);
            }
        }
        if next_item >= total && session.pending() == 0 && finished >= total {
            break Ok(());
        }
        if next_item >= total {
            // No more requests are coming. A well-behaved plugin waits on
            // stdin forever otherwise.
            let _ = session.finish_sending().await;
        }

        let answer = match session.next_answer().await {
            Ok(Some(a)) => a,
            Ok(None) => break Ok(()),
            Err(e) => break Err(e),
        };
        let Some(&index) = pending_item.get(&answer.name) else {
            continue;
        };
        pending_item.remove(&answer.name);

        let item = &mut items[index];
        item.results.push(answer.result);
        item.outstanding -= 1;
        if item.outstanding > 0 {
            continue;
        }

        let merged = input::merge(&item.results);
        if merged.error.is_some() {
            report.failed += 1;
        } else {
            applied.push((item.path.clone(), merged));
        }
        finished += 1;
        progress(finished, total);

        if applied.len() >= APPLY_BATCH {
            report.tagged += apply(gallery, plugin, std::mem::take(&mut applied)).await;
        }
    };

    // Whatever happened, what finished is written: a stalled run still keeps
    // the work it did, which is the whole resumability argument.
    report.tagged += apply(gallery, plugin, applied).await;
    session.shutdown().await;
    gallery.refresh_autocomplete().await;

    result.map(|()| report)
}

/// Build the temp files for one item.
///
/// A still is one part; a clip is `frames` stills sampled across it. The name
/// carries the item index so two files called `IMG_0001.jpg` in different
/// folders cannot collide in the scratch directory.
async fn materialize(
    gallery: &Arc<Gallery>,
    item: &Item,
    scratch: &std::path::Path,
    index: usize,
    max_edge: u32,
    frames: u32,
) -> Result<Vec<Part>, std::io::Error> {
    let mut bytes = Vec::new();
    if item.media_type == MediaType::Video {
        let Ok(absolute) = gallery.root.resolve(&item.path) else {
            return Ok(Vec::new());
        };
        let absolute = absolute.as_path().to_path_buf();
        bytes = tokio::task::spawn_blocking(move || input::video_parts(&absolute, max_edge, frames))
            .await
            .unwrap_or_default();
    } else if let Some(one) = input::image_bytes(&gallery.thumbs, &item.path, max_edge).await {
        bytes.push(one);
    }

    let mut parts = Vec::with_capacity(bytes.len());
    for (n, body) in bytes.into_iter().enumerate() {
        let name = format!("{index}-{n}.webp");
        let path: PathBuf = scratch.join(&name);
        tokio::fs::write(&path, &body).await?;
        parts.push(Part { name, path });
    }
    Ok(parts)
}

/// Write a batch of finished items: companion first, index second.
async fn apply(
    gallery: &Arc<Gallery>,
    plugin: &Installed,
    batch: Vec<(RelPath, MergedItem)>,
) -> usize {
    let mut written = 0;
    for (path, merged) in batch {
        let Ok(absolute) = gallery.root.resolve(&path) else {
            continue;
        };
        let absolute = absolute.as_path().to_path_buf();
        let media_type = media_type_for(&path);
        let name = plugin.manifest.tag_prefix.clone();
        let version = plugin.manifest.version.clone();
        let tags = merged.tags.clone();
        let meta = merged.meta.clone();

        let wrote = tokio::task::spawn_blocking(move || {
            modify_companion(&absolute, media_type, |companion| {
                // The second check, under the lock. The plan's answer is
                // hours old by now on a large run.
                if let Some(existing) = companion.tags.plugins.get(&name)
                    && version_at_least(&existing.version, &version)
                {
                    return Outcome::Leave(false);
                }
                // The bucket is replaced wholesale, which is what makes a
                // re-run under a newer version a *re-tag* rather than a union
                // with what the old model thought.
                companion.tags.plugins.insert(
                    name.clone(),
                    PluginTagEntry {
                        version: version.clone(),
                        tags: tags.clone(),
                        ..Default::default()
                    },
                );
                if let Some(meta) = meta.clone() {
                    companion.meta.plugins.insert(name.clone(), meta);
                }
                Outcome::Write(true)
            })
        })
        .await
        .unwrap_or(Err(WriteError::Io(std::io::Error::other("join failed"))));

        match wrote {
            Ok(true) => {
                written += 1;
                if let Err(e) = crate::services::gallery::index_one(gallery, &path).await {
                    log::warn!("could not index {}: {e}", path.as_str());
                }
            }
            Ok(false) => {}
            Err(e) => log::warn!("could not write {}: {e}", path.as_str()),
        }
    }
    written
}

/// Whether this file already carries tags from this plugin at this version or
/// newer. The planning-time half of the predicate; [`apply`] repeats it under
/// the lock.
fn already_tagged(gallery: &Arc<Gallery>, plugin: &Installed, path: &RelPath) -> bool {
    let Ok(absolute) = gallery.root.resolve(path) else {
        return false;
    };
    let Ok(Some(companion)) = crate::companion::reader::read_companion(absolute.as_path()) else {
        return false;
    };
    companion
        .tags
        .plugins
        .get(&plugin.manifest.tag_prefix)
        .is_some_and(|e| version_at_least(&e.version, &plugin.manifest.version))
}

/// Is `have` at least `want`, comparing dotted numeric components?
///
/// Non-numeric components compare as text, and a missing component is zero, so
/// `1.2` is at least `1.2.0`. An unparseable version on either side falls back
/// to string equality rather than guessing — the cost of being wrong is one
/// wasted re-tag, and the alternative is silently never re-tagging.
fn version_at_least(have: &str, want: &str) -> bool {
    let parts = |v: &str| -> Vec<Option<u64>> {
        v.split('.').map(|p| p.trim().parse::<u64>().ok()).collect()
    };
    let (h, w) = (parts(have), parts(want));
    if h.iter().any(Option::is_none) || w.iter().any(Option::is_none) {
        return have == want;
    }
    for i in 0..h.len().max(w.len()) {
        let a = h.get(i).and_then(|x| *x).unwrap_or(0);
        let b = w.get(i).and_then(|x| *x).unwrap_or(0);
        if a != b {
            return a > b;
        }
    }
    true
}

fn media_type_for(path: &RelPath) -> MediaType {
    std::path::Path::new(path.as_str())
        .extension()
        .and_then(|e| e.to_str())
        .and_then(MediaType::from_extension)
        .unwrap_or(MediaType::Image)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_skip_predicate_is_version_or_higher() {
        // The whole reason a retrained model ships as a version bump.
        assert!(version_at_least("1.0.0", "1.0.0"));
        assert!(version_at_least("1.1.0", "1.0.0"));
        assert!(version_at_least("2.0.0", "1.9.9"));
        assert!(!version_at_least("1.0.0", "1.1.0"));
        assert!(!version_at_least("0.9.0", "1.0.0"));
    }

    #[test]
    fn a_missing_component_is_zero() {
        assert!(version_at_least("1.2", "1.2.0"));
        assert!(version_at_least("1.2.1", "1.2"));
        assert!(!version_at_least("1.2", "1.2.1"));
    }

    #[test]
    fn an_unparseable_version_falls_back_to_equality() {
        // Being wrong here costs one re-tag. Guessing an ordering for
        // "2.0-rc1" against "2.0" could cost a gallery its re-tag forever.
        assert!(version_at_least("2.0-rc1", "2.0-rc1"));
        assert!(!version_at_least("2.0-rc1", "1.0.0"));
        assert!(!version_at_least("2.0.0", "2.0-rc1"));
    }
}
