//! Reading and writing tags, ratings, and the other companion-backed metadata.
//!
//! Every write goes through `modify_companion`: read the sidecar (or start a
//! fresh one), mutate, write atomically, then re-index. Sharing that helper is
//! what keeps a hand edit, a batch edit, a plugin result, and a duplicate merge
//! producing byte-identical files.
//!
//! Two stores must be updated together after any write — `tag_index` (so
//! filters see the change) and `tag_counts` plus the autocomplete engine (so
//! suggestions do). Updating only the first is invisible until someone types in
//! the tag box, which is why the refresh is part of every write path here
//! rather than left to the caller.

use std::collections::BTreeSet;

use serde::Serialize;

use crate::companion::reader;
use crate::companion::writer;
use crate::companion::schema::{CompanionFile, CoreMeta, MediaType};
use crate::AppState;

/// Read a companion file, apply a mutation, and write it back atomically.
pub(crate) fn modify_companion(
    path: &str,
    mutate: impl FnOnce(&mut CompanionFile),
) -> Result<CompanionFile, String> {
    let media_path = std::path::Path::new(path);

    let mut companion = reader::read_companion_optional(media_path)
        .map_err(|e| e.to_string())?
        .unwrap_or_else(|| {
            let ext = media_path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");
            let media_type = MediaType::from_extension(ext)
                .unwrap_or(MediaType::Image);
            let name = media_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown");
            CompanionFile::new(name, media_type)
        });

    mutate(&mut companion);

    writer::write_companion(media_path, &mut companion)
        .map_err(|e| e.to_string())?;

    Ok(companion)
}

#[derive(Debug, Serialize)]
pub struct TagInfo {
    pub namespace: String,
    pub tag: String,
}

/// Get all tags for a media file.
#[tauri::command]
pub async fn get_tags(
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<Vec<TagInfo>, String> {
    get_tags_impl(&state, path).await
}

pub async fn get_tags_impl(
    state: &AppState,
    path: String,
) -> Result<Vec<TagInfo>, String> {
    let db = state.cache_db.lock().await;
    let db = db.as_ref().ok_or("No gallery open")?;

    let tags = db
        .get_tags_for_file(&path)
        .map_err(|e| e.to_string())?;

    Ok(tags
        .into_iter()
        .map(|(ns, tag)| TagInfo {
            namespace: ns,
            tag,
        })
        .collect())
}

/// Add a user tag to a media file.
#[tauri::command]
pub async fn add_user_tag(
    state: tauri::State<'_, AppState>,
    path: String,
    tag: String,
) -> Result<(), String> {
    add_user_tag_impl(&state, path, tag).await
}

pub async fn add_user_tag_impl(
    state: &AppState,
    path: String,
    tag: String,
) -> Result<(), String> {
    single(add_user_tag_batch_impl(state, vec![path], tag).await)
}

/// Adapt a batch impl to a single-file command. The batch impls log and skip a
/// file they can't write so one bad sidecar doesn't abort the rest; for a
/// one-file call that "skip" *is* the failure, and the caller (a UI action on a
/// specific image) needs to hear about it.
fn single(result: Result<u64, String>) -> Result<(), String> {
    match result? {
        0 => Err("Failed to update the file's metadata".to_string()),
        _ => Ok(()),
    }
}

/// Remove a user tag from a media file.
#[tauri::command]
pub async fn remove_user_tag(
    state: tauri::State<'_, AppState>,
    path: String,
    tag: String,
) -> Result<(), String> {
    remove_user_tag_impl(&state, path, tag).await
}

pub async fn remove_user_tag_impl(
    state: &AppState,
    path: String,
    tag: String,
) -> Result<(), String> {
    single(remove_user_tag_batch_impl(state, vec![path], tag).await)
}

/// Add a user tag to multiple media files at once.
#[tauri::command]
pub async fn add_user_tag_batch(
    state: tauri::State<'_, AppState>,
    paths: Vec<String>,
    tag: String,
) -> Result<u64, String> {
    add_user_tag_batch_impl(&state, paths, tag).await
}

pub async fn add_user_tag_batch_impl(
    state: &AppState,
    paths: Vec<String>,
    tag: String,
) -> Result<u64, String> {
    edit_user_tag_batch(state, paths, tag, TagEdit::Add).await
}

/// Which way [`edit_user_tag_batch`] moves a tag.
#[derive(Clone, Copy)]
enum TagEdit {
    Add,
    Remove,
}

/// Add or remove one user tag across a set of files, reindexing each companion
/// and nudging its tag count, then refreshing autocomplete once at the end.
///
/// The cache lock is taken per file rather than held across the loop: the
/// companion rewrite in between is synchronous disk I/O, and holding the single
/// SQLite connection through it would stall every other command for the whole
/// batch. Per-file failures are logged and skipped so one unwritable sidecar
/// doesn't abort the rest.
async fn edit_user_tag_batch(
    state: &AppState,
    paths: Vec<String>,
    tag: String,
    edit: TagEdit,
) -> Result<u64, String> {
    let mut count = 0u64;
    for path in &paths {
        let tag_clone = tag.clone();
        let mutate = |c: &mut CompanionFile| match edit {
            TagEdit::Add => {
                if !c.tags.user.contains(&tag_clone) {
                    c.tags.user.push(tag_clone);
                }
            }
            TagEdit::Remove => c.tags.user.retain(|t| t != &tag_clone),
        };
        match modify_companion(path, mutate) {
            Ok(companion) => {
                let db = state.cache_db.lock().await;
                if let Some(db) = db.as_ref() {
                    let _ = db.reindex_tags_for_file(path, &companion);
                    let _ = match edit {
                        TagEdit::Add => db.increment_tag_count("user", &tag),
                        TagEdit::Remove => db.decrement_tag_count("user", &tag),
                    };
                }
                count += 1;
            }
            Err(e) => log::warn!("Failed to edit tags on {}: {}", path, e),
        }
    }

    let db = state.cache_db.lock().await;
    if let Some(db) = db.as_ref() {
        if let Ok(counts) = db.query_all_tag_counts() {
            state.autocomplete.refresh(counts).await;
        }
    }

    Ok(count)
}

/// Remove a user tag from multiple media files at once.
#[tauri::command]
pub async fn remove_user_tag_batch(
    state: tauri::State<'_, AppState>,
    paths: Vec<String>,
    tag: String,
) -> Result<u64, String> {
    remove_user_tag_batch_impl(&state, paths, tag).await
}

pub async fn remove_user_tag_batch_impl(
    state: &AppState,
    paths: Vec<String>,
    tag: String,
) -> Result<u64, String> {
    edit_user_tag_batch(state, paths, tag, TagEdit::Remove).await
}

// ---------------------------------------------------------------------------
// Gallery-wide user-tag management (rename / merge / delete)
// ---------------------------------------------------------------------------

/// One user tag in the gallery, with the number of files carrying it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserTagSummary {
    pub tag: String,
    pub count: u32,
}

/// Outcome of a gallery-wide tag rewrite.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TagEditResult {
    /// Files whose companion was rewritten.
    pub files_changed: usize,
    /// Files indexed under a source tag that could not be rewritten (sidecar
    /// missing, unreadable, or read-only) — the tag may still be on disk there.
    pub files_failed: usize,
}

/// List every user tag in the open gallery with its file count.
#[tauri::command]
pub async fn list_user_tags(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<UserTagSummary>, String> {
    list_user_tags_impl(&state).await
}

pub async fn list_user_tags_impl(state: &AppState) -> Result<Vec<UserTagSummary>, String> {
    let db = state.cache_db.lock().await;
    let db = db.as_ref().ok_or("No gallery open")?;

    Ok(db
        .tags_in_namespace_with_counts("user")
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|c| UserTagSummary {
            tag: c.tag,
            count: c.count,
        })
        .collect())
}

/// Paths of every file carrying any of the given user tags.
///
/// Backs the tag manager's preview strip: it shows what a rename/merge/delete
/// is about to touch. Goes through the tag index rather than the filter
/// grammar because a tag may contain spaces or operator-like words, which a
/// query string can't express unambiguously.
#[tauri::command]
pub async fn paths_for_user_tags(
    state: tauri::State<'_, AppState>,
    tags: Vec<String>,
    limit: Option<usize>,
) -> Result<Vec<String>, String> {
    paths_for_user_tags_impl(&state, tags, limit).await
}

pub async fn paths_for_user_tags_impl(
    state: &AppState,
    tags: Vec<String>,
    limit: Option<usize>,
) -> Result<Vec<String>, String> {
    let db = state.cache_db.lock().await;
    let db = db.as_ref().ok_or("No gallery open")?;

    let mut paths = BTreeSet::new();
    for tag in &tags {
        for path in db.query_tag("user", tag).map_err(|e| e.to_string())? {
            paths.insert(path);
        }
    }

    Ok(match limit {
        Some(n) => paths.into_iter().take(n).collect(),
        None => paths.into_iter().collect(),
    })
}

/// Rename a user tag across the whole gallery. If `to` already exists on a
/// file, the two collapse into one (i.e. a rename onto an existing tag is a
/// merge).
#[tauri::command]
pub async fn rename_user_tag(
    state: tauri::State<'_, AppState>,
    from: String,
    to: String,
) -> Result<TagEditResult, String> {
    rename_user_tag_impl(&state, from, to).await
}

pub async fn rename_user_tag_impl(
    state: &AppState,
    from: String,
    to: String,
) -> Result<TagEditResult, String> {
    rewrite_user_tags(state, vec![from], Some(to)).await
}

/// Merge several user tags into one across the whole gallery. `target` need
/// not already exist, and may be one of `sources` (the others fold into it).
#[tauri::command]
pub async fn merge_user_tags(
    state: tauri::State<'_, AppState>,
    sources: Vec<String>,
    target: String,
) -> Result<TagEditResult, String> {
    merge_user_tags_impl(&state, sources, target).await
}

pub async fn merge_user_tags_impl(
    state: &AppState,
    sources: Vec<String>,
    target: String,
) -> Result<TagEditResult, String> {
    rewrite_user_tags(state, sources, Some(target)).await
}

/// Delete user tags from every file in the gallery.
#[tauri::command]
pub async fn delete_user_tags(
    state: tauri::State<'_, AppState>,
    tags: Vec<String>,
) -> Result<TagEditResult, String> {
    delete_user_tags_impl(&state, tags).await
}

pub async fn delete_user_tags_impl(
    state: &AppState,
    tags: Vec<String>,
) -> Result<TagEditResult, String> {
    rewrite_user_tags(state, tags, None).await
}

/// Shared engine behind rename / merge / delete: rewrite `sources` to `target`
/// (or drop them when `target` is `None`) on every file the tag index says
/// carries one of them.
async fn rewrite_user_tags(
    state: &AppState,
    sources: Vec<String>,
    target: Option<String>,
) -> Result<TagEditResult, String> {
    let mut sources: Vec<String> = sources
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    sources.sort();
    sources.dedup();
    if sources.is_empty() {
        return Err("No tags given".to_string());
    }

    let target = match target {
        Some(t) => {
            let t = t.trim().to_string();
            if t.is_empty() {
                return Err("Target tag is empty".to_string());
            }
            Some(t)
        }
        None => None,
    };

    // Merging a tag into itself is a no-op for that source; the rest still fold
    // into it. Renaming a tag to its own name drops out here entirely.
    if let Some(t) = &target {
        sources.retain(|s| s != t);
    }
    if sources.is_empty() {
        return Ok(TagEditResult::default());
    }

    // Snapshot the affected paths and release the lock before touching disk —
    // the rewrite below is file I/O over an unbounded number of files and must
    // not hold the single SQLite connection for its whole duration.
    let paths: BTreeSet<String> = {
        let db = state.cache_db.lock().await;
        let db = db.as_ref().ok_or("No gallery open")?;
        let mut set = BTreeSet::new();
        for source in &sources {
            for path in db.query_tag("user", source).map_err(|e| e.to_string())? {
                set.insert(path);
            }
        }
        set
    };

    let mut result = TagEditResult::default();
    for path in &paths {
        match rewrite_companion_user_tags(path, &sources, target.as_deref()) {
            Ok(Some(companion)) => {
                let db = state.cache_db.lock().await;
                if let Some(db) = db.as_ref() {
                    let _ = db.reindex_tags_for_file(path, &companion);
                }
                result.files_changed += 1;
            }
            // Indexed under the tag but the companion no longer has it —
            // nothing to write, and the reindex below fixes the stale row.
            Ok(None) => {}
            Err(e) => {
                log::warn!("Failed to rewrite tags for {}: {}", path, e);
                result.files_failed += 1;
            }
        }
    }

    // Counts are rebuilt wholesale instead of nudged per file: one rewrite
    // touches every file carrying the tag, and the resulting per-tag deltas are
    // exactly what a single GROUP BY already computes.
    let db = state.cache_db.lock().await;
    if let Some(db) = db.as_ref() {
        let _ = db.rebuild_tag_counts();
        if let Ok(counts) = db.query_all_tag_counts() {
            state.autocomplete.refresh(counts).await;
        }
    }

    Ok(result)
}

/// Apply a source→target rewrite to one companion file, writing it back only
/// if something actually changed. Returns the rewritten companion, or `None`
/// when the file carried none of the source tags.
fn rewrite_companion_user_tags(
    path: &str,
    sources: &[String],
    target: Option<&str>,
) -> Result<Option<CompanionFile>, String> {
    let media_path = std::path::Path::new(path);
    let mut companion = reader::read_companion_optional(media_path)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "companion file missing".to_string())?;

    let mut changed = false;
    let mut next: Vec<String> = Vec::with_capacity(companion.tags.user.len());
    for tag in std::mem::take(&mut companion.tags.user) {
        if sources.iter().any(|s| s == &tag) {
            changed = true;
            // The target takes the first matched source's slot, so a renamed
            // tag keeps its place in the list instead of jumping to the end.
            if let Some(t) = target {
                if !next.iter().any(|e| e == t) {
                    next.push(t.to_string());
                }
            }
        } else if next.iter().any(|e| e == &tag) {
            // Collapsed into an earlier entry (target already present).
            changed = true;
        } else {
            next.push(tag);
        }
    }

    if !changed {
        companion.tags.user = next;
        return Ok(None);
    }

    companion.tags.user = next;
    writer::write_companion(media_path, &mut companion).map_err(|e| e.to_string())?;
    Ok(Some(companion))
}

/// Set the rating for multiple media files at once (0-5, 0 = unrated).
#[tauri::command]
pub async fn set_rating_batch(
    state: tauri::State<'_, AppState>,
    paths: Vec<String>,
    rating: u8,
) -> Result<u64, String> {
    set_rating_batch_impl(&state, paths, rating).await
}

pub async fn set_rating_batch_impl(
    state: &AppState,
    paths: Vec<String>,
    rating: u8,
) -> Result<u64, String> {
    let db_rating = if rating == 0 { None } else { Some(rating) };
    let now = chrono::Utc::now().to_rfc3339();
    let mut count = 0u64;
    for path in &paths {
        match modify_companion(path, |c| {
            let core = c.meta.core.get_or_insert_with(CoreMeta::default);
            core.rating = db_rating;
            core.date_rated = if db_rating.is_some() {
                Some(now.clone())
            } else {
                None
            };
        }) {
            Ok(_) => {
                count += 1;
                let db = state.cache_db.lock().await;
                if let Some(db) = db.as_ref() {
                    let _ = db.update_rating(path, db_rating);
                }
            }
            Err(e) => log::warn!("Failed to rate {}: {}", path, e),
        }
    }
    Ok(count)
}

/// Set the rating for a media file (0-5, 0 = unrated).
#[tauri::command]
pub async fn set_rating(
    state: tauri::State<'_, AppState>,
    path: String,
    rating: u8,
) -> Result<(), String> {
    set_rating_impl(&state, path, rating).await
}

pub async fn set_rating_impl(
    state: &AppState,
    path: String,
    rating: u8,
) -> Result<(), String> {
    single(set_rating_batch_impl(state, vec![path], rating).await)
}

/// Set the colour label on a batch of files. `None` clears it.
///
/// Writes the companion (the record of intent) and then mirrors the value into
/// `media_meta.color_label`, exactly as [`set_rating_batch_impl`] does — the
/// filter compiles to SQL over that column and never opens a sidecar, so
/// skipping the mirror would leave `color:red` unable to see the label the user
/// just set.
pub async fn set_color_label_batch_impl(
    state: &AppState,
    paths: Vec<String>,
    label: Option<String>,
) -> Result<u64, String> {
    let mut count = 0u64;
    for path in &paths {
        match modify_companion(path, |c| {
            let core = c.meta.core.get_or_insert_with(CoreMeta::default);
            core.color_label = label.clone();
        }) {
            Ok(_) => {
                count += 1;
                let db = state.cache_db.lock().await;
                if let Some(db) = db.as_ref() {
                    let _ = db.update_color_label(path, label.as_deref());
                }
            }
            Err(e) => log::warn!("Failed to set colour label on {}: {}", path, e),
        }
    }
    Ok(count)
}

/// Set the colour label for a media file. `None` clears it.
#[tauri::command]
pub async fn set_color_label(
    state: tauri::State<'_, AppState>,
    path: String,
    label: Option<String>,
) -> Result<(), String> {
    set_color_label_impl(&state, path, label).await
}

pub async fn set_color_label_impl(
    state: &AppState,
    path: String,
    label: Option<String>,
) -> Result<(), String> {
    single(set_color_label_batch_impl(state, vec![path], label).await)
}

/// Set the colour label on every path in `paths` (multi-select).
#[tauri::command]
pub async fn set_color_label_batch(
    state: tauri::State<'_, AppState>,
    paths: Vec<String>,
    label: Option<String>,
) -> Result<u64, String> {
    set_color_label_batch_impl(&state, paths, label).await
}

/// Set the description for a media file. `None` (or blank) clears it.
///
/// Writes the companion first — it is the record of intent — then mirrors the
/// text into `media_meta.description`, exactly as [`set_color_label_batch_impl`]
/// does. Without the mirror, `desc:sunset` could not see the description the
/// user just typed: the filter compiles to SQL over `media_meta` and never
/// opens a sidecar.
///
/// One path, not a batch: the two writers are the info panel and the merge
/// dialog, both of which describe one file. Setting the *same* prose on a
/// selection is not a thing anyone has asked for, and a generator writes
/// different text per file, so it would not use such a command either.
pub async fn set_description_impl(
    state: &AppState,
    path: String,
    description: Option<String>,
) -> Result<(), String> {
    // Blank is an absent description, not an empty one: a cleared textarea and
    // a never-filled one are the same state to every reader.
    let description = description
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty());
    modify_companion(&path, |c| {
        let core = c.meta.core.get_or_insert_with(CoreMeta::default);
        core.description = description.clone();
    })?;
    let db = state.cache_db.lock().await;
    if let Some(db) = db.as_ref() {
        let _ = db.update_description(&path, description.as_deref());
    }
    Ok(())
}

#[tauri::command]
pub async fn set_description(
    state: tauri::State<'_, AppState>,
    path: String,
    description: Option<String>,
) -> Result<(), String> {
    set_description_impl(&state, path, description).await
}

/// Set notes for a media file.
#[tauri::command]
pub async fn set_notes(
    _state: tauri::State<'_, AppState>,
    path: String,
    notes: Option<String>,
) -> Result<(), String> {
    modify_companion(&path, |c| {
        let core = c.meta.core.get_or_insert_with(CoreMeta::default);
        core.notes = notes;
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a media file with a companion carrying `tags` as user tags.
    fn seed(dir: &std::path::Path, name: &str, tags: &[&str]) -> String {
        let media = dir.join(name);
        std::fs::write(&media, b"jpegbytes").unwrap();
        let mut companion = CompanionFile::new(name, MediaType::Image);
        companion.tags.user = tags.iter().map(|t| t.to_string()).collect();
        writer::write_companion(&media, &mut companion).unwrap();
        media.to_string_lossy().to_string()
    }

    fn user_tags(path: &str) -> Vec<String> {
        reader::read_companion(std::path::Path::new(path))
            .unwrap()
            .tags
            .user
    }

    #[test]
    fn rename_keeps_the_tag_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let path = seed(tmp.path(), "a.jpg", &["beach", "sunset", "family"]);

        let changed =
            rewrite_companion_user_tags(&path, &["sunset".to_string()], Some("golden hour"))
                .unwrap();

        assert!(changed.is_some());
        assert_eq!(user_tags(&path), ["beach", "golden hour", "family"]);
    }

    #[test]
    fn merge_collapses_duplicates() {
        let tmp = tempfile::tempdir().unwrap();
        // Both a source and the target on the same file: one entry must survive.
        let path = seed(tmp.path(), "a.jpg", &["dog", "puppy", "pet"]);

        rewrite_companion_user_tags(
            &path,
            &["dog".to_string(), "puppy".to_string()],
            Some("pet"),
        )
        .unwrap();

        assert_eq!(user_tags(&path), ["pet"]);
    }

    #[test]
    fn delete_drops_only_the_named_tags() {
        let tmp = tempfile::tempdir().unwrap();
        let path = seed(tmp.path(), "a.jpg", &["beach", "sunset", "family"]);

        rewrite_companion_user_tags(&path, &["beach".to_string(), "family".to_string()], None)
            .unwrap();

        assert_eq!(user_tags(&path), ["sunset"]);
    }

    #[test]
    fn untouched_file_is_not_rewritten() {
        let tmp = tempfile::tempdir().unwrap();
        let path = seed(tmp.path(), "a.jpg", &["beach"]);
        let before = std::fs::read_to_string(reader::companion_path(
            std::path::Path::new(&path),
            crate::companion::reader::CompanionLocation::default(),
        ))
        .unwrap();

        // A stale index row can point here even though the tag is gone.
        let result = rewrite_companion_user_tags(&path, &["sunset".to_string()], Some("x")).unwrap();
        assert!(result.is_none());

        let after = std::fs::read_to_string(reader::companion_path(
            std::path::Path::new(&path),
            crate::companion::reader::CompanionLocation::default(),
        ))
        .unwrap();
        assert_eq!(before, after, "companion rewritten despite no change");
    }

    #[test]
    fn missing_companion_is_an_error_not_a_new_sidecar() {
        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("orphan.jpg");
        std::fs::write(&media, b"jpegbytes").unwrap();
        let path = media.to_string_lossy().to_string();

        assert!(rewrite_companion_user_tags(&path, &["beach".to_string()], None).is_err());
        assert!(!tmp.path().join(".lightview").exists());
    }
}
