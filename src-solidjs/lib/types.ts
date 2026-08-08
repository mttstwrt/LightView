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

export interface GalleryOpenResult {
  path: string;
  total_media: number;
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
  /** Colour label, lowercase, or null. One of `COLOR_LABELS`. */
  color_label: string | null;
  last_viewed: number | null;
  date_added: number | null;
  last_rated: number | null;
  duration?: number | null;
  width?: number | null;
  height?: number | null;
  /** Base64 ThumbHash placeholder (~25 bytes decoded); null until the item's
   *  thumbnail has been generated at least once. Decoded client-side into a
   *  blurry data-URL placeholder — see lib/thumbhashPlaceholder.ts. */
  thumbhash?: string | null;
}

// ---------------------------------------------------------------------------
// Timeline types
// ---------------------------------------------------------------------------

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

export interface MemoryStatus {
  total_ram_mb: number;
  available_ram_mb: number;
}

// ---------------------------------------------------------------------------
// Settings types
// ---------------------------------------------------------------------------

export type CompanionLocation = "lightview_folder" | "alongside";

export interface AppSettings {
  display: {
    thumbnail_size: number;
    /** Minimum thumbnail/row size (px) the zoom control allows. */
    thumb_size_min: number;
    /** Maximum thumbnail/row size (px) the zoom control allows. */
    thumb_size_max: number;
    grid_gap: number;
    background_color: string;
    video_hover_preview: boolean;
    video_autoplay_loop: boolean;
    gif_autoplay_grid: boolean;
    /** Autoplay short videos in the grid (muted, looping), like GIFs. */
    video_autoplay_grid: boolean;
    /** Max duration (seconds) a video may have to qualify for grid autoplay. */
    video_autoplay_max_seconds: number;
    scroll_blur: boolean;
    map_dark_mode: boolean;
    /** Serve a 1600px aspect-preserving tier in the justified view when zoomed
     *  in, instead of upscaling the 512px tier. Generated for visible cells
     *  only, so disk cost scales with what you actually view zoomed in. */
    justified_high_detail: boolean;
    /** Where the mobile filter/sort sheet (opened by the search button)
     *  appears: pinned to the top under the safe-area inset, or as a
     *  thumb-reachable sheet sliding up from the bottom. Mobile web only. */
    mobile_filter_sheet: "top" | "bottom";
    /** Start playing a video automatically when you settle on it in the
     *  viewer (always muted, with a tap-to-unmute pill). Off = tap play. */
    video_autoplay_viewer: boolean;
    /** Open a gallery scrolled to the *end* of the grid rather than the start,
     *  so browsing runs bottom-to-top. Only changes where the view lands; the
     *  sort order itself is unaffected (flip that in the sort menu). */
    start_at_bottom: boolean;
  };
  performance: {
    preload_count: number;
    lru_cache_size: number;
    thumbnail_threads: number;
  };
  storage: {
    companion_location: CompanionLocation;
  };
  // Filter query applied automatically when a gallery is first opened (app or web).
  default_filter: {
    enabled: boolean;
    query: string;
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
