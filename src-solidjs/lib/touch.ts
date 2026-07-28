// Shared touch-gesture primitives. Dependency-free helpers used by the viewer
// (swipe / pinch / pan) and the gallery grid (pinch-to-density). The bespoke
// state machines live in the components; this module only holds the geometry
// and the tuning constants so they stay consistent across call sites.

export interface Point {
  x: number;
  y: number;
}

/** Euclidean distance between two pointers (for pinch scale). */
export function pointerDistance(a: Point, b: Point): number {
  return Math.hypot(a.x - b.x, a.y - b.y);
}

/** Midpoint between two pointers (pinch focal point / zoom anchor). */
export function pointerMidpoint(a: Point, b: Point): Point {
  return { x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 };
}

// --- Tuning constants ---------------------------------------------------

/** Max ms between two taps to count as a double-tap. Deliberately tighter
 *  than the platform's own ~300 ms: nothing waits out this window any more
 *  (the single-tap action fires immediately — see MediaViewer's `handleTap`),
 *  so its only job is to keep two deliberate taps together while a slow
 *  tap-pause-tap stays two separate taps. */
export const DOUBLE_TAP_MS = 250;

/** Movement (px) below which a touch is still considered a tap, not a drag. */
export const TAP_SLOP_PX = 10;

/** Min px a one-finger drag must travel before we lock it to an axis
 *  (horizontal = navigate, vertical = dismiss). */
export const AXIS_LOCK_PX = 12;

/** Fraction of viewport width a horizontal swipe must cross to flip to the
 *  next/previous photo (a fast flick can trigger below this — see
 *  `SWIPE_VELOCITY`). */
export const SWIPE_NAV_RATIO = 0.25;

/** Min px/ms flick velocity that triggers navigation regardless of distance. */
export const SWIPE_VELOCITY = 0.5;

/** Px a downward swipe must travel to dismiss the viewer. */
export const SWIPE_DISMISS_PX = 110;

/** Scale the photo eases toward at full dismiss travel (Apple-Photos shrink). */
export const DISMISS_MIN_SCALE = 0.85;

/** Hold duration (ms) before a stationary touch counts as a long-press.
 *  Matches the typical native threshold so it feels like the platform's own
 *  context-menu gesture. */
export const LONG_PRESS_MS = 500;

// --- Long-press helpers ---------------------------------------------------

/**
 * Swallow the compatibility `click` the browser synthesizes when the finger
 * lifts after a long-press. iOS Safari fires it even after a multi-second
 * hold, and it would land on whatever opened under the finger (a context
 * menu's first item, the viewer backdrop, the grid cell). Call this from the
 * `pointerup` that ends a fired long-press — the click follows within the
 * same event sequence, so a short window is enough; if the platform never
 * fires one (Android suppresses it), the listener times out harmlessly.
 */
export function suppressNextClick(windowMs = 150): void {
  let timer: number | null = null;
  const swallow = (e: MouseEvent) => {
    e.preventDefault();
    e.stopPropagation();
    cleanup();
  };
  const cleanup = () => {
    window.removeEventListener("click", swallow, true);
    if (timer !== null) {
      clearTimeout(timer);
      timer = null;
    }
  };
  window.addEventListener("click", swallow, true);
  timer = window.setTimeout(cleanup, windowMs);
}
