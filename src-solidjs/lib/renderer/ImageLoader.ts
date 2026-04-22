// ---------------------------------------------------------------------------
// Image loader — off-thread decoding via Web Worker (Phase 5)
// ---------------------------------------------------------------------------
//
// Shared between Canvas2D and WebGL renderers. Manages a pressure-aware LRU
// cache of decoded images so the renderer can draw immediately without waiting
// for async loads.
//
// Phase 3 enhancements:
//   - Dynamic cache capacity driven by MemoryPressureMonitor
//   - ImageBitmap.close() called on eviction to release GPU memory immediately
//   - Viewport-distance + recency hybrid eviction scoring
//
// Phase 5 enhancements:
//   - All fetch + decode moved to a Dedicated Web Worker
//   - Main thread only receives already-decoded ImageBitmap objects
//   - Priority tiers (viewport=0, buffer=1, background=2) respected by worker
//   - Zero-copy ImageBitmap transfer via postMessage transferables

import type {
  WorkerRequest,
  WorkerResponse,
  DecodeRequest,
} from "./imageDecodeWorker";

export type DecodePriority = 0 | 1 | 2;
export type ImageReadyCallback = (path: string, img: ImageBitmap) => void;

const MAX_CONCURRENT_LOADS = 12;
const DEFAULT_CACHE_SIZE = 300;

interface CacheEntry {
  img: ImageBitmap;
  /** URL this bitmap was fetched from — a mismatch on request() forces refetch. */
  url: string;
  /** Monotonically increasing access counter for recency tracking. */
  lastAccess: number;
}

export class ImageLoader {
  /** path -> decoded image + metadata (LRU by access counter). */
  private cache = new Map<string, CacheEntry>();
  /** Paths currently queued or inflight in the worker. */
  private pending = new Set<string>();
  /** URL each pending request was issued for (so the decoded bitmap records it). */
  private pendingUrls = new Map<string, string>();
  /** Paths that failed loading — won't be retried until evicted. */
  private failed = new Set<string>();
  /** Monotonically increasing counter for recency tracking. */
  private accessCounter = 0;

  private onReady: ImageReadyCallback;
  private onError: ((path: string) => void) | null;
  private _maxCacheSize: number;

  /** Optional: map of path -> index for viewport distance calculation. */
  private pathToIndex: Map<string, number> | null = null;
  /** Optional: current viewport center index for distance scoring. */
  private viewportCenter = 0;

  /** Dedicated Web Worker for off-thread image decoding. */
  private worker: Worker;

  constructor(
    onReady: ImageReadyCallback,
    onError: ((path: string) => void) | null = null,
    maxCacheSize = DEFAULT_CACHE_SIZE,
  ) {
    this.onReady = onReady;
    this.onError = onError;
    this._maxCacheSize = maxCacheSize;

    // Launch the decode worker
    this.worker = new Worker(
      new URL("./imageDecodeWorker.ts", import.meta.url),
      { type: "module" },
    );

    this.worker.onmessage = (e: MessageEvent<WorkerResponse>) => {
      this.handleWorkerMessage(e.data);
    };
  }

  // -------------------------------------------------------------------------
  // Worker message handler
  // -------------------------------------------------------------------------

  private handleWorkerMessage(msg: WorkerResponse) {
    switch (msg.type) {
      case "decoded": {
        const url = this.pendingUrls.get(msg.path) ?? "";
        this.pendingUrls.delete(msg.path);
        this.pending.delete(msg.path);
        this.insertCache(msg.path, msg.bitmap, url);
        this.onReady(msg.path, msg.bitmap);
        break;
      }
      case "error": {
        this.pendingUrls.delete(msg.path);
        this.pending.delete(msg.path);
        this.failed.add(msg.path);
        this.onError?.(msg.path);
        break;
      }
    }
  }

  private postToWorker(msg: WorkerRequest) {
    this.worker.postMessage(msg);
  }

  // -------------------------------------------------------------------------
  // Pressure-aware API
  // -------------------------------------------------------------------------

  /** Update cache capacity dynamically (called by MemoryPressureMonitor). */
  setMaxCacheSize(size: number) {
    this._maxCacheSize = size;
    this.trimToCapacity();
  }

  get maxCacheSize(): number {
    return this._maxCacheSize;
  }

  /** Provide viewport context for distance-based eviction. */
  setViewportContext(pathToIndex: Map<string, number>, centerIndex: number) {
    this.pathToIndex = pathToIndex;
    this.viewportCenter = centerIndex;
  }

  /** Emergency purge: evict everything except the given keep set. */
  emergencyPurge(keepPaths: Set<string>) {
    for (const [path, entry] of this.cache) {
      if (!keepPaths.has(path)) {
        entry.img.close();
        this.cache.delete(path);
      }
    }
  }

  // -------------------------------------------------------------------------
  // Core API
  // -------------------------------------------------------------------------

  /** Get a cached image, or null if not yet loaded. */
  get(path: string): ImageBitmap | null {
    const entry = this.cache.get(path);
    if (entry) {
      // Update recency
      entry.lastAccess = ++this.accessCounter;
      return entry.img;
    }
    return null;
  }

  /**
   * Request an image. If a bitmap is already cached for the same URL, returns
   * it immediately. On URL mismatch (e.g. LOD tier change), the old bitmap is
   * still returned as a placeholder while a fresh decode is queued; the new
   * bitmap replaces it via onReady when it arrives.
   * @param priority 0=viewport, 1=buffer, 2=background (default: 1)
   */
  request(
    path: string,
    url: string,
    priority: DecodePriority = 1,
  ): ImageBitmap | null {
    const cached = this.cache.get(path);
    if (cached && cached.url === url) {
      cached.lastAccess = ++this.accessCounter;
      return cached.img;
    }

    if (!this.pending.has(path) && !this.failed.has(path)) {
      this.pending.add(path);
      this.pendingUrls.set(path, url);
      this.postToWorker({
        type: "decode",
        path,
        url,
        priority,
      });
    }

    return cached?.img ?? null;
  }

  /** Cancel all pending loads and clear the queue (e.g. on generation change). */
  cancelAll() {
    this.postToWorker({ type: "cancelAll" });
    this.pending.clear();
    this.pendingUrls.clear();
    this.failed.clear();
  }

  /** Evict a specific path from cache. */
  evict(path: string) {
    const entry = this.cache.get(path);
    if (entry) {
      entry.img.close();
      this.cache.delete(path);
    }
    this.failed.delete(path);
    if (this.pending.has(path)) {
      this.postToWorker({ type: "cancel", path });
      this.pending.delete(path);
      this.pendingUrls.delete(path);
    }
  }

  /** Evict paths not in the provided keep set. */
  evictExcept(keepPaths: Set<string>) {
    for (const [p, entry] of this.cache) {
      if (!keepPaths.has(p)) {
        entry.img.close();
        this.cache.delete(p);
      }
    }
  }

  /** Check if a path has a cached image. */
  has(path: string): boolean {
    return this.cache.has(path);
  }

  /** Number of cached images. */
  get size(): number {
    return this.cache.size;
  }

  destroy() {
    this.cancelAll();
    for (const [, entry] of this.cache) {
      entry.img.close();
    }
    this.cache.clear();
    this.worker.terminate();
  }

  // -------------------------------------------------------------------------
  // Internal
  // -------------------------------------------------------------------------

  private insertCache(path: string, img: ImageBitmap, url: string) {
    const existing = this.cache.get(path);
    if (existing && existing.img !== img) {
      existing.img.close();
    }
    this.cache.set(path, {
      img,
      url,
      lastAccess: ++this.accessCounter,
    });
    this.trimToCapacity();
  }

  /**
   * Trim cache to capacity using a hybrid score: viewport distance + recency.
   * Items far from the viewport and accessed least recently are evicted first.
   */
  private trimToCapacity() {
    while (this.cache.size > this._maxCacheSize) {
      const victim = this.findEvictionCandidate();
      if (victim) {
        victim[1].img.close();
        this.cache.delete(victim[0]);
      } else {
        // Fallback: evict oldest by insertion order
        const oldest = this.cache.keys().next().value;
        if (oldest !== undefined) {
          const entry = this.cache.get(oldest)!;
          entry.img.close();
          this.cache.delete(oldest);
        } else {
          break;
        }
      }
    }
  }

  /**
   * Find the best eviction candidate using hybrid scoring (Phase 3.3).
   * Score = distance_from_viewport * weight + inverse_recency * weight
   * Higher score = better candidate for eviction.
   */
  private findEvictionCandidate(): [string, CacheEntry] | null {
    let worstPath: string | null = null;
    let worstEntry: CacheEntry | null = null;
    let worstScore = -Infinity;

    const maxAccess = this.accessCounter;

    for (const [path, entry] of this.cache) {
      // Distance component
      let distance = 0;
      if (this.pathToIndex) {
        const idx = this.pathToIndex.get(path);
        if (idx !== undefined) {
          distance = Math.abs(idx - this.viewportCenter);
        } else {
          // Path not in current set — high eviction priority
          distance = 100_000;
        }
      }

      // Recency component: how many accesses ago (normalized)
      const staleness = maxAccess - entry.lastAccess;

      // Combined score: distance is primary, recency breaks ties
      const score = distance * 10 + staleness;

      if (score > worstScore) {
        worstScore = score;
        worstPath = path;
        worstEntry = entry;
      }
    }

    if (worstPath && worstEntry) {
      return [worstPath, worstEntry];
    }
    return null;
  }
}
