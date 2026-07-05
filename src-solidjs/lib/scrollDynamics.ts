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
// buffer useful on touch flings (docs/scrollLoadingRedesign.md).
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

export interface ScrollFrame {
  /** Current window.scrollY. */
  y: number;
  /** Absolute scroll delta (px) since the previous frame. */
  dy: number;
  /** Scroll speed (px/s). */
  velocity: number;
  /** 1 = down, -1 = up. */
  direction: 1 | -1;
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
   * localhost or idle ⇒ exactly `base` (today's static behavior). Also
   * `base` on a constrained network, where speculation isn't worth the data.
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
  const [settled, setSettled] = createSignal(true);

  let lastY = window.scrollY;
  let lastTs = performance.now();
  let vel = 0;
  let dir: 1 | -1 = 1;
  let rafId = 0;
  let settleTimer: ReturnType<typeof setTimeout> | undefined;
  let settleDebounce: ReturnType<typeof setTimeout> | undefined;

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
    if (constrainedNetwork()) return base;
    const rh = opts.rowHeight();
    if (rh <= 0) return base;
    const extra = Math.ceil((velocity() * ewmaImageLoadMs()) / 1000 / rh);
    return Math.max(base, Math.min(max, base + extra));
  };

  const markSettled = () => {
    if (untrack(decodeGate)) setDecodeGate(false);
    if (settleDebounce) clearTimeout(settleDebounce);
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

      if (vel > window.innerHeight * FLING_VIEWPORTS_PER_SEC) {
        if (untrack(settled)) setSettled(false);
        if (settleDebounce) clearTimeout(settleDebounce);
        settleDebounce = setTimeout(() => setSettled(true), SETTLE_DEBOUNCE_MS);
      }

      opts.onFrame({ y, dy, velocity: vel, direction: dir });
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
