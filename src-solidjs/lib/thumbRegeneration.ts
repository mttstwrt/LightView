// ---------------------------------------------------------------------------
// "This path's thumbnail bytes changed" — one subscription.
//
// Regenerating a thumbnail (context menu → Regenerate) leaves the grid holding
// a URL whose bytes the browser has already cached, so the cell keeps showing
// the stale image until something changes the URL. The grid learns about it
// through a DOM event the caller of `regenerate_thumbnail` dispatches once the
// call resolves.
//
// A DOM event rather than a server event on purpose: the regeneration was
// started by this client and finished when its own request returned, so
// routing it through the broadcast channel would mean every other client
// invalidating a URL whose bytes it has not fetched. The server does emit
// nothing here, and that is correct — the tier files changed, not the row.
// ---------------------------------------------------------------------------

import { onCleanup, onMount } from "solid-js";

/** Dispatched with `{ path }` once a regeneration request has returned. */
export const THUMB_REGENERATED_EVENT = "lightview:thumb-regenerated";

/** Tell every listening cell that `path`'s thumbnail bytes were replaced. */
export function announceThumbRegenerated(path: string) {
  window.dispatchEvent(
    new CustomEvent(THUMB_REGENERATED_EVENT, { detail: { path } }),
  );
}

/**
 * Call `onRegenerated(path)` whenever a thumbnail's bytes are replaced.
 *
 * Must run under a component's reactive owner — the listener is torn down via
 * `onCleanup`.
 */
export function onThumbRegenerated(onRegenerated: (path: string) => void): void {
  onMount(() => {
    const onDomEvent = (e: Event) => {
      const path = (e as CustomEvent<{ path: string }>).detail?.path;
      if (path) onRegenerated(path);
    };
    window.addEventListener(THUMB_REGENERATED_EVENT, onDomEvent);
    onCleanup(() => window.removeEventListener(THUMB_REGENERATED_EVENT, onDomEvent));
  });
}
