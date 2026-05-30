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

