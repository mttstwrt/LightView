import { Show, createSignal, onCleanup } from "solid-js";
import { open } from "@tauri-apps/plugin-dialog";
import { listen } from "@tauri-apps/api/event";
import { galleryPath, setGalleryPath, setTotalCount, setLoading, displayPaths, setDisplayPaths, sortedItems, setSortedItems, loading, selectedPaths, setSelectedPaths, toggleSelection, clearSelection, selectAll, totalCount } from "./stores/galleryStore";
import { viewerOpen, closeViewer, openViewer, nextImage, prevImage, viewerIndex, toggleInfoPanel } from "./stores/viewerStore";
import { settings, sortField, sortOrder, subSortField, subSortOrder, groupBy, loadSettingsFromGallery } from "./stores/settingsStore";
import { openGallery, getSortedItems, getRecentGalleries, removeRecentGallery, setRating, type RecentGallery } from "./lib/ipc";
import { GalleryGrid } from "./components/gallery/GalleryGrid";
import { MediaViewer } from "./components/viewer/MediaViewer";
import { TopBar } from "./components/topbar/TopBar";
import { ContextMenu, type ContextMenuState } from "./components/shared/ContextMenu";
import { SelectionBar } from "./components/gallery/SelectionBar";
import { pluginActivity, type PluginActivity } from "./stores/pluginStore";
import { cancelPluginBatch } from "./lib/ipc";
import { thumbGenActivity } from "./stores/thumbnailProgressStore";
import { ScrollBar, type ScrollIndicator } from "./components/shared/ScrollBar";
import { DebugOverlay } from "./components/debug/DebugOverlay";
import { DuplicatesPanel } from "./components/DuplicatesPanel";
import type { SortedItem, SortField } from "./lib/types";

// ---------------------------------------------------------------------------
// Scrollbar indicator helpers
// ---------------------------------------------------------------------------

function buildScrollIndicators(items: SortedItem[], field: SortField): ScrollIndicator[] {
  if (items.length === 0) return [];

  switch (field) {
    case "date":
      return buildDateIndicators(items);
    case "name":
      return buildNameIndicators(items);
    case "size":
      return buildSizeIndicators(items);
    case "rating":
      return buildRatingIndicators(items);
    default:
      return [];
  }
}

function buildDateIndicators(items: SortedItem[]): ScrollIndicator[] {
  const indicators: ScrollIndicator[] = [];
  let lastLabel = "";
  for (let i = 0; i < items.length; i++) {
    const ts = items[i].date_taken;
    if (!ts) continue;
    const d = new Date(ts * 1000);
    const label = d.toLocaleDateString(undefined, { month: "short", year: "numeric" });
    if (label !== lastLabel) {
      indicators.push({ position: i / items.length, label });
      lastLabel = label;
    }
  }
  return dedupeIndicators(indicators);
}

function buildNameIndicators(items: SortedItem[]): ScrollIndicator[] {
  const indicators: ScrollIndicator[] = [];
  let lastChar = "";
  for (let i = 0; i < items.length; i++) {
    const name = items[i].path.split("/").pop() ?? "";
    const ch = name.charAt(0).toUpperCase();
    if (ch && ch !== lastChar) {
      indicators.push({ position: i / items.length, label: ch });
      lastChar = ch;
    }
  }
  return dedupeIndicators(indicators);
}

function formatSizeShort(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(0)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  return `${(bytes / (1024 * 1024 * 1024)).toFixed(1)} GB`;
}

function buildSizeIndicators(items: SortedItem[]): ScrollIndicator[] {
  // Place indicators at size order-of-magnitude boundaries
  const thresholds = [
    100 * 1024,        // 100 KB
    500 * 1024,        // 500 KB
    1024 * 1024,       // 1 MB
    5 * 1024 * 1024,   // 5 MB
    10 * 1024 * 1024,  // 10 MB
    50 * 1024 * 1024,  // 50 MB
    100 * 1024 * 1024, // 100 MB
  ];
  const indicators: ScrollIndicator[] = [];
  let tIdx = 0;
  // Determine sort direction from first vs last
  const ascending = items.length > 1 && items[0].file_size <= items[items.length - 1].file_size;

  for (let i = 0; i < items.length && tIdx < thresholds.length; i++) {
    const size = items[i].file_size;
    const threshold = thresholds[ascending ? tIdx : thresholds.length - 1 - tIdx];
    const crossed = ascending ? size >= threshold : size <= threshold;
    if (crossed) {
      indicators.push({ position: i / items.length, label: formatSizeShort(threshold) });
      tIdx++;
    }
  }
  return dedupeIndicators(indicators);
}

function buildRatingIndicators(items: SortedItem[]): ScrollIndicator[] {
  const indicators: ScrollIndicator[] = [];
  let lastRating = -1;
  for (let i = 0; i < items.length; i++) {
    const r = items[i].rating ?? 0;
    if (r !== lastRating) {
      indicators.push({ position: i / items.length, label: r === 0 ? "Unrated" : "\u2605".repeat(r) });
      lastRating = r;
    }
  }
  return dedupeIndicators(indicators);
}

/** Thin out indicators so they don't overlap — keep at most ~15, evenly spaced. */
function dedupeIndicators(indicators: ScrollIndicator[]): ScrollIndicator[] {
  if (indicators.length <= 15) return indicators;
  const step = Math.ceil(indicators.length / 15);
  const result: ScrollIndicator[] = [];
  for (let i = 0; i < indicators.length; i += step) {
    result.push(indicators[i]);
  }
  return result;
}

function getThumbLabelForItems(items: SortedItem[], field: SortField, fraction: number): string {
  if (items.length === 0) return "";
  const idx = Math.min(Math.floor(fraction * items.length), items.length - 1);
  const item = items[idx];

  switch (field) {
    case "date": {
      const ts = item.date_taken;
      if (!ts) return "No date";
      const d = new Date(ts * 1000);
      return d.toLocaleDateString(undefined, { day: "numeric", month: "short", year: "numeric" });
    }
    case "name": {
      const name = item.path.split("/").pop() ?? "";
      return name.length > 20 ? name.slice(0, 20) + "\u2026" : name;
    }
    case "size":
      return formatSizeShort(item.file_size);
    case "rating": {
      const r = item.rating ?? 0;
      return r === 0 ? "Unrated" : "\u2605".repeat(r);
    }
    default:
      return "";
  }
}

export function App() {
  const [debugOpen, setDebugOpen] = createSignal(false);
  const [duplicatesOpen, setDuplicatesOpen] = createSignal(false);
  const [contextMenu, setContextMenu] = createSignal<ContextMenuState | null>(null);
  const [galleryContentHeight, setGalleryContentHeight] = createSignal(0);

  // Scrollbar sort indicators
  const scrollIndicators = () => buildScrollIndicators(sortedItems(), sortField());
  const thumbLabel = (fraction: number) => getThumbLabelForItems(sortedItems(), sortField(), fraction);

  const openPath = async (path: string) => {
    setLoading(true);
    try {
      const result = await openGallery(path);
      setGalleryPath(result.path);
      setTotalCount(result.total_media);

      // Restore per-gallery settings from .lightview folder
      await loadSettingsFromGallery();

      const sorted = await getSortedItems(sortField(), sortOrder(), groupBy(), undefined, subSortField(), subSortOrder());
      setSortedItems(sorted.items);
      setDisplayPaths(sorted.items.map((item) => item.path));
    } catch (e) {
      console.error("Failed to open gallery:", e);
    } finally {
      setLoading(false);
    }
  };

  // Listen for directory passed via CLI argument
  const unlisten = listen<string>("open-directory", (event) => {
    openPath(event.payload);
  });
  onCleanup(() => { unlisten.then((fn) => fn()); });

  // Listen for external filesystem changes (files added/removed outside the app)
  const unlistenFs = listen<{ added: string[]; removed: string[] }>("gallery:fs-changed", async (event) => {
    const { added, removed } = event.payload;

    if (removed.length > 0) {
      const removedSet = new Set(removed);
      setDisplayPaths(displayPaths().filter((p) => !removedSet.has(p)));
      setSortedItems(sortedItems().filter((item) => !removedSet.has(item.path)));
      setSelectedPaths((prev) => {
        const next = new Set(prev);
        for (const p of removed) next.delete(p);
        return next.size !== prev.size ? next : prev;
      });
    }

    if (added.length > 0) {
      // Re-fetch sorted items to get correct insertion order
      try {
        const sorted = await getSortedItems(sortField(), sortOrder(), groupBy(), undefined, subSortField(), subSortOrder());
        setSortedItems(sorted.items);
        setDisplayPaths(sorted.items.map((item) => item.path));
      } catch (e) {
        console.error("Failed to refresh after file addition:", e);
      }
    }

    setTotalCount((c) => c + added.length - removed.length);
  });
  onCleanup(() => { unlistenFs.then((fn) => fn()); });

  const handleOpenFolder = async () => {
    try {
      const selected = await open({ directory: true, multiple: false });
      if (selected) {
        await openPath(selected as string);
      }
    } catch (e) {
      console.error("Dialog failed:", e);
    }
  };

  const handleKeyDown = (e: KeyboardEvent) => {
    if (viewerOpen()) {
      if (e.key === "Escape") {
        closeViewer();
      } else if (e.key === "ArrowRight") {
        nextImage(displayPaths().length);
      } else if (e.key === "ArrowLeft") {
        prevImage();
      } else if (e.key === "i" || e.key === "I") {
        toggleInfoPanel();
      } else if (e.key >= "0" && e.key <= "5" && !e.ctrlKey && !e.metaKey && !e.altKey) {
        const active = document.activeElement;
        if (active instanceof HTMLInputElement || active instanceof HTMLTextAreaElement) return;
        e.preventDefault();
        const paths = displayPaths();
        const idx = viewerIndex();
        if (idx >= 0 && idx < paths.length) {
          const newRating = Number(e.key);
          setRating(paths[idx], newRating).catch(() => {});
          window.dispatchEvent(new CustomEvent("lightview:rating-changed", { detail: { path: paths[idx], rating: newRating } }));
        }
      }
    } else {
      if (e.key === "Escape") {
        clearSelection();
      }
      if ((e.ctrlKey || e.metaKey) && e.key === "a" && galleryPath()) {
        e.preventDefault();
        selectAll(displayPaths());
      }
    }
    if (e.key === "F12") {
      setDebugOpen((prev) => !prev);
    }
  };

  window.addEventListener("keydown", handleKeyDown);
  onCleanup(() => window.removeEventListener("keydown", handleKeyDown));

  return (
    <div
      class="min-h-screen w-screen relative"
      style={{ background: settings().display.background_color }}
    >
      <Show
        when={galleryPath()}
        fallback={<WelcomeScreen onOpen={handleOpenFolder} onOpenPath={openPath} />}
      >
        <TopBar onOpenFolder={handleOpenFolder} onOpenDuplicates={() => setDuplicatesOpen(true)} />
        <GalleryGrid
          paths={displayPaths()}
          onItemClick={(index) => {
            clearSelection();
            openViewer(index);
          }}
          onItemSelect={(path) => toggleSelection(path)}
          onDragSelect={(paths) => setSelectedPaths(new Set(paths))}
          onBackgroundClick={clearSelection}
          selectedPaths={selectedPaths()}
          onItemContextMenu={(e, path, index) => {
            setContextMenu({ x: e.clientX, y: e.clientY, path, index });
          }}
          loading={loading()}
          onContentHeight={setGalleryContentHeight}
        />
        <ScrollBar
          contentHeight={galleryContentHeight()}
          indicators={scrollIndicators()}
          getThumbLabel={thumbLabel}
        />
        <Show when={selectedPaths().size > 0}>
          <SelectionBar
            selectedPaths={selectedPaths()}
            onClear={clearSelection}
          />
        </Show>
        <ContextMenu
          state={contextMenu()}
          onClose={() => setContextMenu(null)}
          paths={displayPaths()}
          selectedPaths={selectedPaths()}
          onFilesRemoved={(removed) => {
            const removedSet = new Set(removed);
            setDisplayPaths(displayPaths().filter((p) => !removedSet.has(p)));
            clearSelection();
            setTotalCount((c) => Math.max(0, c - removed.length));
          }}
        />
        <Show when={viewerOpen()}>
          <MediaViewer
            paths={displayPaths()}
            currentIndex={viewerIndex()}
            onClose={closeViewer}
            onNext={() => nextImage(displayPaths().length)}
            onPrev={prevImage}
          />
        </Show>
        <Show when={duplicatesOpen()}>
          <DuplicatesPanel onClose={() => setDuplicatesOpen(false)} />
        </Show>
      </Show>
      <Show when={debugOpen()}>
        <DebugOverlay />
      </Show>
      <Show when={pluginActivity()}>
        <PluginToast />
      </Show>
      <Show when={thumbGenActivity()}>
        <ThumbnailToast />
      </Show>
    </div>
  );
}

function PluginToast() {
  const activity = () => pluginActivity()!;
  const statusColor = () => {
    switch (activity().status) {
      case "running": return "text-teal-400";
      case "done": return "text-green-400";
      case "error": return "text-red-400";
      case "cancelled": return "text-yellow-400";
    }
  };
  const borderColor = () => {
    switch (activity().status) {
      case "running": return "border-teal-500/30";
      case "done": return "border-green-500/30";
      case "error": return "border-red-500/30";
      case "cancelled": return "border-yellow-500/30";
    }
  };
  const progress = () => {
    const a = activity();
    if (a.total === 0) return 0;
    return Math.round((a.completed / a.total) * 100);
  };

  return (
    <div
      class={`fixed bottom-4 right-4 z-[150] flex flex-col gap-2 px-4 py-2.5 rounded-lg border ${borderColor()}`}
      style={{
        background: "rgba(18, 18, 18, 0.95)",
        "backdrop-filter": "blur(12px)",
        "min-width": "240px",
      }}
    >
      <div class="flex items-center gap-3">
        <Show when={activity().status === "running"}>
          <div class="w-3.5 h-3.5 shrink-0 border-2 border-teal-400 border-t-transparent rounded-full animate-spin" />
        </Show>
        <Show when={activity().status === "done"}>
          <svg class="w-3.5 h-3.5 shrink-0 text-green-400" fill="none" stroke="currentColor" viewBox="0 0 24 24">
            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2.5" d="M5 13l4 4L19 7" />
          </svg>
        </Show>
        <Show when={activity().status === "error"}>
          <svg class="w-3.5 h-3.5 shrink-0 text-red-400" fill="none" stroke="currentColor" viewBox="0 0 24 24">
            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2.5" d="M6 18L18 6M6 6l12 12" />
          </svg>
        </Show>
        <Show when={activity().status === "cancelled"}>
          <svg class="w-3.5 h-3.5 shrink-0 text-yellow-400" fill="none" stroke="currentColor" viewBox="0 0 24 24">
            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2.5" d="M6 18L18 6M6 6l12 12" />
          </svg>
        </Show>
        <div class="flex flex-col flex-1 min-w-0">
          <span class={`text-xs font-medium ${statusColor()}`}>{activity().displayName}</span>
          <span class="text-[11px] text-neutral-400">{activity().message}</span>
        </div>
        <Show when={activity().status === "running"}>
          <button
            onClick={() => cancelPluginBatch()}
            class="shrink-0 px-2 py-0.5 text-[10px] rounded cursor-pointer transition-colors bg-neutral-700 text-neutral-400 hover:bg-neutral-600 hover:text-neutral-200"
            title="Cancel"
          >
            Cancel
          </button>
        </Show>
      </div>
      <Show when={activity().status === "running" && activity().total > 0}>
        <div class="w-full h-1.5 bg-neutral-800 rounded-full overflow-hidden">
          <div
            class="h-full bg-teal-500 rounded-full transition-all duration-300"
            style={{ width: `${progress()}%` }}
          />
        </div>
      </Show>
    </div>
  );
}

function ThumbnailToast() {
  const activity = () => thumbGenActivity()!;
  const statusColor = () => {
    switch (activity().status) {
      case "generating": return "text-blue-400";
      case "done": return "text-green-400";
      case "error": return "text-red-400";
    }
  };
  const borderColor = () => {
    switch (activity().status) {
      case "generating": return "border-blue-500/30";
      case "done": return "border-green-500/30";
      case "error": return "border-red-500/30";
    }
  };
  const progress = () => {
    const a = activity();
    if (a.total === 0) return 0;
    return Math.round((a.generated / a.total) * 100);
  };

  return (
    <div
      class={`fixed bottom-14 right-4 z-[150] flex items-center gap-3 px-4 py-2.5 rounded-lg border ${borderColor()}`}
      style={{
        background: "rgba(18, 18, 18, 0.95)",
        "backdrop-filter": "blur(12px)",
      }}
    >
      <Show when={activity().status === "generating"}>
        <div class="w-3.5 h-3.5 border-2 border-blue-400 border-t-transparent rounded-full animate-spin" />
      </Show>
      <Show when={activity().status === "done"}>
        <svg class="w-3.5 h-3.5 text-green-400" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2.5" d="M5 13l4 4L19 7" />
        </svg>
      </Show>
      <Show when={activity().status === "error"}>
        <svg class="w-3.5 h-3.5 text-red-400" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2.5" d="M6 18L18 6M6 6l12 12" />
        </svg>
      </Show>
      <div class="flex flex-col">
        <span class={`text-xs font-medium ${statusColor()}`}>
          Thumbnails{activity().status === "generating" ? ` ${progress()}%` : ""}
        </span>
        <span class="text-[11px] text-neutral-400">{activity().message}</span>
      </div>
    </div>
  );
}

function WelcomeScreen(props: { onOpen: () => void; onOpenPath: (path: string) => void }) {
  const [manualPath, setManualPath] = createSignal("");
  const [error, setError] = createSignal("");
  const [recents, setRecents] = createSignal<RecentGallery[]>([]);

  // Load recent galleries on mount
  getRecentGalleries()
    .then(setRecents)
    .catch(() => {});

  const handleSubmit = (e: Event) => {
    e.preventDefault();
    const p = manualPath().trim();
    if (p) {
      setError("");
      props.onOpenPath(p);
    }
  };

  const handleRemoveRecent = async (e: MouseEvent, path: string) => {
    e.stopPropagation();
    try {
      await removeRecentGallery(path);
      setRecents((prev) => prev.filter((r) => r.path !== path));
    } catch {}
  };

  /** Extract just the folder name for display, show full path underneath. */
  const folderName = (path: string) => {
    const parts = path.replace(/\/+$/, "").split("/");
    return parts[parts.length - 1] || path;
  };

  const formatDate = (ts: number) => {
    const d = new Date(ts * 1000);
    const now = new Date();
    const diffMs = now.getTime() - d.getTime();
    const diffDays = Math.floor(diffMs / (1000 * 60 * 60 * 24));
    if (diffDays === 0) return "Today";
    if (diffDays === 1) return "Yesterday";
    if (diffDays < 7) return `${diffDays} days ago`;
    return d.toLocaleDateString();
  };

  return (
    <div class="h-screen w-full flex flex-col items-center justify-center gap-6">
      <h1 class="text-3xl font-light text-neutral-300">LightView</h1>
      <p class="text-sm text-neutral-500">Open a folder to browse your media</p>
      <button
        onClick={props.onOpen}
        class="px-6 py-3 bg-neutral-800 hover:bg-neutral-700 text-neutral-200 rounded-lg transition-colors text-sm cursor-pointer"
      >
        Open Folder
      </button>

      <Show when={recents().length > 0}>
        <div class="w-full max-w-md px-8 mt-2">
          <div class="text-neutral-500 text-xs mb-2">Recent</div>
          <div class="flex flex-col gap-1">
            {recents().map((r) => (
              <button
                onClick={() => props.onOpenPath(r.path)}
                class="group flex items-center gap-3 w-full px-3 py-2 rounded hover:bg-neutral-800 transition-colors text-left cursor-pointer"
              >
                <div class="flex-shrink-0 w-8 h-8 rounded bg-neutral-800 group-hover:bg-neutral-700 flex items-center justify-center text-neutral-500 text-xs">
                  <svg class="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                    <path stroke-linecap="round" stroke-linejoin="round" stroke-width="1.5" d="M3 7v10a2 2 0 002 2h14a2 2 0 002-2V9a2 2 0 00-2-2h-6l-2-2H5a2 2 0 00-2 2z" />
                  </svg>
                </div>
                <div class="flex-1 min-w-0">
                  <div class="text-sm text-neutral-200 truncate">{folderName(r.path)}</div>
                  <div class="text-xs text-neutral-500 truncate">{r.path}</div>
                </div>
                <div class="flex-shrink-0 flex items-center gap-2">
                  <span class="text-xs text-neutral-600">{formatDate(r.last_opened)}</span>
                  <span
                    onClick={(e) => handleRemoveRecent(e, r.path)}
                    class="opacity-0 group-hover:opacity-100 text-neutral-600 hover:text-neutral-400 transition-opacity cursor-pointer p-1"
                    title="Remove from recent"
                  >
                    <svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                      <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M6 18L18 6M6 6l12 12" />
                    </svg>
                  </span>
                </div>
              </button>
            ))}
          </div>
        </div>
      </Show>

      <div class="text-neutral-600 text-xs">or enter a path</div>
      <form onSubmit={handleSubmit} class="flex gap-2 w-full max-w-lg px-8">
        <input
          type="text"
          value={manualPath()}
          onInput={(e) => setManualPath(e.currentTarget.value)}
          placeholder="/path/to/photos"
          class="flex-1 px-3 py-2 bg-neutral-800 border border-neutral-700 rounded text-sm text-neutral-200 placeholder-neutral-500 outline-none focus:border-neutral-500"
        />
        <button
          type="submit"
          class="px-4 py-2 bg-neutral-700 hover:bg-neutral-600 text-neutral-200 rounded text-sm cursor-pointer transition-colors"
        >
          Go
        </button>
      </form>
      <Show when={error()}>
        <p class="text-red-400 text-xs">{error()}</p>
      </Show>
    </div>
  );
}
