//! Builds and caches animated-GIF frame atlases for canvas playback.
//!
//! WebKitGTK 2.52 animates `<img>` GIFs several times too fast and leaks a
//! freshly decoded copy of every frame on each loop (multi-GB growth). The
//! frontend therefore renders GIFs on a `<canvas>` instead, driven by a sprite
//! sheet we build here: each frame is composited, downscaled to the requested
//! tier, and packed into one PNG, alongside the per-frame delays.
//!
//! Static PNG sprite sheets don't trigger the WebKit animation bug, and we hold
//! exactly one decoded atlas per GIF — so playback is correct-speed and bounded.

use image::AnimationDecoder;

use crate::cache::thumbnails::ThumbTier;
use crate::AppState;

/// A rendered GIF atlas ready to serve.
pub struct GifAtlas {
    /// PNG sprite sheet bytes.
    pub png: Vec<u8>,
    pub frame_count: u32,
    pub frame_w: u32,
    pub frame_h: u32,
    /// Number of columns in the sprite-sheet grid.
    pub cols: u32,
    /// Per-frame delays in milliseconds.
    pub delays: Vec<u16>,
}

/// Largest sprite-sheet edge we'll produce. Caps memory and stays within
/// canvas/texture limits; very-many-frame GIFs get truncated to fit.
const MAX_ATLAS_DIM: u32 = 8192;

fn file_mtime(path: &str) -> u64 {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// How many frames fit in a `MAX_ATLAS_DIM`-bounded grid at this frame size.
fn frame_capacity(fw: u32, fh: u32) -> usize {
    let max_cols = (MAX_ATLAS_DIM / fw.max(1)).max(1);
    let max_rows = (MAX_ATLAS_DIM / fh.max(1)).max(1);
    (max_cols * max_rows) as usize
}

/// Look up a cached atlas, generating and storing it on a miss.
pub async fn get_or_generate(
    state: &AppState,
    tier: ThumbTier,
    path: String,
) -> Result<GifAtlas, String> {
    let mtime = file_mtime(&path);
    let tier_seg = tier.as_segment();

    // Cache hit?
    {
        let db = state.cache_db.lock().await;
        let db = db.as_ref().ok_or("No gallery open")?;
        if let Ok(Some(hit)) = db.get_gif_atlas(&path, tier_seg, mtime) {
            return Ok(GifAtlas {
                png: hit.atlas,
                frame_count: hit.frame_count,
                frame_w: hit.frame_w,
                frame_h: hit.frame_h,
                cols: hit.cols,
                delays: hit.delays,
            });
        }
    }

    // Miss — decode + pack off the async runtime.
    let target = tier.target_size();
    let gen_path = path.clone();
    let atlas = tokio::task::spawn_blocking(move || generate_atlas(&gen_path, target))
        .await
        .map_err(|e| format!("gif atlas task: {e}"))??;

    // Store (best-effort — a failed write just means we regenerate next time).
    {
        let db = state.cache_db.lock().await;
        if let Some(db) = db.as_ref() {
            if let Err(e) = db.upsert_gif_atlas(
                &path,
                tier_seg,
                mtime,
                atlas.frame_count,
                atlas.frame_w,
                atlas.frame_h,
                atlas.cols,
                &atlas.delays,
                &atlas.png,
            ) {
                log::warn!("gif atlas cache write failed for {path}: {e}");
            }
        }
    }

    Ok(atlas)
}

/// Decode a GIF, composite + downscale each frame, and pack them into a PNG
/// sprite sheet. Pure/blocking — call via `spawn_blocking`.
fn generate_atlas(path: &str, target: u32) -> Result<GifAtlas, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("open {path}: {e}"))?;
    let decoder = image::codecs::gif::GifDecoder::new(std::io::BufReader::new(file))
        .map_err(|e| format!("gif open {path}: {e}"))?;

    let mut delays: Vec<u16> = Vec::new();
    let mut frames: Vec<image::RgbaImage> = Vec::new();
    let mut fw = 0u32;
    let mut fh = 0u32;

    for frame in decoder.into_frames() {
        let frame = frame.map_err(|e| format!("gif frame {path}: {e}"))?;

        let (num, den) = frame.delay().numer_denom_ms();
        let raw = if den == 0 { 0 } else { num / den };
        // Match browser behaviour: 0/very-small frame delays render at 100ms.
        let delay_ms = if raw <= 10 {
            100
        } else {
            raw.min(u16::MAX as u32) as u16
        };

        // Frames are pre-composited at the full logical-screen size.
        let buf = frame.into_buffer();
        if fw == 0 {
            let (w, h) = (buf.width(), buf.height());
            let scale = (target as f32 / w.max(h) as f32).min(1.0);
            fw = ((w as f32 * scale).round() as u32).max(1);
            fh = ((h as f32 * scale).round() as u32).max(1);
        }

        let small = if buf.width() == fw && buf.height() == fh {
            buf
        } else {
            image::imageops::resize(&buf, fw, fh, image::imageops::FilterType::Triangle)
        };
        frames.push(small);
        delays.push(delay_ms);

        if frames.len() >= frame_capacity(fw, fh) {
            log::warn!("gif {path} exceeds atlas capacity; truncating animation");
            break;
        }
    }

    if frames.is_empty() {
        return Err(format!("gif {path} has no frames"));
    }

    let n = frames.len() as u32;
    let max_cols = (MAX_ATLAS_DIM / fw).max(1);
    let cols = ((n as f32).sqrt().ceil() as u32).clamp(1, max_cols);
    let rows = n.div_ceil(cols);

    let atlas_w = cols * fw;
    let atlas_h = rows * fh;
    let mut atlas = image::RgbaImage::new(atlas_w, atlas_h);
    for (i, frame) in frames.iter().enumerate() {
        let i = i as u32;
        let x = ((i % cols) * fw) as i64;
        let y = ((i / cols) * fh) as i64;
        image::imageops::replace(&mut atlas, frame, x, y);
    }

    let mut png = std::io::Cursor::new(Vec::new());
    {
        use image::ImageEncoder;
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(atlas.as_raw(), atlas_w, atlas_h, image::ExtendedColorType::Rgba8)
            .map_err(|e| format!("png encode {path}: {e}"))?;
    }

    Ok(GifAtlas {
        png: png.into_inner(),
        frame_count: n,
        frame_w: fw,
        frame_h: fh,
        cols,
        delays,
    })
}
