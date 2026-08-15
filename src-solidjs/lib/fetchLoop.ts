// ---------------------------------------------------------------------------
// The grids' thumbnail fetch loop: two single-flight slots and the order in
// which one pass through them runs.
//
// Every view that streams thumbnails wants the same schedule — evict what has
// scrolled away, serve the cells the user is looking at, and only then
// speculate. What differs between views is *what* each step does, so those
// arrive as callbacks.
//
// The two slots are not one slot, and that is load-bearing. `inFlightFetch`
// carries the drain of cells that are on screen right now and showing nothing;
// `inFlightWarm` carries speculation. Sharing a slot meant a speculative batch
// blocked the visible drain for its whole duration, and a pass bails on its
// first line when the slot is busy — so one 64-image precache of large photos
// stalled *every* subsequent generation request for as long as it ran.
// Measured in GalleryGrid on a 600-photo 12 MP gallery: a single background
// batch issued a second after load held the slot for the entire session, and
// cells the user scrolled to took 30 s to appear, left to the /thumb route's
// one-at-a-time generate-on-miss behind that same batch. In JustifiedGrid,
// where a speculative batch at mid/high detail is 1280/2560px decodes,
// scrolling to a fresh row and waiting ten seconds for five images was the
// *expected* behaviour whenever a warm happened to be in flight.
//
// Speculation is deliberately not re-armed on completion (see `poke`), because
// it is not free: landing-zone warms, look-ahead precache and background
// crawls all land on the same bounded rayon pool as the cells on screen.
// ---------------------------------------------------------------------------

import { onCleanup } from "solid-js";

/** How often the loop wakes on its own. Covers the states no event reports:
 *  an idle viewport with a background crawl to advance, and a warm that
 *  finished with nothing else to trigger the next one. */
const POLL_MS = 500;

export interface FetchLoop {
  /** Run one pass now. */
  schedule(): void;
  /**
   * Ask for a pass, coalescing every request made before the task queue
   * drains into one. This is what a 404 calls: a landing reveals dozens of
   * cold cells within a few milliseconds of each other, and each of those is
   * its own task, so a microtask would not merge them — the first request arms
   * a zero-delay timer and the rest ride it.
   */
  poke(): void;
  /** Run `work` in the visible-drain slot. */
  fetch(work: () => Promise<void>): void;
  /** Run `work` in the speculation slot. */
  warm(work: () => Promise<void>): void;
  fetching(): boolean;
  warming(): boolean;
  /** True once the view is being torn down — long-running work should bail
   *  rather than write into a store that is about to be disposed. */
  aborted(): boolean;
}

export function createFetchLoop(opts: {
  /** Drop state for cells far outside the rendered range. Runs on every pass,
   *  before the early returns below, because those return often enough that
   *  running it last would starve it for as long as there is work queued. */
  evict: () => void;
  /** The view is being scrubbed past faster than anything could load. Eviction
   *  is the only useful work; even speculation is wrong, because the projected
   *  landing zone has moved on by the time a batch is issued. */
  warping: () => boolean;
  /** A fling is in progress: near-viewport generation is wasted (those cells
   *  fly past unseen), so the projected landing zone is warmed instead. */
  flinging: () => boolean;
  /**
   * Issue one batch for cells that are on screen and showing nothing, via
   * `fetch`. Returns true if a batch went out.
   *
   * This runs less often than the name suggests. Both `lightview://thumb` and
   * `GET /thumb` go through `thumb_serve::get_or_generate`, which generates a
   * missing thumbnail inside the request and only 404s once generation has
   * actually failed — so the grids' 404-fed queue is a *recovery* path, not
   * how a cold gallery fills. Measured over a cold 1200-image gallery in
   * Chromium: 880 thumbnail responses, none of them a 404.
   */
  drain: () => boolean;
  /** Warm the projected landing zone of the current fling, via `warm`. */
  warmLanding: () => void;
  /** Warm whatever this view speculates on when at rest, via `warm`. */
  speculate: () => void;
  /** Optional extra brake on speculation: cells already pointed at a
   *  full-resolution source that have not painted yet. JustifiedGrid gates on
   *  it because one such cell is a whole decode of the backend's bounded pool. */
  blocked?: () => boolean;
}): FetchLoop {
  let inFlightFetch: Promise<void> | null = null;
  let inFlightWarm: Promise<void> | null = null;
  let pokeTimer: ReturnType<typeof setTimeout> | undefined;
  let aborted = false;

  const schedule = () => {
    if (pokeTimer !== undefined) {
      clearTimeout(pokeTimer);
      pokeTimer = undefined;
    }
    if (aborted) return;

    opts.evict();
    if (opts.warping()) return;

    const flinging = opts.flinging();
    if (!flinging && !inFlightFetch && opts.drain()) return;

    // Speculation waits for the cells the user is actually looking at.
    if (inFlightFetch || inFlightWarm || opts.blocked?.()) return;
    if (flinging) {
      opts.warmLanding();
      return;
    }
    opts.speculate();
  };

  const poke = () => {
    if (pokeTimer !== undefined || aborted) return;
    pokeTimer = setTimeout(() => {
      pokeTimer = undefined;
      schedule();
    }, 0);
  };

  // Both slots clear in a `.finally()` on the *outer* promise, not in a
  // `try/finally` inside the async body. That placement matters twice over: a
  // batch that bails early on a stale generation would otherwise wedge the
  // slot for the rest of the session, and a `work` that threw synchronously
  // would run an inner `finally` *before* the assignment below and wedge it
  // just as permanently. `(async () => …)()` never throws synchronously, so
  // the promise always exists before anything can settle it.
  //
  // The identity check is what makes a slot safe to clear: only the promise
  // that currently owns the slot releases it, so an overlapping call cannot
  // hand the slot away from work that is still running.

  const fetch = (work: () => Promise<void>) => {
    const p: Promise<void> = (async () => work())()
      .catch((e) => {
        console.error("Thumbnail drain failed:", e);
      })
      .finally(() => {
        if (inFlightFetch !== p) return;
        inFlightFetch = null;
        // Re-arm. Without this the only thing that starts the next batch is
        // the poll, so a deep queue against a still viewport ran batch, then
        // up to POLL_MS of idle backend, then the next batch, however fast the
        // backend actually was. See the note on `drain` for how much of a
        // gallery ever reaches this queue.
        poke();
      });
    inFlightFetch = p;
  };

  const warm = (work: () => Promise<void>) => {
    const p: Promise<void> = (async () => work())()
      .catch((e) => {
        console.error("Thumbnail warm failed:", e);
      })
      .finally(() => {
        // Deliberately no `poke()`: re-arming speculation would turn the
        // background crawl into a continuous one on the same bounded pool the
        // visible cells use.
        if (inFlightWarm === p) inFlightWarm = null;
      });
    inFlightWarm = p;
  };

  const pollId = setInterval(schedule, POLL_MS);
  onCleanup(() => {
    clearInterval(pollId);
    if (pokeTimer !== undefined) clearTimeout(pokeTimer);
    aborted = true;
  });

  return {
    schedule,
    poke,
    fetch,
    warm,
    fetching: () => inFlightFetch !== null,
    warming: () => inFlightWarm !== null,
    aborted: () => aborted,
  };
}
