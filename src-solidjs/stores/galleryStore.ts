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

/** Where each file sits in its block, for the cell's marker and the
 *  Arrange menu. Only filled under the Custom sort. */
export interface BlockPlace {
  name: string;
  first: boolean;
}

const blockByPath = createMemo(() => {
  const map = new Map<string, BlockPlace>();
  let previous: string | null = null;
  for (const item of items()) {
    const name = item.block ?? null;
    if (name) map.set(item.path, { name, first: name !== previous });
    previous = name;
  }
  return map;
});

/** One short message for the person, such as why an arrangement was refused.
 *  Replaced by the next, and cleared after a few seconds. */
const [notice, setNoticeSignal] = createSignal<string | null>(null);
let noticeTimer: ReturnType<typeof setTimeout> | undefined;
export function showNotice(message: string) {
  setNoticeSignal(message);
  clearTimeout(noticeTimer);
  noticeTimer = setTimeout(() => setNoticeSignal(null), 5000);
}

/** The file waiting to be placed after the next cell the person picks. */
const [placing, setPlacing] = createSignal<string | null>(null);

/** The files the "Lock as a Set" dialog is open for, in display order. */
const [lockingPaths, setLockingPaths] = createSignal<string[] | null>(null);

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
  blockByPath,
  notice,
  placing,
  setPlacing,
  lockingPaths,
  setLockingPaths,
  groups,
  setGroups,
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


/** Which query is newest. A response to an older one is dropped: two queries
 *  in flight — a reorder here and the event it causes, or two quick sort
 *  changes — can finish in either order, and the slower must not win. */
let generation = 0;

/** Run the one query and replace the list. */
export async function refresh(next?: Partial<Query>) {
  current = { ...current, ...next };
  const mine = ++generation;
  setLoading(true);
  try {
    const result = await api.items(current);
    if (mine !== generation) return;
    setItems(result.items);
    setGroups(result.groups);
  } finally {
    if (mine === generation) setLoading(false);
  }
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/** Apply one server event.
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
      if (event.added.length > 0) await refresh();
      break;
    }
    case "items-changed":
      // The grid draws rating and colour per cell, so re-fetching the list for
      // one changed field would be a full payload for a star. A batch big
      // enough that patching row by row would cost more than one query takes
      // the query instead — the crossover is where the per-row calls stop
      // being cheaper than the payload they avoid.
      if (event.paths.length > PATCH_LIMIT) await refresh();
      else await Promise.all(event.paths.map(patchItem));
      break;
    case "order-changed":
      // Only the Custom sort is ordered by arrangement; every other client
      // is showing an order this cannot have moved.
      if (current.sort === "custom") await refresh();
      break;
    case "tags-indexed":
      // The vocabulary moved. The item *list* moves with it only when the
      // active filter names a tag — which the client cannot tell without
      // parsing the query, so any active filter re-runs and no filter does
      // nothing. Autocomplete refreshes itself on the next keystroke.
      if (current.filter.trim()) await refresh();
      break;
    case "resync":
      // Typed lag recovery: re-fetch exactly the domains named. `tags` alone
      // moves the list only under an active filter, same as above.
      if (
        event.domains.includes("items") ||
        (event.domains.includes("tags") && current.filter.trim())
      ) {
        await refresh();
      }
      break;
    default:
      break;
  }
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

export function clearSelection() {
  setSelectedPaths(new Set<string>());
}

/** Leave multi-select mode, dropping whatever was selected — the two always go
 *  together, so no caller has to remember both. */
export function exitSelectionMode() {
  setSelectionMode(false);
  clearSelection();
}

export function toggleSelectionMode() {
  if (selectionMode()) exitSelectionMode();
  else setSelectionMode(true);
}

export function selectAll(paths: string[]) {
  setSelectedPaths(new Set<string>(paths));
}

// ---------------------------------------------------------------------------
// Arranging the Custom order
// ---------------------------------------------------------------------------

/** Run one arrangement, one at a time: a second placement computed while the
 *  first is still being written would be computed against the old order. A
 *  refusal — the gallery still indexing, a file already in another block — is
 *  shown rather than thrown. The list follows through `order-changed`. */
let arranging: Promise<unknown> = Promise.resolve();
export function arrange(run: () => Promise<unknown>): Promise<void> {
  const next = arranging.then(run).then(
    () => undefined,
    (e: unknown) => showNotice(e instanceof Error ? e.message : String(e)),
  );
  arranging = next;
  return next;
}

/** Place `path` right behind `after`, as the person sees them. The file after
 *  `after` on screen is sent too: it is how the server tells a reorder inside
 *  a block from a move of the whole block. */
export function placeAfter(path: string, after: string) {
  const paths = displayPaths();
  const before = paths[paths.indexOf(after) + 1] ?? null;
  return arrange(() => api.place(path, after, before === path ? null : before));
}

/** Place `path` above everything in view. Later arrivals still land above it:
 *  it is anchored to the file that was first, not pinned. */
export function placeFirst(path: string) {
  const first = displayPaths().find((p) => p !== path) ?? null;
  if (!first) return Promise.resolve();
  return arrange(() => api.place(path, null, first));
}

/** Open the lock dialog for `paths`, ordered as they are on screen: a set is
 *  locked in the order it is given, and a selection is a set with no order. */
export function openLockDialog(paths: string[]) {
  const at = new Map(displayPaths().map((p, i) => [p, i]));
  setLockingPaths([...paths].sort((a, b) => (at.get(a) ?? 0) - (at.get(b) ?? 0)));
}
