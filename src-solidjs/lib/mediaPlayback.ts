// Remembers grid autoplay playback positions across cell recycling and scroll,
// so a video that scrolls out of view (and is torn down) resumes near where it
// left off when it comes back, instead of restarting from frame zero.
//
// Keyed by media path. Bounded so a long browsing session can't grow it without
// limit; least-recently-touched entries are dropped first.

const MAX_ENTRIES = 256;
const positions = new Map<string, number>();

function touch(path: string, time: number) {
  // Re-insert to move to the end (most-recently-used) for LRU eviction.
  positions.delete(path);
  positions.set(path, time);
  if (positions.size > MAX_ENTRIES) {
    const oldest = positions.keys().next().value;
    if (oldest !== undefined) positions.delete(oldest);
  }
}

/** Record the current playback time for `path` (call from `ontimeupdate`). */
export function saveVideoPosition(el: HTMLVideoElement, path: string): void {
  const t = el.currentTime;
  if (Number.isFinite(t) && t > 0) touch(path, t);
}

/** Restore the saved playback time for `path` onto a freshly-mounted element. */
export function restoreVideoPosition(el: HTMLVideoElement, path: string): void {
  const saved = positions.get(path);
  if (saved == null || saved <= 0) return;
  const seek = () => {
    // Clamp to just inside the duration so we don't land past the end.
    const d = el.duration;
    const target = Number.isFinite(d) && d > 0 ? Math.min(saved, d - 0.05) : saved;
    try { el.currentTime = target; } catch { /* not seekable yet */ }
  };
  if (el.readyState >= 1 /* HAVE_METADATA */) {
    seek();
  } else {
    el.addEventListener("loadedmetadata", seek, { once: true });
  }
}
