import { Show, createSignal, onCleanup } from "solid-js";
import { open } from "@tauri-apps/plugin-dialog";
import { listen } from "@tauri-apps/api/event";
import { galleryPath, setGalleryPath, setTotalCount, setLoading, displayPaths, setDisplayPaths, loading, selectedPaths, setSelectedPaths, toggleSelection, clearSelection, selectAll } from "./stores/galleryStore";
import { viewerOpen, closeViewer, openViewer, nextImage, prevImage, viewerIndex, toggleInfoPanel } from "./stores/viewerStore";
import { settings, sortField, sortOrder, groupBy, loadSettingsFromGallery } from "./stores/settingsStore";
import { openGallery, getSortedItems, getDebugInfo, getRecentGalleries, removeRecentGallery, type DebugInfo, type RecentGallery } from "./lib/ipc";
import { GalleryGrid } from "./components/gallery/GalleryGrid";
import { MediaViewer } from "./components/viewer/MediaViewer";
import { TopBar } from "./components/topbar/TopBar";
import { ContextMenu, type ContextMenuState } from "./components/shared/ContextMenu";
import { SelectionBar } from "./components/gallery/SelectionBar";
import { pluginActivity } from "./stores/pluginStore";

function DebugOverlay() {
  const [info, setInfo] = createSignal<DebugInfo | null>(null);

  const load = async () => {
    try {
      setInfo(await getDebugInfo());
    } catch (e) {
      console.error("Debug info failed:", e);
    }
  };

  load();

  return (
    <div
      class="fixed bottom-4 left-4 z-[100] p-3 rounded text-xs font-mono text-neutral-300 max-w-sm"
      style={{ background: "rgba(0,0,0,0.85)", "backdrop-filter": "blur(8px)", border: "1px solid rgba(255,255,255,0.1)" }}
    >
      <div class="text-neutral-500 mb-2">Hardware Debug</div>
      <Show when={info()} fallback={<div class="text-neutral-500">Loading...</div>}>
        <div class="space-y-0.5">
          <div>Storage: <span class="text-neutral-100">{info()!.storage_type}</span></div>
          <div>Filesystem: <span class="text-neutral-100">{info()!.filesystem}</span></div>
          <div>CPU: <span class="text-neutral-100">{info()!.cpu_cores} cores</span></div>
          <div>RAM: <span class="text-neutral-100">{info()!.total_ram_mb} MB</span></div>
          <div>GPU compute: <span class={info()!.gpu_compute ? "text-green-400" : "text-red-400"}>{info()!.gpu_compute ? "yes" : "no"}</span></div>
          <div>GPU resize: <span class={info()!.gpu_resize_active ? "text-green-400" : "text-neutral-500"}>{info()!.gpu_resize_active ? "active" : "inactive"}</span></div>
          <div>Thumb format: <span class="text-neutral-100">{info()!.thumb_format}</span></div>
          <div>BC7 atlas: <span class={info()!.bc7_atlas_active ? "text-green-400" : "text-neutral-500"}>{info()!.bc7_atlas_active ? `active (${info()!.atlas_entry_count} entries)` : "inactive"}</span></div>
          <div>SQLite thumbs: <span class="text-neutral-100">{info()!.sqlite_thumbnail_count}</span></div>
          <div>Thumb threads: <span class="text-neutral-100">{info()!.thumbnail_threads}</span></div>
          <div>Prefetch: <span class="text-neutral-100">{info()!.prefetch_count}</span></div>
          <div>LRU cache: <span class="text-neutral-100">{info()!.lru_cache_size}</span></div>
          <div>Reflink: <span class={info()!.supports_reflink ? "text-green-400" : "text-neutral-500"}>{info()!.supports_reflink ? "yes" : "no"}</span></div>
        </div>
      </Show>
      <button class="mt-2 text-neutral-500 hover:text-neutral-300 cursor-pointer" onClick={load}>refresh</button>
    </div>
  );
}

export function App() {
  const [debugOpen, setDebugOpen] = createSignal(false);
  const [contextMenu, setContextMenu] = createSignal<ContextMenuState | null>(null);
  const openPath = async (path: string) => {
    setLoading(true);
    try {
      const result = await openGallery(path);
      setGalleryPath(result.path);
      setTotalCount(result.total_media);

      // Restore per-gallery settings from .lightview folder
      await loadSettingsFromGallery();

      const sorted = await getSortedItems(sortField(), sortOrder(), groupBy());
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
        <TopBar onOpenFolder={handleOpenFolder} />
        <GalleryGrid
          paths={displayPaths()}
          onItemClick={(index) => {
            clearSelection();
            openViewer(index);
          }}
          onItemSelect={(path) => toggleSelection(path)}
          onDragSelect={(paths) => setSelectedPaths(new Set(paths))}
          selectedPaths={selectedPaths()}
          onItemContextMenu={(e, path, index) => {
            setContextMenu({ x: e.clientX, y: e.clientY, path, index });
          }}
          loading={loading()}
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
      </Show>
      <Show when={debugOpen()}>
        <DebugOverlay />
      </Show>
      <Show when={pluginActivity()}>
        <PluginToast />
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
    }
  };
  const borderColor = () => {
    switch (activity().status) {
      case "running": return "border-teal-500/30";
      case "done": return "border-green-500/30";
      case "error": return "border-red-500/30";
    }
  };

  return (
    <div
      class={`fixed bottom-4 right-4 z-[150] flex items-center gap-3 px-4 py-2.5 rounded-lg border ${borderColor()}`}
      style={{
        background: "rgba(18, 18, 18, 0.95)",
        "backdrop-filter": "blur(12px)",
      }}
    >
      <Show when={activity().status === "running"}>
        <div class="w-3.5 h-3.5 border-2 border-teal-400 border-t-transparent rounded-full animate-spin" />
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
        <span class={`text-xs font-medium ${statusColor()}`}>{activity().displayName}</span>
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
