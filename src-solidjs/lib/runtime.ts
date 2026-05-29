// Runtime mode detection for the desktop (Tauri) vs. remote browser (web)
// builds of the frontend. The same SPA bundle runs in both; this module is the
// single place that decides which transport and capabilities are available.

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

const TOKEN_COOKIE = "lv_token";

/** Bootstrap the web client's auth token (web mode only).
 *
 *  The remote-access URL embeds a one-time `?token=`. On first load we move it
 *  into an `lv_token` cookie so the browser attaches it automatically to every
 *  same-origin request (`<img>`, `<video>`, and `fetch`), then strip it from
 *  the visible URL. On later loads the cookie already carries it. */
export function initWebAuth(): void {
  if (isTauri() || typeof window === "undefined") return;

  const url = new URL(window.location.href);
  const fromUrl = url.searchParams.get("token");
  if (fromUrl) {
    // Session cookie, scoped to the whole app, not sent cross-site.
    document.cookie = `${TOKEN_COOKIE}=${fromUrl}; path=/; SameSite=Strict`;
    url.searchParams.delete("token");
    window.history.replaceState({}, "", url.toString());
  }
}

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

