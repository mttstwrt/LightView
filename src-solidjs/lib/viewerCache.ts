// ---------------------------------------------------------------------------
// Viewer Image Cache — preloads adjacent full-resolution images via Image()
// ---------------------------------------------------------------------------
//
// Uses HTMLImageElement objects for preloading, which go through the exact same
// webview pipeline as <img src> — guaranteed to work with Tauri's custom
// protocol handler. Preloaded Image elements are swapped directly into the DOM
// for instant display, avoiding any re-fetch or re-decode.
//
// Memory pressure integration (cache is bounded by BOTH an image count and an
// estimated decoded-byte budget, so a few huge full-res images can't blow past
// the memory ceiling that a count-only limit would allow):
//   - Normal:    cache up to 10 images / 512MB, preload 2 ahead/behind
//   - Warning:   cache up to 5 images / 192MB,  preload 1 ahead/behind
//   - Emergency: cache only current image / 64MB floor, no preloading
//
// The level comes from memoryPressure.ts, which reads the host's free RAM on
// the desktop and the device class on the web — see that module for why the
// two runtimes cannot share one signal.

import { mediaUrl } from "./ipc";
import { isVideoPath } from "./mediaExts";
import { MemoryPressureMonitor, type PressureLevel } from "./memoryPressure";

interface CacheEntry {
  img: HTMLImageElement;
  lastAccess: number;
  /** Estimated decoded size in bytes (naturalWidth × naturalHeight × 4). */
  bytes: number;
}

const MB = 1024 * 1024;

const PRESSURE_CONFIGS: Record<
  PressureLevel,
  {
    maxCache: number;
    maxBytes: number;
    preloadRange: number;
    maxConcurrentLoads: number;
  }
> = {
  normal: { maxCache: 10, maxBytes: 512 * MB, preloadRange: 2, maxConcurrentLoads: 3 },
  warning: { maxCache: 5, maxBytes: 192 * MB, preloadRange: 1, maxConcurrentLoads: 2 },
  emergency: { maxCache: 1, maxBytes: 64 * MB, preloadRange: 0, maxConcurrentLoads: 1 },
};

/** Estimate the decoded RGBA footprint of an image element. */
function estimateBytes(img: HTMLImageElement): number {
  const w = img.naturalWidth || 0;
  const h = img.naturalHeight || 0;
  // 4 bytes/px decoded; fall back to a small non-zero floor if dimensions
  // aren't known yet so an entry never counts as weightless.
  return w > 0 && h > 0 ? w * h * 4 : 64 * 1024;
}

export class ViewerImageCache {
  private cache = new Map<string, CacheEntry>();
  private pending = new Set<string>();
  private accessCounter = 0;
  private totalBytes = 0;
  private pressureLevel: PressureLevel = "normal";
  private monitor: MemoryPressureMonitor;
  private readyCallback: ((path: string) => void) | null = null;

  constructor() {
    this.monitor = new MemoryPressureMonitor((level: PressureLevel) => {
      this.pressureLevel = level;
      this.trimToCapacity();
    });
    this.monitor.start();
  }

  private get config() {
    return PRESSURE_CONFIGS[this.pressureLevel];
  }

  /** Register a callback invoked when a preloaded image finishes decoding. */
  onReady(cb: (path: string) => void) {
    this.readyCallback = cb;
  }

  /**
   * Get a fully-loaded Image element for the given path, or null.
   * Touches the entry for LRU tracking.
   */
  get(path: string): HTMLImageElement | null {
    const entry = this.cache.get(path);
    if (entry) {
      entry.lastAccess = ++this.accessCounter;
      return entry.img;
    }
    return null;
  }

  /** Returns true if the path is fully loaded in cache. */
  has(path: string): boolean {
    return this.cache.has(path);
  }

  /** Number of cached images. */
  get size(): number {
    return this.cache.size;
  }

  /** Insert an externally-loaded Image element into the cache. */
  insert(path: string, img: HTMLImageElement) {
    if (this.cache.has(path)) return;
    const bytes = estimateBytes(img);
    this.cache.set(path, {
      img,
      lastAccess: ++this.accessCounter,
      bytes,
    });
    this.totalBytes += bytes;
    this.trimToCapacity();
  }

  /**
   * Preload images around the given index. Loads the current image plus
   * adjacent images based on memory pressure level.
   *
   * Limits concurrent in-flight loads to avoid saturating memory with
   * multiple large full-resolution files being fetched simultaneously.
   */
  preload(paths: string[], currentIndex: number) {
    const { preloadRange, maxConcurrentLoads } = this.config;

    const toLoad: { path: string; distance: number }[] = [];
    for (let offset = -preloadRange; offset <= preloadRange; offset++) {
      const idx = currentIndex + offset;
      if (idx < 0 || idx >= paths.length) continue;
      const path = paths[idx];
      // Skip videos — they'd be fetched by `<img>` only to fail decoding,
      // wasting bandwidth and (pre-axum) blocking the protocol thread.
      // The viewer lazy-mounts <video> on play-click anyway.
      if (isVideoPath(path)) continue;
      if (!this.cache.has(path) && !this.pending.has(path)) {
        toLoad.push({ path, distance: Math.abs(offset) });
      }
    }

    // Load nearest first, but respect concurrent load limit
    toLoad.sort((a, b) => a.distance - b.distance);
    const slotsAvailable = Math.max(0, maxConcurrentLoads - this.pending.size);
    for (const { path } of toLoad.slice(0, slotsAvailable)) {
      this.loadImage(path);
    }
  }

  private loadImage(path: string) {
    if (this.pending.has(path) || this.cache.has(path)) return;
    this.pending.add(path);

    const img = new Image();
    // Apply same styling as the viewer <img> so layout doesn't shift
    img.style.maxWidth = "100vw";
    img.style.maxHeight = "100vh";
    img.style.objectFit = "contain";
    img.draggable = false;

    img.onload = () => {
      this.pending.delete(path);
      const bytes = estimateBytes(img);
      this.cache.set(path, {
        img,
        lastAccess: ++this.accessCounter,
        bytes,
      });
      this.totalBytes += bytes;
      this.trimToCapacity();
      this.readyCallback?.(path);
    };

    img.onerror = () => {
      this.pending.delete(path);
    };

    img.src = mediaUrl(path);
  }

  private trimToCapacity() {
    const { maxCache, maxBytes } = this.config;
    // Evict on either bound, but never drop below one entry — the current
    // image must survive even if it alone exceeds the byte budget (e.g. a
    // single 100MP photo).
    while (
      this.cache.size > 1 &&
      (this.cache.size > maxCache || this.totalBytes > maxBytes)
    ) {
      if (!this.evictLRU()) break;
    }
  }

  /** Evict the least-recently-used entry. Returns false if nothing was evicted. */
  private evictLRU(): boolean {
    let oldestPath: string | null = null;
    let oldestAccess = Infinity;

    for (const [path, entry] of this.cache) {
      if (entry.lastAccess < oldestAccess) {
        oldestAccess = entry.lastAccess;
        oldestPath = path;
      }
    }

    if (oldestPath) {
      const entry = this.cache.get(oldestPath)!;
      entry.img.src = "";  // Release the image data
      this.totalBytes -= entry.bytes;
      this.cache.delete(oldestPath);
      return true;
    }
    return false;
  }

  destroy() {
    this.monitor.stop();
    this.readyCallback = null;
    this.pending.clear();
    for (const [, entry] of this.cache) {
      entry.img.src = "";
    }
    this.cache.clear();
    this.totalBytes = 0;
  }
}
