// Wheel delta normalization.
//
// The grids and the viewer all `preventDefault()` wheel events and drive the
// scroll/pan themselves (WebKitGTK doesn't propagate wheel to window scroll),
// so the browser's own delta → pixels conversion never runs. That makes this
// module the only thing deciding how far a notch travels, and engines disagree
// badly about what `deltaMode: DOM_DELTA_LINE` counts:
//
//   - Chromium (WebView2 and every Chromium browser) and WKWebView report
//     DOM_DELTA_PIXEL with ~100px per notch. Nothing to convert.
//   - WebKitGTK (the Linux desktop webview) reports DOM_DELTA_LINE with
//     deltaY = ±1 per notch — its "line" *is* one notch.
//   - Firefox reports DOM_DELTA_LINE with deltaY = the OS lines-per-notch,
//     normally 3, where a line is a line of text — not a notch.
//
// Treating every line as one notch is what broke Firefox: the grids mapped a
// notch to one thumbnail row, so each wheel tick moved three full rows (~600px
// at the default thumbnail size, more as thumbnails grow) against Chromium's
// ~100px, and a multi-notch flick compounded that into dozens of rows. Line
// mode therefore only means "one notch" under Tauri, which is the sole place
// WebKitGTK is in play — desktop Windows/macOS report pixels anyway, so this
// branch is effectively "is this WebKitGTK?".

import { isTauri } from "./runtime";

/** A line of text, for engines whose line mode counts real lines (Firefox).
 *  ~3 lines/notch × 40 lands near Chromium's ~100–120px per notch. */
const WHEEL_LINE_PX = 40;

/** Pixels a notch should travel when an engine's line mode counts notches and
 *  the caller has no better step to offer (the grids pass a row height). */
const WHEEL_NOTCH_PX = 100;

/**
 * Pixels per unit of a wheel event's delta. Multiply `deltaX`/`deltaY` by this
 * to get pixels; `deltaMode` applies to both axes, so one factor covers both.
 *
 * `notchPx` is the distance one notch should cover on the engines whose line
 * mode counts notches (WebKitGTK) — the grids pass their row height so a notch
 * advances exactly one row there, preserving the desktop feel.
 */
export function wheelPxPerUnit(e: WheelEvent, notchPx: number = WHEEL_NOTCH_PX): number {
  if (e.deltaMode === 2) return window.innerHeight; // DOM_DELTA_PAGE
  if (e.deltaMode === 1) return isTauri() ? notchPx : WHEEL_LINE_PX; // DOM_DELTA_LINE
  return 1; // DOM_DELTA_PIXEL
}
