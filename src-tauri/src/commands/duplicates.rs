//! Duplicate detection and the metadata-preserving merge.
//!
//! `find_duplicates_impl` computes any missing perceptual hashes before
//! grouping, so opening the panel on a cold gallery makes progress rather than
//! returning nothing.
//!
//! `merge_duplicates` takes a *fully resolved* plan: the dialog decides every
//! conflict, and this side applies it. That split is deliberate — conflict
//! resolution is a UI question with no correct default, and putting it here
//! would mean guessing. The merge writes the keeper's companion and optionally
//! its mtime, then trashes the discards through the ordinary capability-gated
//! trash path; it never rewrites image bytes. See `docs/duplicates/README.md`
//! for why EXIF is out of scope and GPS is the exception.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::cache::duplicates::DuplicateGroup;
use crate::commands::tags::modify_companion;
use crate::companion::reader;
use crate::companion::schema::{CoreMeta, Location};
use crate::pipeline::exif;
use crate::AppState;

/// Hashes computed per DB-lock acquisition during a scan. Small enough that a
/// web request queued behind a batch waits milliseconds, not the whole scan.
const PHASH_BATCH: usize = 64;

#[derive(Debug, Serialize)]
pub struct FindDuplicatesResult {
    /// Number of perceptual hashes newly computed in this run.
    pub hashes_computed: usize,
    /// Groups of duplicate images (each group has 2+ items with metadata).
    pub groups: Vec<DuplicateGroup>,
}

/// Scan all thumbnails for visually similar images using perceptual hashing.
///
/// `threshold` controls sensitivity: 0 = exact duplicates only, higher values
/// catch resized/recompressed variants (recommended: 5–10).
///
/// Hashing runs in batches, releasing the cache-DB lock between them, so a
/// first scan over a large gallery doesn't block other commands (the idle
/// worker usually pre-hashes everything anyway).
pub async fn find_duplicates_impl(
    state: &AppState,
    threshold: Option<u32>,
) -> Result<FindDuplicatesResult, String> {
    let threshold = threshold.unwrap_or(8);

    let mut hashes_computed = 0;
    loop {
        let db = state.cache_db.lock().await;
        let db = db.as_ref().ok_or("No gallery open")?;
        let computed = db.compute_phashes_batch(PHASH_BATCH).map_err(|e| e.to_string())?;
        hashes_computed += computed;
        if computed < PHASH_BATCH {
            break;
        }
    }

    let db = state.cache_db.lock().await;
    let db = db.as_ref().ok_or("No gallery open")?;
    let groups = db.find_duplicates(threshold).map_err(|e| e.to_string())?;

    Ok(FindDuplicatesResult {
        hashes_computed,
        groups,
    })
}

#[tauri::command]
pub async fn find_duplicates(
    state: tauri::State<'_, AppState>,
    threshold: Option<u32>,
) -> Result<FindDuplicatesResult, String> {
    find_duplicates_impl(&state, threshold).await
}

/// Mark a set of paths as confirmed non-duplicates. Every pair within the
/// set is recorded so future `find_duplicates` runs skip the union for those
/// pairs and the group stays dismissed.
pub async fn mark_not_duplicates_impl(
    state: &AppState,
    paths: Vec<String>,
) -> Result<usize, String> {
    let db = state.cache_db.lock().await;
    let db = db.as_ref().ok_or("No gallery open")?;
    db.mark_not_duplicates(&paths).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn mark_not_duplicates(
    state: tauri::State<'_, AppState>,
    paths: Vec<String>,
) -> Result<usize, String> {
    mark_not_duplicates_impl(&state, paths).await
}

// ---------------------------------------------------------------------------
// Merge
// ---------------------------------------------------------------------------

/// GPS coordinates for the merge UI (Location without serde attribute noise).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct MergeGps {
    pub lat: f64,
    pub lon: f64,
    pub alt: Option<f64>,
}

impl From<Location> for MergeGps {
    fn from(l: Location) -> Self {
        MergeGps { lat: l.lat, lon: l.lon, alt: l.alt }
    }
}

impl From<MergeGps> for Location {
    fn from(g: MergeGps) -> Self {
        Location { lat: g.lat, lon: g.lon, alt: g.alt }
    }
}

/// Everything the merge dialog needs to show and resolve one candidate file.
#[derive(Debug, Serialize)]
pub struct MergeCandidate {
    pub path: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub file_size: u64,
    /// File modification time (epoch seconds).
    pub mtime: Option<i64>,
    // Companion-sourced fields (None when absent).
    pub user_tags: Vec<String>,
    pub rating: Option<u8>,
    pub color_label: Option<String>,
    pub notes: Option<String>,
    pub companion_location: Option<MergeGps>,
    /// GPS read from embedded EXIF (read-only context; can be promoted into the
    /// keeper's companion location).
    pub exif_location: Option<MergeGps>,
}

/// Read one candidate's mergeable data. Missing companions/metadata are simply
/// reported as empty rather than erroring — a copy with no sidecar is valid.
fn read_merge_candidate(path: &str) -> MergeCandidate {
    let p = Path::new(path);

    let (file_size, mtime) = std::fs::metadata(p)
        .map(|m| {
            let size = m.len();
            let mtime = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64);
            (size, mtime)
        })
        .unwrap_or((0, None));

    let companion = reader::read_companion_optional(p).ok().flatten();
    let core = companion.as_ref().and_then(|c| c.meta.core.clone());

    let (width, height) = core
        .as_ref()
        .and_then(|m| m.media.as_ref())
        .map(|mi| (Some(mi.width), Some(mi.height)))
        .unwrap_or((None, None));

    MergeCandidate {
        path: path.to_string(),
        width,
        height,
        file_size,
        mtime,
        user_tags: companion.as_ref().map(|c| c.tags.user.clone()).unwrap_or_default(),
        rating: core.as_ref().and_then(|m| m.rating),
        color_label: core.as_ref().and_then(|m| m.color_label.clone()),
        notes: core.as_ref().and_then(|m| m.notes.clone()),
        companion_location: core.as_ref().and_then(|m| m.location).map(Into::into),
        exif_location: exif::extract_location(p).map(Into::into),
    }
}

pub async fn get_merge_candidates_impl(
    _state: &AppState,
    paths: Vec<String>,
) -> Result<Vec<MergeCandidate>, String> {
    Ok(paths.iter().map(|p| read_merge_candidate(p)).collect())
}

#[tauri::command]
pub async fn get_merge_candidates(
    state: tauri::State<'_, AppState>,
    paths: Vec<String>,
) -> Result<Vec<MergeCandidate>, String> {
    get_merge_candidates_impl(&state, paths).await
}

/// A resolved merge decision from the dialog.
#[derive(Debug, Deserialize)]
pub struct MergePlan {
    /// File to keep.
    pub keeper: String,
    /// Files to trash after merging their selected data into the keeper.
    pub discard: Vec<String>,
    /// Final resolved user-tag union.
    pub user_tags: Vec<String>,
    pub rating: Option<u8>,
    pub color_label: Option<String>,
    pub notes: Option<String>,
    pub location: Option<MergeGps>,
    /// Epoch seconds to stamp on the keeper file, when the user chose a
    /// different copy's timestamp.
    pub set_mtime: Option<i64>,
}

/// Merge selected metadata from a duplicate group into the keeper, then trash
/// the discarded copies.
///
/// User tags, rating, color label, notes, and location are taken verbatim from
/// the resolved plan. Auto and plugin tags are unioned from every copy (keeper
/// included) since they carry no conflict — more provenance is strictly better.
/// Union the auto and plugin tags across every copy. These carry no conflict —
/// more provenance is strictly better — so they are always merged wholesale.
fn union_auto_plugin_tags(
    paths: &[&str],
) -> (Vec<String>, std::collections::HashMap<String, crate::companion::schema::PluginTagEntry>) {
    let mut auto_union: Vec<String> = Vec::new();
    let mut plugin_union: std::collections::HashMap<
        String,
        crate::companion::schema::PluginTagEntry,
    > = std::collections::HashMap::new();

    for path in paths {
        if let Ok(Some(c)) = reader::read_companion_optional(Path::new(path)) {
            for t in c.tags.auto {
                if !auto_union.contains(&t) {
                    auto_union.push(t);
                }
            }
            for (name, entry) in c.tags.plugins {
                match plugin_union.get_mut(&name) {
                    Some(existing) => {
                        for t in entry.tags {
                            if !existing.tags.contains(&t) {
                                existing.tags.push(t);
                            }
                        }
                    }
                    None => {
                        plugin_union.insert(name, entry);
                    }
                }
            }
        }
    }
    (auto_union, plugin_union)
}

/// Apply a resolved plan (plus the auto/plugin tag union) to the keeper's
/// companion on disk. Returns the written companion for re-indexing.
fn write_merged_companion(
    plan: &MergePlan,
    auto_union: Vec<String>,
    plugin_union: std::collections::HashMap<String, crate::companion::schema::PluginTagEntry>,
) -> Result<crate::companion::schema::CompanionFile, String> {
    let rating = plan.rating;
    let color_label = plan.color_label.clone();
    let notes = plan.notes.clone();
    let location: Option<Location> = plan.location.map(Into::into);

    modify_companion(&plan.keeper, |c| {
        c.tags.user = plan.user_tags.clone();
        c.tags.auto = auto_union.clone();
        c.tags.plugins = plugin_union.clone();

        let core = c.meta.core.get_or_insert_with(CoreMeta::default);
        core.rating = rating;
        core.date_rated = if rating.is_some() {
            Some(chrono::Utc::now().to_rfc3339())
        } else {
            None
        };
        core.color_label = color_label.clone();
        core.notes = notes.clone();
        core.location = location;
    })
}

pub async fn merge_duplicates_impl(
    state: &AppState,
    plan: MergePlan,
) -> Result<(), String> {
    // Fold auto + plugin tags from every copy (keeper included) into the keeper.
    let all_paths: Vec<&str> = std::iter::once(plan.keeper.as_str())
        .chain(plan.discard.iter().map(|s| s.as_str()))
        .collect();
    let (auto_union, plugin_union) = union_auto_plugin_tags(&all_paths);
    let rating = plan.rating;

    let companion = write_merged_companion(&plan, auto_union, plugin_union)?;

    // Stamp the keeper's file mtime if the user picked a different copy's time.
    if let Some(secs) = plan.set_mtime {
        let ft = filetime::FileTime::from_unix_time(secs, 0);
        filetime::set_file_mtime(&plan.keeper, ft).map_err(|e| e.to_string())?;
    }

    // Re-index the keeper's tags and refresh autocomplete/rating cache.
    {
        let db = state.cache_db.lock().await;
        if let Some(db) = db.as_ref() {
            let _ = db.reindex_tags_for_file(&plan.keeper, &companion);
            let _ = db.update_rating(&plan.keeper, rating);
            if let Ok(counts) = db.query_all_tag_counts() {
                state.autocomplete.refresh(counts).await;
            }
        }
    }

    // Trash the discarded copies (removes them from the index too).
    if !plan.discard.is_empty() {
        crate::commands::trash::trash_files_impl(state, plan.discard).await?;
    }

    Ok(())
}

#[tauri::command]
pub async fn merge_duplicates(
    state: tauri::State<'_, AppState>,
    plan: MergePlan,
) -> Result<(), String> {
    merge_duplicates_impl(&state, plan).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::companion::schema::{CompanionFile, CoreMeta, MediaType};
    use crate::companion::writer;

    fn write_media_with_companion(
        dir: &Path,
        name: &str,
        mutate: impl FnOnce(&mut CompanionFile),
    ) -> String {
        let media = dir.join(name);
        std::fs::write(&media, b"jpegbytes").unwrap();
        let mut companion = CompanionFile::new(name, MediaType::Image);
        mutate(&mut companion);
        writer::write_companion(&media, &mut companion).unwrap();
        media.to_string_lossy().to_string()
    }

    #[test]
    fn read_candidate_reports_companion_and_file_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_media_with_companion(tmp.path(), "a.jpg", |c| {
            c.tags.user = vec!["cat".into(), "beach".into()];
            let core = c.meta.core.get_or_insert_with(CoreMeta::default);
            core.rating = Some(4);
            core.notes = Some("nice".into());
        });

        let cand = read_merge_candidate(&path);
        assert_eq!(cand.user_tags, vec!["cat", "beach"]);
        assert_eq!(cand.rating, Some(4));
        assert_eq!(cand.notes.as_deref(), Some("nice"));
        assert!(cand.file_size > 0);
        assert!(cand.mtime.is_some());
    }

    #[test]
    fn merge_unions_auto_tags_and_writes_resolved_scalars() {
        let tmp = tempfile::tempdir().unwrap();
        let keeper = write_media_with_companion(tmp.path(), "keep.jpg", |c| {
            c.tags.user = vec!["cat".into()];
            c.tags.auto = vec!["1girl".into()];
            let core = c.meta.core.get_or_insert_with(CoreMeta::default);
            core.rating = Some(2);
        });
        let other = write_media_with_companion(tmp.path(), "dup.jpg", |c| {
            c.tags.user = vec!["beach".into()];
            c.tags.auto = vec!["outdoors".into()];
            let core = c.meta.core.get_or_insert_with(CoreMeta::default);
            core.rating = Some(5);
            core.notes = Some("from dup".into());
        });

        // Resolve: keep 5-star rating and dup's notes; union of user tags.
        let plan = MergePlan {
            keeper: keeper.clone(),
            discard: vec![other.clone()],
            user_tags: vec!["cat".into(), "beach".into()],
            rating: Some(5),
            color_label: None,
            notes: Some("from dup".into()),
            location: None,
            set_mtime: None,
        };

        let all: Vec<&str> = vec![keeper.as_str(), other.as_str()];
        let (auto_union, plugin_union) = union_auto_plugin_tags(&all);
        write_merged_companion(&plan, auto_union, plugin_union).unwrap();

        let merged = reader::read_companion(Path::new(&keeper)).unwrap();
        assert_eq!(merged.tags.user, vec!["cat", "beach"]);
        assert_eq!(merged.tags.auto, vec!["1girl", "outdoors"]);
        let core = merged.meta.core.unwrap();
        assert_eq!(core.rating, Some(5));
        assert_eq!(core.notes.as_deref(), Some("from dup"));
    }
}
