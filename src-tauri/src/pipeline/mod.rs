//! Turning a file on disk into thumbnail bytes.
//!
//! - [`thumbnailer`] — the CPU decode/resize/encode path and the format and
//!   filter vocabulary. Everything else here feeds it or specialises it.
//! - [`video`] — `ffmpeg`/`ffprobe` probing and frame extraction.
//! - [`heic_cache`] — a bounded LRU over HEIC→JPEG transcodes.
//! - [`exif`] — capture time and GPS, read without a full decode.
//! - [`idle`] — the background worker that drains the backlog when nobody is
//!   using the gallery.
//! - [`gpu_pipeline`] (feature `gpu`) — a fused crop+resize on wgpu, plus the
//!   viewer's image transforms.
//!
//! All CPU-bound work here runs on one bounded rayon pool (`AppState::thumb_pool`),
//! sized from the hardware profile. Speculative work — look-ahead, landing-zone
//! warms, the idle backfill — lands on that same pool, so there is no second
//! pool to escape to and speculation has to be gated by its callers rather than
//! isolated here.
//!
//! Storage is not this module's concern: it produces bytes and hands them to
//! [`crate::cache`]. Serving is not either — `thumb_serve` and `gif_serve` sit
//! above both and are shared by the desktop protocol handler and the HTTP route.

pub mod thumbnailer;
pub mod video;
pub mod heic_cache;
pub mod exif;
pub mod idle;
#[cfg(feature = "gpu")]
pub mod gpu_pipeline;
