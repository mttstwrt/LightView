import { createSignal } from "solid-js";
import type { AppSettings, SortField, SortOrder, GroupBy } from "../lib/types";
import { saveGallerySettings, loadGallerySettings } from "../lib/ipc";

const DEFAULT_SETTINGS: AppSettings = {
  display: {
    thumbnail_size: 200,
    grid_gap: 2,
    background_color: "#0a0a0a",
    video_hover_preview: false,
    gif_autoplay_grid: false,
  },
  performance: {
    preload_count: 3,
    lru_cache_size: 5,
    thumbnail_threads: 6,
    thumbnail_quality: 80,
    thumbnail_resize_filter: "bilinear",
  },
  storage: {
    companion_location: "lightview_folder",
  },
  external_apps: [
    { label: "Gwenview", command: "gwenview", args: ["{file}"] },
    { label: "GIMP", command: "gimp", args: ["{file}"] },
  ],
};

function loadSettings(): AppSettings {
  try {
    const stored = localStorage.getItem("gallery-settings");
    if (stored) {
      return { ...DEFAULT_SETTINGS, ...JSON.parse(stored) };
    }
  } catch {}
  return DEFAULT_SETTINGS;
}

function saveSettings(s: AppSettings) {
  try {
    localStorage.setItem("gallery-settings", JSON.stringify(s));
  } catch {}
}

// ---------------------------------------------------------------------------
// Exported reactive store
// ---------------------------------------------------------------------------

const [settings, setSettingsRaw] = createSignal<AppSettings>(loadSettings());

/** Whether a gallery is currently open (enables backend persistence). */
let galleryOpen = false;

export function setSettings(update: Partial<AppSettings> | ((prev: AppSettings) => AppSettings)) {
  setSettingsRaw((prev) => {
    const next = typeof update === "function" ? update(prev) : { ...prev, ...update };
    saveSettings(next);
    // Also persist to the gallery's .lightview folder when one is open
    if (galleryOpen) {
      saveGallerySettings(JSON.stringify(next)).catch(() => {});
    }
    return next;
  });
}

/** Load settings stored in the current gallery's .lightview folder and apply them.
 *  Falls back to the current (localStorage) settings if nothing is saved. */
export async function loadSettingsFromGallery() {
  galleryOpen = true;
  try {
    const json = await loadGallerySettings();
    if (json) {
      const stored = JSON.parse(json) as Partial<AppSettings>;
      const merged = { ...DEFAULT_SETTINGS, ...stored };
      setSettingsRaw(() => merged);
      saveSettings(merged); // sync localStorage with gallery settings
    } else {
      // First open of this gallery — persist current settings into it
      await saveGallerySettings(JSON.stringify(settings())).catch(() => {});
    }
  } catch {}
}

/** Called when gallery is closed to stop backend persistence. */
export function clearGallerySettingsSync() {
  galleryOpen = false;
}

export { settings };

// ---------------------------------------------------------------------------
// Sort state (not persisted, resets on gallery open)
// ---------------------------------------------------------------------------

const [sortField, setSortField] = createSignal<SortField>("date");
const [sortOrder, setSortOrder] = createSignal<SortOrder>("desc");
const [groupBy, setGroupBy] = createSignal<GroupBy>({ type: "time_period", granularity: "month" });

export { sortField, setSortField, sortOrder, setSortOrder, groupBy, setGroupBy };
