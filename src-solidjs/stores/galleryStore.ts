import { createSignal, createMemo } from "solid-js";
import type {
  GalleryMediaItem,
  GroupHeader,
  SortedItem,
  TimelineEntry,
} from "../lib/types";

// ---------------------------------------------------------------------------
// Gallery state
// ---------------------------------------------------------------------------

const [galleryPath, setGalleryPath] = createSignal<string | null>(null);
const [totalCount, setTotalCount] = createSignal(0);
const [thumbnailsReady, setThumbnailsReady] = createSignal(0);
const [indexingProgress, setIndexingProgress] = createSignal(0);
const [loading, setLoading] = createSignal(false);

// Sorted + filtered items (paths in display order)
const [displayPaths, setDisplayPaths] = createSignal<string[]>([]);

// Full sorted item metadata (for scrollbar indicators, etc.)
const [sortedItems, setSortedItems] = createSignal<SortedItem[]>([]);

// Per-path video duration (seconds) derived from the sorted items, for the grid
// to gate short-video autoplay without changing its `paths: string[]` contract.
// Paths with unknown (NULL) duration are simply absent from the map.
const durationByPath = createMemo(() => {
  const map = new Map<string, number>();
  for (const item of sortedItems()) {
    if (item.duration != null) map.set(item.path, item.duration);
  }
  return map;
});

// Group headers for the current sort/group
const [groups, setGroups] = createSignal<GroupHeader[]>([]);

// Timeline data for scrollbar
const [timeline, setTimeline] = createSignal<TimelineEntry[]>([]);

// Selected items (multi-select)
const [selectedPaths, setSelectedPaths] = createSignal<Set<string>>(new Set());

// View mode: grid (default) vs map (geographic browsing).
export type ViewMode = "grid" | "map";
const [viewMode, setViewMode] = createSignal<ViewMode>("grid");

// Whether the settings panel is open. On mobile the panel is a full-screen
// page, so App uses this to stop rendering the grid behind it (the WebGL/canvas
// grid composites above the DOM panel on some mobile browsers otherwise).
const [settingsOpen, setSettingsOpen] = createSignal(false);

export {
  galleryPath, setGalleryPath,
  totalCount, setTotalCount,
  thumbnailsReady, setThumbnailsReady,
  indexingProgress, setIndexingProgress,
  loading, setLoading,
  displayPaths, setDisplayPaths,
  sortedItems, setSortedItems,
  durationByPath,
  groups, setGroups,
  timeline, setTimeline,
  selectedPaths, setSelectedPaths,
  viewMode, setViewMode,
  settingsOpen, setSettingsOpen,
};

// ---------------------------------------------------------------------------
// Selection helpers
// ---------------------------------------------------------------------------

export function toggleSelection(path: string) {
  setSelectedPaths((prev) => {
    const next = new Set(prev);
    if (next.has(path)) {
      next.delete(path);
    } else {
      next.add(path);
    }
    return next;
  });
}

export function clearSelection() {
  setSelectedPaths(new Set<string>());
}

export function selectAll(paths: string[]) {
  setSelectedPaths(new Set<string>(paths));
}

export function addToSelection(paths: string[]) {
  setSelectedPaths((prev) => {
    const next = new Set(prev);
    for (const p of paths) next.add(p);
    return next;
  });
}
