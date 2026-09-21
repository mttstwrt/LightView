// What kind of client this is.
//
// There is one runtime now, so `isTauri()`, `isWeb()` and `safeListen` are
// gone. The part that has to survive that deletion is `isMobile`, and it needs
// care rather than a mechanical edit: it was defined as `isWeb() && width <
// 640`, so deleting `isWeb()` silently turns it into "narrow window" — and a
// desktop browser dragged narrow would take the mobile path, whose default cell
// size is a *two-column* layout.
//
// So it is redefined deliberately: viewport width **and** touch capability.
// That is a capability rather than a guess, and it answers the question the
// callers are actually asking.

import { createSignal } from "solid-js";

/** Matches Tailwind's `sm` breakpoint, so CSS and JS stay in step. */
const MOBILE_MAX_WIDTH = 640;

// Touch-capability detection. Gesture handlers branch on the live pointer's
// `pointerType` for correctness; this opts CSS and touch-only affordances in
// and out at render time.
let cachedTouch: boolean | null = null;

/** True when the device reports a touch input. Cached after the first call. */
export function hasTouch(): boolean {
  if (cachedTouch !== null) return cachedTouch;
  if (typeof window === "undefined") return false;
  cachedTouch =
    (typeof navigator !== "undefined" && navigator.maxTouchPoints > 0) ||
    "ontouchstart" in window ||
    (typeof window.matchMedia === "function" &&
      window.matchMedia("(pointer: coarse)").matches);
  return cachedTouch;
}

function detectMobile(): boolean {
  return (
    typeof window !== "undefined" &&
    window.innerWidth < MOBILE_MAX_WIDTH &&
    hasTouch()
  );
}

const [mobile, setMobile] = createSignal(detectMobile());

if (typeof window !== "undefined") {
  window.addEventListener("resize", () => setMobile(detectMobile()));
}

/** Reactive: a narrow viewport on a device that can be touched. */
export const isMobile = mobile;

/**
 * Ceiling on the device-pixel-ratio multiplier the grid uses when choosing a
 * tier.
 *
 * The grid sizes its request as `cell CSS px × DPR` — the pixels the cell can
 * actually show. Taken literally on a 3× phone that triples the linear request
 * and so multiplies the image's memory by nine, pushing every ordinary phone
 * cell to the top of the ladder.
 *
 * Measured (Chromium, phone viewport, loading and dropping 1200 thumbnails):
 * memory climbs about five times faster per image at 1024px than at 512px, and
 * about twenty times faster than at 128px. Both browsers plateau eventually —
 * the cache is a fraction of device memory — but a phone's ceiling is low and
 * iOS enforces it by killing the tab rather than by pruning, so how fast a tier
 * takes you there is the whole game.
 *
 * Two is where more stops being visible on a thumbnail and starts being purely
 * cost: a 512px image in a 194px cell on a 3× screen is still 88% of native
 * resolution. Nothing at DPR 2 or below is affected, which is every desktop and
 * most laptops.
 */
const MAX_DPR_SCALE = 2;

/** The DPR multiplier to size a thumbnail request by. */
export function renderScale(): number {
  if (typeof window === "undefined") return 1;
  return Math.min(window.devicePixelRatio || 1, MAX_DPR_SCALE);
}
