import { invoke as _rawInvoke } from "@tauri-apps/api/core";
import { isActive as isPerfActive, recordIpcCall } from "./perfMonitor";
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
// Instrumented invoke — records IPC metrics when perf monitor is active
// ---------------------------------------------------------------------------

async function invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  if (!isPerfActive()) {
    return _rawInvoke<T>(cmd, args);
  }
  const argStr = args ? JSON.stringify(args) : "";
  const start = performance.now();
  const result = await _rawInvoke<T>(cmd, args);
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

export type ResizeFilter = "nearest" | "bilinear" | "lanczos3";

export interface ThumbnailResult {
  path: string;
  width: number;
  height: number;
  media_type: string;
  format: string;
}

/** Build a protocol URL for a cached thumbnail. The `lightview://thumb/` protocol
 *  serves image data directly from SQLite — no JSON serialization overhead. */
export function thumbUrl(path: string): string {
  return `lightview://thumb/${encodeURIComponent(path)}`;
}

/** Build a protocol URL for full-resolution media. The `lightview://media/` protocol
 *  serves binary image data directly — no Base64/JSON overhead. */
export function mediaUrl(path: string): string {
  return `lightview://media/${encodeURIComponent(path)}`;
}

export const getThumbnail = (path: string, resizeFilter?: ResizeFilter) =>
  invoke<ThumbnailResult | null>(
    "get_thumbnail",
    { path, resizeFilter }
  );

export const getThumbnailsBatch = (paths: string[], resizeFilter?: ResizeFilter) =>
  invoke<ThumbnailResult[]>(
    "get_thumbnails_batch",
    { paths, resizeFilter }
  );

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
  filterPaths?: string[]
) =>
  invoke<SortedResult>("get_sorted_items", {
    sortField,
    sortOrder,
    groupBy,
    filterPaths,
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
  maxConcurrent?: number,
  onnxThreads?: number,
) => invoke<void>("run_plugin_batch", { pluginName, mediaPaths, action, maxConcurrent, onnxThreads });

export const cancelPluginBatch = () => invoke<void>("cancel_plugin_batch");

export const installPlugin = (path: string) =>
  invoke<PluginInfo>("install_plugin", { path });

// ---------------------------------------------------------------------------
// File Operations (Copy / Move)
// ---------------------------------------------------------------------------

export interface FileOpResult {
  succeeded: string[];
  failed: { path: string; error: string }[];
}

export const copyFiles = (paths: string[], destination: string) =>
  invoke<FileOpResult>("copy_files", { paths, destination });

export const moveFiles = (paths: string[], destination: string) =>
  invoke<FileOpResult>("move_files", { paths, destination });

// ---------------------------------------------------------------------------
// Duplicate Detection
// ---------------------------------------------------------------------------

export interface DuplicateGroup {
  paths: string[];
  hash: number;
}

export interface FindDuplicatesResult {
  hashes_computed: number;
  groups: DuplicateGroup[];
}

export const findDuplicates = (threshold?: number) =>
  invoke<FindDuplicatesResult>("find_duplicates", { threshold });

// ---------------------------------------------------------------------------
// Settings / Maintenance
// ---------------------------------------------------------------------------

export const getHardwareProfile = () =>
  invoke<HardwareProfile>("get_hardware_profile");

export const getMemoryStatus = () =>
  invoke<MemoryStatus>("get_memory_status");

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
  thumb_width: number;
  thumb_height: number;
  atlas_entry_count: number;
  sqlite_thumbnail_count: number;
  gpu_resize_active: boolean;
}

export const getDebugInfo = () =>
  invoke<DebugInfo>("get_debug_info");

// ---------------------------------------------------------------------------
// Thumbnail Settings
// ---------------------------------------------------------------------------

export type ThumbFormat = "jpeg";

export interface ThumbnailSettings {
  /** Output format: "rgba" (no CPU decode, default) or "jpeg" (compressed). */
  format: ThumbFormat;
  /** Thumbnail width in pixels. */
  width: number;
  /** Thumbnail height in pixels. */
  height: number;
  /** Resize algorithm. */
  resize_filter: ResizeFilter;
}

export const getThumbnailSettings = () =>
  invoke<ThumbnailSettings>("get_thumbnail_settings");

export const updateThumbnailSettings = (settings: ThumbnailSettings) =>
  invoke<ThumbnailSettings>("update_thumbnail_settings", { settings });

export interface CachedThumbnailInfo {
  width: number;
  height: number;
  size_bytes: number;
  format: string;
  resize_filter: string;
}

export const getCachedThumbnailInfo = (path: string) =>
  invoke<CachedThumbnailInfo | null>("get_cached_thumbnail_info", { path });

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
  _rawInvoke<PerfSnapshot>("get_perf_snapshot");
