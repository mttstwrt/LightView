import { Show, For, createSignal, onCleanup, onMount } from "solid-js";
import {
  getUploadConfig,
  uploadFiles,
  type UploadConfig,
  type UploadResult,
} from "../../lib/ipc";

// Match the TopBar's mobile scroll-direction reveal: pixels to scroll past the
// top before we'll hide, and the anti-jitter delta to flip direction.
const REVEAL_AT_TOP = 40;
const DIR_THRESHOLD = 6;

/** Floating upload button for the web client. Lets a phone push photos/videos
 *  into the host gallery; the host organizes them into subfolders. Renders
 *  nothing until it confirms uploads are enabled for the open gallery.
 *
 *  `onUploaded` is called after a successful upload so the gallery can refresh
 *  to show the new items (the web client gets no fs-watch push from the host). */
export function UploadButton(props: { onUploaded?: () => void }) {
  const [config, setConfig] = createSignal<UploadConfig | null>(null);
  const [open, setOpen] = createSignal(false);
  const [files, setFiles] = createSignal<File[]>([]);
  const [album, setAlbum] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [progress, setProgress] = createSignal(0);
  const [result, setResult] = createSignal<UploadResult | null>(null);
  const [error, setError] = createSignal("");
  // Slide the FAB away on scroll-down, reveal on scroll-up — mirrors the
  // filter bar. Never hide while the sheet is open.
  const [scrollHidden, setScrollHidden] = createSignal(false);
  const hidden = () => scrollHidden() && !open();

  let fileInput: HTMLInputElement | undefined;

  onMount(async () => {
    try {
      setConfig(await getUploadConfig());
    } catch {
      // No gallery open or not paired — just stay hidden.
    }
  });

  onMount(() => {
    let lastY = window.scrollY;
    const onScroll = () => {
      const y = window.scrollY;
      const dy = y - lastY;
      if (y < REVEAL_AT_TOP) setScrollHidden(false);
      else if (dy > DIR_THRESHOLD) setScrollHidden(true);
      else if (dy < -DIR_THRESHOLD) setScrollHidden(false);
      lastY = y;
    };
    window.addEventListener("scroll", onScroll, { passive: true });
    onCleanup(() => window.removeEventListener("scroll", onScroll));
  });

  const showAlbum = () => config()?.scheme === "year_album";

  const reset = () => {
    setFiles([]);
    setAlbum("");
    setProgress(0);
    setResult(null);
    setError("");
  };

  const closeSheet = () => {
    if (busy()) return;
    setOpen(false);
    reset();
  };

  const onPick = (e: Event) => {
    const input = e.currentTarget as HTMLInputElement;
    setFiles(input.files ? Array.from(input.files) : []);
    setResult(null);
    setError("");
  };

  const doUpload = async () => {
    if (busy() || files().length === 0) return;
    setBusy(true);
    setError("");
    setResult(null);
    setProgress(0);
    try {
      const res = await uploadFiles(files(), album(), setProgress);
      setResult(res);
      setFiles([]);
      if (res.uploaded.length > 0) props.onUploaded?.();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Show when={config()?.enabled}>
      {/* Floating action button */}
      <button
        type="button"
        onClick={() => setOpen(true)}
        aria-label="Upload photos"
        class="fixed bottom-5 right-5 z-[140] w-14 h-14 rounded-full flex items-center justify-center text-white transition-[transform,opacity] duration-200"
        style={{
          background: "rgb(20, 184, 166)",
          "box-shadow": "0 4px 16px rgba(0,0,0,0.4)",
          // Slide off the bottom-right and fade when scroll-hidden.
          transform: hidden() ? "translateY(150%)" : "translateY(0)",
          opacity: hidden() ? "0" : "1",
          "pointer-events": hidden() ? "none" : "auto",
        }}
      >
        <svg class="w-6 h-6" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 4v12m0-12l-4 4m4-4l4 4M4 20h16" />
        </svg>
      </button>

      {/* Bottom sheet */}
      <Show when={open()}>
        <div
          class="fixed inset-0 z-[200] flex items-end sm:items-center justify-center"
          style={{ background: "rgba(0,0,0,0.6)" }}
          onClick={closeSheet}
        >
          <div
            class="w-full sm:max-w-md rounded-t-2xl sm:rounded-2xl border border-white/10 p-5 flex flex-col gap-4"
            style={{ background: "rgb(18,18,18)", "max-height": "85vh" }}
            onClick={(e) => e.stopPropagation()}
          >
            <div class="flex items-center justify-between">
              <h2 class="text-white text-base font-semibold">Upload photos</h2>
              <button
                type="button"
                onClick={closeSheet}
                disabled={busy()}
                class="text-white/50 hover:text-white disabled:opacity-30"
                aria-label="Close"
              >
                <svg class="w-5 h-5" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                  <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M6 18L18 6M6 6l12 12" />
                </svg>
              </button>
            </div>

            <input
              ref={fileInput}
              type="file"
              accept="image/*,video/*"
              multiple
              class="hidden"
              onChange={onPick}
            />

            <button
              type="button"
              onClick={() => fileInput?.click()}
              disabled={busy()}
              class="w-full py-3 rounded-lg border border-dashed border-white/20 text-white/80 text-sm hover:border-teal-500/50 hover:text-white disabled:opacity-40"
            >
              {files().length > 0
                ? `${files().length} file${files().length === 1 ? "" : "s"} selected — tap to change`
                : "Choose photos or videos"}
            </button>

            <Show when={files().length > 0}>
              <div class="max-h-32 overflow-y-auto flex flex-col gap-1 text-xs text-white/60">
                <For each={files()}>
                  {(f) => (
                    <div class="flex justify-between gap-2">
                      <span class="truncate">{f.name}</span>
                      <span class="shrink-0">{formatSize(f.size)}</span>
                    </div>
                  )}
                </For>
              </div>
            </Show>

            <Show when={showAlbum()}>
              <input
                type="text"
                value={album()}
                onInput={(e) => setAlbum(e.currentTarget.value)}
                placeholder="Album name (optional)"
                disabled={busy()}
                class="w-full px-3 py-2 rounded-lg bg-white/5 border border-white/10 text-white text-sm placeholder:text-white/30 focus:outline-none focus:border-teal-500/50 disabled:opacity-40"
              />
            </Show>

            <Show when={busy()}>
              <div class="w-full h-1.5 rounded-full bg-white/10 overflow-hidden">
                <div
                  class="h-full bg-teal-500 transition-all"
                  style={{ width: `${Math.round(progress() * 100)}%` }}
                />
              </div>
            </Show>

            <Show when={error()}>
              <p class="text-red-400 text-xs">{error()}</p>
            </Show>

            <Show when={result()}>
              {(r) => (
                <div class="text-xs flex flex-col gap-1">
                  <Show when={r().uploaded.length > 0}>
                    <p class="text-green-400">
                      Uploaded {r().uploaded.length} file{r().uploaded.length === 1 ? "" : "s"}.
                    </p>
                  </Show>
                  <For each={r().rejected}>
                    {(rej) => (
                      <p class="text-yellow-400 truncate">
                        {rej.original}: {rej.reason}
                      </p>
                    )}
                  </For>
                </div>
              )}
            </Show>

            <button
              type="button"
              onClick={doUpload}
              disabled={busy() || files().length === 0}
              class="w-full py-3 rounded-lg bg-teal-600 text-white text-sm font-medium hover:bg-teal-500 disabled:opacity-40 disabled:hover:bg-teal-600"
            >
              {busy() ? "Uploading…" : "Upload"}
            </button>
          </div>
        </div>
      </Show>
    </Show>
  );
}

function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(0)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}
