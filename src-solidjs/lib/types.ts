// The wire, mirrored. Every shape here has a counterpart in the Rust crate, and
// the two ship together — there is no version skew to design around.

// ---------------------------------------------------------------------------
// Companion file (mirrors companion::schema)
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
  /** Set membership. A sibling of `user`, not a plugin bucket: a plugin bucket
   *  is versioned and replaced wholesale on a re-run, which is right for
   *  geocoded place names and exactly wrong for a set, which is user-owned and
   *  must survive re-tagging. */
  set: string[];
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
  date_rated?: string;
  color_label?: string;
  notes?: string;
  media?: MediaInfo;
  /** Mirrored from the index so a cache rebuild loses time and nothing else. */
  date_added?: string;
  last_viewed?: string;
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
// Trust (mirrors server::auth::Trust)
// ---------------------------------------------------------------------------

/** What this listener grants. `owner` only ever comes from a loopback bind. */
export type Trust = "device" | "owner";

/** What the client is told about itself, so the UI does not offer what the
 *  server will refuse. The server enforces regardless; this exists only so the
 *  UI does not lie. */
export interface Capabilities {
  trust: Trust;
  upload: boolean;
  /** A runtime question rather than a compile-time one: the X11 clipboard
   *  backend fails on a Wayland session without XWayland, and on a process with
   *  no display at all. */
  clipboard: boolean;
}

// ---------------------------------------------------------------------------
// Filter (mirrors filter::ast)
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

/** `auto` is gone and `set` has arrived. The enum is serialized in both
 *  directions, so this is a wire change rather than only a parser change. */
export type TagNamespace = "user" | "set" | `plugin.${string}` | "any";

/** The two namespaces a person may write to. A plugin bucket is replaced
 *  wholesale by its own run, so it is never writable this way — and the Rust
 *  type has no variant for one, so a request naming it fails to deserialize
 *  rather than reaching a check. */
export type WritableNamespace = "user" | "set";

// ---------------------------------------------------------------------------
// Sort and group (mirrors sort::)
// ---------------------------------------------------------------------------

export type SortField =
  | "date"
  | "size"
  | "name"
  | "rating"
  | "mediatype"
  | "lastviewed"
  | "dateadded"
  | "lastrated";
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

/** The whole grid payload: one query, filter compiled in, groups computed. */
export interface Items {
  items: SortedItem[];
  groups: GroupHeader[];
}

export interface SortedItem {
  /** Gallery-relative, because the database is keyed that way. */
  path: string;
  /** What the grid is ordered and grouped by: capture time when the file has
   *  one, file modification time when it does not — so it is never null. This
   *  is deliberately *not* `date_taken`; that is the camera's timestamp and is
   *  what `date=` filters mean. A screenshot has a date here and no capture
   *  time anywhere. */
  date: number | null;
  file_size: number;
  media_type: string;
  rating: number | null;
  /** Colour label, lowercase, or null. */
  color_label: string | null;
  last_viewed: number | null;
  date_added: number | null;
  last_rated: number | null;
  duration?: number | null;
  width?: number | null;
  height?: number | null;
  /** Base64 ThumbHash (~25 bytes decoded), or null until a thumbnail has been
   *  generated once. Inlined here so the grid paints every cell blurry before
   *  any thumbnail request goes out. */
  thumbhash?: string | null;
}

// ---------------------------------------------------------------------------
// Item detail
// ---------------------------------------------------------------------------

export interface MediaMeta {
  path: string;
  media_type: string;
  file_size: number;
  /** The camera's capture time, or null when the file carries none. */
  date_taken: number | null;
  /** The file's modification time — always present, and what the panel shows
   *  (labelled as such) when there is no capture time to show instead. */
  mtime: number;
  date_added: number | null;
  last_viewed: number | null;
  rating: number | null;
  color_label: string | null;
  width: number | null;
  height: number | null;
  duration: number | null;
  gps: [number, number] | null;
  /** `[namespace, tag]` pairs, as the index holds them. */
  tags: [string, string][];
  notes: string | null;
}

export type ThumbTier = "js" | "j" | "jm" | "jh";

export interface TierPresence {
  tier: ThumbTier;
  edge: number;
  bytes: number | null;
}

// ---------------------------------------------------------------------------
// Trash
// ---------------------------------------------------------------------------

export interface TrashEntry {
  /** `<epoch_ms>_<seq>` — digits and underscores, never a path. Keeping it
   *  opaque is a security requirement: an id that could carry slashes forces
   *  the removal of the check that stops a restore escaping the trash. */
  id: string;
  /** Where it came from, in its own field. */
  relative_path: string;
  file_name: string;
  deleted_at: number;
  size: number;
}

// ---------------------------------------------------------------------------
// Duplicates
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

/** A fully-resolved merge. The dialog resolves the conflicts; the backend
 *  applies the answer. */
export interface MergePlan {
  keeper: string;
  others: string[];
  rating?: number | null;
  color_label?: string | null;
  notes?: string | null;
  location?: { lat: number; lon: number; alt?: number } | null;
  mtime?: number | null;
}

// ---------------------------------------------------------------------------
// Autocomplete
// ---------------------------------------------------------------------------

export interface TagSuggestion {
  namespace: string;
  tag: string;
  count: number;
  score: number;
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/** `.lightview/settings.toml` — durable, and it holds exactly two keys.
 *
 *  Display preferences are **not** here, for any client. A file inside the
 *  gallery is per-gallery, not per-client, so two desktops mounting one share
 *  would fight over thumbnail size. They live in `clientPrefs`. */
export interface GallerySettings {
  /** Applied when the gallery is opened, by any client. */
  default_filter: string;
  /** Hand-edited only: it is the one setting that deletes data, so no command
   *  writes it at any trust level. */
  trash_retention_days: number;
}

/** An external application, as the client sees it. The `command` is never sent
 *  to the client and never accepted from it — `open_with` carries an index. */
export interface ExternalApp {
  label: string;
}

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

export interface PluginInfo {
  name: string;
  display_name: string;
  version: string;
  api_version: number;
  description: string;
  tag_prefix: string;
}

// ---------------------------------------------------------------------------
// Events (mirrors server::events)
// ---------------------------------------------------------------------------

export type Domain = "items" | "tags" | "jobs";

export type ServerEvent =
  | { kind: "fs-changed"; added: string[]; removed: string[] }
  /** Plural, and one per operation rather than one per file: every write in
   *  the system takes a selection, and a 500-photo tag used to be 500 events
   *  answered with 500 metadata calls. */
  | { kind: "items-changed"; paths: string[] }
  | { kind: "tags-indexed" }
  | { kind: "job-progress"; plugin: string; done: number; total: number }
  | { kind: "job-finished"; plugin: string; error: string | null }
  /** You may have missed something in these domains. Re-fetch exactly them. */
  | { kind: "resync"; domains: Domain[] };
