// The open gallery: its items, its groups, and what is selected.
//
// **One list, one query.** `displayPaths` used to be a client-side filter over
// `sortedItems`, kept separate so changing the sort did not re-run the filter.
// The filter now compiles into the same statement the sort orders, so there is
// one payload and the two signals collapse into one — which also deletes the
// round trip where the client handed a list of matched paths straight back to
// the server to be re-expanded.
//
// **One view.** `viewMode`, `enabledViews` and the switcher go with the square
// grid and the map: there is one grid and it is justified.

import { createMemo, createSignal } from "solid-js";

import { api } from "../lib/ipc";
import type { GroupBy, GroupHeader, ServerEvent, SortedItem } from "../lib/types";

const [galleryPath, setGalleryPath] = createSignal<string | null>(null);
const [loading, setLoading] = createSignal(false);
const [items, setItems] = createSignal<SortedItem[]>([]);
const [groups, setGroups] = createSignal<GroupHeader[]>([]);
/** The set the grid is showing, when the filter is exactly one: the grid is
 *  then in that set's order and may be reordered. The server decides, because
 *  only it parses the filter. */
const [currentSet, setCurrentSet] = createSignal<string | null>(null);

/** True when two path lists name the same files in the same order. */
const sameOrder = (a: string[], b: string[]) =>
  a.length === b.length && a.every((p, i) => p === b[i]);

/** True when two aspect maps hold the same ratio for the same paths. */
const sameAspects = (a: Map<string, number>, b: Map<string, number>) =>
  a.size === b.size && [...a].every(([p, v]) => b.get(p) === v);

/** What the grid renders, in order.
 *
 *  **Compared by content, not by identity.** `patchItem` rebuilds the whole
 *  array to change one item's rating or view stamp, so this memo re-ran and
 *  handed the grid a new `paths` array whose contents were identical. That
 *  fires the grid's `on(() => props.paths)` effect — prune, reset the
 *  background cursor, recompute the visible range — for a field the grid does
 *  not lay out from. A view recorded on one device did it to every other
 *  device's grid, because the event is broadcast. */
const displayPaths = createMemo(() => items().map((item) => item.path), [], {
  equals: sameOrder,
});

/** Per-path video duration, for the grid to gate short-video autoplay without
 *  changing its `paths: string[]` contract. Unknown durations are absent. */
const durationByPath = createMemo(() => {
  const map = new Map<string, number>();
  for (const item of items()) {
    if (item.duration != null) map.set(item.path, item.duration);
  }
  return map;
});

/** Per-path aspect ratio, so the justified layout can place a cell before any
 *  thumbnail bytes exist. Unknown dimensions are absent and the layout falls
 *  back to 1:1.
 *
 *  Compared by content for the same reason as `displayPaths`: a rating patch
 *  must not re-run the justified layout, and a path's dimensions never change
 *  once it has them. */
const aspectByPath = createMemo(
  () => {
    const map = new Map<string, number>();
    for (const item of items()) {
      if (item.width && item.height && item.width > 0 && item.height > 0) {
        map.set(item.path, item.width / item.height);
      }
    }
    return map;
  },
  new Map<string, number>(),
  { equals: sameAspects },
);

export interface CellMeta {
  size: number;
  media_type: string;
  width: number | null;
  height: number | null;
}

/** Size, type and dimensions, for the grid's decision about whether a cell can
 *  be served as its original file rather than as a generated tier. */
const mediaMetaByPath = createMemo(() => {
  const map = new Map<string, CellMeta>();
  for (const item of items()) {
    map.set(item.path, {
      size: item.file_size,
      media_type: item.media_type,
      width: item.width ?? null,
      height: item.height ?? null,
    });
  }
  return map;
});

const colorLabelByPath = createMemo(() => {
  const map = new Map<string, string>();
  for (const item of items()) {
    if (item.color_label) map.set(item.path, item.color_label);
  }
  return map;
});

const [selectedPaths, setSelectedPaths] = createSignal<Set<string>>(new Set());

/** Explicit multi-select. Desktop reaches selection through Ctrl/Cmd+click,
 *  which touch has no equivalent for — so a phone flips this on and every tap
 *  toggles a cell instead of opening the viewer. */
const [selectionMode, setSelectionMode] = createSignal(false);
const [settingsOpen, setSettingsOpen] = createSignal(false);

export {
  galleryPath,
  setGalleryPath,
  loading,
  setLoading,
  items,
  setItems,
  displayPaths,
  durationByPath,
  aspectByPath,
  mediaMetaByPath,
  colorLabelByPath,
  groups,
  setGroups,
  currentSet,
  selectedPaths,
  setSelectedPaths,
  selectionMode,
  settingsOpen,
  setSettingsOpen,
};

// ---------------------------------------------------------------------------
// Fetching
// ---------------------------------------------------------------------------

export interface Query {
  sort: string;
  order: string;
  sub_sort?: string | null;
  sub_order?: string | null;
  filter: string;
  group_by: GroupBy;
}

let current: Query = {
  sort: "date",
  order: "desc",
  filter: "",
  group_by: { type: "none" },
};


/** Run the one query and replace the list. */
export async function refresh(next?: Partial<Query>) {
  current = { ...current, ...next };
  setLoading(true);
  try {
    const result = await api.items(current);
    setItems(result.items);
    setGroups(result.groups);
    setCurrentSet(result.set ?? null);
  } finally {
    setLoading(false);
  }
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/** Apply one server event.
 *
 *  **A change applies at once, wherever it came from.** An item that stops
 *  matching the filter disappears rather than lingering until the view is
 *  re-run: the gallery has one owner, and a change they made on another device
 *  is one they expect to see. Keeping the reader's place across it is the
 *  grid's job, not the store's. The one wait is while a set is being dragged
 *  or its order written: the refetch is held until then (`holdRefreshes`).
 *
 *  **A filesystem change sends what changed, not everything.** Re-fetching the
 *  whole sorted list on any addition cost every connected client a
 *  full-library payload per phone upload — and dropped the active filter on
 *  the way, because the refetch passed no filter at all. Removals splice; an
 *  addition the client cannot place (it does not know where the new item sorts,
 *  and it may not match the filter) falls back to one refetch. */
export async function applyEvent(event: ServerEvent) {
  switch (event.kind) {
    case "fs-changed": {
      if (event.removed.length > 0) {
        const gone = new Set(event.removed);
        setItems((list) => list.filter((item) => !gone.has(item.path)));
        setSelectedPaths((prev) => {
          const next = new Set(prev);
          for (const path of gone) next.delete(path);
          return next;
        });
      }
      if (event.added.length > 0) await refreshForEvent();
      break;
    }
    case "items-changed":
      // The grid draws rating and colour per cell, so re-fetching the list for
      // one changed field would be a full payload for a star. A batch big
      // enough that patching row by row would cost more than one query takes
      // the query instead — the crossover is where the per-row calls stop
      // being cheaper than the payload they avoid.
      if (event.paths.length > PATCH_LIMIT) await refreshForEvent();
      else await Promise.all(event.paths.map(patchItem));
      break;
    case "tags-indexed":
      // The vocabulary moved. The item *list* moves with it only when the
      // active filter names a tag — which the client cannot tell without
      // parsing the query, so any active filter re-runs and no filter does
      // nothing. Autocomplete refreshes itself on the next keystroke.
      if (current.filter.trim()) await refreshForEvent();
      break;
    case "resync":
      // Typed lag recovery: re-fetch exactly the domains named. `tags` alone
      // moves the list only under an active filter, same as above.
      if (
        event.domains.includes("items") ||
        (event.domains.includes("tags") && current.filter.trim())
      ) {
        await refreshForEvent();
      }
      break;
    default:
      break;
  }
}

/** The refresh an event asks for: now, or once nothing holds refreshes back.
 *  See `holdRefreshes`. */
function refreshForEvent(): Promise<void> {
  if (holds > 0) {
    refreshOwed = true;
    return Promise.resolve();
  }
  return refresh();
}

/** Above this many changed rows, one query beats N metadata calls. Not
 *  measured: a round trip is a round trip, and a dozen is where the payload a
 *  refetch costs stops being the larger number on any connection. */
const PATCH_LIMIT = 12;

/** Re-read one item's row and splice it in place. */
async function patchItem(path: string) {
  const meta = await api.mediaMeta(path).catch(() => null);
  if (!meta) return;
  setItems((list) =>
    list.map((item) =>
      item.path === path
        ? {
            ...item,
            rating: meta.rating,
            color_label: meta.color_label,
            last_viewed: meta.last_viewed,
          }
        : item,
    ),
  );
}

// ---------------------------------------------------------------------------
// Writes that every caller shares
// ---------------------------------------------------------------------------

/** Persist a rating and keep every consumer in step: the backend, the in-memory
 *  list, and any listener on `lightview:rating-changed`. Every rating write —
 *  keyboard 0–5, info panel, context menu — goes through here. */
export async function rateItem(path: string, rating: number) {
  await api.setRating([path], rating > 0 ? rating : null);
  const lastRated = rating > 0 ? Math.floor(Date.now() / 1000) : null;
  setItems((list) =>
    list.map((item) =>
      item.path === path
        ? { ...item, rating: rating > 0 ? rating : null, last_rated: lastRated }
        : item,
    ),
  );
  window.dispatchEvent(
    new CustomEvent("lightview:rating-changed", { detail: { path, rating } }),
  );
}

/** Set a colour label and keep the list in step, so a `color:` filter and the
 *  cell marker update without a refetch. */
export async function setItemColorLabel(path: string, label: string | null) {
  await api.setColorLabel([path], label);
  setItems((list) =>
    list.map((item) => (item.path === path ? { ...item, color_label: label } : item)),
  );
}

// ---------------------------------------------------------------------------
// Reordering a set
// ---------------------------------------------------------------------------

/** How many things are holding event-driven refreshes back: a drag under way,
 *  and a reorder being written. */
let holds = 0;
/** Whether an event asked for a refresh while one was held. */
let refreshOwed = false;

/** Hold back the refreshes server events ask for, until the matching
 *  `releaseRefreshes`.
 *
 *  While a drag is under way, a refresh would move cells out from under the
 *  pointer; while a reorder is being written, the `tags-indexed` each write
 *  ends with would fetch an order *older* than the one on screen and snap the
 *  grid back, then forward again. Held refreshes collapse into one, run when
 *  the last hold is released. A refresh the reader asks for — a new filter or
 *  sort — is not an event's and is never held. */
export function holdRefreshes() {
  holds++;
}

/** Release one `holdRefreshes`, running the refresh it held back if it was the
 *  last. */
export function releaseRefreshes() {
  holds = Math.max(0, holds - 1);
  if (holds === 0 && refreshOwed) {
    refreshOwed = false;
    void refresh();
  }
}

/** The order waiting behind the one being written, if any. */
let queuedOrder: { set: string; paths: string[] } | null = null;
let writingOrder = false;

/** Show a set in a new order at once, and write it.
 *
 *  **One reorder is written at a time.** A first reorder writes every member's
 *  sidecar and takes seconds over a share, and a reader drags again in that
 *  time. Two writes in flight would interleave per file on the server, or the
 *  older could land last; so a reorder made while one is being written replaces
 *  whatever is queued behind it, and goes when the write finishes. Only the
 *  latest order matters, and it is the one on screen.
 *
 *  A failure refreshes from the server rather than guessing: a write that
 *  stopped partway leaves a mixed order, and the reader should see that one. */
export async function reorderSet(set: string, paths: string[]) {
  const rank = new Map(paths.map((path, i) => [path, i]));
  const at = (path: string) => rank.get(path) ?? paths.length;
  setItems((list) => [...list].sort((a, b) => at(a.path) - at(b.path)));

  if (writingOrder) {
    queuedOrder = { set, paths };
    return;
  }
  writingOrder = true;
  holdRefreshes();
  try {
    let next: { set: string; paths: string[] } | null = { set, paths };
    while (next) {
      queuedOrder = null;
      await api.orderSet(next.set, next.paths);
      next = queuedOrder;
    }
  } catch (err) {
    console.error("Reorder failed:", err);
    queuedOrder = null;
    refreshOwed = true;
  } finally {
    writingOrder = false;
    releaseRefreshes();
  }
}

// ---------------------------------------------------------------------------
// Selection
// ---------------------------------------------------------------------------

export function toggleSelection(path: string) {
  setSelectedPaths((prev) => {
    const next = new Set(prev);
    if (next.has(path)) next.delete(path);
    else next.add(path);
    return next;
  });
}

/** Deselect everything, staying in whatever selection mode is current. */
export function clearSelection() {
  setSelectedPaths(new Set<string>());
}

/** Leave multi-select mode, dropping whatever was selected — the two always go
 *  together, so no caller has to remember both. */
export function exitSelectionMode() {
  setSelectionMode(false);
  clearSelection();
}

/** Enter multi-select mode, or leave it and drop the selection. */
export function toggleSelectionMode() {
  if (selectionMode()) exitSelectionMode();
  else setSelectionMode(true);
}

/** Select exactly `paths`. */
export function selectAll(paths: string[]) {
  setSelectedPaths(new Set<string>(paths));
}
