import { createSignal, Show, For, onCleanup, onMount } from "solid-js";
import { listTrash, restoreTrash, purgeTrash, type TrashEntry } from "../lib/ipc";
import { ConfirmButton } from "./shared/ConfirmButton";

function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(0)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function formatDate(ts: number): string {
  const d = new Date(ts * 1000);
  return d.toLocaleDateString(undefined, {
    day: "numeric",
    month: "short",
    year: "numeric",
  });
}

/** Overlay listing app-trash entries with restore / permanent-delete actions.
 * No thumbnails: trashing drops the cached thumbnail rows, so entries render
 * as name + origin + date. Restores refresh the grid via the fs-changed
 * broadcast — nothing to update here beyond the local list. */
export function TrashPanel(props: { onClose: () => void }) {
  const [entries, setEntries] = createSignal<TrashEntry[]>([]);
  const [loading, setLoading] = createSignal(true);
  const [error, setError] = createSignal<string | null>(null);

  const load = async () => {
    setLoading(true);
    setError(null);
    try {
      setEntries(await listTrash());
    } catch (e) {
      setError(String(e));
    }
    setLoading(false);
  };
  onMount(load);

  const handleKey = (e: KeyboardEvent) => {
    if (e.key === "Escape") {
      e.stopPropagation();
      props.onClose();
    }
  };
  window.addEventListener("keydown", handleKey, true);
  onCleanup(() => window.removeEventListener("keydown", handleKey, true));

  const handleRestore = async (id: string) => {
    try {
      const result = await restoreTrash([id]);
      if (result.failed.length > 0) {
        setError(`Restore failed: ${result.failed[0].error}`);
        return;
      }
      setEntries((prev) => prev.filter((e) => e.id !== id));
    } catch (e) {
      setError(String(e));
    }
  };

  const handlePurge = async (id: string) => {
    try {
      await purgeTrash([id]);
      setEntries((prev) => prev.filter((e) => e.id !== id));
    } catch (e) {
      setError(String(e));
    }
  };

  const handleEmpty = async () => {
    try {
      await purgeTrash();
      setEntries([]);
    } catch (e) {
      setError(String(e));
    }
  };

  return (
    <div
      ref={(el) => {
        // Capture wheel events so the gallery grid underneath never scrolls
        const stop = (e: WheelEvent) => e.stopPropagation();
        el.addEventListener("wheel", stop, { passive: false });
        onCleanup(() => el.removeEventListener("wheel", stop));
      }}
      class="fixed inset-0 z-[200] flex flex-col"
      style={{ background: "rgba(10, 10, 10, 0.98)" }}
    >
      {/* Header */}
      <div class="flex items-center justify-between px-6 py-4 border-b border-neutral-800/60">
        <div class="flex items-center gap-4">
          <span class="text-sm font-medium text-neutral-200">Trash</span>
          <Show when={!loading()}>
            <span class="text-xs text-neutral-500">
              {entries().length} {entries().length === 1 ? "item" : "items"} · restored files return
              to their original folder
            </span>
          </Show>
        </div>
        <div class="flex items-center gap-3">
          <Show when={entries().length > 0}>
            <ConfirmButton label="Empty Trash" confirmLabel="Really delete all?" onConfirm={handleEmpty} />
          </Show>
          <button
            onClick={props.onClose}
            class="w-8 h-8 flex items-center justify-center text-neutral-400 hover:text-neutral-200 rounded transition-colors cursor-pointer"
          >
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
              <path d="M18 6L6 18M6 6l12 12" />
            </svg>
          </button>
        </div>
      </div>

      <Show when={error()}>
        <div class="px-6 py-2 text-xs text-red-400 border-b border-red-900/40 bg-red-950/20">
          {error()}
        </div>
      </Show>

      {/* Entries */}
      <div class="flex-1 overflow-y-auto px-6 py-4">
        <Show when={loading()}>
          <div class="flex items-center justify-center h-full text-neutral-500 text-sm">
            Loading...
          </div>
        </Show>
        <Show when={!loading() && entries().length === 0}>
          <div class="flex items-center justify-center h-full text-neutral-600 text-sm">
            Trash is empty
          </div>
        </Show>
        <Show when={!loading() && entries().length > 0}>
          <div class="flex flex-col gap-1 max-w-3xl mx-auto">
            <For each={entries()}>
              {(entry) => (
                <div
                  class="flex items-center gap-4 px-3 py-2 rounded-lg transition-colors hover:bg-neutral-900/60"
                  style={{ border: "1px solid rgba(255,255,255,0.04)" }}
                >
                  <div class="flex-1 min-w-0 flex flex-col">
                    <span class="text-xs text-neutral-200 truncate" title={entry.original_path}>
                      {entry.file_name}
                    </span>
                    <span class="text-[10px] text-neutral-500 truncate">
                      {entry.original_path}
                    </span>
                  </div>
                  <span class="text-[10px] text-neutral-500 whitespace-nowrap">
                    {formatSize(entry.size)}
                  </span>
                  <span class="text-[10px] text-neutral-500 whitespace-nowrap">
                    deleted {formatDate(entry.deleted_at)}
                  </span>
                  <button
                    onClick={() => handleRestore(entry.id)}
                    class="px-2.5 py-1 text-[10px] rounded cursor-pointer transition-colors bg-teal-700/50 text-teal-200 hover:bg-teal-600/60"
                  >
                    Restore
                  </button>
                  <ConfirmButton
                    label="Delete Forever"
                    confirmLabel="Confirm?"
                    onConfirm={() => handlePurge(entry.id)}
                  />
                </div>
              )}
            </For>
          </div>
        </Show>
      </div>
    </div>
  );
}
