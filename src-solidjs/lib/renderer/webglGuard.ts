// Crash-loop breaker for the WebGL renderer.
//
// Some devices — phones especially — can hard-crash the GPU/WebView process
// when the WebGL renderer initialises (e.g. an out-of-memory texture
// allocation). Because the chosen renderer is persisted, that turns into a boot
// loop: every launch re-selects WebGL, re-crashes, and the user can never reach
// the setting to turn it off.
//
// We break the loop with a flag written to localStorage *before* WebGL is
// touched and cleared once a context has run stably for a moment. If a launch
// finds the flag still set, the previous attempt never finished — so we treat
// WebGL as unsafe and fall back to Canvas2D, persisting the downgrade.

const KEY = "lv-webgl-init-guard";

/** True if a prior WebGL init started but never reported back stable — i.e. the
 *  page most likely crashed mid-init. */
export function webglPreviouslyCrashed(): boolean {
  try {
    return localStorage.getItem(KEY) === "1";
  } catch {
    return false;
  }
}

/** Mark that we're about to create a WebGL context. Persisted so it survives a
 *  hard crash and is still visible on the next launch. */
export function markWebglInitStarted(): void {
  try {
    localStorage.setItem(KEY, "1");
  } catch {}
}

/** Clear the guard — called once the context has run without crashing, or after
 *  a clean (caught) construction failure that didn't take down the page. */
export function markWebglStable(): void {
  try {
    localStorage.removeItem(KEY);
  } catch {}
}
