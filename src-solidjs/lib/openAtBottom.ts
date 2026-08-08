// "Start at bottom": land a freshly opened gallery at the end of the grid.

import { createEffect, on, onCleanup } from "solid-js";

// "Start at bottom": land a freshly opened gallery at the end of the grid
// instead of the top, so browsing runs bottom-to-top.
//
// This is deliberately *not* a one-shot scroll on open. Both grids report a
// content height that keeps changing for a while after the first paint — the
// justified grid re-flows as aspect ratios stream in, and either grid re-lays
// out when the container is first measured — so a single jump would land at the
// bottom of a shorter document and end up stranded mid-gallery. Instead the
// intent stays *armed*: every content-height change re-pins the view to the
// bottom until the layout stops moving, or until the user scrolls and takes
// over.
//
// Disarming keys off wheel/touch/keyboard rather than the `scroll` event,
// because our own `scrollTo` fires `scroll` too and would cancel the arm on its
// first application.

/** How long the layout must hold still before we consider the open finished. */
const SETTLE_MS = 800;

/** Hard ceiling on how long an arm can stay live, in case content height never
 *  stops changing (a gallery still generating thumbnails can keep nudging it). */
const MAX_ARMED_MS = 6000;

interface OpenAtBottomOptions {
  /** The `start_at_bottom` display setting. */
  enabled: () => boolean;
  /** Identifies the open gallery; a change re-arms. Null when none is open. */
  galleryKey: () => string | null;
  /** Full scrollable height of the grid, as the grids report it. */
  contentHeight: () => number;
}

/** Wire up the start-at-bottom behaviour. Call once, inside a root component. */
export function createOpenAtBottom(opts: OpenAtBottomOptions) {
  let armed = false;
  let armedAt = 0;
  let settleTimer: number | undefined;

  const clearSettleTimer = () => {
    if (settleTimer !== undefined) {
      clearTimeout(settleTimer);
      settleTimer = undefined;
    }
  };

  const disarm = () => {
    armed = false;
    clearSettleTimer();
  };

  const scrollToBottom = () => {
    const max = Math.max(
      0,
      document.documentElement.scrollHeight - window.innerHeight,
    );
    if (max <= 0) return;
    window.scrollTo(0, max);
  };

  /** Pin to the bottom and restart the settle countdown. */
  const apply = () => {
    scrollToBottom();
    clearSettleTimer();
    settleTimer = window.setTimeout(disarm, SETTLE_MS);
  };

  // Any deliberate scroll input hands control back to the user immediately.
  const userTookOver = () => disarm();
  const passive = { passive: true } as const;
  window.addEventListener("wheel", userTookOver, passive);
  window.addEventListener("touchstart", userTookOver, passive);
  window.addEventListener("keydown", userTookOver);
  window.addEventListener("pointerdown", userTookOver, passive);
  onCleanup(() => {
    window.removeEventListener("wheel", userTookOver);
    window.removeEventListener("touchstart", userTookOver);
    window.removeEventListener("keydown", userTookOver);
    window.removeEventListener("pointerdown", userTookOver);
    clearSettleTimer();
  });

  // Arm on gallery open — and on switching the setting on, so toggling it
  // demonstrates itself instead of waiting for the next gallery.
  createEffect(
    on([opts.galleryKey, opts.enabled], ([key, enabled]) => {
      if (!enabled || !key) {
        disarm();
        return;
      }
      armed = true;
      armedAt = Date.now();
      // Usually a no-op — a gallery that has just opened has no content height
      // yet, and the effect below does the real work. It matters when the
      // setting is switched on with a gallery already laid out, so the toggle
      // demonstrates itself.
      apply();
    }),
  );

  // Re-pin on every re-flow while armed, and let the layout going quiet (or the
  // ceiling elapsing) end the open.
  createEffect(
    on(opts.contentHeight, (height) => {
      if (!armed || height <= 0) return;
      if (Date.now() - armedAt > MAX_ARMED_MS) {
        disarm();
        return;
      }
      // A frame later: the grid's spacer element is styled from this same
      // signal, so the document is still its old height right now.
      requestAnimationFrame(() => {
        if (armed) apply();
      });
    }),
  );
}
