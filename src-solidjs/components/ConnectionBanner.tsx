import { Show, createSignal } from "solid-js";

// Shown when a request to the server failed for a reason that is neither the
// readiness gate nor a credential — the network, a dropped TLS exception, a
// server that went away. Without it that state is invisible: the browser's own
// HTTP cache keeps painting a full, scrollable grid of already-fetched
// thumbnails while every write and every uncached cell silently fails, which
// reads as "the app is fine, the photos are just slow".
//
// There is no `Reset connection` button any more. It unregistered the service
// worker so iOS would re-prompt for the certificate, and both the worker and
// the second cache it managed are gone — the browser's own cache revalidates
// against the `ETag`, and reloading is the whole recovery.

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
            Can't reach LightView — what you see may be out of date.
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
