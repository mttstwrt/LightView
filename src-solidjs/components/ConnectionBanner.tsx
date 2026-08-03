import { Show, createSignal } from "solid-js";
import { resetServiceWorker } from "../lib/swControl";

// Shown on the web client when the boot round-trip to the server failed but
// the grid rendered anyway — from the IndexedDB snapshot and service-worker
// thumbnail cache. Without it that state is invisible: the app paints a full,
// scrollable gallery while every write, filter, and uncached thumbnail
// silently fails, which reads as "the app is fine, the photos are just slow".
//
// `Reset connection` is the recovery path for the common cause on iOS — a
// dropped certificate exception. See lib/swControl.ts.

export function ConnectionBanner(props: { onRetry: () => void }) {
  const [dismissed, setDismissed] = createSignal(false);
  const [retrying, setRetrying] = createSignal(false);

  const retry = async () => {
    setRetrying(true);
    try {
      await props.onRetry();
    } finally {
      setRetrying(false);
    }
  };

  return (
    <Show when={!dismissed()}>
      <div class="fixed top-0 inset-x-0 z-[60] flex items-center gap-3 px-3 py-2 bg-amber-950/95 border-b border-amber-800/60 text-amber-100 text-xs backdrop-blur">
        <span class="flex-1 min-w-0">
          <b class="font-medium">Offline view.</b>{" "}
          <span class="text-amber-200/80">
            Can't reach LightView — showing the last cached gallery.
          </span>
        </span>
        <button
          onClick={retry}
          disabled={retrying()}
          class="flex-shrink-0 px-2 py-1 rounded bg-amber-900/70 hover:bg-amber-800/70 disabled:opacity-50 transition-colors cursor-pointer"
        >
          {retrying() ? "Retrying…" : "Retry"}
        </button>
        <button
          onClick={() => void resetServiceWorker()}
          title="Unregister the service worker and reload, so the browser can re-prompt for the server's certificate. Your pairing is kept."
          class="flex-shrink-0 px-2 py-1 rounded bg-amber-900/70 hover:bg-amber-800/70 transition-colors cursor-pointer"
        >
          Reset connection
        </button>
        <button
          onClick={() => setDismissed(true)}
          title="Dismiss"
          class="flex-shrink-0 px-1 text-amber-300/70 hover:text-amber-100 transition-colors cursor-pointer"
        >
          ✕
        </button>
      </div>
    </Show>
  );
}
