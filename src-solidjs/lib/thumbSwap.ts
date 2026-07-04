// ---------------------------------------------------------------------------
// Decode-then-swap for grid cells that already display an image.
//
// Pointing a live <img> at a new URL can blank the cell until the new bytes
// decode. Instead, the new URL is decoded off-DOM first and only swapped into
// the cell's thumb map on success — the previous image stays on screen with no
// skeleton flash. A 404 (cold tier) leaves the old image up and reports a miss
// so the caller can queue generation. Extracted from JustifiedGrid's zoom-swap
// logic; used by both grids for the cheap-rung → target-tier upgrade
// (docs/scrollLoadingRedesign.md, Phase 2).
//
// In-flight decodes are tracked per path so they can be cancelled when a cell
// is evicted or the gallery resets — otherwise each abandoned upgrade keeps an
// Image fetching/decoding in the background with nothing to guard it.
// ---------------------------------------------------------------------------

export interface ThumbSwapper {
  /** Decode `url` off-DOM, then apply it to `path`'s cell on success. */
  swap: (path: string, url: string) => void;
  /** Abort the in-flight swap for `path`, if any (eviction). */
  cancel: (path: string) => void;
  /** Abort all in-flight swaps (path-list change, invalidation, unmount). */
  cancelAll: () => void;
}

export function createThumbSwapper(opts: {
  /** Current generation counter — swaps from a stale generation are dropped. */
  generation: () => number;
  /** Whether `path` is still assigned and `gen` is still current. */
  isCurrent: (path: string, gen: number) => boolean;
  /** Commit the decoded URL (mark it loaded + write the thumb map). */
  apply: (path: string, url: string) => void;
  /** The URL 404'd — queue thumbnail generation. */
  onMiss: (path: string) => void;
}): ThumbSwapper {
  const inFlight = new Map<string, HTMLImageElement>();

  const cancel = (path: string) => {
    const img = inFlight.get(path);
    if (!img) return;
    img.onload = null;
    img.onerror = null;
    img.src = "";
    inFlight.delete(path);
  };

  const swap = (path: string, url: string) => {
    cancel(path);
    const gen = opts.generation();
    const img = new Image();
    inFlight.set(path, img);
    img.onload = () => {
      inFlight.delete(path);
      if (!opts.isCurrent(path, gen)) return;
      opts.apply(path, url);
    };
    img.onerror = () => {
      inFlight.delete(path);
      if (!opts.isCurrent(path, gen)) return;
      opts.onMiss(path);
    };
    img.src = url;
  };

  const cancelAll = () => {
    for (const path of [...inFlight.keys()]) cancel(path);
  };

  return { swap, cancel, cancelAll };
}
