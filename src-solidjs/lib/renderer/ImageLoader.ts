// ---------------------------------------------------------------------------
// Image loader — fetches thumbnail URLs and decodes into Image objects
// ---------------------------------------------------------------------------
//
// Shared between Canvas2D and WebGL renderers. Manages an LRU cache of
// decoded images so the renderer can draw immediately without waiting
// for async loads.

export type ImageReadyCallback = (path: string, img: HTMLImageElement) => void;

const MAX_CONCURRENT_LOADS = 12;

export class ImageLoader {
  /** path -> decoded image (LRU by insertion order). */
  private cache = new Map<string, HTMLImageElement>();
  /** path -> in-flight Image element. */
  private loading = new Map<string, HTMLImageElement>();
  /** Paths queued for loading. */
  private queue: { path: string; url: string }[] = [];
  /** Active concurrent loads. */
  private activeLoads = 0;
  /** URLs that failed loading — won't be retried until evicted. */
  private failed = new Set<string>();

  private onReady: ImageReadyCallback;
  private onError: ((path: string) => void) | null;
  private maxCacheSize: number;

  constructor(
    onReady: ImageReadyCallback,
    onError: ((path: string) => void) | null = null,
    maxCacheSize = 600,
  ) {
    this.onReady = onReady;
    this.onError = onError;
    this.maxCacheSize = maxCacheSize;
  }

  /** Get a cached image, or null if not yet loaded. */
  get(path: string): HTMLImageElement | null {
    const img = this.cache.get(path);
    if (img) {
      // Move to end (most recently used)
      this.cache.delete(path);
      this.cache.set(path, img);
      return img;
    }
    return null;
  }

  /** Request an image. If cached, returns immediately. Otherwise queues a load. */
  request(path: string, url: string): HTMLImageElement | null {
    const cached = this.get(path);
    if (cached) return cached;

    if (this.loading.has(path) || this.failed.has(path)) return null;

    this.queue.push({ path, url });
    this.drain();
    return null;
  }

  /** Cancel all pending loads and clear the queue (e.g. on generation change). */
  cancelAll() {
    for (const [, img] of this.loading) {
      img.src = "";
    }
    this.loading.clear();
    this.queue.length = 0;
    this.activeLoads = 0;
    this.failed.clear();
  }

  /** Evict a specific path from cache. */
  evict(path: string) {
    this.cache.delete(path);
    this.failed.delete(path);
    const loading = this.loading.get(path);
    if (loading) {
      loading.src = "";
      this.loading.delete(path);
      this.activeLoads = Math.max(0, this.activeLoads - 1);
    }
  }

  /** Evict paths not in the provided keep set. */
  evictExcept(keepPaths: Set<string>) {
    for (const p of [...this.cache.keys()]) {
      if (!keepPaths.has(p)) this.cache.delete(p);
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
    this.cache.clear();
  }

  // -------------------------------------------------------------------------
  // Internal
  // -------------------------------------------------------------------------

  private drain() {
    while (this.activeLoads < MAX_CONCURRENT_LOADS && this.queue.length > 0) {
      const { path, url } = this.queue.shift()!;
      if (this.cache.has(path) || this.loading.has(path)) continue;
      this.startLoad(path, url);
    }
  }

  private startLoad(path: string, url: string) {
    this.activeLoads++;
    const img = new Image();
    this.loading.set(path, img);

    img.onload = () => {
      this.loading.delete(path);
      this.activeLoads--;
      this.insertCache(path, img);
      this.onReady(path, img);
      this.drain();
    };

    img.onerror = () => {
      this.loading.delete(path);
      this.activeLoads--;
      this.failed.add(path);
      this.onError?.(path);
      this.drain();
    };

    img.crossOrigin = "anonymous";
    img.src = url;
  }

  private insertCache(path: string, img: HTMLImageElement) {
    this.cache.set(path, img);
    // Evict oldest entries if over capacity
    while (this.cache.size > this.maxCacheSize) {
      const oldest = this.cache.keys().next().value;
      if (oldest !== undefined) this.cache.delete(oldest);
    }
  }
}
