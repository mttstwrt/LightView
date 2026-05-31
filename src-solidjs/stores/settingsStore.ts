import { createSignal } from "solid-js";
import type { AppSettings, SortField, SortOrder, GroupBy } from "../lib/types";
import { saveGallerySettings, loadGallerySettings } from "../lib/ipc";
import { webglPreviouslyCrashed, markWebglStable } from "../lib/renderer/webglGuard";

const DEFAULT_SETTINGS: AppSettings = {
  display: {
    thumbnail_size: 200,
    grid_gap: 2,
    background_color: "#0a0a0a",
    video_hover_preview: false,
    video_autoplay_loop: false,
    gif_autoplay_grid: false,
    scroll_blur: false,
    renderer_mode: "canvas",
    map_dark_mode: true,
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

/** If WebGL crashed on the previous launch, downgrade the saved renderer to
 *  Canvas2D so we don't boot-loop. Consumes the crash guard so a later manual
 *  re-enable gets a fresh attempt. No-op for any other renderer/state. */
function applyRendererSafeMode(s: AppSettings): AppSettings {
  if (s.display.renderer_mode === "webgl" && webglPreviouslyCrashed()) {
    markWebglStable();
    console.warn("WebGL crashed on the previous launch — falling back to Canvas2D");
    return { ...s, display: { ...s.display, renderer_mode: "canvas" } };
  }
  return s;
}

function loadSettings(): AppSettings {
  let s = DEFAULT_SETTINGS;
  try {
    const stored = localStorage.getItem("gallery-settings");
    if (stored) {
      s = { ...DEFAULT_SETTINGS, ...JSON.parse(stored) };
    }
  } catch {}
  const safe = applyRendererSafeMode(s);
  if (safe !== s) saveSettings(safe); // persist the downgrade
  return safe;
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
      const merged = applyRendererSafeMode({ ...DEFAULT_SETTINGS, ...stored });
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
const [subSortField, setSubSortField] = createSignal<SortField>("date");
const [subSortOrder, setSubSortOrder] = createSignal<SortOrder>("desc");
const [groupBy, setGroupBy] = createSignal<GroupBy>({ type: "time_period", granularity: "month" });

export { sortField, setSortField, sortOrder, setSortOrder, subSortField, setSubSortField, subSortOrder, setSubSortOrder, groupBy, setGroupBy };
