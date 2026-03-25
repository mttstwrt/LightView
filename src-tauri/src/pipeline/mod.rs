pub mod thumbnailer;
pub mod gpu;
#[cfg(feature = "gpu")]
pub mod gpu_pipeline;
// Legacy alias — gpu_pipeline supersedes gpu_resize
#[cfg(all(feature = "gpu-resize", not(feature = "gpu")))]
pub mod gpu_resize;
