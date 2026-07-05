// ---------------------------------------------------------------------------
// Performance monitor — zero-cost when inactive
// ---------------------------------------------------------------------------
//
// All metric collection is gated behind `active`. When the debug overlay
// mounts it calls `startMonitoring()`, which flips the flag and begins
// rAF/interval loops. `stopMonitoring()` tears everything down so there
// is exactly zero overhead during normal use — with one exception: the
// image-load-latency EWMA is always on (a multiply-add per load), because
// the grids' adaptive prefetch depth consumes it outside the overlay.

const HISTORY_LEN = 120; // ~2 minutes at 1 sample/sec

// -------------------------------------------------------------------------
// Ring buffer
// -------------------------------------------------------------------------

export class RingBuffer {
  readonly data: Float64Array;
  private head = 0;
  private _count = 0;

  constructor(public readonly capacity: number) {
    this.data = new Float64Array(capacity);
  }

  push(value: number) {
    this.data[this.head] = value;
    this.head = (this.head + 1) % this.capacity;
    if (this._count < this.capacity) this._count++;
  }

  /** Return samples oldest→newest. */
  toArray(): number[] {
    const out: number[] = new Array(this._count);
    const start = this._count < this.capacity ? 0 : this.head;
    for (let i = 0; i < this._count; i++) {
      out[i] = this.data[(start + i) % this.capacity];
    }
    return out;
  }

  get count() {
    return this._count;
  }

  get last() {
    if (this._count === 0) return 0;
    return this.data[(this.head - 1 + this.capacity) % this.capacity];
  }

  clear() {
    this.head = 0;
    this._count = 0;
  }
}

// -------------------------------------------------------------------------
// IPC interception — patched into ipc.ts via `wrapInvoke`
// -------------------------------------------------------------------------

/** Cumulative IPC bytes sent (estimated from JSON serialization). */
let ipcBytesSent = 0;
/** Cumulative IPC bytes received (estimated). */
let ipcBytesReceived = 0;
/** Total IPC calls since monitoring started. */
let ipcCallCount = 0;
/** Cumulative IPC latency (ms) for averaging. */
let ipcLatencySum = 0;

// Snapshot values from last sample for delta calculation.
let prevIpcBytesSent = 0;
let prevIpcBytesReceived = 0;
let prevIpcCallCount = 0;

let _active = false;

export function isActive() {
  return _active;
}

/**
 * Called by the IPC layer on every invoke when monitoring is active.
 * Intentionally synchronous and allocation-free on the hot path.
 */
export function recordIpcCall(
  commandName: string,
  argBytes: number,
  responseBytes: number,
  durationMs: number,
) {
  if (!_active) return;
  ipcBytesSent += argBytes;
  ipcBytesReceived += responseBytes;
  ipcCallCount++;
  ipcLatencySum += durationMs;
}

// -------------------------------------------------------------------------
// Cache miss tracking (called from GalleryGrid thumb error handler)
// -------------------------------------------------------------------------

let cacheMissCount = 0;
let prevCacheMissCount = 0;

export function recordCacheMiss() {
  if (!_active) return;
  cacheMissCount++;
}

// -------------------------------------------------------------------------
// Image load latency — wall-clock from <img> src assignment to onload, for
// freshly-seen URLs only (cached re-displays are skipped). Captures the full
// fetch + decode cost, so it's directly comparable native (WebKitGTK custom
// protocol) vs web (browser + HTTP). Averaged per sample tick.
// -------------------------------------------------------------------------

let imageLoadSum = 0;
let imageLoadCount = 0;

// Always-on EWMA of image load latency, independent of the overlay's `_active`
// gate: the grids size their look-ahead buffer by velocity × this latency
// (docs/scrollLoadingRedesign.md Phase 5), which must work during normal use.
// One multiply-add per fresh load; α=0.2 ≈ the last ~10 loads dominate, so a
// network change re-converges within a screenful of cells.
const IMAGE_LOAD_EWMA_ALPHA = 0.2;
let imageLoadEwma = 0;

/** Smoothed per-image load latency (ms); 0 until the first load is measured. */
export function ewmaImageLoadMs(): number {
  return imageLoadEwma;
}

export function recordImageLoad(durationMs: number) {
  // Seed with the first sample so the estimate doesn't crawl up from 0.
  imageLoadEwma =
    imageLoadEwma === 0
      ? durationMs
      : imageLoadEwma + IMAGE_LOAD_EWMA_ALPHA * (durationMs - imageLoadEwma);
  if (!_active) return;
  imageLoadSum += durationMs;
  imageLoadCount++;
}

// -------------------------------------------------------------------------
// Image cache tracking (sampled each tick via getter)
// -------------------------------------------------------------------------

let _imageCounts: (() => { visible: number; cached: number }) | null = null;
let _viewerCacheCount: (() => number) | null = null;

export function setImageCountSource(fn: () => { visible: number; cached: number }) {
  _imageCounts = fn;
}

export function setViewerCacheCountSource(fn: (() => number) | null) {
  _viewerCacheCount = fn;
}

// -------------------------------------------------------------------------
// Metric histories
// -------------------------------------------------------------------------

export const fps = new RingBuffer(HISTORY_LEN);
export const ipcBandwidthIn = new RingBuffer(HISTORY_LEN);   // KB/s received
export const ipcBandwidthOut = new RingBuffer(HISTORY_LEN);   // KB/s sent
export const ipcCallRate = new RingBuffer(HISTORY_LEN);       // calls/sec
export const ipcAvgLatency = new RingBuffer(HISTORY_LEN);     // ms
export const cacheMisses = new RingBuffer(HISTORY_LEN);       // misses/sec
export const domNodes = new RingBuffer(HISTORY_LEN);          // count
export const diskReadRate = new RingBuffer(HISTORY_LEN);      // KB/s
export const diskWriteRate = new RingBuffer(HISTORY_LEN);     // KB/s
export const visibleImages = new RingBuffer(HISTORY_LEN);     // count
export const cachedImages = new RingBuffer(HISTORY_LEN);      // count
export const jsHeapUsed = new RingBuffer(HISTORY_LEN);        // MB
export const imageLoadLatency = new RingBuffer(HISTORY_LEN);  // ms (avg/sample)
export const imageLoadRate = new RingBuffer(HISTORY_LEN);     // loads/sec

// -------------------------------------------------------------------------
// FPS measurement (rAF-based, only runs when active)
// -------------------------------------------------------------------------

let fpsRafId = 0;
let frameCount = 0;
let fpsLastSample = 0;

function fpsLoop(now: number) {
  if (!_active) return;
  frameCount++;
  if (now - fpsLastSample >= 1000) {
    fps.push(frameCount);
    frameCount = 0;
    fpsLastSample = now;
  }
  fpsRafId = requestAnimationFrame(fpsLoop);
}

// -------------------------------------------------------------------------
// 1-second sample interval
// -------------------------------------------------------------------------

let sampleIntervalId: ReturnType<typeof setInterval> | null = null;
let prevDiskRead = 0;
let prevDiskWrite = 0;
let prevSampleTime = 0;

type PerfSnapshot = {
  disk_read_bytes: number;
  disk_write_bytes: number;
  cached_thumbnails: number;
  cache_size_bytes: number;
  thumb_pool_active_threads: number;
};

let fetchSnapshot: (() => Promise<PerfSnapshot>) | null = null;

export function setPerfSnapshotFetcher(fn: () => Promise<PerfSnapshot>) {
  fetchSnapshot = fn;
}

async function sampleTick() {
  if (!_active) return;
  const now = performance.now();
  const dtSec = prevSampleTime > 0 ? (now - prevSampleTime) / 1000 : 1;
  prevSampleTime = now;

  // IPC deltas
  const sentDelta = ipcBytesSent - prevIpcBytesSent;
  const recvDelta = ipcBytesReceived - prevIpcBytesReceived;
  const callDelta = ipcCallCount - prevIpcCallCount;
  prevIpcBytesSent = ipcBytesSent;
  prevIpcBytesReceived = ipcBytesReceived;
  prevIpcCallCount = ipcCallCount;

  ipcBandwidthOut.push(sentDelta / 1024 / dtSec);
  ipcBandwidthIn.push(recvDelta / 1024 / dtSec);
  ipcCallRate.push(callDelta / dtSec);
  ipcAvgLatency.push(callDelta > 0 ? ipcLatencySum / callDelta : 0);
  ipcLatencySum = 0;

  // Cache misses delta
  const missDelta = cacheMissCount - prevCacheMissCount;
  prevCacheMissCount = cacheMissCount;
  cacheMisses.push(missDelta / dtSec);

  // Image load latency (avg ms over this tick) + load rate
  imageLoadLatency.push(imageLoadCount > 0 ? imageLoadSum / imageLoadCount : 0);
  imageLoadRate.push(imageLoadCount / dtSec);
  imageLoadSum = 0;
  imageLoadCount = 0;

  // DOM nodes
  domNodes.push(document.querySelectorAll("*").length);

  // JS heap (Chrome/WebKitGTK with --enable-features)
  const perf = performance as any;
  if (perf.memory) {
    jsHeapUsed.push(perf.memory.usedJSHeapSize / (1024 * 1024));
  }

  // Image cache counts (pulled fresh each tick)
  if (_imageCounts) {
    const counts = _imageCounts();
    visibleImages.push(counts.visible);
    cachedImages.push(counts.cached + (_viewerCacheCount?.() ?? 0));
  }

  // Backend snapshot (disk IO)
  if (fetchSnapshot) {
    try {
      const snap = await fetchSnapshot();
      const readDelta = snap.disk_read_bytes - prevDiskRead;
      const writeDelta = snap.disk_write_bytes - prevDiskWrite;
      // Only push delta rates after first sample (prevDiskRead != 0)
      if (prevDiskRead > 0) {
        diskReadRate.push(readDelta / 1024 / dtSec);
        diskWriteRate.push(writeDelta / 1024 / dtSec);
      }
      prevDiskRead = snap.disk_read_bytes;
      prevDiskWrite = snap.disk_write_bytes;
    } catch {
      // Backend not ready — skip this tick
    }
  }
}

// -------------------------------------------------------------------------
// Start / stop
// -------------------------------------------------------------------------

export function startMonitoring() {
  if (_active) return;
  _active = true;

  // Reset cumulative counters so deltas start fresh
  ipcBytesSent = 0;
  ipcBytesReceived = 0;
  ipcCallCount = 0;
  ipcLatencySum = 0;
  prevIpcBytesSent = 0;
  prevIpcBytesReceived = 0;
  prevIpcCallCount = 0;
  cacheMissCount = 0;
  prevCacheMissCount = 0;
  imageLoadSum = 0;
  imageLoadCount = 0;
  prevDiskRead = 0;
  prevDiskWrite = 0;
  prevSampleTime = 0;

  // Start FPS loop
  frameCount = 0;
  fpsLastSample = performance.now();
  fpsRafId = requestAnimationFrame(fpsLoop);

  // Start 1-second sample interval
  sampleIntervalId = setInterval(sampleTick, 1000);
  // Take an immediate first sample
  sampleTick();
}

export function stopMonitoring() {
  if (!_active) return;
  _active = false;

  if (fpsRafId) {
    cancelAnimationFrame(fpsRafId);
    fpsRafId = 0;
  }
  if (sampleIntervalId !== null) {
    clearInterval(sampleIntervalId);
    sampleIntervalId = null;
  }

  // Clear histories so stale data doesn't show on next open
  fps.clear();
  ipcBandwidthIn.clear();
  ipcBandwidthOut.clear();
  ipcCallRate.clear();
  ipcAvgLatency.clear();
  cacheMisses.clear();
  domNodes.clear();
  diskReadRate.clear();
  diskWriteRate.clear();
  visibleImages.clear();
  cachedImages.clear();
  jsHeapUsed.clear();
  imageLoadLatency.clear();
  imageLoadRate.clear();
}
