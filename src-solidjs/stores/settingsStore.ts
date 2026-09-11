// Settings, in the two places they actually belong.
//
// **Display preferences are per client, for every client including the local
// one.** They used to be per gallery on the desktop and per client on the web —
// but a file inside the gallery is per *gallery*, not per client, so two
// desktops mounting one share would fight over thumbnail size, which is the
// exact thing a per-client preference exists to prevent. One mechanism, and no
// local-versus-remote branch survives into the frontend.
//
// **The gallery's own settings file holds exactly two keys**: the default
// filter, which is user intent and must survive a cache format bump, and trash
// retention, which is hand-edited only because it is the one setting in the
// system that deletes data.

import { createSignal } from "solid-js";

import { api } from "../lib/ipc";
import { loadPref, savePref } from "../lib/clientPrefs";
import { isMobile } from "../lib/runtime";
import type {
  Capabilities,
  GallerySettings,
  GroupBy,
  SortField,
  SortOrder,
} from "../lib/types";

const PREFS_KEY = "prefs";

/** Desktop default cell size: about six columns on a wide window. */
const DESKTOP_THUMBNAIL_SIZE = 200;
/** Columns a phone should open on. */
const MOBILE_COLUMNS = 2;
const DEFAULT_GRID_GAP = 2;

/**
 * The default cell size, which on a phone is a *column count* in disguise.
 *
 * `thumbnail_size` is a size, not a count, so the same 200px that gives a
 * desktop six columns gives a 390px phone exactly one — a grid one photo wide,
 * whose 390px cells then ask for the largest tier: the most expensive thing the
 * grid can do, on the device least able to afford it.
 *
 * Measured against the **short** edge, so a phone opened in landscape gets a
 * portrait-sensible size rather than two enormous cells that become one on
 * rotation. Bucket-midpoint arithmetic, matching the pinch/Ctrl-wheel stepper,
 * so the layout has room either side of the target before it snaps.
 */
function defaultThumbnailSize(gap: number): number {
  if (!isMobile() || typeof window === "undefined") return DESKTOP_THUMBNAIL_SIZE;
  const short = Math.min(window.innerWidth, window.innerHeight);
  const upper = (short + gap) / MOBILE_COLUMNS - gap;
  const lower = (short + gap) / (MOBILE_COLUMNS + 1) - gap;
  return Math.round((upper + lower) / 2);
}

/** Everything a client decides for itself. Nothing here reaches the server. */
export interface DisplayPrefs {
  thumbnail_size: number;
  thumb_size_min: number;
  thumb_size_max: number;
  grid_gap: number;
  background_color: string;
  video_hover_preview: boolean;
  video_autoplay_loop: boolean;
  gif_autoplay_grid: boolean;
  video_autoplay_grid: boolean;
  video_autoplay_max_seconds: number;
  scroll_blur: boolean;
  /** Serve a larger aspect-preserving tier when zoomed in, rather than
   *  upscaling the base rung. Generated for visible cells only. */
  justified_high_detail: boolean;
  /** Where the mobile filter/sort sheet appears: pinned under the safe-area
   *  inset, or as a thumb-reachable sheet from the bottom. */
  mobile_filter_sheet: "top" | "bottom";
  video_autoplay_viewer: boolean;
  /** Open scrolled to the end of the grid rather than the start. Only changes
   *  where the view lands; the sort order itself is unaffected. */
  start_at_bottom: boolean;
  preload_count: number;
  lru_cache_size: number;
}

const DEFAULT_PREFS: DisplayPrefs = {
  thumbnail_size: defaultThumbnailSize(DEFAULT_GRID_GAP),
  thumb_size_min: 120,
  thumb_size_max: 700,
  grid_gap: DEFAULT_GRID_GAP,
  background_color: "#0a0a0a",
  video_hover_preview: false,
  video_autoplay_loop: false,
  gif_autoplay_grid: false,
  video_autoplay_grid: false,
  video_autoplay_max_seconds: 30,
  scroll_blur: false,
  justified_high_detail: true,
  mobile_filter_sheet: "top",
  video_autoplay_viewer: true,
  start_at_bottom: false,
  preload_count: 3,
  lru_cache_size: 5,
};

/** Merge stored preferences over the defaults.
 *
 *  A flat merge is safe here because the shape is flat — the sectioned version
 *  it replaces needed per-section merging precisely because a shallow spread
 *  let one stored section wipe out every default added since. */
function merge(stored: Partial<DisplayPrefs> | null): DisplayPrefs {
  return { ...DEFAULT_PREFS, ...(stored ?? {}) };
}

const [prefs, setPrefsRaw] = createSignal<DisplayPrefs>(merge(loadPref(PREFS_KEY)));

export function setPrefs(
  update: Partial<DisplayPrefs> | ((prev: DisplayPrefs) => DisplayPrefs),
) {
  setPrefsRaw((prev) => {
    const next = typeof update === "function" ? update(prev) : { ...prev, ...update };
    savePref(PREFS_KEY, next);
    return next;
  });
}

export { prefs };

// ---------------------------------------------------------------------------
// The gallery's own two settings
// ---------------------------------------------------------------------------

const [gallerySettings, setGallerySettings] = createSignal<GallerySettings>({
  default_filter: "",
  trash_retention_days: 30,
});

export { gallerySettings };

export async function loadGallerySettings() {
  try {
    setGallerySettings(await api.settings());
  } catch {
    // A gallery whose settings file is unreadable opens with the defaults; the
    // server logs it, and refusing to open would be a worse failure.
  }
}

/** Write the default filter. A `Device` command, because under `--serve` the
 *  phone is the only UI there is. */
export async function saveDefaultFilter(filter: string) {
  setGallerySettings(await api.setDefaultFilter(filter));
}

// ---------------------------------------------------------------------------
// Capabilities
// ---------------------------------------------------------------------------

// Optimistically `device`, which is the *narrower* answer: a UI that briefly
// hides an action it turns out to have is a flicker, while one that briefly
// offers an action the server will refuse is a 403 the user caused.
const [capabilities, setCapabilities] = createSignal<Capabilities>({
  trust: "device",
  upload: false,
  clipboard: false,
});

export { capabilities };

export async function loadCapabilities() {
  try {
    setCapabilities(await api.capabilities());
  } catch {
    // Leave the narrow default.
  }
}

/** Whether this client may reach the `Owner` half of the command table. */
export function isOwner(): boolean {
  return capabilities().trust === "owner";
}

// ---------------------------------------------------------------------------
// Sort state — not persisted, and reset on gallery open
// ---------------------------------------------------------------------------

const [sortField, setSortField] = createSignal<SortField>("date");
const [sortOrder, setSortOrder] = createSignal<SortOrder>("desc");
const [subSortField, setSubSortField] = createSignal<SortField>("date");
const [subSortOrder, setSubSortOrder] = createSignal<SortOrder>("desc");
const [groupBy, setGroupBy] = createSignal<GroupBy>({
  type: "time_period",
  granularity: "month",
});

export {
  sortField,
  setSortField,
  sortOrder,
  setSortOrder,
  subSortField,
  setSubSortField,
  subSortOrder,
  setSubSortOrder,
  groupBy,
  setGroupBy,
};
