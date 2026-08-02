// Running tally behind the "Generating N / M" indicator.
//
// Both grids keep the same two counters and update them at the same four
// moments (a cell 404s, stale queue entries are dropped, a batch lands, the
// whole streaming state resets). Spelled out inline, the "are we finished?"
// check in particular was repeated four times per grid — and it has to fire
// from every one of them, because any of them can be the one that empties the
// queue. Missing it leaves the indicator stuck on screen forever.

import {
  thumbGenStarted,
  thumbGenProgress,
  thumbGenFinished,
} from "../stores/thumbnailProgressStore";

export interface ThumbProgress {
  /** A cell missed and was queued for generation. */
  queued: () => void;
  /** `n` queued paths were dropped without generating (scrolled out of range). */
  dropped: (n: number) => void;
  /** `n` paths finished generating. */
  completed: (n: number) => void;
  /** Forget everything (generation bump, invalidation, tier change). */
  reset: () => void;
}

/**
 * Track thumbnail-generation progress for one grid.
 *
 * `isIdle` reports whether the grid has nothing left queued or in flight; it's
 * consulted after every count change to decide whether to close out the
 * indicator, since the caller's queues are what actually define "done".
 */
export function createThumbProgress(isIdle: () => boolean): ThumbProgress {
  let total = 0;
  let done = 0;

  /** Close out the indicator if the grid has gone quiet, else report progress. */
  const settle = () => {
    if (isIdle()) {
      thumbGenFinished(done);
      total = 0;
      done = 0;
    } else {
      thumbGenProgress(done, total);
    }
  };

  return {
    queued() {
      total++;
      if (total === 1) thumbGenStarted(total);
      else thumbGenProgress(done, total);
    },
    dropped(n) {
      if (n <= 0) return;
      // Never below `done`: those are already-generated thumbnails, and a total
      // under the completed count would render as "Generating 5 / 3".
      total = Math.max(done, total - n);
      // Dropping may have emptied the queue, and the dropped items were counted
      // when they were queued — so the indicator is showing and needs closing.
      if (isIdle()) {
        thumbGenFinished(done);
        total = 0;
        done = 0;
      }
    },
    completed(n) {
      done += n;
      settle();
    },
    reset() {
      total = 0;
      done = 0;
    },
  };
}
