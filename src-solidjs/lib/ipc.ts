import { invoke as _rawInvoke } from "@tauri-apps/api/core";
import { isActive as isPerfActive, recordIpcCall } from "./perfMonitor";
import {
  isTauri,
  NOT_PAIRED_EVENT,
  PASSWORD_CHALLENGE_EVENT,
} from "./runtime";
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

/** Pending password-challenge promise. While set, every 401-LV-Password
 *  response awaits the same prompt instead of stacking modals.
 *  Resolves with `true` if the user supplied the right password, `false`
 *  if they cancelled. */
let _pendingPasswordChallenge: Promise<boolean> | null = null;

function _requestPassword(): Promise<boolean> {
  if (_pendingPasswordChallenge) return _pendingPasswordChallenge;
  _pendingPasswordChallenge = new Promise<boolean>((resolve) => {
    const onResolved = (e: Event) => {
      window.removeEventListener("lightview:password-resolved", onResolved);
      _pendingPasswordChallenge = null;
      resolve(Boolean((e as CustomEvent<boolean>).detail));
    };
    window.addEventListener("lightview:password-resolved", onResolved);
    window.dispatchEvent(new CustomEvent(PASSWORD_CHALLENGE_EVENT));
  });
  return _pendingPasswordChallenge;
}

/** Web-client transport: POST to the read-only `/api/invoke` bridge.
 *
 *  Auth handling:
 *   - The `lv_device` cookie rides along automatically (same-origin).
 *   - 401 with `WWW-Authenticate: LV-Password` → ask the user for the
 *     password, then transparently retry. The caller never sees the 401.
 *   - 401 without that header → device is not paired (or revoked); emit
 *     `NOT_PAIRED_EVENT` so the router can redirect to `/pair`. */
async function _httpInvoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  const body = JSON.stringify({ command: cmd, args: args ?? {} });
  const doFetch = () =>
    fetch("/api/invoke", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body,
    });

  let res = await doFetch();
  if (res.status === 401) {
    const challenge = res.headers.get("www-authenticate") ?? "";
    if (challenge.toLowerCase().includes("lv-password")) {
      const accepted = await _requestPassword();
      if (accepted) {
        res = await doFetch();
      }
    } else {
      window.dispatchEvent(new Event(NOT_PAIRED_EVENT));
    }
  }

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
export type ThumbTier = "s" | "m" | "l" | "p" | "j" | "jm" | "jh";

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
 *  non-http(s) URI scheme, so this single path covers both elements.
 *
 *  `fitEdge` (px) requests an aspect-preserving backend resize to that longest
 *  edge for native raster stills, so the webview decodes a cell-sized image off
 *  its main thread instead of the full original. Omit it for whole-file serving
 *  (video, GIF playback, non-raster formats). */
export function mediaUrl(path: string, fitEdge?: number): string {
  const rel = path.startsWith("/") ? path.slice(1) : path;
  const q = fitEdge && fitEdge > 0 ? `?fit=${Math.round(fitEdge)}` : "";
  // Web client: same-origin HTTP route served by the axum server. Cookie auth.
  if (!isTauri()) {
    return `/media/${encodeMediaPath(rel)}${q}`;
  }
  if (!_mediaServerUrl) {
    _mediaServerUrl = (globalThis as any).__LV_MEDIA_URL__ ?? null;
  }
  if (!_mediaServerUrl) return "";
  return `${_mediaServerUrl}/media/${encodeMediaPath(rel)}${q}`;
}

/** Alias for `mediaUrl`. Kept for the `<video>` callsite where the name
 *  documents intent. */
export const videoSrc = mediaUrl;

/** Build a URL for a GIF frame atlas (PNG sprite sheet + `X-Gif-*` headers),
 *  served only by the axum HTTP server so the frontend can `fetch()` the body
 *  and metadata together. Used for canvas GIF playback — see `GifCanvas`. */
export function gifAtlasUrl(path: string, tier: ThumbTier = "m"): string {
  const rel = path.startsWith("/") ? path.slice(1) : path;
  if (!isTauri()) {
    return `/gif-atlas/${tier}/${encodeMediaPath(rel)}`;
  }
  if (!_mediaServerUrl) {
    _mediaServerUrl = (globalThis as any).__LV_MEDIA_URL__ ?? null;
  }
  if (!_mediaServerUrl) return "";
  return `${_mediaServerUrl}/gif-atlas/${tier}/${encodeMediaPath(rel)}`;
}

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

/** One plugin-namespace tag write, as pushed by a remote tagging worker (a
 * paired machine runs the tagger locally and reports results over HTTP).
 * Same write path as a local plugin run, so `NOT has::plugin.<prefix>`
 * filters see the file as tagged afterwards. Used by `lightview-worker`,
 * not this UI — kept here to document the contract. */
export interface PluginTagWrite {
  path: string;
  tagPrefix: string;
  version: string;
  tags: string[];
  meta?: unknown;
}

export const applyPluginTags = (entries: PluginTagWrite[]) =>
  invoke<FileOpResult>("apply_plugin_tags", { entries });

// ---------------------------------------------------------------------------
// Remote tagging jobs (web-triggered, executed by a paired lightview-worker;
// see docs/workerTagging.md and stores/taggingStore.ts)
// ---------------------------------------------------------------------------

export type TaggingJobState = "queued" | "running" | "done" | "failed" | "cancelled";

export interface TaggingJob {
  id: string;
  pluginName: string;
  tagPrefix: string;
  displayName: string;
  target: { paths: string[] } | { filter: string };
  /** When set, only this worker may claim the job (explicit "run on the
   * server" / "run on worker X" choice). */
  pinnedWorker: string | null;
  state: TaggingJobState;
  /** Fixed when the job is claimed; 0 for still-queued filter jobs. */
  total: number;
  completed: number;
  failed: number;
  claimedBy: string | null;
  workerName: string | null;
  error: string | null;
  createdAt: number;
  updatedAt: number;
}

export interface WorkerStatus {
  workerId: string;
  workerName: string;
  plugins: PluginInfo[];
  lastSeen: number;
  /** True for the in-process executor on the server host itself. */
  local: boolean;
  busyJobId: string | null;
}

export interface TaggingStatus {
  workers: WorkerStatus[];
  jobs: TaggingJob[];
}

export const getTaggingStatus = () => invoke<TaggingStatus>("get_tagging_status");

/** Enqueue a tagging job for a connected worker: either an explicit path list
 * (selection) or a filter query resolved at claim time ("tag all untagged").
 * `workerId` pins the job to one worker — used to choose between the server's
 * own executor and a remote worker when both offer the plugin. */
export const enqueueTaggingJob = (
  pluginName: string,
  target: { paths: string[] } | { filter: string },
  workerId?: string,
) => invoke<TaggingJob>("enqueue_tagging_job", { pluginName, ...target, workerId });

export const cancelTaggingJob = (jobId: string) =>
  invoke<void>("cancel_tagging_job", { jobId });

// ---------------------------------------------------------------------------
// File Operations (Copy / Move / Trash)
// ---------------------------------------------------------------------------

export interface FileOpResult {
  succeeded: string[];
  failed: { path: string; error: string }[];
}

export interface MovedFile {
  from: string;
  to: string;
}

export interface MoveResult {
  /** Files that stayed in the gallery (old path -> new path); re-key the cells. */
  moved: MovedFile[];
  /** Files that left the gallery; remove the cells. */
  removed: string[];
  failed: { path: string; error: string }[];
}

export const copyFiles = (paths: string[], destination: string) =>
  invoke<FileOpResult>("copy_files", { paths, destination });

export const moveFiles = (paths: string[], destination: string) =>
  invoke<MoveResult>("move_files", { paths, destination });

export const trashFiles = (paths: string[]) =>
  invoke<FileOpResult>("trash_files", { paths });

export const copyFilesToClipboard = (paths: string[]) =>
  invoke<void>("copy_files_to_clipboard", { paths });

// ---------------------------------------------------------------------------
// Server capabilities (what a remote client may do; see capabilitiesStore)
// ---------------------------------------------------------------------------

export interface ServerCapabilities {
  metadataWrite: boolean;
  delete: boolean;
  localFs: boolean;
  plugins: boolean;
}

export const getServerCapabilities = () =>
  invoke<ServerCapabilities>("get_server_capabilities");

/** Desktop-only host toggle for the web client's delete capability. */
export const getRemoteDeleteConfig = () => invoke<boolean>("get_remote_delete_config");
export const setRemoteDeleteConfig = (enabled: boolean) =>
  invoke<void>("set_remote_delete_config", { enabled });

/** DOM event dispatched on web after `regenerate_thumbnail` resolves, standing
 * in for the desktop-only `thumb:regenerated` Tauri event so grids cache-bust. */
export const THUMB_REGENERATED_EVENT = "lv:thumb-regenerated";

// ---------------------------------------------------------------------------
// App-managed trash (delete moves into <gallery>/.lightview/trash/)
// ---------------------------------------------------------------------------

export interface TrashEntry {
  id: string;
  /** Gallery-relative path the entry restores to. */
  original_path: string;
  file_name: string;
  /** Unix seconds. */
  deleted_at: number;
  size: number;
}

export const listTrash = () => invoke<TrashEntry[]>("list_trash");

/** Restored paths come back in `succeeded`; the grid refreshes via the
 * fs-changed broadcast, so callers only need this for error reporting. */
export const restoreTrash = (ids: string[]) =>
  invoke<FileOpResult>("restore_trash", { ids });

/** No args = empty the whole trash. */
export const purgeTrash = (ids?: string[], olderThanDays?: number) =>
  invoke<number>("purge_trash", { ids: ids ?? null, olderThanDays: olderThanDays ?? null });

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

export interface MergeGps {
  lat: number;
  lon: number;
  alt: number | null;
}

export interface MergeCandidate {
  path: string;
  width: number | null;
  height: number | null;
  file_size: number;
  mtime: number | null;
  user_tags: string[];
  rating: number | null;
  color_label: string | null;
  notes: string | null;
  companion_location: MergeGps | null;
  exif_location: MergeGps | null;
}

export interface MergePlan {
  keeper: string;
  discard: string[];
  user_tags: string[];
  rating: number | null;
  color_label: string | null;
  notes: string | null;
  location: MergeGps | null;
  set_mtime: number | null;
}

export const getMergeCandidates = (paths: string[]) =>
  invoke<MergeCandidate[]>("get_merge_candidates", { paths });

export const mergeDuplicates = (plan: MergePlan) =>
  invoke<void>("merge_duplicates", { plan });

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
  lan_ip: string | null;
  /** Base URL of the running server (no path) — e.g. `https://192.168.0.5:8723`. */
  base_url: string | null;
  clients_seen: number;
  firewall_hint: string | null;
}

export interface RemoteDeviceInfo {
  id: string;
  name: string;
  created_at: number;
  last_seen: number;
  last_auth_at: number;
  revoked_at: number | null;
}

export interface RemoteAuthState {
  devices: RemoteDeviceInfo[];
  password_set: boolean;
  inactivity_secs: number;
}

export interface PairingCode {
  code: string;
  kind: "qr" | "pin";
  expires_at: number;
  /** URL embedded in the QR code (or pair-page link for PIN flow). Null when
   *  the server is running but no LAN IP could be detected. */
  pairing_url: string | null;
}

export const enableRemoteAccess = (port?: number) =>
  invoke<RemoteAccessInfo>("enable_remote_access", { port });

export const disableRemoteAccess = () =>
  invoke<void>("disable_remote_access");

export const getRemoteAccessInfo = () =>
  invoke<RemoteAccessInfo | null>("get_remote_access_info");

export const getRemoteAuthState = () =>
  invoke<RemoteAuthState>("get_remote_auth_state");

export const generatePairingCode = (kind: "qr" | "pin") =>
  invoke<PairingCode>("generate_pairing_code", { kind });

export const revokeRemoteDevice = (deviceId: string) =>
  invoke<void>("revoke_remote_device", { deviceId });

export const deleteRemoteDevice = (deviceId: string) =>
  invoke<void>("delete_remote_device", { deviceId });

export const setRemotePassword = (password: string) =>
  invoke<void>("set_remote_password", { password });

export const clearRemotePassword = () =>
  invoke<void>("clear_remote_password");

export const setRemoteInactivity = (secs: number) =>
  invoke<void>("set_remote_inactivity", { secs });

// ---------------------------------------------------------------------------
// Device uploads (web client → host gallery)
// ---------------------------------------------------------------------------

/** How uploaded files are foldered under `Uploads/` on the host. Mirrors the
 *  Rust `UploadScheme`. */
export type UploadScheme = "year" | "year_month" | "year_album" | "flat";

export interface UploadConfig {
  enabled: boolean;
  scheme: UploadScheme;
}

/** Works in both modes: Tauri command on desktop, `/api/invoke` bridge on the
 *  web client (the only upload-related command on the read-only allowlist). */
export const getUploadConfig = () => invoke<UploadConfig>("get_upload_config");

/** Host-only (desktop) — sets the per-gallery upload config. */
export const setUploadConfig = (enabled: boolean, scheme: UploadScheme) =>
  invoke<void>("set_upload_config", { enabled, scheme });

export interface UploadResult {
  uploaded: { original: string; stored: string }[];
  rejected: { original: string; reason: string }[];
}

/** Low-level POST to `/api/upload` with upload-progress reporting. Uses
 *  XMLHttpRequest because `fetch` can't report request-body upload progress. */
function _postUpload(
  form: FormData,
  onProgress?: (fraction: number) => void,
): Promise<{ status: number; auth: string; text: string }> {
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    xhr.open("POST", "/api/upload");
    if (onProgress && xhr.upload) {
      xhr.upload.onprogress = (e) => {
        if (e.lengthComputable) onProgress(e.loaded / e.total);
      };
    }
    xhr.onload = () =>
      resolve({
        status: xhr.status,
        auth: xhr.getResponseHeader("www-authenticate") ?? "",
        text: xhr.responseText,
      });
    xhr.onerror = () => reject(new Error("network error during upload"));
    xhr.send(form);
  });
}

/** Upload media files from the web client into the host gallery. Web-only.
 *
 *  `album` is honored only by the host's `year_album` scheme; it is appended
 *  *before* the files so the server can apply it to them (the backend reads
 *  fields in order). 401 handling mirrors `_httpInvoke`: an `LV-Password`
 *  challenge prompts and retries once; any other 401 means the device is no
 *  longer paired. */
export async function uploadFiles(
  files: File[],
  album?: string,
  onProgress?: (fraction: number) => void,
): Promise<UploadResult> {
  const buildForm = () => {
    const form = new FormData();
    if (album && album.trim()) form.append("album", album.trim());
    for (const f of files) form.append("file", f, f.name);
    return form;
  };

  let res = await _postUpload(buildForm(), onProgress);
  if (res.status === 401) {
    if (res.auth.toLowerCase().includes("lv-password")) {
      const accepted = await _requestPassword();
      if (accepted) res = await _postUpload(buildForm(), onProgress);
    } else {
      window.dispatchEvent(new Event(NOT_PAIRED_EVENT));
    }
  }
  if (res.status < 200 || res.status >= 300) {
    throw new Error(`upload failed (${res.status}): ${res.text || ""}`);
  }
  return JSON.parse(res.text) as UploadResult;
}

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

/** Process-level rendering prefs (read at startup; changes need a restart).
 *  `null` for a field means "use the built-in default". */
export interface RenderConfig {
  gpu_acceleration: boolean | null;
  gtk_backend: string | null;
}

export const getRenderConfig = () =>
  invoke<RenderConfig>("get_render_config");

export const setRenderConfig = (
  gpuAcceleration: boolean | null,
  gtkBackend: string | null,
) => invoke<void>("set_render_config", { gpuAcceleration, gtkBackend });

/** Gallery-wide default filter (shared by desktop and LAN web clients). */
export const getGalleryDefaultFilter = () =>
  invoke<{ enabled: boolean; query: string } | null>("get_gallery_default_filter");

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
  supports_reflink: boolean;
  thumbnail_threads: number;
  prefetch_count: number;
  lru_cache_size: number;
  standard_thumb_size: number;
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
// Performance Snapshot (debug overlay)
// ---------------------------------------------------------------------------

export interface PerfSnapshot {
  disk_read_bytes: number;
  disk_write_bytes: number;
  cached_thumbnails: number;
  cache_size_bytes: number;
  thumb_pool_active_threads: number;
}

export const getPerfSnapshot = () =>
  _transport<PerfSnapshot>("get_perf_snapshot");
