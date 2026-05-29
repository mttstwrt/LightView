//! Shared thumbnail-serving logic, decoupled from any HTTP response type.
//!
//! Both the Tauri `lightview://thumb/...` protocol handler (desktop webview)
//! and the axum `/thumb/...` route (remote web client) need to read cached
//! thumbnails from SQLite, generate them on a miss, and decode ThumbHash
//! placeholders. The functions here return plain outcomes so each caller can
//! build its own response (`tauri::http::Response` vs `axum::response`).

use crate::cache::coalescer::Role;
use crate::cache::thumbnails::ThumbTier;
use crate::commands;
use crate::AppState;

/// Result of a thumbnail lookup/generation.
pub enum ThumbOutcome {
    Hit { data: Vec<u8>, format: String },
    /// No cached bytes and (for the generate path) generation failed.
    Miss,
    /// No gallery is open — the read-only protocol connection is absent.
    NoGallery,
}

/// Read a cached thumbnail directly from SQLite via the read-only protocol
/// connection. Returns `Miss` on a genuine cache miss.
pub fn read_cached_thumbnail(state: &AppState, tier: ThumbTier, path: &str) -> ThumbOutcome {
    let proto_db = state.thumb_protocol_db.lock().unwrap();
    let conn = match proto_db.as_ref() {
        Some(c) => c,
        None => return ThumbOutcome::NoGallery,
    };

    let sql = format!("SELECT thumbnail, format FROM {} WHERE path = ?1", tier.table());
    match conn.prepare_cached(&sql).and_then(|mut stmt| {
        stmt.query_row(rusqlite::params![path], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?))
        })
    }) {
        Ok((data, format)) => ThumbOutcome::Hit { data, format },
        Err(rusqlite::Error::QueryReturnedNoRows) => ThumbOutcome::Miss,
        Err(e) => {
            log::warn!("thumb cache read failed for {} (tier {:?}): {}", path, tier, e);
            ThumbOutcome::Miss
        }
    }
}

/// Full lookup-then-generate. On a cache miss, generates on the thumb pool with
/// coalescing so concurrent requests for the same key decode only once.
pub async fn get_or_generate(state: &AppState, tier: ThumbTier, path: String) -> ThumbOutcome {
    match read_cached_thumbnail(state, tier, &path) {
        ThumbOutcome::Miss => {}
        other => return other,
    }

    let key = (path.clone(), tier);
    let (role, notify) = state.thumb_gen_coalescer.acquire(key.clone());

    match role {
        Role::Generator => {
            let result = commands::media::generate_and_store_tier(state, &path, tier).await;
            state.thumb_gen_coalescer.release(&key);
            match result {
                Ok((data, format)) => ThumbOutcome::Hit { data, format },
                Err(e) => {
                    log::warn!("generate-on-miss failed for {} (tier {:?}): {}", path, tier, e);
                    ThumbOutcome::Miss
                }
            }
        }
        Role::Waiter => {
            let listener = notify.notified();
            tokio::pin!(listener);
            // Enrol in the wake queue before re-checking the cache to avoid
            // missing a notify that races with the generator's release.
            listener.as_mut().enable();

            if let ThumbOutcome::Hit { data, format } = read_cached_thumbnail(state, tier, &path) {
                return ThumbOutcome::Hit { data, format };
            }
            listener.await;
            read_cached_thumbnail(state, tier, &path)
        }
    }
}

/// Result of decoding a ThumbHash placeholder.
pub enum ThumbhashOutcome {
    Png(Vec<u8>),
    Miss,
    NoGallery,
    Error,
}

/// Read the ~25-byte ThumbHash blob for a path and decode it into a tiny PNG.
pub fn render_thumbhash_png(state: &AppState, path: &str) -> ThumbhashOutcome {
    let proto_db = state.thumb_protocol_db.lock().unwrap();
    let conn = match proto_db.as_ref() {
        Some(c) => c,
        None => return ThumbhashOutcome::NoGallery,
    };

    let blob: Result<Option<Vec<u8>>, rusqlite::Error> = conn
        .prepare_cached("SELECT thumbhash FROM thumbnails WHERE path = ?1")
        .and_then(|mut stmt| {
            stmt.query_row(rusqlite::params![path], |row| row.get::<_, Option<Vec<u8>>>(0))
        });

    let Ok(Some(hash)) = blob else {
        return ThumbhashOutcome::Miss;
    };

    let Ok((w, h, rgba)) = thumbhash::thumb_hash_to_rgba(&hash) else {
        return ThumbhashOutcome::Error;
    };

    let mut png = std::io::Cursor::new(Vec::with_capacity(2048));
    let enc = image::codecs::png::PngEncoder::new(&mut png);
    use image::ImageEncoder;
    if enc
        .write_image(&rgba, w as u32, h as u32, image::ExtendedColorType::Rgba8)
        .is_err()
    {
        return ThumbhashOutcome::Error;
    }

    ThumbhashOutcome::Png(png.into_inner())
}

/// MIME type for a stored thumbnail format string.
pub fn thumb_mime(format: &str) -> &'static str {
    match format {
        "webp" => "image/webp",
        "png" => "image/png",
        _ => "image/jpeg",
    }
}
