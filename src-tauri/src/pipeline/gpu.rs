/// GPU compute capability is detected on the frontend via WebGPU.
/// The frontend calls `notify_gpu_capabilities` after probing the GPU adapter.
///
/// This module provides the Tauri command for the frontend to report back
/// GPU capabilities, which updates the HardwareProfile and enables the
/// BC7 atlas path if BC7 texture compression is supported.

use crate::AppState;

/// Called by the frontend after WebGPU initialization to report GPU capabilities.
/// Enables the BC7 atlas path if the GPU supports texture-compression-bc.
#[tauri::command]
pub async fn notify_gpu_capabilities(
    state: tauri::State<'_, AppState>,
    gpu_compute: bool,
    bc7_supported: bool,
) -> Result<(), String> {
    // Update the hardware profile's gpu_compute flag.
    // Note: HardwareProfile is behind Arc so we can't mutate it directly.
    // Instead, we use the AtomicBool to control the BC7 path.
    if bc7_supported && gpu_compute {
        log::info!("Frontend reports GPU with BC7 support — enabling atlas path");

        // If a gallery is already open, initialize the atlas now.
        let gallery_path = state.current_gallery.read().await.clone();
        if let Some(path) = gallery_path {
            let lightview_dir = std::path::Path::new(&path).join(".lightview");
            if state.thumb_atlas.lock().await.is_none() {
                match crate::cache::atlas::ThumbAtlas::open(&lightview_dir) {
                    Ok(atlas) => {
                        log::info!("BC7 atlas opened with {} thumbnails", atlas.len());
                        let mut a = state.thumb_atlas.lock().await;
                        *a = Some(atlas);
                        state
                            .use_bc7_atlas
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    Err(e) => {
                        log::warn!("Failed to open BC7 atlas: {}", e);
                    }
                }
            }
        }
    } else {
        log::info!("Frontend reports no BC7 GPU support — using JPEG fallback");
        state
            .use_bc7_atlas
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

    Ok(())
}
