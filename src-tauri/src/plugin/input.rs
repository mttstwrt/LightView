//! Turning one media item into the request(s) a plugin actually sees, and the
//! several results back into one.
//!
//! Plugins used to receive whatever was on disk. That was wrong in two ways at
//! once, and both are fixed here because they are the same decision — what a
//! plugin's input should look like, made by the host, which is the only party
//! that already owns decoders, a bounded thumbnail pool and (remotely) control
//! over how many bytes cross a network.
//!
//! **Stills are scaled to the size the plugin asked for.** A 448-pixel model
//! handed a 60-megapixel original decodes it in Python and throws away 99% of
//! the pixels. `PluginInputConfig::max_edge` states the longest edge the plugin
//! wants; the remote worker asks for it as `?fit=`, and the local paths
//! materialize a temp file. Never upscales.
//!
//! **Videos become frames, and the plugin never learns they were videos.**
//! Every tagger used to carry `is_video`, `predict_video`, ffmpeg subprocess
//! plumbing and its own sampling policy — four copies that could silently
//! disagree, in scripts on their own release cadence. The host samples
//! `video_frames` frames across the clip, sends them as ordinary still
//! requests, and merges the results with [`merge_parts`]. Shipping whole clips
//! instead was rejected: `?fit=` cannot resize a video, so 64 files on disk
//! would be tens of gigabytes rather than a few hundred KB each.
//!
//! The three drivers — `lightview-worker`, `tagging::local`, and the desktop's
//! `run_plugin_batch` — differ only in how a part is produced (HTTP download
//! vs. local decode) and how progress is reported. Everything between is
//! [`PartTracker`], so a plugin cannot behave differently depending on where it
//! happens to be running.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::manifest::PluginInputConfig;
use super::runner::PluginResult;
use crate::companion::schema::MediaType;

/// Upper bound on `video_frames`, whatever a manifest asks for.
///
/// Load-bearing, not defensive: a remote worker holds one disk permit per part
/// and all of an item's parts must be in flight together, so a manifest asking
/// for more frames than the worker's window would deadlock the job by
/// construction. Well under it, and far past any useful sampling density.
pub const MAX_VIDEO_FRAMES: u32 = 16;

/// Tag prefix the merge treats as an argmax rather than a set.
///
/// A convention of the bundled taggers rather than part of the protocol: they
/// emit exactly one `rating:<name>` chosen as the argmax over a score vector.
/// A plain union across frames would produce up to one rating per frame, which
/// is the one place merging is not free. Plugins that never emit this prefix
/// are unaffected.
const RATING_PREFIX: &str = "rating:";
/// Where those per-class scores live in a result's `meta`.
const RATING_SCORES_KEY: &str = "rating_scores";

/// Which slice of a media file one plugin request covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    /// The file itself, scaled to the plugin's requested edge.
    Whole,
    /// Frame `index` of `count`, evenly spaced across the clip.
    Frame { index: u32, count: u32 },
}

/// What the host does to a plugin's input before the plugin sees it — the
/// concrete behavioural difference between the two protocol versions.
///
/// This is why [`crate::plugin::PLUGIN_API_VERSION`] is worth having at all. A
/// plugin written against version 0 expects original file paths and carries its
/// own ffmpeg sampling; one written against 1 expects prepared stills and never
/// learns a video was involved. Handing either the other's input silently
/// produces wrong results — a version 0 plugin would sample frames of a frame,
/// a version 1 plugin would return nothing for every video — so the host reads
/// the declaration and does what that plugin expects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputPolicy {
    /// Original paths, one request per file, nothing materialized. The pre-1
    /// contract, kept working for plugins that still declare it.
    Passthrough,
    /// Host-prepared input: stills scaled to `max_edge`, videos split into
    /// frames.
    Prepared(PluginInputConfig),
}

impl InputPolicy {
    /// What this host will do for the plugin described by `manifest`.
    pub fn for_manifest(manifest: &super::manifest::PluginManifest) -> Self {
        if manifest.api_version == 0 {
            InputPolicy::Passthrough
        } else {
            InputPolicy::Prepared(manifest.input.clone())
        }
    }

    /// Longest edge to scale stills to; 0 for the original.
    pub fn max_edge(&self) -> u32 {
        match self {
            InputPolicy::Passthrough => 0,
            InputPolicy::Prepared(cfg) => cfg.max_edge,
        }
    }

    /// Whether any request can require a file the host has to produce.
    ///
    /// Drives whether a driver bounds how many requests are in flight: with
    /// nothing materialized there is no disk to bound, and gating on results
    /// would turn `UpFront` delivery into `Incremental` — the exact change that
    /// deadlocks a version 0 plugin.
    pub fn materializes(&self) -> bool {
        match self {
            InputPolicy::Passthrough => false,
            InputPolicy::Prepared(cfg) => cfg.max_edge > 0 || cfg.video_frames > 0,
        }
    }
}

/// How many requests this media file becomes, and what each covers.
///
/// GIFs count as stills deliberately: they already have a frame-atlas path in
/// the thumbnail pipeline, ffmpeg is not involved, and a tagger asked to
/// describe an animated GIF wants the picture, not five samples of a loop.
pub fn plan_parts(path: &Path, policy: &InputPolicy) -> Vec<Part> {
    let InputPolicy::Prepared(cfg) = policy else {
        return vec![Part::Whole];
    };
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if MediaType::from_extension(ext) != Some(MediaType::Video) {
        return vec![Part::Whole];
    }
    let count = cfg.video_frames.clamp(1, MAX_VIDEO_FRAMES);
    (0..count)
        .map(|index| Part::Frame { index, count })
        .collect()
}

/// One media item's merged outcome, ready to be written to a companion file.
#[derive(Debug, Clone, PartialEq)]
pub struct MergedItem {
    pub path: String,
    pub tags: Vec<String>,
    pub meta: Option<serde_json::Value>,
    /// Set when no part produced tags; the item counts as failed.
    pub error: Option<String>,
}

/// Combine a media item's per-part results into one.
///
/// The union is exact rather than approximate. The taggers aggregate frames by
/// taking an element-wise maximum of their score vectors and thresholding once,
/// and `max(s) > T` holds exactly when some `s > T` — so a plain union of the
/// per-frame tag sets reproduces what plugin-side sampling produced, tag for
/// tag. Order is first-seen, which keeps the highest-scoring frame's ranking at
/// the front.
///
/// `rating:` is the exception, and the reason this function exists rather than
/// a one-line `flat_map`. See [`RATING_PREFIX`].
pub fn merge_parts(path: &str, results: Vec<PluginResult>) -> MergedItem {
    if results.len() == 1 {
        // Single-part items — every still — must be passed through untouched.
        // Merging is for videos; a still's tags and meta are the plugin's,
        // verbatim, exactly as before this module existed.
        let r = results.into_iter().next().expect("len checked");
        return MergedItem {
            path: path.to_string(),
            tags: r.tags,
            meta: r.meta,
            error: r.error,
        };
    }

    let mut errors: Vec<String> = Vec::new();
    let mut tags: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut ratings: Vec<String> = Vec::new();
    let mut rating_max: HashMap<String, f64> = HashMap::new();
    let mut base_meta: Option<serde_json::Value> = None;
    let mut contributing = 0usize;

    for result in results {
        if let Some(e) = result.error {
            errors.push(e);
            continue;
        }
        contributing += 1;
        for tag in result.tags {
            if let Some(rating) = tag.strip_prefix(RATING_PREFIX) {
                ratings.push(rating.to_string());
            } else if seen.insert(tag.clone()) {
                tags.push(tag);
            }
        }
        if let Some(meta) = result.meta {
            for (name, score) in scores_in(&meta) {
                let slot = rating_max.entry(name).or_insert(f64::MIN);
                *slot = slot.max(score);
            }
            if base_meta.is_none() {
                base_meta = Some(meta);
            }
        }
    }

    if contributing == 0 {
        return MergedItem {
            path: path.to_string(),
            tags: Vec::new(),
            meta: None,
            error: Some(if errors.is_empty() {
                "no frame produced a result".to_string()
            } else {
                errors.join("; ")
            }),
        };
    }

    // Argmax over the per-frame maxima — the same computation the plugin would
    // have done had it seen every frame itself. With no score vector to work
    // from, fall back to the rating the most frames chose (ties to the first),
    // which is the best a set of bare labels supports.
    let winner = argmax(&rating_max).or_else(|| plurality(&ratings));
    if let Some(name) = winner {
        tags.insert(0, format!("{RATING_PREFIX}{name}"));
    }

    let meta = base_meta.map(|mut meta| {
        if let Some(obj) = meta.as_object_mut() {
            if !rating_max.is_empty() {
                obj.insert(
                    RATING_SCORES_KEY.to_string(),
                    serde_json::json!(rating_max
                        .iter()
                        .map(|(k, v)| (k.clone(), serde_json::json!(v)))
                        .collect::<serde_json::Map<_, _>>()),
                );
            }
            // The count the host actually got, which is not `count` when a
            // frame failed to decode — worth recording, since it is the
            // difference between a well-sampled clip and a lucky one.
            obj.insert("video_frames_sampled".to_string(), serde_json::json!(contributing));
        }
        meta
    });

    MergedItem {
        path: path.to_string(),
        tags,
        meta,
        error: None,
    }
}

fn scores_in(meta: &serde_json::Value) -> Vec<(String, f64)> {
    meta.get(RATING_SCORES_KEY)
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_f64().map(|f| (k.clone(), f)))
                .collect()
        })
        .unwrap_or_default()
}

fn argmax(scores: &HashMap<String, f64>) -> Option<String> {
    scores
        .iter()
        // Ties broken by name so the result does not depend on hash order.
        .max_by(|a, b| a.1.total_cmp(b.1).then_with(|| b.0.cmp(a.0)))
        .map(|(name, _)| name.clone())
}

fn plurality(values: &[String]) -> Option<String> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for v in values {
        *counts.entry(v.as_str()).or_insert(0) += 1;
    }
    // `max_by_key` keeps the *last* maximum, so iterating in reverse makes a
    // tie resolve to the first frame that voted for it.
    values.iter().rev().max_by_key(|v| counts[v.as_str()]).cloned()
}

// ---------------------------------------------------------------------------
// Producing a part locally
// ---------------------------------------------------------------------------

/// Write the bytes for one part into `dir`, returning the file the plugin
/// should be pointed at — or `None` when the original file is already exactly
/// what the plugin asked for and copying it would be pure waste.
///
/// `None` is the common case and worth keeping: a still with no `max_edge`
/// declared is handed over in place, which is what the local paths did before
/// this existed. Blocking work (decode, ffmpeg); callers run it on the
/// thumbnail pool rather than on an async worker thread.
pub fn materialize_part(
    source: &Path,
    part: Part,
    policy: &InputPolicy,
    dir: &Path,
    name: &str,
) -> Result<Option<PathBuf>, String> {
    use crate::pipeline::thumbnailer::{self, ThumbFormat};

    let edge = policy.max_edge();
    match part {
        Part::Whole if edge == 0 => Ok(None),
        Part::Whole => {
            // `generate_for_path_fit` never upscales, so a source already within
            // the requested edge would round-trip through a re-encode for
            // nothing — the common case for a phone-sized image and a
            // 448-pixel model is *not* this, but a folder of small images
            // should not pay a WebP encode each.
            if let Some((w, h)) = thumbnailer::header_dimensions(source) {
                if w.max(h) <= edge {
                    return Ok(None);
                }
            }
            let thumb = thumbnailer::generate_for_path_fit(
                source,
                thumbnailer::filter_for_size(edge),
                ThumbFormat::Webp,
                edge,
            )
            .map_err(|e| e.to_string())?;
            let dest = dir.join(format!("{name}.webp"));
            std::fs::write(&dest, &thumb.data).map_err(|e| e.to_string())?;
            Ok(Some(dest))
        }
        Part::Frame { index, count } => {
            let dest = dir.join(format!("{name}.webp"));
            let data = encode_video_frame(source, index, count, edge)?;
            std::fs::write(&dest, &data).map_err(|e| e.to_string())?;
            Ok(Some(dest))
        }
    }
}

/// Extract frame `index` of `count` and encode it for a plugin.
///
/// Shared with the `/media?frame=` route, which is how a remote worker gets the
/// same frames without ffmpeg of its own. `edge` of 0 means "whatever the clip
/// is" — ffmpeg still scales inside its filter graph when an edge is given, so
/// a 4K clip crosses no pipe at 4K.
pub fn encode_video_frame(
    source: &Path,
    index: u32,
    count: u32,
    edge: u32,
) -> Result<Vec<u8>, String> {
    use crate::pipeline::thumbnailer::{self, ThumbFormat};
    use crate::pipeline::video;

    let info = video::probe(source).map_err(|e| e.to_string())?;
    let at = info.duration.and_then(|d| {
        let times = video::sample_timestamps(d, count.max(1));
        times.get(index as usize).copied()
    });
    // `edge` of 0 asks for the clip's own resolution; `extract_frame_at` reads
    // that as "no downscale" through `cover_dims`.
    let frame = video::extract_frame_at(source, edge, at).map_err(|e| e.to_string())?;
    let target = if edge == 0 { frame.width.max(frame.height) } else { edge };
    thumbnailer::fit_rgba(
        &frame.rgba,
        frame.width,
        frame.height,
        target,
        thumbnailer::filter_for_size(target),
        ThumbFormat::Webp,
    )
    .map_err(|e| e.to_string())
}

/// Max materialized-but-not-yet-tagged files a local driver keeps on disk.
///
/// Smaller than the remote worker's window because there is nothing to hide
/// here: a local part is a decode, not a network round trip, so a deep queue
/// buys no latency and only costs temp space. Zero when nothing is
/// materialized — see [`InputPolicy::materializes`].
pub const MAX_LOCAL_PENDING: usize = 16;

/// What a local driver must release once a part resolves.
pub struct LocalPart {
    /// Temp file to delete. `None` when the plugin was pointed at the original,
    /// which is the whole of the passthrough policy and most of the prepared
    /// one for already-small stills.
    pub temp: Option<PathBuf>,
    _permit: Option<tokio::sync::OwnedSemaphorePermit>,
}

impl LocalPart {
    /// Delete the temp file, if there was one. Dropping the value releases the
    /// in-flight slot.
    pub async fn release(self) {
        if let Some(path) = &self.temp {
            let _ = tokio::fs::remove_file(path).await;
        }
    }
}

/// Feed a plugin's request channel from local files, preparing each part as a
/// slot frees up.
///
/// The local counterpart of the worker's downloader, and the reason the desktop
/// and the in-process executor no longer each have their own idea of what a
/// plugin receives. Under [`InputPolicy::Passthrough`] it is deliberately
/// *not* gated on results: nothing is written to disk, so there is nothing to
/// bound, and gating would convert up-front delivery into incremental delivery
/// — which is precisely what deadlocks a pre-streaming plugin.
///
/// Materialization is blocking (decode, ffmpeg), so it runs on the shared
/// thumbnail pool rather than on an async worker thread. Ending the task drops
/// `req_tx`, which closes the plugin's stdin.
pub fn spawn_local_producer(
    pool: std::sync::Arc<rayon::ThreadPool>,
    paths: Vec<String>,
    policy: InputPolicy,
    temp_dir: PathBuf,
    req_tx: tokio::sync::mpsc::Sender<super::runner::PluginRequest>,
    tracker: std::sync::Arc<tokio::sync::Mutex<PartTracker<LocalPart>>>,
    failed: std::sync::Arc<std::sync::atomic::AtomicUsize>,
) -> tokio::task::JoinHandle<()> {
    use std::sync::atomic::Ordering;
    use tokio::sync::Semaphore;

    tokio::spawn(async move {
        let slots = policy
            .materializes()
            .then(|| std::sync::Arc::new(Semaphore::new(MAX_LOCAL_PENDING)));
        let mut seq: u64 = 0;

        for path in paths {
            let source = PathBuf::from(&path);
            let parts = plan_parts(&source, &policy);
            let item = tracker.lock().await.begin_item(path.clone(), parts.len());

            for part in parts {
                seq += 1;
                let name = format!("{seq:08}");
                let permit = match &slots {
                    Some(s) => match s.clone().acquire_owned().await {
                        Ok(p) => Some(p),
                        Err(_) => return,
                    },
                    None => None,
                };

                let prepared = prepare_on_pool(&pool, &source, part, &policy, &temp_dir, &name).await;
                let request_path = match prepared {
                    Ok(Some(temp)) => temp,
                    // Nothing to prepare: the original *is* what the plugin
                    // asked for.
                    Ok(None) => source.clone(),
                    Err(e) => {
                        log::warn!("{path}: could not prepare input for the plugin: {e}");
                        failed.fetch_add(1, Ordering::Relaxed);
                        tracker.lock().await.part_failed(item);
                        continue;
                    }
                };

                let request_str = request_path.to_string_lossy().to_string();
                // Register before sending: a fast plugin can answer before the
                // next line of this function runs.
                tracker.lock().await.sent(
                    item,
                    &request_str,
                    LocalPart {
                        temp: (request_path != source).then_some(request_path),
                        _permit: permit,
                    },
                );
                let request = super::runner::PluginRequest {
                    action: "tag".to_string(),
                    path: request_str,
                };
                if req_tx.send(request).await.is_err() {
                    return; // plugin died; the result loop reports it
                }
            }
        }
    })
}

/// Run [`materialize_part`] on the thumbnail pool and await the answer.
async fn prepare_on_pool(
    pool: &std::sync::Arc<rayon::ThreadPool>,
    source: &Path,
    part: Part,
    policy: &InputPolicy,
    dir: &Path,
    name: &str,
) -> Result<Option<PathBuf>, String> {
    if matches!(part, Part::Whole) && policy.max_edge() == 0 {
        return Ok(None); // nothing to do; don't occupy a pool thread to say so
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    let (source, policy, dir, name) = (
        source.to_path_buf(),
        policy.clone(),
        dir.to_path_buf(),
        name.to_string(),
    );
    pool.spawn(move || {
        let _ = tx.send(materialize_part(&source, part, &policy, &dir, &name));
    });
    rx.await.unwrap_or_else(|_| Err("input preparation was dropped".to_string()))
}

// ---------------------------------------------------------------------------
// Tracking parts back to items
// ---------------------------------------------------------------------------

/// A part handed to the plugin and not yet answered.
struct InFlight<P> {
    item: u64,
    /// Whatever the driver needs to release when the part resolves — a disk
    /// permit for the worker, a temp file for the local paths, `()` when the
    /// original file was handed over in place.
    pub payload: P,
    sent_at: std::time::Instant,
    results_when_sent: u64,
}

struct Item {
    path: String,
    /// Parts registered but not yet resolved. The item is finished at zero.
    outstanding: usize,
    results: Vec<PluginResult>,
}

/// Bookkeeping between "a request went to the plugin" and "this media item is
/// done", shared by all three drivers.
///
/// Parameterised on the payload each driver attaches to an in-flight part, the
/// way the frontend's thumb queue is parameterised on its own — the worker
/// carries a disk permit and a temp file, the local paths carry a temp file or
/// nothing at all.
///
/// Completed items land in one queue rather than being returned from whichever
/// call happened to finish them, because the call that finishes an item is
/// often on the *producer* task (a download that failed, a frame that would not
/// decode) and it has nowhere to hand a result. The driver drains the queue.
///
/// Keys are the request *file name*, not the full path. A plugin that
/// canonicalizes its input echoes back a different string for the same file — a
/// symlinked `TMPDIR` is enough — and an unmatched result strands its part.
/// Names are unique within a job, so this identifies the file exactly while
/// surviving any amount of directory rewriting.
pub struct PartTracker<P> {
    in_flight: HashMap<String, InFlight<P>>,
    items: HashMap<u64, Item>,
    completed: Vec<MergedItem>,
    next_item: u64,
    results_matched: u64,
}

impl<P> Default for PartTracker<P> {
    fn default() -> Self {
        PartTracker {
            in_flight: HashMap::new(),
            items: HashMap::new(),
            completed: Vec::new(),
            next_item: 0,
            results_matched: 0,
        }
    }
}

impl<P> PartTracker<P> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare a media item and how many parts it will be split into. Returns
    /// the item id to pass to [`Self::sent`] / [`Self::part_failed`].
    ///
    /// The full count is registered up front, before any part is produced, so
    /// an item whose first frame comes back before its last frame is requested
    /// cannot be finalized early.
    pub fn begin_item(&mut self, path: String, parts: usize) -> u64 {
        let id = self.next_item;
        self.next_item += 1;
        if parts == 0 {
            // A media file that expands to nothing still has to be accounted
            // for, or the job's counts never add up to its total.
            self.completed.push(MergedItem {
                path,
                tags: Vec::new(),
                meta: None,
                error: Some("nothing to send the plugin for this file".to_string()),
            });
            return id;
        }
        self.items.insert(
            id,
            Item {
                path,
                outstanding: parts,
                results: Vec::new(),
            },
        );
        id
    }

    /// Record that a part of `item` has been handed to the plugin as
    /// `request_path`.
    pub fn sent(&mut self, item: u64, request_path: &str, payload: P) {
        self.in_flight.insert(
            request_key(request_path),
            InFlight {
                item,
                payload,
                sent_at: std::time::Instant::now(),
                results_when_sent: self.results_matched,
            },
        );
    }

    /// A part never reached the plugin (it could not be downloaded, decoded, or
    /// extracted). Resolves the part without a result.
    pub fn part_failed(&mut self, item: u64) {
        self.resolve(item, None);
    }

    /// Match a plugin result back to its item, returning what the part was
    /// holding. `None` means the result could not be attributed — it must not
    /// count as progress, since that is the signature of the wedge the drivers'
    /// silence timers exist to catch.
    pub fn result(&mut self, result: PluginResult) -> Option<P> {
        let entry = self.in_flight.remove(&request_key(&result.path))?;
        self.results_matched += 1;
        self.resolve(entry.item, Some(result));
        Some(entry.payload)
    }

    /// Parts the plugin is never going to answer for, with the payload each was
    /// holding. See [`is_stale`] for the rules; this is what keeps a bounded
    /// in-flight window from becoming a job-size limit.
    pub fn take_stale(&mut self, plugin_idle: Duration) -> Vec<(String, P)> {
        let matched = self.results_matched;
        let has_answered = matched > 0;
        let keys: Vec<String> = self
            .in_flight
            .iter()
            .filter(|(_, e)| {
                is_stale(
                    matched.saturating_sub(e.results_when_sent),
                    e.sent_at.elapsed(),
                    plugin_idle,
                    has_answered,
                )
            })
            .map(|(k, _)| k.clone())
            .collect();

        let mut out = Vec::new();
        for key in keys {
            if let Some(entry) = self.in_flight.remove(&key) {
                self.resolve(entry.item, None);
                out.push((key, entry.payload));
            }
        }
        out
    }

    /// Give up on everything still outstanding — the plugin's stream ended
    /// while parts were unanswered. Without this those items would simply never
    /// be written, which is how a plugin that silently drops a request loses a
    /// file's tags with no error anywhere.
    pub fn drain_unanswered(&mut self) -> Vec<P> {
        let keys: Vec<String> = self.in_flight.keys().cloned().collect();
        let mut out = Vec::new();
        for key in keys {
            if let Some(entry) = self.in_flight.remove(&key) {
                self.resolve(entry.item, None);
                out.push(entry.payload);
            }
        }
        out
    }

    /// Media items whose every part has resolved, ready to be written.
    pub fn take_completed(&mut self) -> Vec<MergedItem> {
        std::mem::take(&mut self.completed)
    }

    pub fn in_flight_count(&self) -> usize {
        self.in_flight.len()
    }

    pub fn results_matched(&self) -> u64 {
        self.results_matched
    }

    fn resolve(&mut self, item: u64, result: Option<PluginResult>) {
        let Some(entry) = self.items.get_mut(&item) else {
            return;
        };
        entry.outstanding = entry.outstanding.saturating_sub(1);
        if let Some(r) = result {
            entry.results.push(r);
        }
        if entry.outstanding > 0 {
            return;
        }
        let Some(finished) = self.items.remove(&item) else {
            return;
        };
        self.completed.push(if finished.results.is_empty() {
            MergedItem {
                path: finished.path,
                tags: Vec::new(),
                meta: None,
                error: Some("the plugin returned no result for this file".to_string()),
            }
        } else {
            merge_parts(&finished.path, finished.results)
        });
    }
}

/// Key a request by its file name — see [`PartTracker`].
fn request_key(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

/// A part is abandoned once the plugin has answered this many *other* requests
/// since it was sent.
///
/// A count rather than a clock, deliberately: a slow tagger legitimately leaves
/// a file queued for a long time, so a wall-clock per-part deadline is either
/// too tight for a CPU-only model or too loose to be useful. No plugin reorders
/// across more requests than can be outstanding at once, so watching this many
/// others get answered means this one was skipped.
pub const STALE_AFTER_RESULTS: u64 = 128;
/// A part outstanding this long while the plugin sits idle is abandoned too.
///
/// The count rule cannot fire at the tail of a job — if the plugin skips the
/// last few requests, no further results arrive to count. Only applies once the
/// plugin has produced something, so a first-run model download (minutes of
/// silence with every slot full) is never mistaken for a wedge.
pub const IDLE_RECLAIM: Duration = Duration::from_secs(5 * 60);

/// Should a request the plugin has not answered be given up on?
///
/// This is what lets a job of any size be started and walked away from. A
/// plugin that skips a request, or answers with a path that cannot be keyed
/// back, used to strand that part's slot for the rest of the job; enough of
/// them and the driver's in-flight window closed permanently, which read as
/// "batches over 64 images hang". Giving up costs one failed image and returns
/// the slot.
pub fn is_stale(
    results_since_sent: u64,
    outstanding: Duration,
    plugin_idle: Duration,
    plugin_has_answered: bool,
) -> bool {
    if results_since_sent >= STALE_AFTER_RESULTS {
        return true;
    }
    plugin_has_answered && plugin_idle >= IDLE_RECLAIM && outstanding >= IDLE_RECLAIM
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(path: &str, tags: &[&str]) -> PluginResult {
        PluginResult {
            path: path.to_string(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            meta: None,
            error: None,
        }
    }

    fn rated(path: &str, tags: &[&str], scores: &[(&str, f64)]) -> PluginResult {
        let map: serde_json::Map<String, serde_json::Value> = scores
            .iter()
            .map(|(k, v)| (k.to_string(), serde_json::json!(v)))
            .collect();
        PluginResult {
            meta: Some(serde_json::json!({ "model": "m", RATING_SCORES_KEY: map })),
            ..result(path, tags)
        }
    }

    fn prepared(max_edge: u32, video_frames: u32) -> InputPolicy {
        InputPolicy::Prepared(PluginInputConfig { max_edge, video_frames })
    }

    #[test]
    fn stills_are_one_part_and_videos_are_several() {
        let policy = prepared(448, 5);
        assert_eq!(plan_parts(Path::new("/g/a.jpg"), &policy), vec![Part::Whole]);
        assert_eq!(plan_parts(Path::new("/g/a.gif"), &policy), vec![Part::Whole]);
        assert_eq!(plan_parts(Path::new("/g/a.mp4"), &policy).len(), 5);
        assert_eq!(
            plan_parts(Path::new("/g/a.MOV"), &policy)[2],
            Part::Frame { index: 2, count: 5 }
        );
    }

    /// A plugin on the pre-1 contract does its own video sampling, so splitting
    /// a clip for it would hand it frames of a frame.
    #[test]
    fn a_passthrough_plugin_still_receives_whole_videos() {
        assert_eq!(
            plan_parts(Path::new("/g/a.mp4"), &InputPolicy::Passthrough),
            vec![Part::Whole]
        );
        assert!(!InputPolicy::Passthrough.materializes());
        assert_eq!(InputPolicy::Passthrough.max_edge(), 0);
    }

    /// A manifest asking for more frames than a driver's in-flight window would
    /// deadlock the job, so the clamp is correctness rather than politeness.
    #[test]
    fn the_frame_count_is_clamped() {
        assert_eq!(
            plan_parts(Path::new("/g/a.mp4"), &prepared(0, 10_000)).len(),
            MAX_VIDEO_FRAMES as usize
        );
        assert_eq!(plan_parts(Path::new("/g/a.mp4"), &prepared(0, 0)).len(), 1);
    }

    /// The property that makes host-side sampling safe: for everything except
    /// the rating, a union reproduces the element-wise-max-then-threshold the
    /// plugins did internally.
    #[test]
    fn frame_tags_are_unioned_in_first_seen_order() {
        let merged = merge_parts(
            "/g/clip.mp4",
            vec![
                result("0.webp", &["1girl", "outdoors"]),
                result("1.webp", &["outdoors", "night"]),
            ],
        );
        assert_eq!(merged.tags, vec!["1girl", "outdoors", "night"]);
        assert!(merged.error.is_none());
        assert_eq!(merged.path, "/g/clip.mp4");
    }

    /// The one part of merging that is not free: a union would give a clip up
    /// to one rating per frame where there should be exactly one.
    #[test]
    fn one_rating_survives_a_merge_chosen_by_argmax() {
        let merged = merge_parts(
            "/g/clip.mp4",
            vec![
                rated("0.webp", &["rating:general", "sky"], &[("general", 0.9), ("explicit", 0.1)]),
                rated("1.webp", &["rating:explicit", "sky"], &[("general", 0.2), ("explicit", 0.95)]),
            ],
        );
        assert_eq!(merged.tags, vec!["rating:explicit", "sky"]);
        // The merged scores are the per-frame maxima, so a re-merge is stable.
        let scores = merged.meta.as_ref().unwrap()[RATING_SCORES_KEY].clone();
        assert_eq!(scores["general"], serde_json::json!(0.9));
        assert_eq!(scores["explicit"], serde_json::json!(0.95));
        assert_eq!(merged.meta.as_ref().unwrap()["video_frames_sampled"], 2);
        // Unrelated meta from the first contributing frame is preserved.
        assert_eq!(merged.meta.as_ref().unwrap()["model"], "m");
    }

    /// With no score vector there is nothing to argmax, so the frames vote.
    #[test]
    fn ratings_without_scores_fall_back_to_a_plurality() {
        let merged = merge_parts(
            "/g/clip.mp4",
            vec![
                result("0.webp", &["rating:sensitive"]),
                result("1.webp", &["rating:general"]),
                result("2.webp", &["rating:general"]),
            ],
        );
        assert_eq!(merged.tags, vec!["rating:general"]);
    }

    /// A still must come out of the merge byte-identical to what the plugin
    /// emitted — this path runs for every image in every job.
    #[test]
    fn a_single_part_item_is_passed_through_untouched() {
        let mut r = rated("0.webp", &["rating:general", "sky"], &[("general", 0.9)]);
        r.path = "0.webp".into();
        let merged = merge_parts("/g/a.jpg", vec![r.clone()]);
        assert_eq!(merged.tags, r.tags);
        assert_eq!(merged.meta, r.meta);
        assert_eq!(merged.path, "/g/a.jpg");
    }

    #[test]
    fn a_clip_whose_frames_all_failed_is_an_error() {
        let mut a = result("0.webp", &[]);
        a.error = Some("decode failed".into());
        let mut b = result("1.webp", &[]);
        b.error = Some("decode failed".into());
        let merged = merge_parts("/g/clip.mp4", vec![a, b]);
        assert!(merged.error.unwrap().contains("decode failed"));
        assert!(merged.tags.is_empty());
    }

    /// Some frames failing is not the clip failing — a fade to black at one
    /// sample point should not cost the whole file its tags.
    #[test]
    fn a_clip_with_one_bad_frame_still_produces_tags() {
        let mut bad = result("0.webp", &[]);
        bad.error = Some("no frame".into());
        let merged = merge_parts("/g/clip.mp4", vec![bad, result("1.webp", &["sky"])]);
        assert_eq!(merged.tags, vec!["sky"]);
        assert!(merged.error.is_none());
    }

    #[test]
    fn an_item_completes_only_when_every_part_has_resolved() {
        let mut t: PartTracker<u32> = PartTracker::new();
        let item = t.begin_item("/g/clip.mp4".into(), 3);
        for i in 0..3u32 {
            t.sent(item, &format!("/tmp/x/{i}.webp"), i);
        }
        assert_eq!(t.in_flight_count(), 3);

        assert_eq!(t.result(result("/tmp/x/0.webp", &["a"])), Some(0));
        assert!(t.take_completed().is_empty(), "completed before every frame answered");
        t.result(result("/tmp/x/1.webp", &["b"])).unwrap();
        assert!(t.take_completed().is_empty());
        t.result(result("/tmp/x/2.webp", &["c"])).unwrap();

        let done = t.take_completed();
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].path, "/g/clip.mp4");
        assert_eq!(done[0].tags, vec!["a", "b", "c"]);
        assert_eq!(t.in_flight_count(), 0);
    }

    /// A frame that never downloaded resolves its part from the producer task,
    /// which has nowhere to hand a merged item — hence the queue.
    #[test]
    fn a_part_that_never_reached_the_plugin_still_completes_its_item() {
        let mut t: PartTracker<()> = PartTracker::new();
        let item = t.begin_item("/g/clip.mp4".into(), 2);
        t.sent(item, "/tmp/x/0.webp", ());
        t.part_failed(item);
        assert!(t.take_completed().is_empty());
        t.result(result("/tmp/x/0.webp", &["sky"])).unwrap();
        let done = t.take_completed();
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].tags, vec!["sky"], "one bad frame must not lose the clip");
    }

    /// A plugin canonicalizing its input under a symlinked TMPDIR echoes back a
    /// different string for the same file; the file name still identifies it.
    #[test]
    fn results_match_through_a_rewritten_directory() {
        let mut t: PartTracker<()> = PartTracker::new();
        let item = t.begin_item("/g/a.jpg".into(), 1);
        t.sent(item, "/tmp/lv/000007.webp", ());
        t.result(result("/private/var/tmp/lv/000007.webp", &["x"]))
            .expect("should match on file name");
        assert_eq!(t.take_completed()[0].tags, vec!["x"]);
    }

    /// An unmatched result must not register as progress: it is the symptom of
    /// the wedge, not evidence against it.
    #[test]
    fn an_unmatched_result_is_rejected_and_counts_for_nothing() {
        let mut t: PartTracker<()> = PartTracker::new();
        let item = t.begin_item("/g/a.jpg".into(), 1);
        t.sent(item, "/tmp/lv/1.webp", ());
        assert!(t.result(result("/tmp/lv/999.webp", &["x"])).is_none());
        assert_eq!(t.results_matched(), 0);
        assert_eq!(t.in_flight_count(), 1);
        assert!(t.take_completed().is_empty());
    }

    #[test]
    fn stale_parts_are_reclaimed_and_complete_their_item() {
        let mut t: PartTracker<&str> = PartTracker::new();
        // Answer enough other requests that the first is definitively skipped.
        let skipped = t.begin_item("/g/skipped.jpg".into(), 1);
        t.sent(skipped, "/tmp/lv/0.webp", "permit-0");
        for i in 1..=STALE_AFTER_RESULTS {
            let item = t.begin_item(format!("/g/{i}.jpg"), 1);
            let path = format!("/tmp/lv/{i}.webp");
            t.sent(item, &path, "p");
            t.result(result(&path, &["ok"])).unwrap();
        }
        t.take_completed();

        let stale = t.take_stale(Duration::ZERO);
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].1, "permit-0");
        let done = t.take_completed();
        assert_eq!(done[0].path, "/g/skipped.jpg");
        assert!(done[0].error.is_some());
        assert_eq!(t.in_flight_count(), 0);
    }

    /// Before the plugin has answered anything — a first-run model download —
    /// nothing is reclaimed however long every slot has been held.
    #[test]
    fn nothing_is_reclaimed_while_the_plugin_has_never_answered() {
        let mut t: PartTracker<()> = PartTracker::new();
        let item = t.begin_item("/g/a.jpg".into(), 1);
        t.sent(item, "/tmp/lv/0.webp", ());
        assert!(t.take_stale(Duration::from_secs(3600)).is_empty());
    }

    /// The plugin exited with requests unanswered: those files must be
    /// reported, not silently dropped.
    #[test]
    fn draining_reports_everything_left_outstanding() {
        let mut t: PartTracker<()> = PartTracker::new();
        let a = t.begin_item("/g/a.jpg".into(), 1);
        t.sent(a, "/tmp/lv/0.webp", ());
        assert_eq!(t.drain_unanswered().len(), 1);
        let done = t.take_completed();
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].path, "/g/a.jpg");
        assert!(done[0].error.is_some());
    }
}
