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
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Maximum number of decoded ThumbHash PNGs to keep cached. Each entry is tiny
/// (a ~32px PNG, a few hundred bytes), so even at capacity this is well under a
/// few MB. On overflow the whole map is cleared — a coarse bound that avoids
/// any LRU bookkeeping on the hot read path.
const THUMBHASH_PNG_CACHE_CAP: usize = 4096;

/// Bounded cache of decoded ThumbHash → PNG bytes. Placeholders are re-requested
/// repeatedly while fling-scrolling the grid; decoding the hash and PNG-encoding
/// it on every request is pure waste since the output is deterministic per path.
/// Keyed by absolute path (galleries use absolute paths, so cross-gallery key
/// collisions don't occur); [`AppState`] clears it when the gallery's protocol
/// pool is swapped so it can't grow across sessions.
#[derive(Default)]
pub struct ThumbhashPngCache {
    inner: Mutex<HashMap<String, Arc<Vec<u8>>>>,
}

impl ThumbhashPngCache {
    fn get(&self, path: &str) -> Option<Arc<Vec<u8>>> {
        self.inner.lock().unwrap().get(path).cloned()
    }

    fn insert(&self, path: String, png: Arc<Vec<u8>>) {
        let mut map = self.inner.lock().unwrap();
        if map.len() >= THUMBHASH_PNG_CACHE_CAP {
            map.clear();
        }
        map.insert(path, png);
    }

    /// Drop all cached PNGs. Called when the active gallery changes.
    pub fn clear(&self) {
        self.inner.lock().unwrap().clear();
    }
}

/// Result of a thumbnail lookup/generation.
pub enum ThumbOutcome {
    Hit { data: Vec<u8>, format: String },
    /// No cached bytes and (for the generate path) generation failed.
    Miss,
    /// No gallery is open — the read-only protocol connection is absent.
    NoGallery,
}

/// Read a cached thumbnail directly from SQLite via the read-only protocol
/// connection pool. Returns `Miss` on a genuine cache miss. This is a blocking
/// call (synchronous SQLite read) — call it from the blocking pool, not the UI
/// thread (see [`read_cached_thumbnail_async`]).
pub fn read_cached_thumbnail(state: &AppState, tier: ThumbTier, path: &str) -> ThumbOutcome {
    let pool = {
        let guard = state.thumb_protocol_db.read().unwrap();
        match guard.as_ref() {
            Some(p) => p.clone(),
            None => return ThumbOutcome::NoGallery,
        }
    };

    let sql = tier.select_blob_sql();
    let result = pool.with_conn(|conn| {
        conn.prepare_cached(sql).and_then(|mut stmt| {
            stmt.query_row(rusqlite::params![path], |row| {
                Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?))
            })
        })
    });
    match result {
        Ok((data, format)) => ThumbOutcome::Hit { data, format },
        Err(rusqlite::Error::QueryReturnedNoRows) => ThumbOutcome::Miss,
        Err(e) => {
            log::warn!("thumb cache read failed for {} (tier {:?}): {}", path, tier, e);
            ThumbOutcome::Miss
        }
    }
}

/// Async wrapper around [`read_cached_thumbnail`] that runs the blocking SQLite
/// read on the blocking thread pool, keeping it off both the UI thread and the
/// async worker threads. Combined with the connection pool, concurrent reads
/// fan out across connections instead of serializing.
pub async fn read_cached_thumbnail_async(
    state: &AppState,
    tier: ThumbTier,
    path: String,
) -> ThumbOutcome {
    let state = state.clone();
    match tokio::task::spawn_blocking(move || read_cached_thumbnail(&state, tier, &path)).await {
        Ok(outcome) => outcome,
        Err(_) => ThumbOutcome::Miss,
    }
}

/// Full lookup-then-generate. On a cache miss, generates on the thumb pool with
/// coalescing so concurrent requests for the same key decode only once.
pub async fn get_or_generate(state: &AppState, tier: ThumbTier, path: String) -> ThumbOutcome {
    match read_cached_thumbnail_async(state, tier, path.clone()).await {
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
    if let Some(png) = state.thumbhash_png_cache.get(path) {
        return ThumbhashOutcome::Png(png.as_ref().clone());
    }

    let pool = {
        let guard = state.thumb_protocol_db.read().unwrap();
        match guard.as_ref() {
            Some(p) => p.clone(),
            None => return ThumbhashOutcome::NoGallery,
        }
    };

    let blob: Result<Option<Vec<u8>>, rusqlite::Error> = pool.with_conn(|conn| {
        conn.prepare_cached("SELECT thumbhash FROM thumbnails WHERE path = ?1")
            .and_then(|mut stmt| {
                stmt.query_row(rusqlite::params![path], |row| row.get::<_, Option<Vec<u8>>>(0))
            })
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

    let bytes = png.into_inner();
    state
        .thumbhash_png_cache
        .insert(path.to_string(), Arc::new(bytes.clone()));
    ThumbhashOutcome::Png(bytes)
}

/// MIME type for a stored thumbnail format string.
pub fn thumb_mime(format: &str) -> &'static str {
    match format {
        "webp" => "image/webp",
        "png" => "image/png",
        _ => "image/jpeg",
    }
}
