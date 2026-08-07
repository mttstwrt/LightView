// Application settings, and the sort/group controls that travel with them.
//
// Settings have two homes and the split is deliberate: on the desktop they
// persist per gallery through the backend (and are mirrored to a hand-editable
// settings.toml the fs-watcher hot-reloads), while the web client keeps its own
// copy in local storage. A phone and the desktop looking at the same gallery
// should not fight over thumbnail size.

import { createSignal } from "solid-js";
import type { AppSettings, SortField, SortOrder, GroupBy } from "../lib/types";
import { saveGallerySettings, loadGallerySettings } from "../lib/ipc";
import { isWeb } from "../lib/runtime";
import { loadPref, savePref } from "../lib/clientPrefs";

const SETTINGS_PREF = "settings";

const DEFAULT_SETTINGS: AppSettings = {
  display: {
    thumbnail_size: 200,
    thumb_size_min: 120,
    thumb_size_max: 700,
    grid_gap: 2,
    background_color: "#0a0a0a",
    video_hover_preview: false,
    video_autoplay_loop: false,
    gif_autoplay_grid: false,
    video_autoplay_grid: false,
    video_autoplay_max_seconds: 30,
    scroll_blur: false,
    map_dark_mode: true,
    justified_high_detail: true,
    mobile_filter_sheet: "top",
    video_autoplay_viewer: true,
    start_at_bottom: false,
  },
  performance: {
    preload_count: 3,
    lru_cache_size: 5,
    thumbnail_threads: 6,
  },
  storage: {
    companion_location: "lightview_folder",
  },
  default_filter: {
    enabled: false,
    query: "",
  },
  external_apps: [
    { label: "Gwenview", command: "gwenview", args: ["{file}"] },
    { label: "GIMP", command: "gimp", args: ["{file}"] },
  ],
};

/** Merge stored settings over the defaults section-by-section. Settings saved
 *  by an older build lack keys added since; a shallow top-level spread would
 *  let a stored `display` object wipe out new display defaults entirely, so
 *  every setting added later had to be read with an ad-hoc `?? fallback`.
 *  Merging per section keeps new defaults present and the AppSettings type
 *  honest (no key is ever undefined at runtime). */
function mergeSettings(stored: Partial<AppSettings>): AppSettings {
  return {
    display: { ...DEFAULT_SETTINGS.display, ...stored.display },
    performance: { ...DEFAULT_SETTINGS.performance, ...stored.performance },
    storage: { ...DEFAULT_SETTINGS.storage, ...stored.storage },
    default_filter: { ...DEFAULT_SETTINGS.default_filter, ...stored.default_filter },
    external_apps: stored.external_apps ?? DEFAULT_SETTINGS.external_apps,
  };
}

// ---------------------------------------------------------------------------
// Exported reactive store
// ---------------------------------------------------------------------------
//
// Settings are persisted per-gallery in each gallery's
// `.lightview/settings.toml` (the source of truth) and loaded by
// loadSettingsFromGallery() when a gallery opens. There is no global store:
// before a gallery is open the only screen is the gallery selector, which no
// setting affects, so we simply start from in-memory defaults.

const [settings, setSettingsRaw] = createSignal<AppSettings>(DEFAULT_SETTINGS);

/** Whether a gallery is currently open (enables backend persistence). */
let galleryOpen = false;

export function setSettings(update: Partial<AppSettings> | ((prev: AppSettings) => AppSettings)) {
  setSettingsRaw((prev) => {
    const next = typeof update === "function" ? update(prev) : { ...prev, ...update };
    // The web client has no local gallery settings.toml (every browser talks to
    // the same host), so it persists per-client in localStorage. Desktop
    // persists to the open gallery's settings.toml; with no gallery open there's
    // nothing to persist for (the selector screen uses no settings).
    if (isWeb()) {
      savePref(SETTINGS_PREF, next);
    } else if (galleryOpen) {
      saveGallerySettings(JSON.stringify(next)).catch(() => {});
    }
    return next;
  });
}

/** Apply settings pushed from the backend because `settings.toml` was edited
 *  outside the app (hand edit). Updates the in-memory store only — it must NOT
 *  write back, or it would clobber the user's edit and fight the fs watcher. */
export function applyExternalSettings(json: string) {
  try {
    const stored = JSON.parse(json) as Partial<AppSettings>;
    setSettingsRaw(() => mergeSettings(stored));
  } catch {}
}

/** Load settings stored in the current gallery's .lightview/settings.toml and
 *  apply them. A gallery with no saved settings starts from the hard defaults,
 *  independent of any other gallery's configuration. */
export async function loadSettingsFromGallery() {
  galleryOpen = true;
  try {
    const json = await loadGallerySettings();
    if (json) {
      const stored = JSON.parse(json) as Partial<AppSettings>;
      setSettingsRaw(() => mergeSettings(stored));
    } else {
      // First open of this gallery — start from hard defaults and seed the
      // gallery's settings.toml with them, so each gallery is self-contained.
      setSettingsRaw(() => DEFAULT_SETTINGS);
      await saveGallerySettings(JSON.stringify(DEFAULT_SETTINGS)).catch(() => {});
    }
  } catch {}
}

/** Load the web client's per-client settings from localStorage. The web client
 *  has no gallery settings.toml, so this restores each browser's own display
 *  preferences (e.g. GIF-in-grid playback) on open. No-op values simply fall
 *  back to the hard defaults. */
export function loadWebSettings() {
  const stored = loadPref<Partial<AppSettings>>(SETTINGS_PREF);
  if (stored) {
    setSettingsRaw(() => mergeSettings(stored));
  }
}

/** Called when gallery is closed to stop backend persistence. */

export { settings };

// ---------------------------------------------------------------------------
// Sort state (not persisted, resets on gallery open)
// ---------------------------------------------------------------------------

const [sortField, setSortField] = createSignal<SortField>("date");
const [sortOrder, setSortOrder] = createSignal<SortOrder>("desc");
const [subSortField, setSubSortField] = createSignal<SortField>("date");
const [subSortOrder, setSubSortOrder] = createSignal<SortOrder>("desc");
const [groupBy, setGroupBy] = createSignal<GroupBy>({ type: "time_period", granularity: "month" });

export { sortField, setSortField, sortOrder, setSortOrder, subSortField, setSubSortField, subSortOrder, setSubSortOrder, groupBy, setGroupBy };
