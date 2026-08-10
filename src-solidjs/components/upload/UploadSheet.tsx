import { Show, For, createSignal } from "solid-js";
import { uploadFiles, type UploadResult } from "../../lib/ipc";
import { uploadConfig } from "../../stores/uploadStore";

/** Upload photos/videos from this device into the host gallery, which files
 *  them into subfolders by capture date.
 *
 *  Sheet only — no trigger of its own. Uploading is one entry in the command
 *  list (docs/frontend/chrome.md); the floating button this used to carry is
 *  now the command list's, which is why the three hide conditions that button
 *  needed are gone.
 *
 *  `onUploaded` is called after a successful upload so the gallery can refresh
 *  to show the new items (the web client gets no fs-watch push from the host). */
export function UploadSheet(props: { open: boolean; onClose: () => void; onUploaded?: () => void }) {
  const [files, setFiles] = createSignal<File[]>([]);
  const [album, setAlbum] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [progress, setProgress] = createSignal(0);
  const [result, setResult] = createSignal<UploadResult | null>(null);
  const [error, setError] = createSignal("");

  let fileInput: HTMLInputElement | undefined;

  const showAlbum = () => uploadConfig()?.scheme === "year_album";

  const close = () => {
    if (busy()) return;
    setFiles([]);
    setAlbum("");
    setProgress(0);
    setResult(null);
    setError("");
    props.onClose();
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
    <Show when={props.open}>
      <div
        class="fixed inset-0 z-[200] flex items-end sm:items-center justify-center"
        style={{ background: "rgba(0,0,0,0.6)" }}
        onClick={close}
      >
        <div
          class="w-full sm:max-w-md rounded-t-2xl sm:rounded-2xl border border-white/10 px-5 pt-5 flex flex-col gap-4"
          style={{
            background: "rgb(18,18,18)",
            "max-height": "85vh",
            // Clear the home indicator on a bottom-anchored phone sheet.
            "padding-bottom": "calc(env(safe-area-inset-bottom, 0px) + 1.25rem)",
          }}
          onClick={(e) => e.stopPropagation()}
        >
          <div class="flex items-center justify-between">
            <h2 class="text-white text-base font-semibold">Upload photos</h2>
            <button
              type="button"
              onClick={close}
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
  );
}

function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(0)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}
