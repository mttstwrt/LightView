// Wheel-driven window scrolling with momentum smoothing.
//
// WebKitGTK doesn't propagate wheel events to window scroll natively, so the
// grids intercept them and drive the scroll themselves. Two quirks (vs.
// Chromium in the web client) shape this:
//
//   1. Traditional mouse wheels arrive as DOM_DELTA_LINE (deltaY ±1 per notch),
//      not pixels — normalized by deltaMode via `wheelPxPerUnit`, which maps one
//      WebKitGTK notch to one grid row. `lib/wheel.ts` has the full table.
//   2. Fractional `window.scrollBy()` steps get rounded away every frame, so
//      relative stepping silently drops the tail of each gesture by an amount
//      that varies with frame timing. We instead animate an absolute float
//      target and keep our own float position, immune to engine rounding.
//
// Both grids need exactly this, differing only in their row height and what
// Ctrl+wheel means (resize thumbnails vs. change the target row height), so
// those are the two hooks the caller supplies.

import { wheelPxPerUnit } from "./wheel";

/** Fraction of the remaining distance covered per frame. */
const DECAY = 0.8;
/** Distance (px) from the target at which we snap and stop. */
const SETTLE = 0.5;

export interface WheelScrollOptions {
  /** Pixels one wheel notch should travel — the grid's row height. */
  rowHeight: () => number;
  /** Called once the animation settles, so the caller can drain its fetch queue. */
  onSettle: () => void;
  /**
   * Ctrl/Cmd+wheel handler (zoom). Return `true` if it was handled, in which
   * case the event does not scroll. Returning `false` falls through to a normal
   * scroll — which is what lets a Ctrl+drag selection extend past the viewport.
   */
  onZoom: (e: WheelEvent) => boolean;
}

export interface WheelScroll {
  /** Attach the listener. Returns a disposer. */
  attach: () => () => void;
}

/**
 * Create a wheel-driven momentum scroller for `window`. The returned handle's
 * `attach()` registers the (non-passive) listener and gives back a disposer
 * that also cancels any in-flight animation frame.
 */
export function createWheelScroll(opts: WheelScrollOptions): WheelScroll {
  let targetY = 0; // absolute scroll target (float)
  let currentY = 0; // our float view of scrollY, immune to engine rounding
  let animating = false;
  let rafId = 0;

  const drain = () => {
    const diff = targetY - currentY;
    if (Math.abs(diff) < SETTLE) {
      currentY = targetY;
      window.scrollTo(0, Math.round(targetY));
      animating = false;
      rafId = 0;
      opts.onSettle();
      return;
    }
    currentY += diff * (1 - DECAY);
    window.scrollTo(0, Math.round(currentY));
    rafId = requestAnimationFrame(drain);
  };

  const onWheel = (e: WheelEvent) => {
    if ((e.ctrlKey || e.metaKey) && opts.onZoom(e)) {
      e.preventDefault();
      return;
    }
    e.preventDefault();
    // At the start of a gesture, re-sync our float position from the DOM so we
    // pick up any scrollbar drag / keyboard scroll that happened between
    // gestures. While animating we keep accumulating into targetY.
    if (!animating) {
      currentY = window.scrollY;
      targetY = window.scrollY;
      animating = true;
    }
    const maxScroll = Math.max(
      0,
      document.documentElement.scrollHeight - window.innerHeight,
    );
    const deltaPx = e.deltaY * wheelPxPerUnit(e, opts.rowHeight());
    targetY = Math.max(0, Math.min(maxScroll, targetY + deltaPx));
    if (!rafId) rafId = requestAnimationFrame(drain);
  };

  return {
    attach() {
      window.addEventListener("wheel", onWheel, { passive: false });
      return () => {
        window.removeEventListener("wheel", onWheel);
        if (rafId) cancelAnimationFrame(rafId);
      };
    },
  };
}
