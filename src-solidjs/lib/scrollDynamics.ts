// Scroll velocity, direction, and the decode gate — shared by both grids.

import { createSignal, untrack, type Accessor } from "solid-js";
import { isTauri } from "./runtime";
import { ewmaImageLoadMs } from "./perfMonitor";

// ---------------------------------------------------------------------------
// Shared scroll dynamics for the virtualized grid views.
//
// Owns the window scroll listener (rAF-coalesced), velocity/direction
// tracking, and the WebKitGTK decode gate. Extracted from GalleryGrid /
// JustifiedGrid so both share one implementation and one set of tuning
// constants (same pattern as galleryControls.ts).
//
// The decode gate exists for exactly one reason: WebKitGTK decodes images on
// the webview's main thread, so assigning a wall of new <img> srcs mid-scroll
// buries the thread and scrolling chokes. Real browsers (the web client)
// decode async off-thread, so the gate never engages there — new cells load
// while the scroll is still moving, which is what keeps the virtual-scroll
// buffer useful on touch flings (docs/decisions/0007-two-zone-render-window.md).
// ---------------------------------------------------------------------------

// Decode-work rate above which the gate engages, in decoded pixels per second
// (velocity/rowHeight × cellsPerRow × per-cell decoded pixels). Pixel-weighted
// rather than cells/s so a dense wall of tiny 128px thumbs isn't gated as
// aggressively as a few huge 1024px ones. Calibrated to the old 2500 px/s
// threshold at default desktop settings (~250px cells, ~7 columns, 512px
// tier): 2500/258 × 7 × 512² ≈ 18M px/s.
const GATE_DECODE_PX_PER_SEC = 18_000_000;
// Idle gap (ms) after the last gated frame before scrolling is considered
// settled and the gate releases (in case `scrollend` doesn't fire).
const SCROLL_SETTLE_MS = 120;
// A scroll frame older than this means scrolling has stopped — velocity()
// reads 0 rather than the stale last-frame value.
const VELOCITY_STALE_MS = 150;
// Scroll speed (viewport-heights per second) above which newly revealed cells
// are treated as pass-through: the grids give them the cheap rung (tiny tier)
// and upgrade to the target tier once scrolling settles. Wheel scrolling sits
// well under this; touch flings and scrollbar drags exceed it.
const FLING_VIEWPORTS_PER_SEC = 1.5;
// Quiet gap (ms) below the fling threshold before the scroll counts as
// settled — debounces the cheap-rung → target-tier upgrade pass.
const SETTLE_DEBOUNCE_MS = 150;
// A single scroll frame moving more than this many viewports is a jump — the
// user warped (scrollbar tap/drag, scroll-to-index) rather than scrolled.
const JUMP_VIEWPORTS = 2;
// How long the view must then sit still before it counts as settled.
//
// A jump is instantaneous: one huge-delta frame and `scrollend` right behind
// it, with no velocity in between. So the fling debounce — which is what keeps
// cells on the cheap rung while the user is still moving — never engages, and
// every jump immediately committed a full-resolution upgrade of the whole
// inner window. Someone tapping around the scrollbar paid a screenful of
// target-tier decodes per tap, including for positions they left 150 ms later.
// Those bitmaps stay in the browser's image cache long after their cells are
// evicted, so on a phone a few dozen jumps exhaust the content process and the
// tab is killed ("A problem repeatedly occurred").
//
// Sized to a human's repeat-tap cadence: land once and you get full resolution
// a beat later; keep jumping and the intermediate stops never upgrade at all.
const JUMP_DWELL_MS = 450;
// How long after a scrollbar release the view still counts as unsettled.
//
// JUMP_DWELL_MS above can only react to a warp *after* it has happened, which
// is the best we can do for scroll-to-index. A scrollbar gesture we know about
// while it is still running, and it needs a longer tail: a scrollbar is worked
// in bursts — press, look, adjust, press again — and each landing reveals a
// screenful of cells the grid has never shown. Upgrading every one of those
// stops to the target tier is what makes a phone run out of memory, because
// the browser keeps the decoded bitmaps in its own image cache long after the
// cells are evicted and nothing the page does to the <img> gives that memory
// back. One upgrade at the end of a burst instead of one per stop is the
// difference between tens of megabytes and hundreds.
const SCROLLBAR_RELEASE_MS = 900;
// How far ahead (seconds) a fling's momentum is projected when estimating
// where it will land. iOS-style deceleration coasts roughly velocity × 0.5s
// past the current position; projecting a bit short of that keeps the warm
// window overlapping the real landing spot even when the user drags the
// fling shorter. Recomputed every drain, so an interrupted or redirected
// fling self-corrects.
const FLING_PROJECTION_S = 0.35;

// Cheap-rung experiment on WebKitGTK: when true, gated frames assign the tiny
// "s" tier instead of nothing (128px decodes are ~16× cheaper than 512px).
// Off until measured with the debug overlay — WebKitGTK decodes on the main
// thread, which is the reason the hard gate exists at all.
export const CHEAP_RUNG_DURING_GATE = false;

/**
 * True when the browser reports a constrained network: Save-Data enabled or a
 * 2g-class effective connection. Progressive — Safari and WebKitGTK lack
 * `navigator.connection`, so this is false there and nothing changes. While
 * constrained, the grids hold cells at the cheap rung (skip tier upgrades)
 * and `bufferAheadRows` stops deepening the prefetch window, both of which
 * would spend the user's data budget on speculation.
 */
export function constrainedNetwork(): boolean {
  const conn = (navigator as {
    connection?: { saveData?: boolean; effectiveType?: string };
  }).connection;
  if (!conn) return false;
  return (
    conn.saveData === true ||
    conn.effectiveType === "2g" ||
    conn.effectiveType === "slow-2g"
  );
}

// ---------------------------------------------------------------------------
// Scrollbar gesture state
//
// Module-level rather than per-instance: there is one scrollbar and one grid on
// screen at a time, and the grid must not have to know the scrollbar exists.
// ---------------------------------------------------------------------------

const [scrollBarActive, setScrollBarActive] = createSignal(false);
let scrollBarReleaseTimer: ReturnType<typeof setTimeout> | undefined;

/** Called by `ScrollBar` on every press and move of a scrollbar gesture. */
export function markScrollBarActive() {
  if (scrollBarReleaseTimer) clearTimeout(scrollBarReleaseTimer);
  scrollBarReleaseTimer = undefined;
  if (!untrack(scrollBarActive)) setScrollBarActive(true);
}

/** Called by `ScrollBar` when the pointer lifts; starts the release tail. */
export function markScrollBarReleased() {
  if (scrollBarReleaseTimer) clearTimeout(scrollBarReleaseTimer);
  scrollBarReleaseTimer = setTimeout(() => {
    scrollBarReleaseTimer = undefined;
    setScrollBarActive(false);
  }, SCROLLBAR_RELEASE_MS);
}

export interface ScrollFrame {
  /** Current window.scrollY. */
  y: number;
  /** Absolute scroll delta (px) since the previous frame. */
  dy: number;
  /** Scroll speed (px/s). */
  velocity: number;
  /** 1 = down, -1 = up. */
  direction: 1 | -1;
  /**
   * This frame warped the view rather than scrolling it (scrollbar tap/drag,
   * scroll-to-index). The grids drop queued work for the old position on one
   * of these; [`JUMP_DWELL_MS`] also holds the resolution ladder back until
   * the view stops moving.
   */
  isJump: boolean;
}

export interface ScrollDynamics {
  /** Scroll speed (px/s); 0 once no scroll frame has arrived recently. */
  velocity: () => number;
  /** Last scroll direction: 1 = down, -1 = up. */
  direction: () => 1 | -1;
  /**
   * Reactive: true while src assignment should be deferred to protect the
   * webview's main-thread decoding. Always false outside WebKitGTK.
   */
  decodeGate: Accessor<boolean>;
  /**
   * Reactive: false while a fling is in progress (velocity above the
   * viewport-relative threshold), flipping back true shortly after it calms.
   * Also false for the whole of a scrollbar gesture and its release tail.
   * Drives cheap-rung assignment and the upgrade pass on all platforms.
   */
  settled: Accessor<boolean>;
  /**
   * Estimated scroll position (px) where the current fling will land:
   * scrollY + signed velocity × FLING_PROJECTION_S, clamped to the document's
   * scroll range. Equals the current scrollY when not scrolling. Drives
   * landing-zone thumbnail warming during flings.
   */
  projectedLandingY: () => number;
  /**
   * Rows to render ahead of the scroll direction: `base` plus however many
   * rows the current velocity covers in one measured image-load round-trip
   * (velocity × ewmaImageLoadMs), clamped to `max` so DOM growth stays
   * bounded. A slow network or big tiers ⇒ deeper prefetch; instant
   * localhost or idle ⇒ exactly `base` (today's static behavior). Held at
   * `base` on a constrained network, where speculation isn't worth the data,
   * and during a scrollbar gesture, where the direction it extrapolates from
   * is meaningless.
   */
  bufferAheadRows: (base: number, max: number) => number;
  /** Release the gate / mark settled now (scrollend, wheel animation done). */
  markSettled: () => void;
  dispose: () => void;
}

export function createScrollDynamics(opts: {
  /** Current row pitch (px) — converts px/s into rows/s. */
  rowHeight: () => number;
  /** Cells revealed per row scrolled (an estimate is fine). */
  cellsPerRow: () => number;
  /** Approximate decoded pixels per newly revealed cell (tier size²). */
  cellCostPx: () => number;
  /** Called once per animation frame while scrolling, after state updates. */
  onFrame: (frame: ScrollFrame) => void;
}): ScrollDynamics {
  const [decodeGate, setDecodeGate] = createSignal(false);
  const [rested, setRested] = createSignal(true);
  // A scrollbar gesture keeps the view unsettled for its whole duration, so a
  // burst of scrollbar stops costs one tier upgrade at the end rather than one
  // per stop (see SCROLLBAR_RELEASE_MS). Everything downstream — the cheap-rung
  // assignment in both grids and the drain-on-settle effect — reads this.
  const settled = () => rested() && !scrollBarActive();
  const setSettled = setRested;

  let lastY = window.scrollY;
  let lastTs = performance.now();
  let vel = 0;
  let dir: 1 | -1 = 1;
  let rafId = 0;
  let settleTimer: ReturnType<typeof setTimeout> | undefined;
  let settleDebounce: ReturnType<typeof setTimeout> | undefined;
  /** When the last positional jump landed; 0 = none this session. */
  let lastJumpTs = 0;

  const velocity = () =>
    performance.now() - lastTs > VELOCITY_STALE_MS ? 0 : vel;

  const projectedLandingY = () => {
    const maxScroll = Math.max(
      0,
      document.documentElement.scrollHeight - window.innerHeight,
    );
    const projected = window.scrollY + dir * velocity() * FLING_PROJECTION_S;
    return Math.max(0, Math.min(maxScroll, projected));
  };

  const bufferAheadRows = (base: number, max: number) => {
    // The adaptive part of this buffer is a bet that the scroll will keep going
    // the way it is going, which is how a fling behaves and how a scrollbar
    // does not: a warp reports an enormous velocity for a single frame and then
    // stops dead, so the bet inflates the window to `max` rows of look-ahead
    // around a position the user is about to leave. Every one of those rows is
    // a thumbnail the browser then holds on to. Stay at `base` for the whole
    // gesture, the same concession `constrainedNetwork` makes.
    if (scrollBarActive() || constrainedNetwork()) return base;
    const rh = opts.rowHeight();
    if (rh <= 0) return base;
    const extra = Math.ceil((velocity() * ewmaImageLoadMs()) / 1000 / rh);
    return Math.max(base, Math.min(max, base + extra));
  };

  const markSettled = () => {
    // Scrolling has stopped — collapse the tracked velocity to 0 immediately.
    // Without this, `velocity()` keeps reporting the last fling speed for up to
    // VELOCITY_STALE_MS after the final scroll frame. `scrollend` (which calls
    // this) routinely fires inside that window, so the grids' scheduleFetch
    // would take the `velocity() > VELOCITY_FAST` fling branch — warming a
    // *projected* landing zone instead of generating thumbnails for the viewport
    // the fling actually landed on. That left hard flicks with blank/late cells
    // until the next 500ms poll or a manual scroll (docs/decisions/0007-two-zone-render-window.md).
    vel = 0;
    if (untrack(decodeGate)) setDecodeGate(false);
    if (settleDebounce) clearTimeout(settleDebounce);

    // A jump lands with `scrollend` immediately behind it, so settling here
    // would upgrade the whole inner window to the target tier before the user
    // has stopped navigating — see JUMP_DWELL_MS. Hold for the rest of the
    // dwell instead; another jump inside it re-arms, so a run of taps never
    // pays for the positions it passes through.
    const remaining = JUMP_DWELL_MS - (performance.now() - lastJumpTs);
    if (remaining > 0) {
      if (untrack(settled)) setSettled(false);
      settleDebounce = setTimeout(() => setSettled(true), remaining);
      return;
    }
    if (!untrack(settled)) setSettled(true);
  };

  const onScroll = () => {
    if (rafId) return;
    rafId = requestAnimationFrame((now) => {
      const y = window.scrollY;
      const dt = (now - lastTs) / 1000;
      const dy = Math.abs(y - lastY);
      vel = dt > 0 ? dy / dt : 0;
      dir = y >= lastY ? 1 : -1;
      lastY = y;
      lastTs = now;

      if (isTauri()) {
        const rh = opts.rowHeight();
        const decodePxPerSec =
          rh > 0 ? (vel / rh) * opts.cellsPerRow() * opts.cellCostPx() : 0;
        if (decodePxPerSec > GATE_DECODE_PX_PER_SEC) {
          if (!untrack(decodeGate)) setDecodeGate(true);
          if (settleTimer) clearTimeout(settleTimer);
          settleTimer = setTimeout(markSettled, SCROLL_SETTLE_MS);
        }
      }

      // A warp (scrollbar tap/drag, scroll-to-index) rather than a scroll.
      const isJump = dy > window.innerHeight * JUMP_VIEWPORTS;
      if (isJump) lastJumpTs = now;

      if (isJump || vel > window.innerHeight * FLING_VIEWPORTS_PER_SEC) {
        if (untrack(settled)) setSettled(false);
        if (settleDebounce) clearTimeout(settleDebounce);
        settleDebounce = setTimeout(
          () => setSettled(true),
          isJump ? JUMP_DWELL_MS : SETTLE_DEBOUNCE_MS,
        );
      }

      opts.onFrame({ y, dy, velocity: vel, direction: dir, isJump });
      rafId = 0;
    });
  };
  window.addEventListener("scroll", onScroll, { passive: true });

  return {
    velocity,
    direction: () => dir,
    projectedLandingY,
    bufferAheadRows,
    decodeGate,
    settled,
    markSettled,
    dispose: () => {
      window.removeEventListener("scroll", onScroll);
      if (rafId) cancelAnimationFrame(rafId);
      if (settleTimer) clearTimeout(settleTimer);
      if (settleDebounce) clearTimeout(settleDebounce);
    },
  };
}
