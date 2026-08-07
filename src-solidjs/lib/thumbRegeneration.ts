// ---------------------------------------------------------------------------
// "This path's thumbnail bytes changed" — one subscription, two transports.
//
// Regenerating a thumbnail (context menu → Regenerate) leaves the grid holding
// a URL whose bytes the webview has already cached, so the cell keeps showing
// the stale image until something changes the URL. The grids learn about it
// through whichever channel exists on the platform they're running on:
//
//   desktop — the Tauri `thumb:regenerated` event, emitted by the
//             regenerate_thumbnail command after it rewrites the row
//   web     — a THUMB_REGENERATED_EVENT DOM event, dispatched by ContextMenu
//             once the /api/invoke call resolves (a browser has no Tauri IPC)
//
// Both grids subscribed to both channels with the same dozen lines, down to
// the `.then()` that stores the unlisten handle — the kind of duplication
// where a fix lands in one grid and not the other. Callers now supply only
// what differs: what to do with the path.
// ---------------------------------------------------------------------------

import { onCleanup, onMount } from "solid-js";
import { safeListen as listen, type UnlistenFn } from "./runtime";
import { THUMB_REGENERATED_EVENT } from "./ipc";

/**
 * Call `onRegenerated(path)` whenever a thumbnail's bytes are replaced.
 *
 * Must run under a component's reactive owner (both listeners are torn down
 * via `onCleanup`). Safe to call on either platform: `safeListen` is a no-op
 * off-desktop, and the DOM event simply never fires on desktop.
 */
export function onThumbRegenerated(onRegenerated: (path: string) => void): void {
  let unlisten: UnlistenFn | undefined;

  onMount(() => {
    // `listen` resolves asynchronously, so the handle can arrive after an
    // immediate unmount — hence the cleanup below reads the variable rather
    // than closing over a value.
    listen<{ path: string }>("thumb:regenerated", (event) => {
      onRegenerated(event.payload.path);
    }).then((fn) => {
      unlisten = fn;
    });

    const onDomEvent = (e: Event) => {
      const path = (e as CustomEvent<{ path: string }>).detail?.path;
      if (path) onRegenerated(path);
    };
    window.addEventListener(THUMB_REGENERATED_EVENT, onDomEvent);
    onCleanup(() => window.removeEventListener(THUMB_REGENERATED_EVENT, onDomEvent));
  });

  onCleanup(() => unlisten?.());
}
