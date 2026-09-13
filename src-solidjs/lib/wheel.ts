// Wheel delta normalization.
//
// The grid and the viewer both `preventDefault()` wheel events and drive the
// scroll/pan themselves, so the browser's own delta → pixels conversion never
// runs. That makes this module the only thing deciding how far a notch travels,
// and engines disagree about what `deltaMode: DOM_DELTA_LINE` counts:
//
//   - Chromium and WebKit report DOM_DELTA_PIXEL with ~100px per notch.
//     Nothing to convert.
//   - Firefox reports DOM_DELTA_LINE with deltaY = the OS lines-per-notch,
//     normally 3, where a line is a line of text — not a notch.
//
// Treating a line as a notch is what broke Firefox: the grid maps a notch to
// one thumbnail row, so each tick moved three full rows (~600px at the default
// thumbnail size, more as thumbnails grow) against Chromium's ~100px, and a
// multi-notch flick compounded that into dozens of rows.
//
// WebKitGTK was the one engine whose "line" really was one notch, and it left
// with the desktop webview — so there is no longer an engine that wants a
// caller-supplied notch size, and the parameter that carried one is gone.

/** A line of text, for engines whose line mode counts real lines (Firefox).
 *  ~3 lines/notch × 40 lands near Chromium's ~100–120px per notch. */
const WHEEL_LINE_PX = 40;

/** Pixels one unit of `deltaY` represents for this event. */
export function wheelPxPerUnit(e: WheelEvent): number {
  if (e.deltaMode === 2) return window.innerHeight; // DOM_DELTA_PAGE
  if (e.deltaMode === 1) return WHEEL_LINE_PX; // DOM_DELTA_LINE
  return 1; // DOM_DELTA_PIXEL
}
