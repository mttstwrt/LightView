/**
 * WebGPU capability detection for LightView.
 *
 * Probes the GPU adapter on startup and reports BC7 support back to the
 * Rust backend via IPC. The backend uses this to decide whether to use
 * the BC7 atlas for persistent thumbnail storage (GPU-accelerated encoding).
 *
 * All thumbnail rendering is handled server-side — the frontend receives
 * JPEG data and displays it via standard <img> elements.
 */

import { notifyGpuCapabilities } from "./ipc";

let gpuSupportsBC7 = false;
let gpuInitPromise: Promise<boolean> | null = null;

/**
 * Initialize WebGPU and check for BC7 texture support.
 * Reports capabilities to the Rust backend.
 * Safe to call multiple times — caches the result.
 */
export async function initGPU(): Promise<boolean> {
  if (gpuInitPromise) return gpuInitPromise;

  gpuInitPromise = (async () => {
    if (!navigator.gpu) {
      console.log("WebGPU not available — backend will use JPEG thumbnails");
      await notifyGpuCapabilities(false, false).catch(() => {});
      return false;
    }

    const adapter = await navigator.gpu.requestAdapter();
    if (!adapter) {
      console.log("No WebGPU adapter — backend will use JPEG thumbnails");
      await notifyGpuCapabilities(false, false).catch(() => {});
      return false;
    }

    // Check for BC7 (texture-compression-bc) support
    const bc7 = adapter.features.has("texture-compression-bc");
    gpuSupportsBC7 = bc7;

    await notifyGpuCapabilities(true, bc7).catch(() => {});

    console.log(
      bc7
        ? "WebGPU: BC7 supported — backend will use GPU BC7 encoding for atlas"
        : "WebGPU: no BC7 support — backend will use CPU encoding"
    );

    return bc7;
  })();

  return gpuInitPromise;
}

/** Check if BC7 is supported (after init). */
export function isBC7Available(): boolean {
  return gpuSupportsBC7;
}
