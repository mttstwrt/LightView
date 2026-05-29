import { invoke as _rawInvoke } from "@tauri-apps/api/core";
import { isActive as isPerfActive, recordIpcCall } from "./perfMonitor";
import { isTauri } from "./runtime";
import type {
  GalleryOpenResult,
  GalleryStats,
  GroupBy,
  HardwareProfile,
  MemoryStatus,
  PluginInfo,
  PluginRunResult,
  SortField,
  SortOrder,
  SortedResult,
  TagSuggestion,
  TimelineEntry,
} from "./types";

// ---------------------------------------------------------------------------
// Transport — Tauri IPC on desktop, HTTP bridge in the browser
// ---------------------------------------------------------------------------

/** Web-client transport: POST to the read-only `/api/invoke` bridge. The auth
 *  token rides along as a same-origin cookie (set by `initWebAuth`), so no
 *  header is needed here. Only read-only commands are accepted server-side. */
async function _httpInvoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  const res = await fetch("/api/invoke", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ command: cmd, args: args ?? {} }),
  });
  if (!res.ok) {
    const detail = await res.text().catch(() => res.statusText);
    throw new Error(`invoke ${cmd} failed (${res.status}): ${detail}`);
  }
  const text = await res.text();
  return (text ? JSON.parse(text) : undefined) as T;
}

/** Dispatch a command over the active transport. */
function _transport<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  return isTauri() ? _rawInvoke<T>(cmd, args) : _httpInvoke<T>(cmd, args);
}

// ---------------------------------------------------------------------------
// Instrumented invoke — records IPC metrics when perf monitor is active
// ---------------------------------------------------------------------------

async function invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  if (!isPerfActive()) {
    return _transport<T>(cmd, args);
  }
  const argStr = args ? JSON.stringify(args) : "";
  const start = performance.now();
  const result = await _transport<T>(cmd, args);
  const elapsed = performance.now() - start;
  const resStr = result !== undefined && result !== null ? JSON.stringify(result) : "";
  recordIpcCall(cmd, argStr.length, resStr.length, elapsed);
  return result;
}

// ---------------------------------------------------------------------------
// Gallery
// ---------------------------------------------------------------------------

export const openGallery = (path: string) =>
  invoke<GalleryOpenResult>("open_gallery", { path });

export const closeGallery = () => invoke<void>("close_gallery");

export const getGalleryInfo = () =>
  invoke<GalleryOpenResult | null>("get_gallery_info");

// ---------------------------------------------------------------------------
// Media
// ---------------------------------------------------------------------------


export interface ThumbnailResult {
  path: string;
  width: number;
  height: number;
  media_type: string;
  format: string;
}

/** LOD tier for thumbnails; see docs/thumbnailStreamingResearch.md and
 *  `ThumbTier` in src-tauri/src/cache/thumbnails.rs. */
export type ThumbTier = "s" | "m" | "l" | "p";

/** Build a protocol URL for a cached thumbnail at a given tier. The
 *  `lightview://thumb/<tier>/<path>` protocol serves image data directly
 *  from SQLite — no JSON serialization overhead. When `tier` is omitted
 *  the backend falls back to the standard (m) tier for legacy URLs. */
export function thumbUrl(path: string, tier: ThumbTier = "m"): string {
  if (isTauri()) {
    return `lightview://thumb/${tier}/${encodeURIComponent(path)}`;
  }
  // Web client: same-origin HTTP route served by the axum server. Cookie auth.
  const rel = path.startsWith("/") ? path.slice(1) : path;
  return `/thumb/${tier}/${encodeMediaPath(rel)}`;
}

/** Build a protocol URL for the decoded ThumbHash placeholder of a path.
 *  Returns a ~32x32 PNG generated on-the-fly from the ~25-byte hash. */
export function thumbhashUrl(path: string): string {
  if (isTauri()) {
    return `lightview://thumbhash/${encodeURIComponent(path)}`;
  }
  const rel = path.startsWith("/") ? path.slice(1) : path;
  return `/thumbhash/${encodeMediaPath(rel)}`;
}

/** Base URL of the local HTTP media server (e.g. `http://127.0.0.1:52431`).
 *  Populated by the Rust backend via an initialization script (see
 *  `setup` in `main.rs`). Falls back to an IPC fetch if the script has
 *  not yet executed. */
let _mediaServerUrl: string | null =
  (globalThis as any).__LV_MEDIA_URL__ ?? null;

/** Eagerly fetch the media server URL and cache it. Safe to call
 *  multiple times — subsequent calls are no-ops once the URL is known.
 *  Call from app startup so `mediaUrl()` can run synchronously later. */
export async function initMediaServer(): Promise<string> {
  // Web client serves media from its own origin, so there is no separate URL
  // to prime — `mediaUrl()` returns same-origin relative paths.
  if (!isTauri()) return "";
  if (_mediaServerUrl) return _mediaServerUrl;
  const injected = (globalThis as any).__LV_MEDIA_URL__;
  if (typeof injected === "string" && injected.length > 0) {
    _mediaServerUrl = injected;
    return injected;
  }
  const url = await invoke<string>("get_media_server_url");
  _mediaServerUrl = url;
  return url;
}

/** Percent-encode each path segment independently so `/` is preserved but
 *  special characters (spaces, unicode, `?`, `#`, etc.) are encoded.
 *  Axum's router decodes captures but rejects paths containing raw
 *  encoded slashes, so we must keep `/` literal. */
function encodeMediaPath(path: string): string {
  return path.split("/").map(encodeURIComponent).join("/");
}

/** Build a URL for full-resolution media (image or video). Served by the
 *  local axum HTTP server, which streams the file with Range support — so
 *  large videos don't get buffered into memory and large images don't
 *  block the protocol thread. WebKitGTK also refuses `<video>` from any
 *  non-http(s) URI scheme, so this single path covers both elements. */
export function mediaUrl(path: string): string {
  const rel = path.startsWith("/") ? path.slice(1) : path;
  // Web client: same-origin HTTP route served by the axum server. Cookie auth.
  if (!isTauri()) {
    return `/media/${encodeMediaPath(rel)}`;
  }
  if (!_mediaServerUrl) {
    _mediaServerUrl = (globalThis as any).__LV_MEDIA_URL__ ?? null;
  }
  if (!_mediaServerUrl) return "";
  return `${_mediaServerUrl}/media/${encodeMediaPath(rel)}`;
}

/** Alias for `mediaUrl`. Kept for the `<video>` callsite where the name
 *  documents intent. */
export const videoSrc = mediaUrl;

export interface ThumbHashResult {
  path: string;
  /** Base64-encoded hash bytes, or null if not yet generated. */
  hash: string | null;
}

/** Bulk fetch of ThumbHash placeholders for a list of paths. Missing paths
 *  return `{ hash: null }`; caller can fall back to the skeleton texture. */
export const getThumbhashes = (paths: string[]) =>
  invoke<ThumbHashResult[]>("get_thumbhashes", { paths });

/** Lazily generate high-resolution tier thumbnails (L / P). Re-decodes
 *  each source image at the tier's target size. Returns the count of
 *  newly cached thumbnails; the caller should refetch the tier URL
 *  (with a new cache-buster) after this resolves. */
export const ensureTierThumbnails = (paths: string[], tier: ThumbTier) =>
  invoke<number>("ensure_tier_thumbnails", { paths, tier });

export const getThumbnail = (path: string) =>
  invoke<ThumbnailResult | null>("get_thumbnail", { path });

export const getThumbnailsBatch = (paths: string[]) =>
  invoke<ThumbnailResult[]>("get_thumbnails_batch", { paths });

export interface PrecacheResult {
  generated: number;
  failed: string[];
}

export const precacheThumbnails = (paths: string[]) =>
  invoke<PrecacheResult>("precache_thumbnails", { paths });

export const getFullMedia = (path: string) =>
  invoke<string>("get_full_media", { path });

export const getMediaMeta = (path: string) =>
  invoke<{
    path: string;
    media_type: string;
    file_size: number;
    date_taken: number | null;
    width: number | null;
    height: number | null;
    duration_seconds: number | null;
    rating: number | null;
    last_rated: number | null;
  } | null>("get_media_meta", { path });

// ---------------------------------------------------------------------------
// Tags
// ---------------------------------------------------------------------------

export const getTags = (path: string) =>
  invoke<{ namespace: string; tag: string }[]>("get_tags", { path });

export const addUserTag = (path: string, tag: string) =>
  invoke<void>("add_user_tag", { path, tag });

export const removeUserTag = (path: string, tag: string) =>
  invoke<void>("remove_user_tag", { path, tag });

export const setRating = (path: string, rating: number) =>
  invoke<void>("set_rating", { path, rating });

export const setColorLabel = (path: string, label: string | null) =>
  invoke<void>("set_color_label", { path, label });

export const setNotes = (path: string, notes: string | null) =>
  invoke<void>("set_notes", { path, notes });

export const addUserTagBatch = (paths: string[], tag: string) =>
  invoke<number>("add_user_tag_batch", { paths, tag });

export const removeUserTagBatch = (paths: string[], tag: string) =>
  invoke<number>("remove_user_tag_batch", { paths, tag });

export const setRatingBatch = (paths: string[], rating: number) =>
  invoke<number>("set_rating_batch", { paths, rating });

// ---------------------------------------------------------------------------
// Filter
// ---------------------------------------------------------------------------

export const applyFilter = (query: string) =>
  invoke<string[]>("apply_filter", { query });

export const clearFilter = () => invoke<string[]>("clear_filter");

// ---------------------------------------------------------------------------
// Geo / map view
// ---------------------------------------------------------------------------

export interface GeoBbox {
  south: number;
  west: number;
  north: number;
  east: number;
}

export interface GeoCluster {
  lat: number;
  lon: number;
  count: number;
  sample_path: string;
}

export interface GeoQueryResult {
  clusters: GeoCluster[];
  total: number;
}

export const getGeoPoints = (
  bbox: GeoBbox,
  zoom: number,
  filter?: string,
) => invoke<GeoQueryResult>("get_geo_points", { bbox, zoom, filter });

export const getGeoPaths = (
  bbox: GeoBbox,
  filter?: string,
) => invoke<string[]>("get_geo_paths", { bbox, filter });

// ---------------------------------------------------------------------------
// Autocomplete
// ---------------------------------------------------------------------------

export const autocompleteTags = (
  query: string,
  namespace?: string,
  limit?: number
) =>
  invoke<TagSuggestion[]>("autocomplete_tags", { query, namespace, limit });

export const getRecentTags = () =>
  invoke<string[]>("get_recent_tags");

// ---------------------------------------------------------------------------
// Sort
// ---------------------------------------------------------------------------

export const getSortedItems = (
  sortField: SortField,
  sortOrder: SortOrder,
  groupBy: GroupBy,
  filterPaths?: string[],
  subSortField?: SortField,
  subSortOrder?: SortOrder,
) =>
  invoke<SortedResult>("get_sorted_items", {
    sortField,
    sortOrder,
    groupBy,
    filterPaths,
    subSortField,
    subSortOrder,
  });

export const getTimelineIndex = (itemsPerRow: number) =>
  invoke<TimelineEntry[]>("get_timeline_index", { itemsPerRow });

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

export const listPlugins = () => invoke<PluginInfo[]>("list_plugins");

export const runPlugin = (
  pluginName: string,
  mediaPath: string,
  action: string
) => invoke<PluginRunResult>("run_plugin", { pluginName, mediaPath, action });

export const runPluginBatch = (
  pluginName: string,
  mediaPaths: string[],
  action: string,
) => invoke<void>("run_plugin_batch", { pluginName, mediaPaths, action });

export const cancelPluginBatch = () => invoke<void>("cancel_plugin_batch");

export const installPlugin = (path: string) =>
  invoke<PluginInfo>("install_plugin", { path });

// ---------------------------------------------------------------------------
// File Operations (Copy / Move / Trash)
// ---------------------------------------------------------------------------

export interface FileOpResult {
  succeeded: string[];
  failed: { path: string; error: string }[];
}

export const copyFiles = (paths: string[], destination: string) =>
  invoke<FileOpResult>("copy_files", { paths, destination });

export const moveFiles = (paths: string[], destination: string) =>
  invoke<FileOpResult>("move_files", { paths, destination });

export const trashFiles = (paths: string[]) =>
  invoke<FileOpResult>("trash_files", { paths });

export const copyFilesToClipboard = (paths: string[]) =>
  invoke<void>("copy_files_to_clipboard", { paths });

// ---------------------------------------------------------------------------
// Duplicate Detection
// ---------------------------------------------------------------------------

export interface DuplicateItem {
  path: string;
  width: number | null;
  height: number | null;
  file_size: number;
  date_taken: number | null;
  is_best: boolean;
}

export interface DuplicateGroup {
  items: DuplicateItem[];
  hash: number;
}

export interface FindDuplicatesResult {
  hashes_computed: number;
  groups: DuplicateGroup[];
}

export const findDuplicates = (threshold?: number) =>
  invoke<FindDuplicatesResult>("find_duplicates", { threshold });

export const markNotDuplicates = (paths: string[]) =>
  invoke<number>("mark_not_duplicates", { paths });

// ---------------------------------------------------------------------------
// Settings / Maintenance
// ---------------------------------------------------------------------------

export const getHardwareProfile = () =>
  invoke<HardwareProfile>("get_hardware_profile");

export const getMemoryStatus = () =>
  invoke<MemoryStatus>("get_memory_status");

export interface RemoteAccessInfo {
  port: number;
  token: string;
  lan_ip: string | null;
  url: string | null;
  clients_seen: number;
  firewall_hint: string | null;
}

export const enableRemoteAccess = (port?: number) =>
  invoke<RemoteAccessInfo>("enable_remote_access", { port });

export const disableRemoteAccess = () =>
  invoke<void>("disable_remote_access");

export const getRemoteAccessInfo = () =>
  invoke<RemoteAccessInfo | null>("get_remote_access_info");

export const reindexGallery = () => invoke<number>("reindex_gallery");

export const rebuildThumbnails = () => invoke<number>("rebuild_thumbnails");

export const regenerateThumbnail = (path: string) =>
  invoke<void>("regenerate_thumbnail", { path });

export const clearCache = () => invoke<void>("clear_cache");

export const getGalleryStats = () =>
  invoke<GalleryStats>("get_gallery_stats");

export const saveGallerySettings = (settingsJson: string) =>
  invoke<void>("save_gallery_settings", { settingsJson });

export const loadGallerySettings = () =>
  invoke<string | null>("load_gallery_settings");

export interface RecentGallery {
  path: string;
  last_opened: number;
}

export const getRecentGalleries = () =>
  invoke<RecentGallery[]>("get_recent_galleries");

export const removeRecentGallery = (path: string) =>
  invoke<void>("remove_recent_gallery", { path });

export const openWith = (command: string, args: string[]) =>
  invoke<void>("open_with", { command, args });

export interface DebugInfo {
  storage_type: string;
  filesystem: string;
  cpu_cores: number;
  total_ram_mb: number;
  gpu_compute: boolean;
  supports_reflink: boolean;
  thumbnail_threads: number;
  prefetch_count: number;
  lru_cache_size: number;
  bc7_atlas_active: boolean;
  thumb_format: string;
  standard_thumb_size: number;
  atlas_entry_count: number;
  sqlite_thumbnail_count: number;
  gpu_resize_active: boolean;
  gdk_backend: string;
  webkit_disable_dmabuf: boolean;
}

export const getDebugInfo = () =>
  invoke<DebugInfo>("get_debug_info");

// ---------------------------------------------------------------------------
// Thumbnail Info
// ---------------------------------------------------------------------------

export interface CachedThumbnailInfo {
  width: number;
  height: number;
  size_bytes: number;
  format: string;
  resize_filter: string;
}

export const getCachedThumbnailInfo = (path: string) =>
  invoke<CachedThumbnailInfo | null>("get_cached_thumbnail_info", { path });

export interface ThumbnailTierInfo {
  tier: string;
  width: number;
  height: number;
  size_bytes: number;
  format: string;
  resize_filter: string | null;
}

export const getAllThumbnailTiers = (path: string) =>
  invoke<ThumbnailTierInfo[]>("get_all_thumbnail_tiers", { path });

// ---------------------------------------------------------------------------
// Viewer (GPU-accelerated transforms)
// ---------------------------------------------------------------------------

export interface ImageTransform {
  rotation_degrees?: number;
  exposure?: number;
  saturation?: number;
  contrast?: number;
}

export const getTransformedMedia = (path: string, transform: ImageTransform) =>
  invoke<string>("get_transformed_media", { path, transform });

export const recordView = (path: string) =>
  invoke<void>("record_view", { path });

// ---------------------------------------------------------------------------
// GPU
// ---------------------------------------------------------------------------

export const notifyGpuCapabilities = (gpuCompute: boolean, bc7Supported: boolean) =>
  invoke<void>("notify_gpu_capabilities", { gpuCompute, bc7Supported });

// ---------------------------------------------------------------------------
// Performance Snapshot (debug overlay)
// ---------------------------------------------------------------------------

export interface PerfSnapshot {
  disk_read_bytes: number;
  disk_write_bytes: number;
  cached_thumbnails: number;
  cache_size_bytes: number;
  atlas_entries: number;
  thumb_pool_active_threads: number;
}

export const getPerfSnapshot = () =>
  _transport<PerfSnapshot>("get_perf_snapshot");
