// How long a thumbnail takes to arrive, smoothed.
//
// One number, two modules: `ThumbnailCell` records each completed load and
// `scrollDynamics` sizes its look-ahead buffer as velocity × this latency. It
// lives in its own file because neither of those owns the other's concern —
// it used to sit in `perfMonitor`, which was the debug overlay's module, and
// deleting the overlay would have silently taken the adaptive prefetch with it
// on exactly the connection it exists for.
//
// The EWMA is unconditional: the overlay could be switched off, but the buffer
// it feeds is always in use.

/** α=0.2 — the last ~10 loads dominate, so a change of network re-converges
 *  within a screenful of cells. */
const ALPHA = 0.2;

let ewma = 0;

/** Smoothed per-image load latency (ms); 0 until the first load is measured. */
export function ewmaImageLoadMs(): number {
  return ewma;
}

export function recordImageLoad(durationMs: number) {
  // Seed with the first sample so the estimate doesn't crawl up from 0.
  ewma = ewma === 0 ? durationMs : ewma + ALPHA * (durationMs - ewma);
}
