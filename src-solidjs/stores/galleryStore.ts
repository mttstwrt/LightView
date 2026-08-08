// The open gallery: its items, the current sort/filter result, and selection.
//
// `sortedItems` is the full ordered list from the backend; `displayPaths` is
// what the grid actually renders after filtering. They are separate signals so
// changing the filter does not invalidate everything derived from the sort.

import { createSignal, createMemo } from "solid-js";
import type {
  GroupHeader,
  SortedItem,
} from "../lib/types";
import { loadPref, savePref } from "../lib/clientPrefs";
import { setRating as setRatingIpc } from "../lib/ipc";

// ---------------------------------------------------------------------------
// Gallery state
// ---------------------------------------------------------------------------

const [galleryPath, setGalleryPath] = createSignal<string | null>(null);
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

// Per-path aspect ratio (width / height) derived from the sorted items, for the
// justified view to lay out cells before thumbnail bytes exist. Paths with
// unknown dimensions are absent; the layout falls back to 1:1 for those.
const aspectByPath = createMemo(() => {
  const map = new Map<string, number>();
  for (const item of sortedItems()) {
    if (item.width != null && item.height != null && item.width > 0 && item.height > 0) {
      map.set(item.path, item.width / item.height);
    }
  }
  return map;
});

// Per-path file size + media type, for the justified view to decide whether a
// cell can be served as its original file (cheap, native formats) instead of a
// generated high-detail thumbnail when zoomed in.
export interface MediaMeta {
  size: number;
  media_type: string;
  /** Source pixel dimensions, when known. Used to gate "serve original" on the
   *  decode cost (megapixels), not just file bytes. */
  width: number | null;
  height: number | null;
}
const mediaMetaByPath = createMemo(() => {
  const map = new Map<string, MediaMeta>();
  for (const item of sortedItems()) {
    map.set(item.path, {
      size: item.file_size,
      media_type: item.media_type,
      width: item.width ?? null,
      height: item.height ?? null,
    });
  }
  return map;
});

// Group headers for the current sort/group
const [groups, setGroups] = createSignal<GroupHeader[]>([]);

// Timeline data for scrollbar

// Selected items (multi-select)
const [selectedPaths, setSelectedPaths] = createSignal<Set<string>>(new Set());

// Explicit multi-select mode. Desktop reaches selection through Ctrl/Cmd+click,
// which touch has no equivalent for — so a phone flips this on from the Select
// button and every tap toggles a cell instead of opening the viewer.
const [selectionMode, setSelectionMode] = createSignal(false);

// View mode: grid (uniform squares), justified (aspect-preserving rows), or
// map (geographic browsing).
export type ViewMode = "grid" | "justified" | "map";

// Persist the chosen layout per client so the last-used view is restored on the
// next open (localStorage — per browser/device, matching per-client settings).
const VIEW_MODE_PREF = "viewMode";
const savedViewMode = loadPref<ViewMode>(VIEW_MODE_PREF);
const [viewMode, setViewModeRaw] = createSignal<ViewMode>(
  savedViewMode === "grid" || savedViewMode === "justified" || savedViewMode === "map"
    ? savedViewMode
    : "grid",
);
const setViewMode = ((value) => {
  const next = setViewModeRaw(value as any);
  savePref(VIEW_MODE_PREF, next);
  return next;
}) as typeof setViewModeRaw;

// Whether the settings panel is open. On mobile the panel is a full-screen
// page, so App uses this to stop rendering the grid behind it.
const [settingsOpen, setSettingsOpen] = createSignal(false);

export {
  galleryPath, setGalleryPath,
  thumbnailsReady, setThumbnailsReady,
  indexingProgress, setIndexingProgress,
  loading, setLoading,
  displayPaths, setDisplayPaths,
  sortedItems, setSortedItems,
  durationByPath,
  aspectByPath,
  mediaMetaByPath,
  groups, setGroups,
  selectedPaths, setSelectedPaths,
  selectionMode,
  viewMode, setViewMode,
  settingsOpen, setSettingsOpen,
};

// ---------------------------------------------------------------------------
// Rating
// ---------------------------------------------------------------------------

/** Persist a rating and keep every consumer in sync: the backend (IPC), the
 *  in-memory sorted items, and any listener on the
 *  `lightview:rating-changed` event (info panel). All rating writes —
 *  keyboard 0–5, info panel, context menu — go through here. */
export async function rateItem(path: string, rating: number) {
  await setRatingIpc(path, rating);
  const lastRated = rating > 0 ? Math.floor(Date.now() / 1000) : null;
  setSortedItems((items) =>
    items.map((it) =>
      it.path === path ? { ...it, rating: rating > 0 ? rating : null, last_rated: lastRated } : it,
    ),
  );
  window.dispatchEvent(
    new CustomEvent("lightview:rating-changed", { detail: { path, rating } }),
  );
}

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

/** Enter multi-select mode (tap-to-toggle). */
function enterSelectionMode() {
  setSelectionMode(true);
}

/** Leave multi-select mode, dropping whatever was selected — the two always
 *  go together, so no caller has to remember to do both. */
export function exitSelectionMode() {
  setSelectionMode(false);
  clearSelection();
}

export function toggleSelectionMode() {
  if (selectionMode()) exitSelectionMode();
  else enterSelectionMode();
}

export function selectAll(paths: string[]) {
  setSelectedPaths(new Set<string>(paths));
}

