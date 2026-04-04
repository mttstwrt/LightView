use serde::Serialize;

use crate::companion::reader;
use crate::companion::writer;
use crate::companion::schema::{CompanionFile, CoreMeta, MediaType};
use crate::AppState;

/// Read a companion file, apply a mutation, and write it back atomically.
fn modify_companion(
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
    let tag_clone = tag.clone();
    let companion = modify_companion(&path, |c| {
        if !c.tags.user.contains(&tag_clone) {
            c.tags.user.push(tag_clone);
        }
    })?;

    let db = state.cache_db.lock().await;
    if let Some(db) = db.as_ref() {
        let _ = db.reindex_tags_for_file(&path, &companion);
        let _ = db.increment_tag_count("user", &tag);

        if let Ok(counts) = db.query_all_tag_counts() {
            state.autocomplete.refresh(counts).await;
        }
    }

    Ok(())
}

/// Remove a user tag from a media file.
#[tauri::command]
pub async fn remove_user_tag(
    state: tauri::State<'_, AppState>,
    path: String,
    tag: String,
) -> Result<(), String> {
    let tag_clone = tag.clone();
    let companion = modify_companion(&path, |c| {
        c.tags.user.retain(|t| t != &tag_clone);
    })?;

    let db = state.cache_db.lock().await;
    if let Some(db) = db.as_ref() {
        let _ = db.reindex_tags_for_file(&path, &companion);
        let _ = db.decrement_tag_count("user", &tag);

        if let Ok(counts) = db.query_all_tag_counts() {
            state.autocomplete.refresh(counts).await;
        }
    }

    Ok(())
}

/// Add a user tag to multiple media files at once.
#[tauri::command]
pub async fn add_user_tag_batch(
    state: tauri::State<'_, AppState>,
    paths: Vec<String>,
    tag: String,
) -> Result<u64, String> {
    let mut count = 0u64;
    for path in &paths {
        let tag_clone = tag.clone();
        match modify_companion(path, |c| {
            if !c.tags.user.contains(&tag_clone) {
                c.tags.user.push(tag_clone);
            }
        }) {
            Ok(companion) => {
                let db = state.cache_db.lock().await;
                if let Some(db) = db.as_ref() {
                    let _ = db.reindex_tags_for_file(path, &companion);
                    let _ = db.increment_tag_count("user", &tag);
                }
                count += 1;
            }
            Err(e) => {
                log::warn!("Failed to tag {}: {}", path, e);
            }
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
    let mut count = 0u64;
    for path in &paths {
        let tag_clone = tag.clone();
        match modify_companion(path, |c| {
            c.tags.user.retain(|t| t != &tag_clone);
        }) {
            Ok(companion) => {
                let db = state.cache_db.lock().await;
                if let Some(db) = db.as_ref() {
                    let _ = db.reindex_tags_for_file(path, &companion);
                    let _ = db.decrement_tag_count("user", &tag);
                }
                count += 1;
            }
            Err(e) => {
                log::warn!("Failed to remove tag from {}: {}", path, e);
            }
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

/// Set the rating for multiple media files at once (0-5, 0 = unrated).
#[tauri::command]
pub async fn set_rating_batch(
    state: tauri::State<'_, AppState>,
    paths: Vec<String>,
    rating: u8,
) -> Result<u64, String> {
    let db_rating = if rating == 0 { None } else { Some(rating) };
    let mut count = 0u64;
    for path in &paths {
        match modify_companion(path, |c| {
            let core = c.meta.core.get_or_insert_with(CoreMeta::default);
            core.rating = db_rating;
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
    let db_rating = if rating == 0 { None } else { Some(rating) };
    modify_companion(&path, |c| {
        let core = c.meta.core.get_or_insert_with(CoreMeta::default);
        core.rating = db_rating;
    })?;

    let db = state.cache_db.lock().await;
    if let Some(db) = db.as_ref() {
        let _ = db.update_rating(&path, db_rating);
    }

    Ok(())
}

/// Set the color label for a media file.
#[tauri::command]
pub async fn set_color_label(
    _state: tauri::State<'_, AppState>,
    path: String,
    label: Option<String>,
) -> Result<(), String> {
    modify_companion(&path, |c| {
        let core = c.meta.core.get_or_insert_with(CoreMeta::default);
        core.color_label = label;
    })?;
    Ok(())
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
