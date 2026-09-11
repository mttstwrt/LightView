// The only module that talks to the backend.
//
// There is one transport. `isTauri()`, `safeListen` and the dual-default
// capabilities store are gone with the second runtime: everything is
// `POST /api/invoke` plus the media and thumbnail routes.
//
// Two behaviours are absorbed here so they never leak to callers, and each one
// is a bug the previous arrangement had:
//
//  1. A `401` carrying `WWW-Authenticate: LV-Password` raises a challenge,
//     waits for the modal, and retries — and **concurrent 401s share one
//     pending promise**. Without that, a grid firing twenty requests produces
//     twenty stacked modals.
//  2. A `401` *without* that header means the credential is missing or
//     revoked. On a served bind that is "not paired" and the client goes to
//     pairing. **On a loopback bind it is a dead end**: there is no pairing
//     flow, and a browser cannot read `instance.json` to find the new URL —
//     that is exactly the filesystem access the trust model exists to
//     withhold. So it reports that the session has ended and a new one will
//     open a new tab.

import type {
  Capabilities,
  DuplicateGroup,
  GallerySettings,
  Items,
  MediaMeta,
  MergePlan,
  PluginInfo,
  SortedItem,
  TagSuggestion,
  ThumbTier,
  TierPresence,
  TrashEntry,
  WritableNamespace,
} from "./types";

// ---------------------------------------------------------------------------
// Paths on the wire
// ---------------------------------------------------------------------------

/** Percent-encode each segment independently and leave `/` literal.
 *
 *  axum decodes captures but rejects paths containing raw encoded slashes, so a
 *  single `encodeURIComponent` over the whole path 404s every file in a
 *  subdirectory. This is the rule that travels with gallery-relative paths. */
export function encodePath(path: string): string {
  return path.split("/").map(encodeURIComponent).join("/");
}

export function thumbUrl(path: string, tier: ThumbTier): string {
  return `/thumb/${tier}/${encodePath(path)}`;
}

export function mediaUrl(path: string, fit?: number): string {
  const base = `/media/${encodePath(path)}`;
  return fit ? `${base}?fit=${fit}` : base;
}

// ---------------------------------------------------------------------------
// Auth interruptions
// ---------------------------------------------------------------------------

export type AuthInterruption =
  /** Present the password modal; resolve the returned promise with the answer,
   *  or reject to give up. */
  | { kind: "password" }
  /** No usable credential, and there is a pairing flow to go to. */
  | { kind: "not-paired" }
  /** No usable credential, and there is not. The process restarted. */
  | { kind: "session-ended" };

type Listener = (interruption: AuthInterruption) => void;
const listeners = new Set<Listener>();

/** Subscribe to auth interruptions. `App` wires the modal and the banner. */
export function onAuthInterruption(listener: Listener): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

function announce(interruption: AuthInterruption) {
  for (const listener of listeners) listener(interruption);
}

/** Resolved by the password modal. One at a time, shared by every caller that
 *  hit a challenge while it was open. */
let pendingPassword: Promise<boolean> | null = null;
let resolvePassword: ((accepted: boolean) => void) | null = null;

/** Called by the modal when the user submits or cancels. */
export function answerPasswordChallenge(accepted: boolean) {
  resolvePassword?.(accepted);
  resolvePassword = null;
  pendingPassword = null;
}

function challenge(): Promise<boolean> {
  // The shared promise. Twenty concurrent 401s raise one modal.
  if (!pendingPassword) {
    pendingPassword = new Promise<boolean>((resolve) => {
      resolvePassword = resolve;
    });
    announce({ kind: "password" });
  }
  return pendingPassword;
}

/** Whether this client is talking to a bind that has a pairing flow at all. */
let hasPairing = true;
export function setHasPairing(value: boolean) {
  hasPairing = value;
}

// ---------------------------------------------------------------------------
// The one call
// ---------------------------------------------------------------------------

export class IpcError extends Error {
  constructor(
    message: string,
    readonly status: number,
  ) {
    super(message);
  }
}

async function post(command: string, args: unknown): Promise<Response> {
  return fetch("/api/invoke", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ command, args: args ?? {} }),
  });
}

export async function invoke<T>(command: string, args?: unknown): Promise<T> {
  let response = await post(command, args);

  if (response.status === 401) {
    if (response.headers.get("www-authenticate") === "LV-Password") {
      const accepted = await challenge();
      if (!accepted) throw new IpcError("password required", 401);
      response = await post(command, args);
    } else {
      announce(hasPairing ? { kind: "not-paired" } : { kind: "session-ended" });
      throw new IpcError("not authenticated", 401);
    }
  }

  if (!response.ok) {
    const body = await response.text();
    let message = body;
    try {
      message = (JSON.parse(body) as { error?: string }).error ?? body;
    } catch {
      // A non-JSON body is a middleware refusal (503, 403), which already
      // reads as prose.
    }
    throw new IpcError(message || response.statusText, response.status);
  }
  return (await response.json()) as T;
}

// ---------------------------------------------------------------------------
// Bootstrap
// ---------------------------------------------------------------------------

export interface AuthStatus {
  trust: "device" | "owner";
  password_required: boolean;
  pairing: boolean;
}

export async function authStatus(): Promise<AuthStatus> {
  const response = await fetch("/auth/status");
  const status = (await response.json()) as AuthStatus;
  setHasPairing(status.pairing);
  return status;
}

/** Exchange `?t=<token>` for the session cookie.
 *
 *  Single use and rotated on redemption, so the caller must clear it from the
 *  address bar immediately — see `index.tsx`. */
export async function redeemLaunchToken(token: string): Promise<boolean> {
  const response = await fetch("/auth/launch", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ token }),
  });
  return response.ok;
}

export async function redeemPairingCode(
  code: string,
  name: string,
): Promise<boolean> {
  const response = await fetch("/pair/redeem", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ code, name }),
  });
  return response.ok;
}

export async function submitPassword(password: string): Promise<boolean> {
  const response = await fetch("/auth/password", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ password }),
  });
  return response.ok;
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

export const api = {
  capabilities: () => invoke<Capabilities>("get_capabilities"),

  items: (request: {
    sort?: string;
    order?: string;
    sub_sort?: string | null;
    sub_order?: string | null;
    filter?: string;
    group_by?: unknown;
  }) => invoke<Items>("get_items", request),

  mediaMeta: (path: string) =>
    invoke<MediaMeta | null>("get_media_meta", { path }),

  tiers: (path: string) =>
    invoke<TierPresence[]>("get_all_thumbnail_tiers", { path }),

  tierTotals: () =>
    invoke<{ tier: ThumbTier; bytes: number }[]>("get_tier_totals"),

  autocomplete: (query: string, namespace?: string, limit = 20) =>
    invoke<TagSuggestion[]>("autocomplete", { query, namespace, limit }),

  settings: () => invoke<GallerySettings>("get_settings"),

  setDefaultFilter: (filter: string) =>
    invoke<GallerySettings>("set_default_filter", { filter }),

  /** Every tag in one writable namespace, most-used first. The tag manager's
   *  list; answered from the same in-memory vocabulary autocomplete queries. */
  listTags: (namespace: WritableNamespace) =>
    invoke<{ namespace: string; tag: string; count: number }[]>("list_tags", {
      namespace,
    }),

  /** A capped sample of the files a tag selection covers, so a gallery-wide
   *  rewrite can be confirmed against something more than a count. */
  pathsWithTags: (tags: string[], namespace: WritableNamespace, limit = 120) =>
    invoke<string[]>("paths_with_tags", { tags, namespace, limit }),

  // --- Tag writes. Every one names a namespace of `user` or `set`. ---
  addTags: (paths: string[], tags: string[], namespace: WritableNamespace) =>
    invoke<{ changed: number }>("add_tags", { paths, tags, namespace }),

  removeTags: (paths: string[], tags: string[], namespace: WritableNamespace) =>
    invoke<{ changed: number }>("remove_tags", { paths, tags, namespace }),

  renameTag: (from: string, to: string, namespace: WritableNamespace) =>
    invoke<{ changed: number }>("rename_tag", { from, to, namespace }),

  mergeTags: (sources: string[], target: string, namespace: WritableNamespace) =>
    invoke<{ changed: number }>("merge_tags", { sources, target, namespace }),

  deleteTag: (tag: string, namespace: WritableNamespace) =>
    invoke<{ changed: number }>("delete_tag", { tag, namespace }),

  /** A selection, always — rating one photo is a selection of one, and the
   *  server answers with a single `items-changed` however many there are. */
  setRating: (paths: string[], rating: number | null) =>
    invoke<void>("set_rating", { paths, rating }),

  setColorLabel: (paths: string[], color_label: string | null) =>
    invoke<void>("set_color_label", { paths, color_label }),

  setNotes: (path: string, notes: string | null) =>
    invoke<void>("set_notes", { path, notes }),

  recordView: (path: string) => invoke<void>("record_view", { path }),

  // --- Thumbnails ---
  regenerate: (paths: string[]) =>
    invoke<void>("regenerate_thumbnail", { paths }),

  precache: (tier: ThumbTier, paths: string[]) =>
    invoke<void>("precache_thumbnails", { tier, paths }),

  // --- Trash ---
  trash: (paths: string[]) => invoke<{ entry: string }>("trash_files", { paths }),

  listTrash: () => invoke<TrashEntry[]>("list_trash"),

  /** Both fields, always: the id names the entry and the path names the
   *  destination, and neither is derived from the other. */
  restoreTrash: (id: string, relative_path: string) =>
    invoke<void>("restore_trash", { id, relative_path }),

  /** `Owner` only — permanent deletion is not "move to trash". */
  purgeTrash: (entry?: string) => invoke<{ purged: number }>("purge_trash", { entry }),

  // --- Duplicates ---
  findDuplicates: (threshold?: number) =>
    invoke<DuplicateGroup[]>("find_duplicates", { threshold }),

  /** Every copy in a group, in one round trip. The same row shape the info
   *  panel reads — a merge candidate is a row plus its tags and notes. */
  mergeCandidates: (paths: string[]) =>
    invoke<MediaMeta[]>("get_merge_candidates", { paths }),

  /** `Owner` only — it rewrites a companion, stamps an mtime and trashes
   *  files. A remote client may find duplicates and not resolve them. */
  mergeDuplicates: (plan: MergePlan) =>
    invoke<{ keeper: string; trashed: number; trash_entry: string }>(
      "merge_duplicates",
      plan,
    ),

  // --- Owner-only filesystem operations ---
  copyFiles: (paths: string[], destination: string) =>
    invoke<{ count: number }>("copy_files", { paths, destination }),

  moveFiles: (paths: string[], destination: string) =>
    invoke<{ count: number }>("move_files", { paths, destination }),

  clipboardFiles: (paths: string[], cut = false) =>
    invoke<void>("clipboard_files", { paths, cut }),

  /** An **index** into server-side configuration. No request can name a
   *  program. */
  openWith: (app_index: number, path: string) =>
    invoke<void>("open_with", { app_index, path }),

  externalApps: () => invoke<{ label: string }[]>("list_external_apps"),

  /** One level of the directory picker. The parent comes back with the
   *  listing rather than being computed here — a browser doing its own string
   *  surgery on a path is how a picker ends up asking for a file. */
  listDirs: (path?: string) =>
    invoke<{
      path: string;
      parent: string | null;
      entries: { name: string; path: string }[];
    }>("list_dirs", { path }),

  // --- Plugins ---
  listPlugins: () => invoke<PluginInfo[]>("list_plugins"),

  /** Run a plugin over a selection. Progress arrives as `job-progress` events
   *  and the terminal `job-finished`, never as a return value: a run over a
   *  thousand files outlives any request. */
  runPlugin: (plugin: string, paths: string[]) =>
    invoke<{ started: boolean }>("run_plugin", { plugin, paths }),
};

// ---------------------------------------------------------------------------
// Uploads
// ---------------------------------------------------------------------------

/** Upload files, reporting progress as a fraction.
 *
 *  The one `XMLHttpRequest` in the codebase, and it is here for a reason
 *  `fetch` cannot answer: `fetch` exposes no upload progress at all. The
 *  streaming-request alternative (`ReadableStream` body with `duplex: "half"`)
 *  is Chromium-only and needs HTTP/2, so it is not an option for the phone this
 *  exists for — and a phone pushing a four-gigabyte clip over Wi-Fi with no
 *  indication of progress looks hung. */
export function upload(
  files: File[],
  onProgress?: (fraction: number) => void,
): Promise<string[]> {
  const form = new FormData();
  for (const file of files) form.append("file", file, file.name);

  return new Promise((resolve, reject) => {
    const request = new XMLHttpRequest();
    request.open("POST", "/api/upload");
    request.upload.onprogress = (e) => {
      if (e.lengthComputable) onProgress?.(e.loaded / e.total);
    };
    request.onload = () => {
      if (request.status < 200 || request.status >= 300) {
        reject(new IpcError(request.responseText, request.status));
        return;
      }
      try {
        resolve((JSON.parse(request.responseText) as { uploaded: string[] }).uploaded);
      } catch (e) {
        reject(new IpcError(String(e), request.status));
      }
    };
    request.onerror = () => reject(new IpcError("upload failed", 0));
    request.send(form);
  });
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/** Subscribe to the server's one event stream.
 *
 *  `onopen` re-fetches boot state rather than replaying history, and that is a
 *  requirement rather than a nicety: `EventSource` reconnects silently, and on
 *  a phone that happens constantly — screen lock, Wi-Fi to LTE, backgrounding.
 *  Without it the client sits on a confidently wrong grid indefinitely. */
export function subscribe(
  onEvent: (event: import("./types").ServerEvent) => void,
  onReconnect: () => void,
): () => void {
  let opened = false;
  const source = new EventSource("/api/events");

  source.onopen = () => {
    // The first open is the initial load, which the caller has already done.
    if (opened) onReconnect();
    opened = true;
  };
  source.onmessage = (message) => {
    try {
      onEvent(JSON.parse(message.data));
    } catch {
      // A malformed frame is not worth tearing the stream down for.
    }
  };

  return () => source.close();
}

export type { SortedItem };
