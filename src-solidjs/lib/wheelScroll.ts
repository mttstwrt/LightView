// Wheel-driven scrolling of the gallery's scroll host, with momentum smoothing.
//
// WebKitGTK doesn't propagate wheel events to window scroll natively, so the
// grids intercept them and drive the scroll themselves. Two quirks (vs.
// Chromium in the web client) shape this:
//
//   1. Traditional mouse wheels arrive as DOM_DELTA_LINE (deltaY ±1 per notch),
//      not pixels — normalized by deltaMode via `wheelPxPerUnit`, which maps one
//      WebKitGTK notch to one grid row. `lib/wheel.ts` has the full table.
//   2. Fractional relative scroll steps get rounded away every frame, so
//      relative stepping silently drops the tail of each gesture by an amount
//      that varies with frame timing. We instead animate an absolute float
//      target and keep our own float position, immune to engine rounding.
//
// Both grids need exactly this, differing only in their row height and what
// Ctrl+wheel means (resize thumbnails vs. change the target row height), so
// those are the two hooks the caller supplies.
//
// The horizontal axis is handled the same way, for the canvas. It is not an
// option: `maxScrollX()` is 0 on a host that cannot scroll sideways, so the x
// target is pinned at 0 there and the axis never writes. It has to be here
// rather than left to the browser, because this handler calls
// `preventDefault()` on every wheel event it sees — a canvas that did not
// carry x would swallow every trackpad sideways swipe and pan nowhere.

import { wheelPxPerUnit } from "./wheel";
import { maxScroll, maxScrollX, scrollLeft, scrollToX, scrollToY, scrollTop } from "./scrollHost";

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
 * Create a wheel-driven momentum scroller for the gallery's scroll host. The
 * returned handle's
 * `attach()` registers the (non-passive) listener and gives back a disposer
 * that also cancels any in-flight animation frame.
 */
export function createWheelScroll(opts: WheelScrollOptions): WheelScroll {
  let targetY = 0; // absolute scroll target (float)
  let currentY = 0; // our float view of scrollY, immune to engine rounding
  let targetX = 0;
  let currentX = 0;
  let animating = false;
  let rafId = 0;

  const drain = () => {
    const diffY = targetY - currentY;
    const diffX = targetX - currentX;
    // Skipped entirely on a vertical-only host, where neither can leave 0 — no
    // per-frame write of a `scrollLeft` that is already 0. Read before the
    // assignments below, so the frame that lands back on x = 0 still writes it.
    const movesX = targetX !== 0 || currentX !== 0;
    if (Math.abs(diffY) < SETTLE && Math.abs(diffX) < SETTLE) {
      currentY = targetY;
      currentX = targetX;
      scrollToY(Math.round(targetY));
      if (movesX) scrollToX(Math.round(targetX));
      animating = false;
      rafId = 0;
      opts.onSettle();
      return;
    }
    currentY += diffY * (1 - DECAY);
    currentX += diffX * (1 - DECAY);
    scrollToY(Math.round(currentY));
    if (movesX) scrollToX(Math.round(currentX));
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
      currentY = targetY = scrollTop();
      currentX = targetX = scrollLeft();
      animating = true;
    }
    const px = wheelPxPerUnit(e, opts.rowHeight());
    // Shift+wheel is how a plain mouse asks for a sideways scroll. Some
    // engines already report it as deltaX and some do not, so honour whichever
    // arrived rather than assuming: doubling it would scroll twice as far on
    // the engines that do the conversion themselves.
    const sideways = e.shiftKey && e.deltaX === 0;
    const dx = (sideways ? e.deltaY : e.deltaX) * px;
    const dy = (sideways ? 0 : e.deltaY) * px;
    targetY = Math.max(0, Math.min(maxScroll(), targetY + dy));
    targetX = Math.max(0, Math.min(maxScrollX(), targetX + dx));
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
