// Wheel-driven scrolling of the gallery's scroll host, with momentum smoothing.
//
// The grid intercepts wheel events and drives the scroll itself, for two
// reasons that outlive the desktop webview this was first written for:
//
//   1. Ctrl+wheel is a zoom, and deciding that per-event means owning the
//      event — returning `false` from `onZoom` is what lets a Ctrl+drag
//      selection keep scrolling past the viewport.
//   2. Fractional relative scroll steps get rounded away every frame, so
//      relative stepping silently drops the tail of each gesture by an amount
//      that varies with frame timing. We instead animate an absolute float
//      target and keep our own float position, immune to engine rounding.
//
// Deltas are normalized by `deltaMode` through `wheelPxPerUnit`; `lib/wheel.ts`
// has the engine table.

import { wheelPxPerUnit } from "./wheel";
import { maxScroll, scrollToY, scrollTop } from "./scrollHost";

/** Fraction of the remaining distance covered per frame. */
const DECAY = 0.8;
/** Distance (px) from the target at which we snap and stop. */
const SETTLE = 0.5;
/**
 * Quiet time after the last notch that ends a zoom gesture.
 *
 * A wheel has no gesture-end event, so this is the only way to know. Long
 * enough to survive the pause between two deliberate notches, short enough that
 * a zoom and a later scroll are not treated as one gesture.
 */
const ZOOM_GESTURE_END_MS = 220;

export interface WheelScrollOptions {
  /** Called once the animation settles, so the caller can drain its fetch queue. */
  onSettle: () => void;
  /**
   * Ctrl/Cmd+wheel handler (zoom). Return `true` if it was handled, in which
   * case the event does not scroll. Returning `false` falls through to a normal
   * scroll — which is what lets a Ctrl+drag selection extend past the viewport.
   */
  onZoom: (e: WheelEvent) => boolean;
  /**
   * The edges of a zoom *gesture*, as opposed to one notch of it.
   *
   * Ctrl+wheel is continuous — a twelve percent step per notch, twenty of them
   * in a spin — and a caller holding the gallery's position across a zoom has
   * to capture what it is holding once, at the start. Re-deriving it each notch
   * accumulates rounding into visible drift, and a notch that clamps at the end
   * of the content would destroy the reading it was derived from.
   *
   * `onSettle` cannot serve: a handled zoom returns before the momentum
   * animation is touched, so during a zoom it fires zero times — and it is the
   * tail of a scroll, not the start of anything. The end is a debounce, because
   * a wheel gesture has no terminating event of its own.
   */
  onZoomStart?: () => void;
  onZoomEnd?: () => void;
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
  let animating = false;
  let rafId = 0;
  let zooming = false;
  let zoomEndTimer: ReturnType<typeof setTimeout> | undefined;

  const endZoom = () => {
    if (zoomEndTimer) {
      clearTimeout(zoomEndTimer);
      zoomEndTimer = undefined;
    }
    if (!zooming) return;
    zooming = false;
    opts.onZoomEnd?.();
  };

  const drain = () => {
    const diff = targetY - currentY;
    if (Math.abs(diff) < SETTLE) {
      currentY = targetY;
      scrollToY(Math.round(targetY));
      animating = false;
      rafId = 0;
      opts.onSettle();
      return;
    }
    currentY += diff * (1 - DECAY);
    scrollToY(Math.round(currentY));
    rafId = requestAnimationFrame(drain);
  };

  const onWheel = (e: WheelEvent) => {
    if ((e.ctrlKey || e.metaKey) && opts.onZoom(e)) {
      e.preventDefault();
      if (!zooming) {
        zooming = true;
        opts.onZoomStart?.();
      }
      if (zoomEndTimer) clearTimeout(zoomEndTimer);
      zoomEndTimer = setTimeout(endZoom, ZOOM_GESTURE_END_MS);
      return;
    }
    // A scroll ends a zoom gesture immediately: whatever was being held in
    // place, the reader has just chosen a new place.
    if (zooming) endZoom();
    e.preventDefault();
    // At the start of a gesture, re-sync our float position from the DOM so we
    // pick up any scrollbar drag / keyboard scroll that happened between
    // gestures. While animating we keep accumulating into targetY.
    if (!animating) {
      currentY = scrollTop();
      targetY = scrollTop();
      animating = true;
    }
    const deltaPx = e.deltaY * wheelPxPerUnit(e);
    targetY = Math.max(0, Math.min(maxScroll(), targetY + deltaPx));
    if (!rafId) rafId = requestAnimationFrame(drain);
  };

  return {
    attach() {
      window.addEventListener("wheel", onWheel, { passive: false });
      return () => {
        window.removeEventListener("wheel", onWheel);
        if (rafId) cancelAnimationFrame(rafId);
        endZoom();
      };
    },
  };
}
