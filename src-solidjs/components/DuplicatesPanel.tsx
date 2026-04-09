import { createSignal, Show, For, onCleanup } from "solid-js";
import { findDuplicates, thumbUrl, trashFiles, type DuplicateGroup, type DuplicateItem } from "../lib/ipc";
import { setDisplayPaths, displayPaths } from "../stores/galleryStore";
import { setTotalCount } from "../stores/galleryStore";

const THRESHOLD_PRESETS = [
  { label: "Exact", value: 0, desc: "Identical perceptual hash" },
  { label: "Strict", value: 3, desc: "Nearly identical" },
  { label: "Normal", value: 8, desc: "Resized / recompressed" },
  { label: "Loose", value: 12, desc: "Similar composition" },
] as const;

function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(0)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function formatRes(w: number | null, h: number | null): string {
  if (w == null || h == null) return "Unknown";
  return `${w}\u00D7${h}`;
}

export function DuplicatesPanel(props: { onClose: () => void }) {
  const [groups, setGroups] = createSignal<DuplicateGroup[]>([]);
  const [scanning, setScanning] = createSignal(false);
  const [scanned, setScanned] = createSignal(false);
  const [threshold, setThreshold] = createSignal(8);
  const [hashesComputed, setHashesComputed] = createSignal(0);

  const scan = async () => {
    setScanning(true);
    setScanned(false);
    try {
      const result = await findDuplicates(threshold());
      setGroups(result.groups);
      setHashesComputed(result.hashes_computed);
      setScanned(true);
    } catch (e) {
      console.error("Duplicate scan failed:", e);
    }
    setScanning(false);
  };

  const handleTrash = async (path: string, groupIdx: number) => {
    try {
      await trashFiles([path]);
      // Remove from this group
      setGroups((prev) => {
        const updated = [...prev];
        const group = updated[groupIdx];
        const newItems = group.items.filter((it) => it.path !== path);
        if (newItems.length < 2) {
          // Group dissolved
          updated.splice(groupIdx, 1);
        } else {
          // Recompute best if the best was trashed
          const hadBest = newItems.some((it) => it.is_best);
          if (!hadBest) {
            let bestIdx = 0;
            let bestRes = 0;
            for (let i = 0; i < newItems.length; i++) {
              const res = (newItems[i].width ?? 0) * (newItems[i].height ?? 0);
              if (res > bestRes || (res === bestRes && newItems[i].file_size < newItems[bestIdx].file_size)) {
                bestRes = res;
                bestIdx = i;
              }
            }
            newItems[bestIdx] = { ...newItems[bestIdx], is_best: true };
          }
          updated[groupIdx] = { ...group, items: newItems };
        }
        return updated;
      });
      // Remove from gallery display
      const removedSet = new Set([path]);
      setDisplayPaths(displayPaths().filter((p) => !removedSet.has(p)));
      setTotalCount((c) => Math.max(0, c - 1));
    } catch (e) {
      console.error("Trash failed:", e);
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
          <span class="text-sm font-medium text-neutral-200">Duplicate Detection</span>
          <Show when={scanned()}>
            <span class="text-xs text-neutral-500">
              {groups().length} {groups().length === 1 ? "group" : "groups"} found
              {hashesComputed() > 0 && ` (${hashesComputed()} new hashes computed)`}
            </span>
          </Show>
        </div>
        <button
          onClick={props.onClose}
          class="w-8 h-8 flex items-center justify-center text-neutral-400 hover:text-neutral-200 rounded transition-colors cursor-pointer"
        >
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <path d="M18 6L6 18M6 6l12 12" />
          </svg>
        </button>
      </div>

      {/* Controls */}
      <div class="flex items-center gap-4 px-6 py-3 border-b border-neutral-800/40">
        <span class="text-xs text-neutral-400">Sensitivity</span>
        <div class="flex gap-1">
          <For each={THRESHOLD_PRESETS as unknown as typeof THRESHOLD_PRESETS[number][]}>
            {(p) => (
              <button
                class={`px-2 py-0.5 text-xs rounded cursor-pointer transition-colors ${
                  threshold() === p.value
                    ? "bg-teal-700/60 text-teal-200"
                    : "bg-neutral-800 text-neutral-400 hover:bg-neutral-700 hover:text-neutral-300"
                }`}
                onClick={() => setThreshold(p.value)}
                title={p.desc}
              >
                {p.label}
              </button>
            )}
          </For>
        </div>
        <button
          onClick={scan}
          disabled={scanning()}
          class="px-4 py-1.5 text-xs rounded cursor-pointer transition-colors bg-teal-700/60 text-teal-200 hover:bg-teal-600/60 disabled:opacity-50 disabled:cursor-not-allowed"
        >
          {scanning() ? "Scanning..." : "Scan"}
        </button>
      </div>

      {/* Results */}
      <div class="dupes-scroll flex-1 overflow-y-auto px-6 py-4">
        <Show when={!scanned() && !scanning()}>
          <div class="flex items-center justify-center h-full text-neutral-600 text-sm">
            Choose sensitivity and click Scan to find duplicates
          </div>
        </Show>
        <Show when={scanning()}>
          <div class="flex items-center justify-center h-full gap-3">
            <div class="w-4 h-4 border-2 border-teal-400 border-t-transparent rounded-full animate-spin" />
            <span class="text-sm text-neutral-400">Computing hashes and finding duplicates...</span>
          </div>
        </Show>
        <Show when={scanned() && groups().length === 0}>
          <div class="flex items-center justify-center h-full text-neutral-500 text-sm">
            No duplicates found at this sensitivity level
          </div>
        </Show>
        <Show when={scanned() && groups().length > 0}>
          <div
            class="grid gap-4"
            style={{ "grid-template-columns": "repeat(auto-fill, minmax(380px, 1fr))" }}
          >
            <For each={groups()}>
              {(group, groupIdx) => (
                <div
                  class="flex flex-col gap-2 rounded-lg p-3"
                  style={{ background: "rgba(255, 255, 255, 0.02)", border: "1px solid rgba(255,255,255,0.04)" }}
                >
                  <span class="text-[11px] uppercase tracking-wider text-neutral-500 font-medium">
                    Group {groupIdx() + 1} — {group.items.length} images
                  </span>
                  <div class="flex gap-2 flex-wrap">
                    <For each={group.items}>
                      {(item) => (
                        <DuplicateCard
                          item={item}
                          onTrash={() => handleTrash(item.path, groupIdx())}
                        />
                      )}
                    </For>
                  </div>
                </div>
              )}
            </For>
          </div>
        </Show>
      </div>
    </div>
  );
}

function DuplicateCard(props: { item: DuplicateItem; onTrash: () => void }) {
  const fileName = () => {
    const parts = props.item.path.split("/");
    return parts[parts.length - 1] || props.item.path;
  };

  return (
    <div
      class="relative flex flex-col rounded-lg overflow-hidden transition-all"
      style={{
        width: "160px",
        border: props.item.is_best
          ? "2px solid rgba(34, 197, 94, 0.7)"
          : "2px solid rgba(255, 255, 255, 0.06)",
        background: "rgba(30, 30, 30, 0.6)",
      }}
    >
      {/* Best badge */}
      <Show when={props.item.is_best}>
        <div class="absolute top-1.5 left-1.5 z-10 px-1.5 py-0.5 rounded text-[10px] font-medium bg-green-600/80 text-green-100">
          Best
        </div>
      </Show>

      {/* Thumbnail */}
      <div class="w-full h-[120px] bg-neutral-900 flex items-center justify-center overflow-hidden">
        <img
          src={thumbUrl(props.item.path)}
          alt={fileName()}
          class="max-w-full max-h-full object-contain"
          loading="lazy"
        />
      </div>

      {/* Info */}
      <div class="px-2.5 py-2 flex flex-col gap-1">
        <span class="text-[11px] text-neutral-300 truncate" title={props.item.path}>
          {fileName()}
        </span>
        <div class="flex items-center justify-between">
          <span class="text-[10px] text-neutral-500">
            {formatRes(props.item.width, props.item.height)}
          </span>
          <span class="text-[10px] text-neutral-500">
            {formatSize(props.item.file_size)}
          </span>
        </div>

        {/* Trash button — don't show for best */}
        <Show when={!props.item.is_best}>
          <button
            onClick={(e) => { e.stopPropagation(); props.onTrash(); }}
            class="mt-1 px-2 py-1 text-[10px] rounded cursor-pointer transition-colors bg-red-900/30 text-red-400 hover:bg-red-800/40 hover:text-red-300"
          >
            Trash
          </button>
        </Show>
      </div>
    </div>
  );
}
