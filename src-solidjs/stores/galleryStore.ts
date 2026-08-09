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
import {
  setRating as setRatingIpc,
  setColorLabel as setColorLabelIpc,
  getEnabledViews,
} from "../lib/ipc";

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

/** Path → colour label, for the cell marker and the context menu's tick. A
 *  memo rather than a scan per lookup: both callers ask once per rendered cell. */
const colorLabelByPath = createMemo(() => {
  const map = new Map<string, string>();
  for (const item of sortedItems()) {
    if (item.color_label) map.set(item.path, item.color_label);
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

/** The views, in switcher order, with the labels every switcher uses. One
 *  table rather than a literal per call site: the top bar, the mobile settings
 *  panel and the desktop enable/disable list all render the same three. */
export const VIEW_CHOICES: { mode: ViewMode; label: string; title: string }[] = [
  { mode: "grid", label: "Grid", title: "Uniform square grid" },
  { mode: "justified", label: "Justified", title: "Aspect-preserving rows" },
  { mode: "map", label: "Map", title: "Geographic map" },
];

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

// Which views the *gallery* offers, as opposed to which one this client last
// used. A view the gallery has turned off generates no thumbnails for its tier
// (see the Rust `views` module), so offering it in the switcher would show a
// grid that has to decode the whole library on the spot. Optimistically all
// three until the backend answers — the same reasoning as capabilitiesStore's
// baseline: assuming none makes the switcher flash empty.
const ALL_VIEWS: ViewMode[] = ["grid", "justified", "map"];
const [enabledViews, setEnabledViewsRaw] = createSignal<ViewMode[]>(ALL_VIEWS);

/** Apply the gallery's enabled-view list, falling back to a view that exists
 *  if the one this client last used has since been turned off. A gallery with
 *  every view disabled is left on the stored view rather than on nothing —
 *  App renders a grid either way, and stranding the user on a blank screen
 *  they cannot navigate out of is worse than honouring a stale preference. */
export function applyEnabledViews(views: string[]) {
  const next = ALL_VIEWS.filter((v) => views.includes(v));
  setEnabledViewsRaw(next);
  if (next.length > 0 && !next.includes(viewMode())) setViewMode(next[0]);
}

/** Fetch the gallery's enabled views. Called once per gallery open on desktop
 *  and once at boot on the web client. A failure leaves the optimistic
 *  all-views default: the switcher offering a view whose tier isn't pre-warmed
 *  is a slow grid, while hiding one the gallery does offer is a feature the
 *  user cannot reach. */
export async function loadEnabledViews() {
  try {
    applyEnabledViews(await getEnabledViews());
  } catch (e) {
    console.warn("Failed to load enabled views:", e);
  }
}

export { enabledViews };

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
  colorLabelByPath,
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

/** Set a colour label and keep `sortedItems` in step, so a `color:` filter and
 *  the cell marker update without a refetch — the same reason `rateItem`
 *  exists rather than calling the IPC wrapper directly. */
export async function setItemColorLabel(path: string, label: string | null) {
  await setColorLabelIpc(path, label);
  setSortedItems((items) =>
    items.map((it) => (it.path === path ? { ...it, color_label: label } : it)),
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

