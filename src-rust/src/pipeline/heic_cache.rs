//! In-memory cache for HEIC→JPEG transcodes.
//!
//! HEIC decode is slow (~500ms for a 12 MP iPhone photo), and the viewer
//! frequently re-requests the same file (back/forward navigation, retries
//! by the webview, the inflight request cancelled-and-replayed pattern).
//! This cache holds the produced JPEG bytes keyed by `(path, mtime)`, so
//! repeats are essentially free. mtime is part of the key so external edits
//! invalidate stale entries automatically.
//!
//! Bounded LRU — capacity is small enough that the worst-case memory
//! footprint stays in the single-digit MB range.
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use super::thumbnailer::{self, HeicPixels, ThumbError};

const CAPACITY: usize = 12;

#[derive(Clone, Eq, PartialEq, Hash)]
struct Key {
    path: String,
    mtime: SystemTime,
}

struct Entry {
    jpeg: Vec<u8>,
    last_used: u64,
}

struct Cache {
    map: HashMap<Key, Entry>,
    counter: u64,
}

fn cache() -> &'static Mutex<Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(Cache {
            map: HashMap::with_capacity(CAPACITY),
            counter: 0,
        })
    })
}

fn mtime_of(path: &str) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

/// Look up cached JPEG bytes; returns a clone if `(path, mtime)` is a hit.
fn lookup(path: &str) -> Option<Vec<u8>> {
    let mtime = mtime_of(path)?;
    let key = Key { path: path.to_string(), mtime };
    let mut c = cache().lock().ok()?;
    c.counter += 1;
    let counter = c.counter;
    let entry = c.map.get_mut(&key)?;
    entry.last_used = counter;
    Some(entry.jpeg.clone())
}

fn store(path: &str, jpeg: Vec<u8>) {
    let Some(mtime) = mtime_of(path) else { return };
    let Ok(mut c) = cache().lock() else { return };
    c.counter += 1;
    let counter = c.counter;

    if c.map.len() >= CAPACITY {
        let oldest = c
            .map
            .iter()
            .min_by_key(|(_, e)| e.last_used)
            .map(|(k, _)| k.clone());
        if let Some(k) = oldest {
            c.map.remove(&k);
        }
    }

    c.map.insert(
        Key { path: path.to_string(), mtime },
        Entry { jpeg, last_used: counter },
    );
}

/// Get a HEIC→JPEG transcode for `path`, using the cache when possible.
/// On miss, decodes the HEIC, encodes to JPEG, stores, and returns the bytes.
pub fn get_or_transcode(path: &Path) -> Result<Vec<u8>, ThumbError> {
    let path_str = path.to_string_lossy();

    if let Some(jpeg) = lookup(&path_str) {
        return Ok(jpeg);
    }

    let dec = thumbnailer::decode_heic_natural(path)?;
    // Skip the alpha-strip pass when libheif gave us RGB directly.
    let jpeg = match dec.pixels {
        HeicPixels::Rgb(rgb) => thumbnailer::encode_rgb_to_jpeg(&rgb, dec.width, dec.height)?,
        HeicPixels::Rgba(rgba) => thumbnailer::encode_rgba_to_jpeg(&rgba, dec.width, dec.height)?,
    };
    store(&path_str, jpeg.clone());
    Ok(jpeg)
}
