// Runtime mode detection for the desktop (Tauri) vs. remote browser (web)
// builds of the frontend. The same SPA bundle runs in both; this module is the
// single place that decides which transport and capabilities are available.

import { createSignal } from "solid-js";
import {
  listen as _tauriListen,
  type EventCallback,
  type UnlistenFn,
} from "@tauri-apps/api/event";

export type { UnlistenFn };

/** True when running inside the Tauri webview (desktop app). Tauri injects
 *  `__TAURI_INTERNALS__` onto the global before any app code runs. */
export function isTauri(): boolean {
  return typeof (globalThis as any).__TAURI_INTERNALS__ !== "undefined";
}

/** True when running as the remote web client (a plain browser). */
export function isWeb(): boolean {
  return !isTauri();
}

// Mobile detection: web client + narrow viewport. The desktop web client on a
// wide monitor is unaffected. Threshold matches Tailwind's `sm` breakpoint so
// CSS and JS stay in sync. Reactive — components that read `isMobile()` will
// re-run on window resize / orientation change.
const MOBILE_MAX_WIDTH = 640;

function detectMobile(): boolean {
  return (
    isWeb() &&
    typeof window !== "undefined" &&
    window.innerWidth < MOBILE_MAX_WIDTH
  );
}

const [_isMobile, _setIsMobile] = createSignal(detectMobile());

if (typeof window !== "undefined") {
  window.addEventListener("resize", () => _setIsMobile(detectMobile()));
}

/** Reactive: true on the web client at narrow viewports. False on desktop. */
export const isMobile = _isMobile;

// Touch-capability detection. Unlike `isMobile()` (a viewport-width heuristic
// for the web client only), this answers "can the user touch the screen?" — so
// it's true on phones, tablets, and touchscreen laptops alike, in both the
// desktop and web builds. Gesture handlers branch on the live pointer's
// `pointerType` for correctness; this is used to opt CSS (e.g. `touch-action`)
// and touch-only affordances in/out at render time.
let _hasTouch: boolean | null = null;

/** True when the device reports a touch input. Cached after first call. */
export function hasTouch(): boolean {
  if (_hasTouch !== null) return _hasTouch;
  if (typeof window === "undefined") return false;
  _hasTouch =
    (typeof navigator !== "undefined" && navigator.maxTouchPoints > 0) ||
    "ontouchstart" in window ||
    (typeof window.matchMedia === "function" &&
      window.matchMedia("(pointer: coarse)").matches);
  return _hasTouch;
}

/** Event emitted when the server asks the web client to (re-)authenticate
 *  with the gallery password — i.e. it responded 401 with
 *  `WWW-Authenticate: LV-Password`. The App listens for this and shows the
 *  password modal; the modal calls `resolvePasswordChallenge()` once the
 *  password has been accepted so the pending fetch can be retried. */
export const PASSWORD_CHALLENGE_EVENT = "lightview:password-challenge";

/** Event emitted when the server says the device cookie is missing or
 *  invalid (401 without a password challenge). The router uses this to
 *  redirect to `/pair`. */
export const NOT_PAIRED_EVENT = "lightview:not-paired";

/** Tauri event listener that no-ops in web mode (the remote backend can't push
 *  events to a browser). Drop-in for `@tauri-apps/api/event`'s `listen` so call
 *  sites are unchanged — import it as `{ safeListen as listen }`. */
export async function safeListen<T>(
  event: string,
  handler: EventCallback<T>,
): Promise<UnlistenFn> {
  if (isWeb()) return (() => {}) as UnlistenFn;
  return _tauriListen<T>(event, handler);
}

