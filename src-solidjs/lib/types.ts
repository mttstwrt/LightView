// ---------------------------------------------------------------------------
// Companion file types (mirrors Rust companion::schema)
// ---------------------------------------------------------------------------

export interface CompanionFile {
  schema_version: number;
  file: string;
  file_hash: string;
  media_type: MediaType;
  created: string;
  modified: string;
  tags: TagCollection;
  meta: MetaCollection;
}

export type MediaType = "image" | "video" | "gif";

export interface TagCollection {
  user: string[];
  auto: string[];
  plugins: Record<string, PluginTagEntry>;
}

export interface PluginTagEntry {
  version: string;
  tags: string[];
  [key: string]: unknown;
}

export interface MetaCollection {
  core?: CoreMeta;
  plugins: Record<string, unknown>;
}

export interface CoreMeta {
  rating?: number;
  color_label?: string;
  notes?: string;
  media?: MediaInfo;
}

export interface MediaInfo {
  width: number;
  height: number;
  duration_seconds?: number;
  codec?: string;
  has_audio?: boolean;
  fps?: number;
}

// ---------------------------------------------------------------------------
// Gallery types
// ---------------------------------------------------------------------------

export interface GalleryMediaItem {
  path: string;
  name: string;
  media_type: MediaType;
  thumbnail_url: string | null;
  size: number;
  mtime: number;
  date_taken?: number;
  rating?: number;
}

export interface GalleryOpenResult {
  path: string;
  total_media: number;
  provider_type: string;
}

// ---------------------------------------------------------------------------
// Filter types (mirrors Rust filter::ast)
// ---------------------------------------------------------------------------

export type FilterExpr =
  | { type: "tag"; namespace: TagNamespace; value: string }
  | { type: "and"; left: FilterExpr; right: FilterExpr }
  | { type: "or"; left: FilterExpr; right: FilterExpr }
  | { type: "not"; expr: FilterExpr }
  | { type: "rating"; op: "gte" | "lte" | "eq"; value: number }
  | { type: "media_type"; value: MediaType }
  | { type: "has_namespace"; namespace: TagNamespace }
  | { type: "color_label"; value: string };

export type TagNamespace = "user" | "auto" | `plugin.${string}` | "any";

// ---------------------------------------------------------------------------
// Sort and group types (mirrors Rust sort module)
// ---------------------------------------------------------------------------

export type SortField = "date" | "size" | "name" | "rating" | "media_type" | "lastviewed" | "dateadded" | "lastrated";
export type SortOrder = "asc" | "desc";

export type GroupBy =
  | { type: "time_period"; granularity: "day" | "month" | "year" }
  | { type: "media_type" }
  | { type: "size_range" }
  | { type: "tag"; namespace: string; tag_prefix: string }
  | { type: "none" };

export interface GroupHeader {
  label: string;
  start_index: number;
  count: number;
}

export interface SortedResult {
  items: SortedItem[];
  groups: GroupHeader[];
}

export interface SortedItem {
  path: string;
  date_taken: number | null;
  file_size: number;
  media_type: string;
  rating: number | null;
  last_viewed: number | null;
  date_added: number | null;
  last_rated: number | null;
}

// ---------------------------------------------------------------------------
// Timeline types
// ---------------------------------------------------------------------------

export interface TimelineEntry {
  row_index: number;
  date: number;
  label: string;
}

// ---------------------------------------------------------------------------
// Autocomplete types
// ---------------------------------------------------------------------------

export interface TagSuggestion {
  namespace: string;
  tag: string;
  count: number;
  score: number;
}

// ---------------------------------------------------------------------------
// Hardware types
// ---------------------------------------------------------------------------

export interface HardwareProfile {
  storage_type: string;
  filesystem: string;
  supports_reflink: boolean;
  cpu_cores: number;
  gpu_compute: boolean;
  total_ram_mb: number;
}

export interface MemoryStatus {
  total_ram_mb: number;
  available_ram_mb: number;
}

// ---------------------------------------------------------------------------
// Settings types
// ---------------------------------------------------------------------------

export type CompanionLocation = "lightview_folder" | "alongside";
export type RendererMode = "dom" | "canvas" | "webgl";

export interface AppSettings {
  display: {
    thumbnail_size: number;
    grid_gap: number;
    background_color: string;
    video_hover_preview: boolean;
    video_autoplay_loop: boolean;
    gif_autoplay_grid: boolean;
    scroll_blur: boolean;
    renderer_mode: RendererMode;
    map_dark_mode: boolean;
  };
  performance: {
    preload_count: number;
    lru_cache_size: number;
    thumbnail_threads: number;
  };
  storage: {
    companion_location: CompanionLocation;
  };
  external_apps: ExternalApp[];
}

export interface ExternalApp {
  label: string;
  command: string;
  args: string[];
}

// ---------------------------------------------------------------------------
// Plugin types
// ---------------------------------------------------------------------------

export interface PluginInfo {
  name: string;
  display_name: string;
  version: string;
  description: string;
  tag_prefix: string;
}

export interface PluginRunResult {
  path: string;
  tags_added: string[];
  success: boolean;
  error: string | null;
}

export interface GalleryStats {
  total_media: number;
  index_size_bytes: number;
  cache_size_bytes: number;
  unique_tags: number;
}
