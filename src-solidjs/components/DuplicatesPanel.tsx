import { createSignal, Show, For, onCleanup } from "solid-js";
import { findDuplicates, markNotDuplicates, thumbUrl, mediaUrl, trashFiles, type DuplicateGroup, type DuplicateItem } from "../lib/ipc";
import { setDisplayPaths, displayPaths } from "../stores/galleryStore";
import { setTotalCount } from "../stores/galleryStore";
import { capabilities } from "../stores/capabilitiesStore";
import { InfoPanel } from "./viewer/InfoPanel";
import { MergeDialog } from "./MergeDialog";

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

function formatDate(ts: number | null): string {
  if (ts == null) return "";
  const d = new Date(ts * 1000);
  return d.toLocaleDateString(undefined, { day: "numeric", month: "short", year: "numeric" });
}

export function DuplicatesPanel(props: { onClose: () => void }) {
  const [groups, setGroups] = createSignal<DuplicateGroup[]>([]);
  const [scanning, setScanning] = createSignal(false);
  const [scanned, setScanned] = createSignal(false);
  const [threshold, setThreshold] = createSignal(8);
  const [hashesComputed, setHashesComputed] = createSignal(0);

  // Merge dialog: which group index is being merged (null = closed)
  const [mergeGroup, setMergeGroup] = createSignal<number | null>(null);

  // Preview state: which group and which item index within it
  const [previewGroup, setPreviewGroup] = createSignal<number | null>(null);
  const [previewIndex, setPreviewIndex] = createSignal(0);
  const [infoOpen, setInfoOpen] = createSignal(false);

  const previewItem = (): DuplicateItem | null => {
    const gi = previewGroup();
    if (gi === null) return null;
    const g = groups()[gi];
    if (!g) return null;
    return g.items[previewIndex()] ?? null;
  };

  const openPreview = (groupIdx: number, itemIdx: number) => {
    setPreviewGroup(groupIdx);
    setPreviewIndex(itemIdx);
  };

  const closePreview = () => {
    setPreviewGroup(null);
    setPreviewIndex(0);
    setInfoOpen(false);
  };

  // Keyboard: arrows cycle within group, i toggles info, Escape closes preview or panel
  const handleKey = (e: KeyboardEvent) => {
    const typingInInput =
      e.target instanceof HTMLInputElement || e.target instanceof HTMLTextAreaElement;
    if (previewGroup() !== null) {
      const g = groups()[previewGroup()!];
      if (!g) return;
      if (e.key === "ArrowRight") {
        e.preventDefault();
        setPreviewIndex((i) => (i + 1) % g.items.length);
      } else if (e.key === "ArrowLeft") {
        e.preventDefault();
        setPreviewIndex((i) => (i - 1 + g.items.length) % g.items.length);
      } else if ((e.key === "i" || e.key === "I") && !typingInInput) {
        e.preventDefault();
        setInfoOpen((v) => !v);
      } else if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        closePreview();
      }
    } else if (e.key === "Escape") {
      e.stopPropagation();
      props.onClose();
    }
  };
  window.addEventListener("keydown", handleKey, true);
  onCleanup(() => window.removeEventListener("keydown", handleKey, true));

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

  const handleNotDuplicates = async (groupIdx: number) => {
    const group = groups()[groupIdx];
    if (!group) return;
    const paths = group.items.map((it) => it.path);
    try {
      await markNotDuplicates(paths);
      setGroups((prev) => prev.filter((_, i) => i !== groupIdx));
      if (previewGroup() === groupIdx) closePreview();
    } catch (e) {
      console.error("Mark not duplicates failed:", e);
    }
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

  // After a merge: the keeper survives, the discarded copies are gone. Remove
  // discarded paths from the group (dissolving it if fewer than 2 remain) and
  // from the gallery display.
  const handleMerged = (groupIdx: number, discarded: string[]) => {
    const removedSet = new Set(discarded);
    setGroups((prev) => {
      const updated = [...prev];
      const group = updated[groupIdx];
      if (!group) return prev;
      const newItems = group.items.filter((it) => !removedSet.has(it.path));
      if (newItems.length < 2) {
        updated.splice(groupIdx, 1);
      } else {
        updated[groupIdx] = { ...group, items: newItems };
      }
      return updated;
    });
    setDisplayPaths(displayPaths().filter((p) => !removedSet.has(p)));
    setTotalCount((c) => Math.max(0, c - discarded.length));
    setMergeGroup(null);
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
                  <div class="flex items-center justify-between">
                    <span class="text-[11px] uppercase tracking-wider text-neutral-500 font-medium">
                      Group {groupIdx() + 1} — {group.items.length} images
                    </span>
                    <div class="flex items-center gap-1.5">
                      <Show when={capabilities().delete}>
                        <button
                          onClick={() => setMergeGroup(groupIdx())}
                          class="px-2 py-0.5 text-[10px] rounded cursor-pointer transition-colors bg-teal-800/50 text-teal-300 hover:bg-teal-700/60 hover:text-teal-200"
                          title="Merge — keep one file, fold selected metadata from the others into it, then trash the rest"
                        >
                          Merge
                        </button>
                      </Show>
                      <button
                        onClick={() => handleNotDuplicates(groupIdx())}
                        class="px-2 py-0.5 text-[10px] rounded cursor-pointer transition-colors bg-neutral-800 text-neutral-400 hover:bg-neutral-700 hover:text-neutral-200"
                        title="Mark these images as not duplicates — they won't be grouped together in future scans"
                      >
                        Not duplicates
                      </button>
                    </div>
                  </div>
                  <div class="flex gap-2 flex-wrap">
                    <For each={group.items}>
                      {(item, itemIdx) => (
                        <DuplicateCard
                          item={item}
                          onTrash={() => handleTrash(item.path, groupIdx())}
                          onClick={() => openPreview(groupIdx(), itemIdx())}
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

      {/* Merge dialog */}
      <Show when={mergeGroup() !== null && groups()[mergeGroup()!]}>
        {(group) => (
          <MergeDialog
            paths={group().items.map((it) => it.path)}
            bestPath={group().items.find((it) => it.is_best)?.path ?? null}
            onCancel={() => setMergeGroup(null)}
            onMerged={(_keeper, discarded) => handleMerged(mergeGroup()!, discarded)}
          />
        )}
      </Show>

      {/* Full-res preview overlay */}
      <Show when={previewItem()}>
        {(item) => {
          const g = () => groups()[previewGroup()!];
          return (
            <div
              class="fixed inset-0 z-[210] flex flex-col items-center justify-center"
              style={{ background: "rgba(0, 0, 0, 0.95)" }}
              onClick={closePreview}
              ref={(el) => {
                const stopWheel = (e: WheelEvent) => { e.preventDefault(); e.stopPropagation(); };
                el.addEventListener("wheel", stopWheel, { passive: false });
                onCleanup(() => el.removeEventListener("wheel", stopWheel));
              }}
            >
              {/* Image */}
              <img
                src={mediaUrl(item().path)}
                class="max-w-[90vw] max-h-[80vh] object-contain select-none"
                draggable={false}
                onClick={(e) => e.stopPropagation()}
              />

              {/* Bottom info bar */}
              <div class="absolute bottom-0 left-0 right-0 flex items-center justify-center gap-6 px-6 py-3" style={{ background: "rgba(0,0,0,0.7)" }}>
                <span class="text-xs text-neutral-300 truncate max-w-[40vw]" title={item().path}>
                  {item().path.split("/").pop()}
                </span>
                <span class="text-xs text-neutral-500">
                  {formatRes(item().width, item().height)}
                </span>
                <span class="text-xs text-neutral-500">
                  {formatSize(item().file_size)}
                </span>
                <Show when={item().date_taken}>
                  <span class="text-xs text-neutral-500">
                    {formatDate(item().date_taken)}
                  </span>
                </Show>
                <Show when={item().is_best}>
                  <span class="px-1.5 py-0.5 rounded text-[10px] font-medium bg-green-600/80 text-green-100">Best</span>
                </Show>
                <span class="text-xs text-neutral-600">
                  {previewIndex() + 1} / {g()?.items.length ?? 0}
                </span>
                <span class="text-[10px] text-neutral-600">
                  Left/Right to compare · i for info
                </span>
                <button
                  class="text-xs text-neutral-300 hover:text-neutral-100 cursor-pointer transition-colors"
                  title="Toggle info panel (i)"
                  onClick={(e) => { e.stopPropagation(); setInfoOpen((v) => !v); }}
                  style={{ opacity: infoOpen() ? "1" : "0.6" }}
                >
                  <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                    <circle cx="12" cy="12" r="10" />
                    <line x1="12" y1="16" x2="12" y2="12" />
                    <line x1="12" y1="8" x2="12.01" y2="8" />
                  </svg>
                </button>
              </div>

              {/* Info panel */}
              <Show when={infoOpen()}>
                <InfoPanel path={item().path} filename={item().path.split("/").pop() ?? ""} />
              </Show>

              {/* Nav arrows */}
              <button
                class="absolute left-0 top-0 h-full w-16 flex items-center justify-center opacity-0 hover:opacity-100 transition-opacity cursor-pointer"
                onClick={(e) => { e.stopPropagation(); setPreviewIndex((i) => (i - 1 + (g()?.items.length ?? 1)) % (g()?.items.length ?? 1)); }}
              >
                <svg width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2"><path d="M15 18l-6-6 6-6" /></svg>
              </button>
              <button
                class="absolute right-0 top-0 h-full w-16 flex items-center justify-center opacity-0 hover:opacity-100 transition-opacity cursor-pointer"
                onClick={(e) => { e.stopPropagation(); setPreviewIndex((i) => (i + 1) % (g()?.items.length ?? 1)); }}
              >
                <svg width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2"><path d="M9 18l6-6-6-6" /></svg>
              </button>

              {/* Close button */}
              <button
                class="absolute top-4 right-4 w-8 h-8 flex items-center justify-center text-neutral-400 hover:text-neutral-200 rounded transition-colors cursor-pointer"
                onClick={(e) => { e.stopPropagation(); closePreview(); }}
              >
                <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                  <path d="M18 6L6 18M6 6l12 12" />
                </svg>
              </button>
            </div>
          );
        }}
      </Show>
    </div>
  );
}

function DuplicateCard(props: { item: DuplicateItem; onTrash: () => void; onClick: () => void }) {
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

      {/* Thumbnail — click to preview */}
      <div
        class="w-full h-[120px] bg-neutral-900 flex items-center justify-center overflow-hidden cursor-pointer hover:brightness-110"
        onClick={(e) => { e.stopPropagation(); props.onClick(); }}
      >
        <img
          src={thumbUrl(props.item.path)}
          alt={fileName()}
          class="max-w-full max-h-full object-contain pointer-events-none"
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
        <Show when={props.item.date_taken}>
          <span class="text-[10px] text-neutral-600">
            {formatDate(props.item.date_taken)}
          </span>
        </Show>

        {/* Trash button — muted for the "best" copy since it's the recommended
            keep. Hidden when this client lacks the delete capability (web with
            remote delete switched off) — resolving is then mark-only. */}
        <Show when={capabilities().delete}>
          <button
            onClick={(e) => { e.stopPropagation(); props.onTrash(); }}
            class={`mt-1 px-2 py-1 text-[10px] rounded cursor-pointer transition-colors ${
              props.item.is_best
                ? "bg-neutral-800/40 text-neutral-500 hover:bg-red-900/30 hover:text-red-400"
                : "bg-red-900/30 text-red-400 hover:bg-red-800/40 hover:text-red-300"
            }`}
            title={props.item.is_best ? "Trash (marked as best — usually the one to keep)" : "Trash"}
          >
            Trash
          </button>
        </Show>
      </div>
    </div>
  );
}
