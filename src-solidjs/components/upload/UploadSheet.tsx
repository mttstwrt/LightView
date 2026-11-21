import { Show, For, createSignal, onMount, onCleanup } from "solid-js";
import { upload } from "../../lib/ipc";
import { isUploadableName } from "../../lib/mediaExts";

/** Upload photos and videos from this device into the gallery.
 *
 *  **Files are chosen two ways, and both pass through `choose`**: the picker,
 *  and a drop of files from another application anywhere on the window. A drop
 *  counts as picking — it opens the sheet with its files chosen, and Upload
 *  sends them — so a stray drop costs nothing. A drag that started in this
 *  window (the viewer's photo on its way out) is never taken as one. The
 *  command list's Upload entry is the other way the sheet opens.
 *
 *  **The sheet holds only files the server will take.** The route stops a
 *  request at the first name it refuses, keeping the files before it and losing
 *  the ones after, so `choose` applies the server's name rule up front and
 *  lists the rest as skipped — a dropped folder among them, since a browser
 *  cannot read one as a file. Landed batches leave the list as they land (see
 *  `upload`), so after a failure Upload re-sends only what did not arrive.
 *
 *  The drop zone lives here because this component is mounted exactly when
 *  uploading is available (`capabilities().upload`), so it needs no gate of
 *  its own. Its overlay sits outside the sheet's `Show`, because a drop is how
 *  a closed sheet opens. While an upload runs a drop is ignored; the shell's
 *  guard (`App`) keeps it from navigating the tab away.
 *
 *  No album field and no refresh callback. Where uploads land is server
 *  configuration (`upload_dir`) rather than a per-upload choice, and what lands
 *  reaches every client — this one included — through the watcher's
 *  `fs-changed`, which is the same path a file copied in over the share takes.
 *  The old client-side refetch existed because the web client got no watcher
 *  push; it gets one now. */
export function UploadSheet(props: { open: boolean; onOpen: () => void; onClose: () => void }) {
  const [files, setFiles] = createSignal<File[]>([]);
  const [skipped, setSkipped] = createSignal<string[]>([]);
  const [busy, setBusy] = createSignal(false);
  const [progress, setProgress] = createSignal(0);
  const [uploaded, setUploaded] = createSignal<string[] | null>(null);
  const [error, setError] = createSignal("");
  const [dropping, setDropping] = createSignal(false);

  let fileInput: HTMLInputElement | undefined;

  const close = () => {
    if (busy()) return;
    setFiles([]);
    setSkipped([]);
    setProgress(0);
    setUploaded(null);
    setError("");
    props.onClose();
  };

  /** Make `chosen` the selection, keeping the files the server will take and
   *  listing the rest — and every entry in `folders` — as skipped. */
  const choose = (chosen: File[], folders: ReadonlySet<File> = new Set()) => {
    const keep: File[] = [];
    const skip: string[] = [];
    for (const file of chosen) {
      if (!folders.has(file) && isUploadableName(file.name)) keep.push(file);
      else skip.push(file.name);
    }
    setFiles(keep);
    setSkipped(skip);
    setUploaded(null);
    setError("");
  };

  const onPick = (e: Event) => {
    const input = e.currentTarget as HTMLInputElement;
    choose(input.files ? Array.from(input.files) : []);
  };

  // A drag that carries files shows the overlay while it is over the window;
  // anything else — text, a link — passes untouched.
  //
  // Over the window is counted, not inferred from the overlay's own leave: a
  // drag that brushes the window's edge can leave before the overlay has been
  // entered, and its leave then lands on whatever was under it. Every element
  // a drag enters it also leaves, so the count is back to zero exactly when the
  // drag is outside — and once the overlay is up nothing under it is entered,
  // so no recycled grid cell can take a leave with it.
  //
  // **A drag that started in this window is never an upload.** The viewer's
  // photo leaves as an image drag carrying `Files`, so without this the overlay
  // would cover the viewer as the drag began, and releasing over the window
  // would stage the photo it came from. The note lasts one drag: it clears on
  // that drag's `dragend` — listened for on its source node, which still
  // receives it if the viewer swapped the photo out mid-drag — or on the next
  // `pointermove` here, since a page receives none while a drag is under way.
  // The second is what keeps a lost `dragend` from disabling drops for the
  // rest of the session. A cancelled `dragstart` starts no drag and sends no
  // `dragend`, so it leaves no note.
  onMount(() => {
    let depth = 0;
    let fromHere = false;
    const started = (e: DragEvent) => {
      if (e.defaultPrevented) return;
      fromHere = true;
      e.target?.addEventListener("dragend", () => { fromHere = false; }, { once: true });
    };
    const over = () => { fromHere = false; };
    const carriesFiles = (e: DragEvent) =>
      !fromHere && !!e.dataTransfer?.types.includes("Files");
    const enter = (e: DragEvent) => {
      if (!carriesFiles(e)) return;
      depth += 1;
      if (!busy()) setDropping(true);
    };
    const leave = (e: DragEvent) => {
      if (!carriesFiles(e)) return;
      depth = Math.max(0, depth - 1);
      if (depth === 0) setDropping(false);
    };
    // A drop ends the drag with no leave after it.
    const end = () => {
      depth = 0;
      setDropping(false);
    };
    window.addEventListener("dragstart", started);
    window.addEventListener("pointermove", over);
    window.addEventListener("dragenter", enter);
    window.addEventListener("dragleave", leave);
    window.addEventListener("drop", end);
    onCleanup(() => {
      window.removeEventListener("dragstart", started);
      window.removeEventListener("pointermove", over);
      window.removeEventListener("dragenter", enter);
      window.removeEventListener("dragleave", leave);
      window.removeEventListener("drop", end);
    });
  });

  /** Take a drop as a pick, then open the sheet on it.
   *
   *  Folders are told apart from files through `items` — `files` lists a
   *  folder as an empty file — and `items` can only be read during this
   *  event, so it is read here, synchronously. */
  const onDrop = (e: DragEvent) => {
    e.preventDefault();
    const items = e.dataTransfer?.items;
    if (!items) return;
    const dropped: File[] = [];
    const folders = new Set<File>();
    for (let i = 0; i < items.length; i++) {
      const file = items[i].kind === "file" ? items[i].getAsFile() : null;
      if (!file) continue;
      dropped.push(file);
      if (items[i].webkitGetAsEntry()?.isDirectory) folders.add(file);
    }
    if (dropped.length === 0) return;
    choose(dropped, folders);
    props.onOpen();
  };

  const doUpload = async () => {
    if (busy() || files().length === 0) return;
    setBusy(true);
    setError("");
    setUploaded(null);
    setProgress(0);
    try {
      setUploaded(await upload(files(), setProgress, (count) => setFiles((f) => f.slice(count))));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
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

            <Show when={skipped().length > 0}>
              <p data-upload-skipped class="text-amber-400/90 text-xs truncate" title={skipped().join("\n")}>
                Skipped {skipped().length} — not a photo or video: {skipped().join(", ")}
              </p>
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

            <Show when={uploaded()?.length}>
              {(count) => (
                <p class="text-green-400 text-xs">
                  Uploaded {count()} file{count() === 1 ? "" : "s"}.
                </p>
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

      {/* The overlay covers the window — the open viewer too — and has no
          hit-testable children, so a file drag is over it and nothing else.
          After the sheet, which shares its layer: a drop on an open sheet
          replaces its files, and would otherwise land on the sheet and be
          lost. */}
      <Show when={dropping()}>
        <div
          data-upload-drop
          class="fixed inset-0 z-[200] flex items-center justify-center p-6"
          style={{ background: "rgba(0,0,0,0.6)" }}
          onDragOver={(e) => {
            e.preventDefault();
            e.dataTransfer!.dropEffect = "copy";
          }}
          onDrop={onDrop}
        >
          <div class="pointer-events-none w-full h-full rounded-2xl border-2 border-dashed border-teal-500/70 flex items-center justify-center">
            <p class="text-white text-base font-semibold">Drop to upload</p>
          </div>
        </div>
      </Show>
    </>
  );
}

/** A byte count as "12 KB" / "3.4 MB". */
function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(0)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}
