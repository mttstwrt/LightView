//! Video probing and frame extraction, delegated to the `ffmpeg`/`ffprobe`
//! CLIs.
//!
//! Everything here is shaped by one workload: a phone camera roll. Those are
//! thousands of `.MOV`/`.mp4` clips at 4K, and the naive "spawn ffprobe, spawn
//! ffmpeg, pipe a full-resolution frame back" costs ~33 MB of pipe traffic and
//! a full-size CPU resize *per clip*, which is why opening such a folder used
//! to stall. Three things keep it cheap:
//!
//! * **ffmpeg does the downscale.** The frame is scaled inside the filter graph
//!   to the size the caller actually wants, so what crosses the pipe is a few
//!   hundred KB rather than tens of MB, and swscale does the work instead of
//!   our resizer. The scale is spelled out as exact pixel dimensions (not
//!   `force_original_aspect_ratio`, whose rounding we'd have to predict), which
//!   also makes the raw output length deterministic — the old code's
//!   "output size mismatch" failure mode simply can't happen now.
//! * **Rotation is honoured.** Phone video is recorded landscape with a display
//!   matrix telling players to rotate it. ffmpeg applies that automatically, so
//!   the frame that comes back is portrait while `ffprobe`'s coded `width`/
//!   `height` are landscape. Reading the rotation and swapping the probed
//!   dimensions is what stops portrait clips from being laid out — and
//!   thumbnailed — sideways.
//! * **One probe per file.** The probe result is memoised (keyed on mtime), so
//!   the thumbnail path and the metadata path that runs right behind it share a
//!   single `ffprobe` spawn instead of taking one each.
//!
//! Both binaries are probed once and remembered: with no ffmpeg installed every
//! video would otherwise pay two failed spawns before falling back to the grey
//! placeholder. Every invocation is also bounded by a timeout — a wedged
//! subprocess would otherwise hold one of the (few) thumbnail pool threads for
//! the life of the process.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use super::thumbnailer::ThumbError;

/// Wall-clock ceiling for an `ffprobe` call.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
/// Wall-clock ceiling for a frame-extraction call. Higher than the probe: a
/// seek into a long clip on a cold spinning disk is legitimately slow.
const EXTRACT_TIMEOUT: Duration = Duration::from_secs(45);
/// Entries kept in the probe memo before it's dropped wholesale. This exists to
/// bridge the few milliseconds between thumbnailing a file and recording its
/// metadata, not to be a long-lived cache, so a coarse bound is fine.
const PROBE_CACHE_CAP: usize = 1024;

/// What a probe tells us about a video, in *display* terms — dimensions have
/// any rotation from the container's display matrix already applied, so they
/// describe the frames a player (and our thumbnailer) actually sees.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    pub duration: Option<f64>,
}

// ---------------------------------------------------------------------------
// Tool availability
// ---------------------------------------------------------------------------

static FFMPEG_PRESENT: OnceLock<bool> = OnceLock::new();
static FFPROBE_PRESENT: OnceLock<bool> = OnceLock::new();

fn binary_present(bin: &str, cache: &OnceLock<bool>) -> bool {
    *cache.get_or_init(|| {
        Command::new(bin)
            .arg("-version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

/// Whether frame extraction is possible at all. Checked once per process.
pub fn ffmpeg_available() -> bool {
    binary_present("ffmpeg", &FFMPEG_PRESENT)
}

/// Whether probing is possible at all. Checked once per process.
pub fn ffprobe_available() -> bool {
    binary_present("ffprobe", &FFPROBE_PRESENT)
}

// ---------------------------------------------------------------------------
// Probing
// ---------------------------------------------------------------------------

type ProbeCache = Mutex<HashMap<PathBuf, (Option<SystemTime>, VideoInfo)>>;

fn probe_cache() -> &'static ProbeCache {
    static CACHE: OnceLock<ProbeCache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

/// Probe a video's display dimensions and duration.
///
/// Memoised on (path, mtime): the thumbnail pipeline and the `media_meta`
/// writer that follows it both need this, and an `ffprobe` spawn per caller is
/// exactly the per-file cost this module exists to remove.
pub fn probe(path: &Path) -> Result<VideoInfo, ThumbError> {
    if !ffprobe_available() {
        return Err(ThumbError::Decode("ffprobe not available".to_string()));
    }

    let mtime = file_mtime(path);
    if let Ok(cache) = probe_cache().lock()
        && let Some((cached_mtime, info)) = cache.get(path)
        && *cached_mtime == mtime
    {
        return Ok(*info);
    }

    let info = probe_uncached(path)?;

    if let Ok(mut cache) = probe_cache().lock() {
        if cache.len() >= PROBE_CACHE_CAP {
            cache.clear();
        }
        cache.insert(path.to_path_buf(), (mtime, info));
    }
    Ok(info)
}

fn probe_uncached(path: &Path) -> Result<VideoInfo, ThumbError> {
    // `-show_streams`/`-show_format` rather than a targeted `-show_entries`:
    // the rotation lives in a nested `side_data_list` whose `-show_entries`
    // spelling has changed across ffmpeg releases, and the extra few KB of
    // JSON for a single stream costs nothing next to the spawn itself.
    let mut cmd = Command::new("ffprobe");
    cmd.args([
        "-v", "error",
        "-select_streams", "v:0",
        "-show_streams",
        "-show_format",
        "-of", "json",
    ])
    .arg(path.as_os_str());

    let stdout = run(cmd, PROBE_TIMEOUT, "ffprobe")?;
    let json: serde_json::Value = serde_json::from_slice(&stdout)
        .map_err(|e| ThumbError::Decode(format!("ffprobe json parse error: {}", e)))?;

    let stream = json
        .get("streams")
        .and_then(|s| s.get(0))
        .ok_or_else(|| ThumbError::Decode("ffprobe found no video stream".to_string()))?;

    let coded_w = stream.get("width").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    let coded_h = stream.get("height").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    if coded_w == 0 || coded_h == 0 {
        return Err(ThumbError::Decode("ffprobe reported zero dimensions".to_string()));
    }

    let (width, height) = if rotation_swaps_axes(rotation_of(stream)) {
        (coded_h, coded_w)
    } else {
        (coded_w, coded_h)
    };

    let duration = positive_number(stream.get("duration"))
        .or_else(|| positive_number(json.get("format").and_then(|f| f.get("duration"))));

    Ok(VideoInfo { width, height, duration })
}

/// ffprobe reports numbers as JSON strings ("12.345") in most writers but as
/// bare numbers in others (and for side-data entries); accept either.
fn number_field(v: Option<&serde_json::Value>) -> Option<f64> {
    let v = v?;
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse::<f64>().ok()))
        .filter(|d| d.is_finite())
}

/// A duration, which is only meaningful when positive — ffprobe writes "N/A"
/// or 0 for streams whose length it can't determine.
fn positive_number(v: Option<&serde_json::Value>) -> Option<f64> {
    number_field(v).filter(|d| *d > 0.0)
}

/// Rotation in degrees, normalised to 0..360.
///
/// ffmpeg ≥ 5 exposes the display matrix as a `rotation` entry in the stream's
/// `side_data_list` (negative, e.g. -90); older builds only surfaced it as the
/// `rotate` stream tag (positive). Only the axis swap matters to us, so the
/// sign convention difference is irrelevant.
fn rotation_of(stream: &serde_json::Value) -> i32 {
    let from_side_data = stream
        .get("side_data_list")
        .and_then(|v| v.as_array())
        .and_then(|list| list.iter().find_map(|sd| number_field(sd.get("rotation"))));

    let degrees = from_side_data.or_else(|| {
        number_field(stream.get("tags").and_then(|t| t.get("rotate")))
    });

    match degrees {
        Some(d) => {
            let r = (d.round() as i64) % 360;
            (if r < 0 { r + 360 } else { r }) as i32
        }
        None => 0,
    }
}

fn rotation_swaps_axes(degrees: i32) -> bool {
    degrees == 90 || degrees == 270
}

// ---------------------------------------------------------------------------
// Frame extraction
// ---------------------------------------------------------------------------

/// A decoded video frame: RGBA pixels plus the dimensions they were decoded at
/// (already downscaled towards the caller's target).
pub struct Frame {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// The clip's true display dimensions, before the downscale.
    pub src_width: u32,
    pub src_height: u32,
}

/// Extract a single representative frame, downscaled towards `target_edge`.
///
/// `target_edge` is the size the caller will resize to, matched the same way
/// the JPEG decoder matches its DCT scaling: both axes are kept at or above it
/// (never upscaling), so a subsequent square centre-crop still has full detail
/// to work with.
pub fn extract_frame(path: &Path, target_edge: u32) -> Result<Frame, ThumbError> {
    if !ffmpeg_available() {
        return Err(ThumbError::Decode("ffmpeg not available".to_string()));
    }

    let info = probe(path)?;
    let (w, h) = cover_dims(info.width, info.height, target_edge);
    let expected = (w as usize) * (h as usize) * 4;

    // Seek a second in first: the opening frames of a clip are routinely black
    // or mid-exposure-ramp. Clips shorter than that (and streams that can't
    // key-seek there) produce nothing, so retry from the very start.
    let rgba = match run_frame(path, Some("1"), w, h, expected) {
        Ok(bytes) => bytes,
        Err(_) => run_frame(path, None, w, h, expected)?,
    };

    Ok(Frame {
        rgba,
        width: w,
        height: h,
        src_width: info.width,
        src_height: info.height,
    })
}

/// Scale factor that keeps *both* axes at or above `target_edge`, never
/// upscaling. Mirrors the JPEG decoder's DCT-scaling rule so the two decode
/// paths hand the resizer comparably sized input.
fn cover_dims(w: u32, h: u32, target_edge: u32) -> (u32, u32) {
    let short = w.min(h);
    if short == 0 || target_edge == 0 || short <= target_edge {
        return (w.max(1), h.max(1));
    }
    let scale = (target_edge as f64) / (short as f64);
    let sw = ((w as f64) * scale).round().max(1.0) as u32;
    let sh = ((h as f64) * scale).round().max(1.0) as u32;
    (sw, sh)
}

fn run_frame(
    path: &Path,
    seek: Option<&str>,
    w: u32,
    h: u32,
    expected: usize,
) -> Result<Vec<u8>, ThumbError> {
    let mut cmd = Command::new("ffmpeg");
    // `-nostdin` as well as the null stdin below: ffmpeg reads its console
    // regardless of whether anything is piped to it.
    cmd.args(["-nostdin", "-v", "error"]);
    if let Some(ss) = seek {
        cmd.args(["-ss", ss]);
    }
    cmd.arg("-i").arg(path.as_os_str());
    // Exact output dimensions, so the raw byte count is known up front and the
    // aspect can't drift on ffmpeg's rounding. Autorotation is inserted ahead
    // of this filter, so `w`/`h` are display-oriented.
    let scale = format!("scale={}:{}:flags=bicubic", w, h);
    cmd.args([
        // Ignore the audio/subtitle/timecode streams a phone clip carries.
        "-map", "0:v:0",
        "-an", "-sn", "-dn",
        "-frames:v", "1",
        "-vf", scale.as_str(),
        "-f", "rawvideo",
        "-pix_fmt", "rgba",
        "pipe:1",
    ]);

    let stdout = run(cmd, EXTRACT_TIMEOUT, "ffmpeg")?;
    if stdout.len() != expected {
        return Err(ThumbError::Decode(format!(
            "ffmpeg output size mismatch: got {} bytes, expected {}",
            stdout.len(),
            expected,
        )));
    }
    Ok(stdout)
}

// ---------------------------------------------------------------------------
// Subprocess plumbing
// ---------------------------------------------------------------------------

/// Run a command to completion under a timeout, returning its stdout.
///
/// stdout and stderr are drained on their own threads: a frame is megabytes and
/// would deadlock against a pipe buffer if we waited for exit before reading.
/// On timeout the child is killed — one video that makes ffmpeg spin must not
/// permanently consume a thumbnail-pool thread.
fn run(mut cmd: Command, timeout: Duration, label: &str) -> Result<Vec<u8>, ThumbError> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| ThumbError::Decode(format!("{} failed to start: {}", label, e)))?;

    let mut out = child.stdout.take();
    let mut err = child.stderr.take();
    let out_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(o) = out.as_mut() {
            let _ = o.read_to_end(&mut buf);
        }
        buf
    });
    let err_reader = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(e) = err.as_mut() {
            let _ = e.read_to_string(&mut buf);
        }
        buf
    });

    let deadline = Instant::now() + timeout;
    // Back off from a tight poll so a fast probe isn't padded by a fixed nap,
    // while a slow decode doesn't burn the thread spinning.
    let mut nap = Duration::from_millis(1);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = out_reader.join();
                    let _ = err_reader.join();
                    return Err(ThumbError::Decode(format!(
                        "{} timed out after {}s",
                        label,
                        timeout.as_secs()
                    )));
                }
                std::thread::sleep(nap);
                nap = (nap * 2).min(Duration::from_millis(25));
            }
            Err(e) => {
                return Err(ThumbError::Decode(format!("{} wait failed: {}", label, e)));
            }
        }
    };

    let stdout = out_reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();

    if !status.success() {
        return Err(ThumbError::Decode(format!("{} failed: {}", label, stderr.trim())));
    }
    Ok(stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cover_dims_keeps_both_axes_at_or_above_target() {
        // 4K landscape down to a 512 square crop: the short edge lands on the
        // target, the long edge stays proportionally larger.
        assert_eq!(cover_dims(3840, 2160, 512), (910, 512));
        // Portrait (what a rotated phone clip looks like after autorotation).
        assert_eq!(cover_dims(1080, 1920, 512), (512, 910));
    }

    #[test]
    fn cover_dims_never_upscales() {
        assert_eq!(cover_dims(320, 240, 512), (320, 240));
        assert_eq!(cover_dims(640, 480, 512), (640, 480));
    }

    #[test]
    fn cover_dims_tolerates_degenerate_input() {
        assert_eq!(cover_dims(0, 0, 512), (1, 1));
        assert_eq!(cover_dims(1920, 1080, 0), (1920, 1080));
    }

    #[test]
    fn rotation_read_from_modern_side_data() {
        let stream = serde_json::json!({
            "width": 1920,
            "height": 1080,
            "side_data_list": [{ "side_data_type": "Display Matrix", "rotation": -90 }],
        });
        assert_eq!(rotation_of(&stream), 270);
        assert!(rotation_swaps_axes(rotation_of(&stream)));
    }

    #[test]
    fn rotation_falls_back_to_the_legacy_rotate_tag() {
        let stream = serde_json::json!({ "tags": { "rotate": "90" } });
        assert_eq!(rotation_of(&stream), 90);
        assert!(rotation_swaps_axes(rotation_of(&stream)));
    }

    #[test]
    fn unrotated_streams_keep_their_axes() {
        let stream = serde_json::json!({ "width": 1920, "height": 1080 });
        assert_eq!(rotation_of(&stream), 0);
        assert!(!rotation_swaps_axes(0));
        assert!(!rotation_swaps_axes(180));
    }

    #[test]
    fn duration_parses_from_string_or_number() {
        assert_eq!(positive_number(Some(&serde_json::json!("12.5"))), Some(12.5));
        assert_eq!(positive_number(Some(&serde_json::json!(12.5))), Some(12.5));
        assert_eq!(positive_number(Some(&serde_json::json!("N/A"))), None);
        assert_eq!(positive_number(Some(&serde_json::json!("0"))), None);
        assert_eq!(positive_number(None), None);
    }

    // --- ffmpeg-backed round trips -----------------------------------------
    // Skipped (rather than failed) where the tools aren't installed, so the
    // suite still runs on a machine without ffmpeg — which is also the
    // configuration these functions have to degrade gracefully on.

    /// Encode a clip with `testsrc` and stamp it with a display matrix, which
    /// is what a phone recording sideways produces. `-display_rotation` is an
    /// input option, so this is a two-step encode-then-remux.
    fn make_clip(dir: &Path, name: &str, w: u32, h: u32, rotation: Option<&str>) -> PathBuf {
        let flat = dir.join(format!("flat-{name}"));
        let status = Command::new("ffmpeg")
            .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
            .arg(format!("testsrc=size={w}x{h}:duration=2:rate=10"))
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p"])
            .arg(&flat)
            .status()
            .expect("spawn ffmpeg");
        assert!(status.success(), "failed to encode test clip");

        let Some(deg) = rotation else { return flat };
        let rotated = dir.join(name);
        let status = Command::new("ffmpeg")
            .args(["-y", "-v", "error", "-display_rotation", deg, "-i"])
            .arg(&flat)
            .args(["-c", "copy"])
            .arg(&rotated)
            .status()
            .expect("spawn ffmpeg");
        assert!(status.success(), "failed to remux test clip");
        rotated
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lightview-video-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn probes_and_extracts_a_plain_clip() {
        if !ffmpeg_available() || !ffprobe_available() {
            eprintln!("skipping: ffmpeg/ffprobe not installed");
            return;
        }
        let dir = temp_dir("plain");
        let clip = make_clip(&dir, "plain.mov", 640, 480, None);

        let info = probe(&clip).expect("probe");
        assert_eq!((info.width, info.height), (640, 480));
        assert!(info.duration.unwrap_or(0.0) > 1.0);

        // Target below the short edge, so the frame comes back downscaled.
        let frame = extract_frame(&clip, 240).expect("extract");
        assert_eq!((frame.width, frame.height), (320, 240));
        assert_eq!((frame.src_width, frame.src_height), (640, 480));
        assert_eq!(frame.rgba.len(), 320 * 240 * 4);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rotated_clip_reports_and_decodes_portrait() {
        if !ffmpeg_available() || !ffprobe_available() {
            eprintln!("skipping: ffmpeg/ffprobe not installed");
            return;
        }
        let dir = temp_dir("rotated");
        // Landscape on disk, portrait on screen — the shape of every clip shot
        // upright on a phone.
        let clip = make_clip(&dir, "rot.mov", 320, 180, Some("90"));

        let info = probe(&clip).expect("probe");
        assert_eq!(
            (info.width, info.height),
            (180, 320),
            "display dimensions must have the rotation applied",
        );

        // ffmpeg autorotates ahead of our scale filter, so the frame arrives
        // portrait. If the two disagreed the raw output would be the wrong
        // length and this would fail rather than silently squashing.
        let frame = extract_frame(&clip, 90).expect("extract");
        assert_eq!((frame.width, frame.height), (90, 160));
        assert_eq!(frame.rgba.len(), 90 * 160 * 4);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_short_clip_falls_back_to_its_first_frame() {
        if !ffmpeg_available() || !ffprobe_available() {
            eprintln!("skipping: ffmpeg/ffprobe not installed");
            return;
        }
        let dir = temp_dir("short");
        // Well under the 1 s seek the extractor tries first.
        let flat = dir.join("short.mov");
        let status = Command::new("ffmpeg")
            .args([
                "-y", "-v", "error", "-f", "lavfi",
                "-i", "testsrc=size=160x120:duration=0.2:rate=10",
                "-c:v", "libx264", "-pix_fmt", "yuv420p",
            ])
            .arg(&flat)
            .status()
            .expect("spawn ffmpeg");
        assert!(status.success());

        let frame = extract_frame(&flat, 512).expect("extract");
        assert_eq!((frame.width, frame.height), (160, 120));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn probing_a_non_video_fails_rather_than_hanging() {
        if !ffprobe_available() {
            eprintln!("skipping: ffprobe not installed");
            return;
        }
        let dir = temp_dir("garbage");
        let junk = dir.join("not-a-video.mov");
        std::fs::write(&junk, b"this is not a container").expect("write");

        assert!(probe(&junk).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
