// The application shell: which view is on screen, and the boot sequence.
//
// Boot differs by runtime, and the difference is the interesting part. The
// desktop opens a gallery and waits. The web client is several round-trips
// from anything renderable, so it paints from the IndexedDB snapshot first,
// then replaces it with `getBootState()` — one call returning gallery info,
// the default filter, and the sorted items with that filter already applied,
// because three serial invokes over a phone's connection is most of the
// perceived boot time.
//
// When that call fails but the snapshot painted anyway, the app looks
// connected while every write and uncached thumbnail silently fails. That is
// what `serverUnreachable` and ConnectionBanner exist for; see
// docs/frontend/README.md.
//
// Everything below the shell reads stores rather than props. This file wires
// the pieces together and owns the keyboard map, the paste/drop handlers, and
// the Tauri event subscriptions — not gallery logic.

import { Show, createEffect, createSignal, lazy, onCleanup, onMount } from "solid-js";
import { open } from "@tauri-apps/plugin-dialog";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { safeListen as listen, NOT_PAIRED_EVENT } from "./lib/runtime";
import { isTauri, isWeb, isMobile } from "./lib/runtime";
import { PasswordModal } from "./components/auth/PasswordModal";
import { galleryPath, setGalleryPath, setLoading, displayPaths, setDisplayPaths, sortedItems, setSortedItems, loading, selectedPaths, setSelectedPaths, toggleSelection, clearSelection, selectAll, selectionMode, exitSelectionMode, viewMode, settingsOpen, setSettingsOpen, aspectByPath, mediaMetaByPath, groups, rateItem, loadEnabledViews } from "./stores/galleryStore";
import { loadBootSnapshot, saveBootSnapshot } from "./lib/bootSnapshot";
import { createOpenAtBottom } from "./lib/openAtBottom";
import { setScrollHost } from "./lib/scrollHost";
import { VIEWER_CLOSE_REQUEST_EVENT } from "./lib/viewerTransition";
import { viewerOpen, closeViewer, openViewer, nextImage, prevImage, viewerIndex, toggleInfoPanel, infoPanelOpen } from "./stores/viewerStore";
import { settings, sortField, sortOrder, subSortField, subSortOrder, groupBy, loadSettingsFromGallery, loadWebSettings, applyExternalSettings } from "./stores/settingsStore";
import { openGallery, getBootState, getSortedItems, getRecentGalleries, removeRecentGallery, applyFilter, getGalleryDefaultFilter, type RecentGallery } from "./lib/ipc";
import { setFilterQuery, refreshFilteredItems } from "./stores/filterStore";
import { GalleryGrid } from "./components/gallery/GalleryGrid";
import { JustifiedGrid } from "./components/gallery/JustifiedGrid";
// Split out of the main bundle, not merely deferred: leaflet plus its CSS is
// 153 kB of a 445 kB build — a third of it — and a gallery browsed only in the
// grids never needs a byte of it. Every other view reuses machinery the main
// bundle already carries, so this is the one view where the split is worth a
// dedicated chunk. See decision 0008.
//
// No `<Suspense>` boundary: the map is already inside a `<Show>`, and Solid's
// `lazy` renders nothing until the chunk resolves, which over loopback or a LAN
// is imperceptible. Add a fallback here if that ever stops being true.
const MapView = lazy(() =>
  import("./components/map/MapView").then((m) => ({ default: m.MapView })),
);
import { MediaViewer } from "./components/viewer/MediaViewer";
import { TopBar } from "./components/topbar/TopBar";
import { TitleBar } from "./components/topbar/TitleBar";
import { WindowResizeGrips } from "./components/WindowResizeGrips";
import { ContextMenu, type ContextMenuState } from "./components/shared/ContextMenu";
import { SelectionBar } from "./components/gallery/SelectionBar";
import { pluginActivity, type PluginActivity } from "./stores/pluginStore";
import { applyJobEvent, applyWorkersEvent, refreshTaggingStatus, activeTaggingJobId } from "./stores/taggingStore";
import { loadCapabilities } from "./stores/capabilitiesStore";
import { cancelPluginBatch, cancelTaggingJob } from "./lib/ipc";
import { thumbGenActivity } from "./stores/thumbnailProgressStore";
import { ScrollBar, type ScrollIndicator } from "./components/shared/ScrollBar";
import { DebugOverlay } from "./components/debug/DebugOverlay";
import { DuplicatesPanel } from "./components/DuplicatesPanel";
import { TrashPanel } from "./components/TrashPanel";
import { TagManagerPanel } from "./components/TagManagerPanel";
import { AutoTagPanel } from "./components/AutoTagPanel";
import { UploadSheet } from "./components/upload/UploadSheet";
import { loadUploadConfig } from "./stores/uploadStore";
import type { CommandHandlers } from "./components/topbar/CommandMenu";
import { ConnectionBanner } from "./components/ConnectionBanner";
import type { SortedItem, SortField } from "./lib/types";

// ---------------------------------------------------------------------------
// Scrollbar indicator helpers
// ---------------------------------------------------------------------------

/** The timestamp column a sort field orders by, for the date-shaped fields.
 *  Returns null for fields that aren't dates — those get their own labelling
 *  below. Keeps "Recently Viewed" and friends navigable on the scrollbar
 *  instead of falling through to a blank track. */
function dateAccessor(field: SortField): ((item: SortedItem) => number | null) | null {
  switch (field) {
    case "date":
      return (it) => it.date_taken;
    case "lastviewed":
      return (it) => it.last_viewed;
    case "dateadded":
      return (it) => it.date_added;
    case "lastrated":
      return (it) => it.last_rated;
    default:
      return null;
  }
}

function buildScrollIndicators(items: SortedItem[], field: SortField): ScrollIndicator[] {
  if (items.length === 0) return [];

  const getDate = dateAccessor(field);
  if (getDate) return buildDateIndicators(items, getDate);

  switch (field) {
    case "name":
      return buildNameIndicators(items);
    case "size":
      return buildSizeIndicators(items);
    case "rating":
      return buildRatingIndicators(items);
    default:
      return [];
  }
}

// Lazily-built, reused date formatters. `Date#toLocaleDateString(locale, opts)`
// constructs a fresh Intl.DateTimeFormat on every call — ~100µs each — so
// calling it once per item turned this O(n) walk into seconds of blocked main
// thread on a large gallery. Hoisting the formatter makes the same walk ~40x
// cheaper; the loop below additionally only formats at a month boundary.
let _monthFmt: Intl.DateTimeFormat | undefined;
const monthFormat = (d: Date) =>
  (_monthFmt ??= new Intl.DateTimeFormat(undefined, { month: "short", year: "numeric" })).format(d);

let _dayFmt: Intl.DateTimeFormat | undefined;
const dayFormat = (d: Date) =>
  (_dayFmt ??= new Intl.DateTimeFormat(undefined, {
    day: "numeric",
    month: "short",
    year: "numeric",
  })).format(d);

function buildDateIndicators(
  items: SortedItem[],
  getDate: (item: SortedItem) => number | null,
): ScrollIndicator[] {
  const indicators: ScrollIndicator[] = [];
  // Compare a cheap numeric month key rather than the rendered label, so the
  // formatter runs once per month boundary (a couple of dozen times) instead
  // of once per item.
  let lastKey = Number.NaN;
  for (let i = 0; i < items.length; i++) {
    const ts = getDate(items[i]);
    if (!ts) continue;
    const d = new Date(ts * 1000);
    const key = d.getFullYear() * 12 + d.getMonth();
    if (key !== lastKey) {
      indicators.push({ position: i / items.length, label: monthFormat(d) });
      lastKey = key;
    }
  }
  return dedupeIndicators(indicators);
}

function buildNameIndicators(items: SortedItem[]): ScrollIndicator[] {
  const indicators: ScrollIndicator[] = [];
  let lastChar = "";
  for (let i = 0; i < items.length; i++) {
    const name = items[i].path.split("/").pop() ?? "";
    const ch = name.charAt(0).toUpperCase();
    if (ch && ch !== lastChar) {
      indicators.push({ position: i / items.length, label: ch });
      lastChar = ch;
    }
  }
  return dedupeIndicators(indicators);
}

function formatSizeShort(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(0)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  return `${(bytes / (1024 * 1024 * 1024)).toFixed(1)} GB`;
}

function buildSizeIndicators(items: SortedItem[]): ScrollIndicator[] {
  // Place indicators at size order-of-magnitude boundaries
  const thresholds = [
    100 * 1024,        // 100 KB
    500 * 1024,        // 500 KB
    1024 * 1024,       // 1 MB
    5 * 1024 * 1024,   // 5 MB
    10 * 1024 * 1024,  // 10 MB
    50 * 1024 * 1024,  // 50 MB
    100 * 1024 * 1024, // 100 MB
  ];
  const indicators: ScrollIndicator[] = [];
  let tIdx = 0;
  // Determine sort direction from first vs last
  const ascending = items.length > 1 && items[0].file_size <= items[items.length - 1].file_size;

  for (let i = 0; i < items.length && tIdx < thresholds.length; i++) {
    const size = items[i].file_size;
    const threshold = thresholds[ascending ? tIdx : thresholds.length - 1 - tIdx];
    const crossed = ascending ? size >= threshold : size <= threshold;
    if (crossed) {
      indicators.push({ position: i / items.length, label: formatSizeShort(threshold) });
      tIdx++;
    }
  }
  return dedupeIndicators(indicators);
}

function buildRatingIndicators(items: SortedItem[]): ScrollIndicator[] {
  const indicators: ScrollIndicator[] = [];
  let lastRating = -1;
  for (let i = 0; i < items.length; i++) {
    const r = items[i].rating ?? 0;
    if (r !== lastRating) {
      indicators.push({ position: i / items.length, label: r === 0 ? "Unrated" : "\u2605".repeat(r) });
      lastRating = r;
    }
  }
  return dedupeIndicators(indicators);
}

/** Thin out indicators so they don't overlap — keep at most ~15, evenly spaced. */
function dedupeIndicators(indicators: ScrollIndicator[]): ScrollIndicator[] {
  if (indicators.length <= 15) return indicators;
  const step = Math.ceil(indicators.length / 15);
  const result: ScrollIndicator[] = [];
  for (let i = 0; i < indicators.length; i += step) {
    result.push(indicators[i]);
  }
  return result;
}

function getThumbLabelForItems(items: SortedItem[], field: SortField, fraction: number): string {
  if (items.length === 0) return "";
  const idx = Math.min(Math.floor(fraction * items.length), items.length - 1);
  const item = items[idx];

  const getDate = dateAccessor(field);
  if (getDate) {
    const ts = getDate(item);
    // "Never" reads right for the un-stamped tail of viewed/rated/added sorts,
    // which SQL parks at the end via NULLS LAST.
    if (!ts) return field === "date" ? "No date" : "Never";
    return dayFormat(new Date(ts * 1000));
  }

  switch (field) {
    case "name": {
      const name = item.path.split("/").pop() ?? "";
      return name.length > 20 ? name.slice(0, 20) + "\u2026" : name;
    }
    case "size":
      return formatSizeShort(item.file_size);
    case "rating": {
      const r = item.rating ?? 0;
      return r === 0 ? "Unrated" : "\u2605".repeat(r);
    }
    default:
      return "";
  }
}

export function App() {
  const [debugOpen, setDebugOpen] = createSignal(false);
  const [duplicatesOpen, setDuplicatesOpen] = createSignal(false);
  const [trashOpen, setTrashOpen] = createSignal(false);
  const [tagManagerOpen, setTagManagerOpen] = createSignal(false);
  const [autoTagOpen, setAutoTagOpen] = createSignal(false);
  const [uploadOpen, setUploadOpen] = createSignal(false);
  const [contextMenu, setContextMenu] = createSignal<ContextMenuState | null>(null);
  const [galleryContentHeight, setGalleryContentHeight] = createSignal(0);
  // Web client: true once a boot round-trip has failed and nothing has
  // succeeded since. Drives ConnectionBanner — the grid can still be fully
  // painted from the IndexedDB snapshot, so without this the failure is
  // invisible.
  const [serverUnreachable, setServerUnreachable] = createSignal(false);

  // "Start at bottom" — open the grid at its end rather than its start.
  createOpenAtBottom({
    enabled: () => settings().display.start_at_bottom,
    galleryKey: galleryPath,
    contentHeight: galleryContentHeight,
  });

  // On mobile the settings panel is a full-screen page, so skip rendering the
  // gallery content behind it entirely.
  const contentHidden = () => isMobile() && settingsOpen();

  // Scrollbar sort indicators, computed on demand and cached per
  // (items, sortField).
  //
  // `indicators` is a JSX getter prop, so ScrollBar re-invokes this on every
  // read — and it reads several times per render pass (the `<Show>` gate, then
  // the `<For>`). Each read walks the whole item list, so on a large gallery
  // the first touch of the scrollbar (the synthesized `mouseenter` is what
  // flips `hovering`, the only thing gating this work) blocked the main thread
  // for seconds at a stretch, and again on every later touch. On a phone that
  // is long enough for the browser to kill the tab as unresponsive.
  //
  // Cached by hand rather than with createMemo: a memo would recompute eagerly
  // on every sortedItems change even for the many sessions that never touch the
  // scrollbar. The signal reads stay in the caller's tracking scope, so
  // consumers still update exactly as before.
  let indicatorCache: { items: SortedItem[]; field: SortField; value: ScrollIndicator[] } | null = null;
  const scrollIndicators = (): ScrollIndicator[] => {
    const items = sortedItems();
    const field = sortField();
    if (!indicatorCache || indicatorCache.items !== items || indicatorCache.field !== field) {
      indicatorCache = { items, field, value: buildScrollIndicators(items, field) };
    }
    return indicatorCache.value;
  };
  const thumbLabel = (fraction: number) => getThumbLabelForItems(sortedItems(), sortField(), fraction);

  // Apply the gallery-wide default filter (if enabled) when a gallery first
  // opens. Seeds the FilterBar so the active query is visible/editable and
  // returns the matching paths to feed the initial sort. Returns undefined
  // when no default filter is set (→ all items).
  //
  // The default filter lives in the gallery's saved settings so every client
  // honours it. On desktop, settings() already mirrors those after
  // loadSettingsFromGallery(); the web client has no local gallery settings,
  // so it reads the gallery-wide value straight from the backend.
  const applyDefaultFilter = async (): Promise<string[] | undefined> => {
    const df = isWeb()
      ? await getGalleryDefaultFilter().catch(() => null)
      : settings().default_filter;
    const query = df?.enabled ? (df.query ?? "").trim() : "";
    if (!query) return undefined;
    setFilterQuery(query);
    try {
      return await applyFilter(query);
    } catch (e) {
      console.error("Default filter failed:", e);
      return undefined;
    }
  };

  const openPath = async (path: string) => {
    setLoading(true);
    try {
      const result = await openGallery(path);
      setGalleryPath(result.path);

      // Restore per-gallery settings from .lightview folder
      await loadSettingsFromGallery();
      await loadEnabledViews();

      const filtered = await applyDefaultFilter();
      const sorted = await getSortedItems(sortField(), sortOrder(), groupBy(), filtered, subSortField(), subSortOrder());
      setSortedItems(sorted.items);
      setDisplayPaths(sorted.items.map((item) => item.path));
    } catch (e) {
      console.error("Failed to open gallery:", e);
    } finally {
      setLoading(false);
    }
  };

  // Web client: it can't open a folder, so load whatever gallery the desktop
  // already has open. Read-only path — no openGallery/index write.
  onMount(async () => {
    if (!isWeb()) return;
    // If the server tells us this browser isn't paired, bounce to the pair
    // page. One-shot — the user comes back via redirect after pairing.
    const onNotPaired = () => {
      window.removeEventListener(NOT_PAIRED_EVENT, onNotPaired);
      window.location.replace("/pair");
    };
    window.addEventListener(NOT_PAIRED_EVENT, onNotPaired);
    onCleanup(() => window.removeEventListener(NOT_PAIRED_EVENT, onNotPaired));

    // Restore this browser's per-client display settings (GIF-in-grid, etc.).
    loadWebSettings();

    // Hydrate the grid from the last session's IndexedDB snapshot while the
    // real boot waterfall runs. IDB resolves in a few ms, the network in a
    // few hundred — so the full grid (ThumbHash placeholders + any service-
    // worker-cached thumbnails) paints before the first response arrives,
    // and the app opens offline. Guarded so late-resolving IDB can never
    // clobber fresh network data.
    void loadBootSnapshot().then((snap) => {
      if (snap && snap.items.length > 0 && sortedItems().length === 0) {
        setGalleryPath(snap.galleryPath);
        setSortedItems(snap.items);
        setDisplayPaths(snap.items.map((item) => item.path));
      }
    });

    await loadWebBootState();
  });

  // Single-round-trip boot: gallery info + default filter + sorted items
  // (filter pre-applied server-side). The old path was three serial invokes —
  // three network RTTs before the grid had any data. Also the Retry action on
  // ConnectionBanner, hence the reachability flag: a failure here means the
  // grid on screen (if any) came from the snapshot and is not live.
  const loadWebBootState = async () => {
    try {
      const boot = await getBootState(sortField(), sortOrder(), groupBy(), subSortField(), subSortOrder());
      if (boot.gallery) {
        setGalleryPath(boot.gallery.path);
        const df = boot.default_filter;
        const query = df?.enabled ? (df.query ?? "").trim() : "";
        if (query) setFilterQuery(query); // seed the FilterBar; boot.sorted already reflects it
        if (boot.sorted) {
          setSortedItems(boot.sorted.items);
          setDisplayPaths(boot.sorted.items.map((item) => item.path));
        }
      }
      setServerUnreachable(false);
    } catch (e) {
      console.error("Failed to load current gallery:", e);
      setServerUnreachable(true);
    }
  };

  // Persist the grid for next boot (web only) — covers every path that
  // refreshes sortedItems (boot, uploads, fs-changes, filter edits). Debounced
  // so scroll-adjacent churn doesn't rewrite a large snapshot repeatedly.
  if (isWeb()) {
    let snapshotTimer: number | undefined;
    createEffect(() => {
      const items = sortedItems();
      const path = galleryPath();
      if (!path || items.length === 0) return;
      clearTimeout(snapshotTimer);
      snapshotTimer = window.setTimeout(() => saveBootSnapshot(path, items), 1500);
    });
    onCleanup(() => clearTimeout(snapshotTimer));
  }

  // Web client: re-fetch items after a device upload so the new photos show
  // up (the browser gets no fs-watch push from the host). Mirrors the file-
  // addition branch of the fs-changed handler.
  const refreshAfterUpload = async () => {
    try {
      const sorted = await getSortedItems(sortField(), sortOrder(), groupBy(), undefined, subSortField(), subSortOrder());
      setSortedItems(sorted.items);
      setDisplayPaths(sorted.items.map((item) => item.path));
    } catch (e) {
      console.error("Failed to refresh after upload:", e);
    }
  };

  // Listen for directory passed via CLI argument
  const unlisten = listen<string>("open-directory", (event) => {
    openPath(event.payload);
  });
  onCleanup(() => { unlisten.then((fn) => fn()); });

  // Apply a batch of external filesystem changes (files added/removed outside
  // the app). Shared by the desktop Tauri event and the web SSE relay below —
  // an empty batch (added/removed both empty) means "refetch everything", which
  // the SSE side sends when its subscriber lagged.
  const handleFsChange = async (added: string[], removed: string[]) => {
    if (removed.length > 0) {
      const removedSet = new Set(removed);
      const remaining = sortedItems().filter((item) => !removedSet.has(item.path));
      // When the app itself deleted the file (trash/move), onFilesRemoved has
      // already dropped it from the lists — the watcher's echo matches nothing,
      // so skip the writes and spare the grid a no-op paths-change pass.
      if (remaining.length !== sortedItems().length) {
        setSortedItems(remaining);
        setDisplayPaths(displayPaths().filter((p) => !removedSet.has(p)));
        setSelectedPaths((prev) => {
          const next = new Set(prev);
          for (const p of removed) next.delete(p);
          return next.size !== prev.size ? next : prev;
        });
      }
    }

    if (added.length > 0 || removed.length === 0) {
      // Re-fetch sorted items to get correct insertion order
      try {
        const sorted = await getSortedItems(sortField(), sortOrder(), groupBy(), undefined, subSortField(), subSortOrder());
        setSortedItems(sorted.items);
        setDisplayPaths(sorted.items.map((item) => item.path));
      } catch (e) {
        console.error("Failed to refresh after file addition:", e);
      }
    }
  };

  // Desktop: external filesystem changes arrive as a Tauri event. (On the web
  // client `listen` is a no-op; the EventSource below covers that case.)
  const unlistenFs = listen<{ added: string[]; removed: string[] }>("gallery:fs-changed", (event) => {
    handleFsChange(event.payload.added, event.payload.removed);
  });
  onCleanup(() => { unlistenFs.then((fn) => fn()); });

  // Web client: subscribe to the host's Server-Sent Events stream so the grid
  // reflects uploads (from any device) and files added directly on the host,
  // without a manual reload. EventSource reconnects on its own; it sends the
  // same-origin `lv_device` cookie so only paired devices connect.
  if (isWeb()) {
    loadCapabilities();
    // Decides whether the command list offers "Upload photos" at all, so it
    // has to land before the user opens the list rather than when a sheet
    // mounts.
    void loadUploadConfig();
    void loadEnabledViews();
    const source = new EventSource("/api/events");
    source.addEventListener("fs-changed", (event) => {
      try {
        const { added = [], removed = [] } = JSON.parse((event as MessageEvent).data || "{}");
        handleFsChange(added, removed);
      } catch (e) {
        console.error("Failed to handle fs-changed SSE event:", e);
      }
    });
    // Remote-tagging: live job progress + worker registry changes, driving
    // the taggingStore (and through it the PluginToast). Snapshots are
    // self-contained, so a missed event just means a slightly stale view
    // until the next one or a menu-open refresh.
    source.addEventListener("tagging-job", (event) => {
      try {
        applyJobEvent(JSON.parse((event as MessageEvent).data));
      } catch (e) {
        console.error("Failed to handle tagging-job SSE event:", e);
      }
    });
    source.addEventListener("tagging-workers", (event) => {
      try {
        applyWorkersEvent(JSON.parse((event as MessageEvent).data));
      } catch (e) {
        console.error("Failed to handle tagging-workers SSE event:", e);
      }
    });
    refreshTaggingStatus();
    onCleanup(() => source.close());
  }

  // Hot-reload settings when settings.toml is hand-edited outside the app.
  // Backend emits this only for genuine external edits, not the app's own saves.
  const unlistenSettings = listen<string>("settings:changed", (event) => {
    applyExternalSettings(event.payload);
  });
  onCleanup(() => { unlistenSettings.then((fn) => fn()); });

  // Companion tags are indexed in the background after a gallery opens, so the
  // grid can render immediately. Once indexing finishes, re-apply the default
  // filter and re-sort so any tag-based default view (and autocomplete) reflects
  // the freshly indexed tags.
  const unlistenTags = listen("gallery:tags-indexed", async () => {
    if (!galleryPath()) return;
    try {
      const filtered = await applyDefaultFilter();
      const sorted = await getSortedItems(sortField(), sortOrder(), groupBy(), filtered, subSortField(), subSortOrder());
      setSortedItems(sorted.items);
      setDisplayPaths(sorted.items.map((item) => item.path));
    } catch (e) {
      console.error("Failed to refresh after tag indexing:", e);
    }
  });
  onCleanup(() => { unlistenTags.then((fn) => fn()); });

  const handleOpenFolder = async () => {
    // The native folder picker only exists on the desktop. The web client
    // views whatever gallery the desktop has open; it cannot pick a new one.
    if (isWeb()) return;
    try {
      const selected = await open({ directory: true, multiple: false });
      if (selected) {
        await openPath(selected as string);
      }
    } catch (e) {
      console.error("Dialog failed:", e);
    }
  };

  // What the command list runs. Declared here because this is where the
  // panels' open state lives; the list itself — what is offered, in what
  // order, on which surface — is `CommandMenu.tsx`.
  const commandHandlers: CommandHandlers = {
    openTagManager: () => setTagManagerOpen(true),
    openDuplicates: () => setDuplicatesOpen(true),
    openTrash: () => setTrashOpen(true),
    openAutoTag: () => setAutoTagOpen(true),
    openUpload: () => setUploadOpen(true),
    openFolder: handleOpenFolder,
    openSettings: () => setSettingsOpen(true),
  };

  // Throttle held arrow keys to one navigation per frame so the viewer
  // doesn't queue up a backlog of image loads that keep playing after release.
  let navPending = false;
  let navDirection: "left" | "right" | null = null;

  const flushNav = () => {
    navPending = false;
    if (!navDirection || !viewerOpen()) { navDirection = null; return; }
    if (navDirection === "right") {
      nextImage(displayPaths().length);
    } else {
      prevImage();
    }
    window.dispatchEvent(new CustomEvent("lightview:scroll-to-index", { detail: viewerIndex() }));
    navDirection = null;
  };

  const handleKeyDown = (e: KeyboardEvent) => {
    const typingInInput =
      e.target instanceof HTMLInputElement || e.target instanceof HTMLTextAreaElement;
    if (viewerOpen()) {
      if (e.key === "Escape") {
        // Ask the viewer to close through its fly-back-to-cell transition; it
        // calls closeViewer() itself once the animation lands (and falls back
        // to closing outright when there's nothing to fly back to).
        window.dispatchEvent(new Event(VIEWER_CLOSE_REQUEST_EVENT));
      } else if (e.key === "ArrowRight") {
        if (typingInInput) return;
        if (e.repeat && navPending) return;
        navDirection = "right";
        if (!navPending) {
          navPending = true;
          requestAnimationFrame(flushNav);
        }
        return;
      } else if (e.key === "ArrowLeft") {
        if (typingInInput) return;
        if (e.repeat && navPending) return;
        navDirection = "left";
        if (!navPending) {
          navPending = true;
          requestAnimationFrame(flushNav);
        }
        return;
      } else if (e.key === "i" || e.key === "I") {
        if (typingInInput) return;
        toggleInfoPanel();
      } else if (e.key === "Tab" && infoPanelOpen() && !typingInInput) {
        // First Tab while the info panel is open jumps focus into the new-tag
        // input. Once focus is in the input (typingInInput), Tab falls through
        // to default traversal.
        e.preventDefault();
        window.dispatchEvent(new CustomEvent("lightview:focus-tag-input"));
      } else if (e.key >= "0" && e.key <= "5" && !e.ctrlKey && !e.metaKey && !e.altKey) {
        if (typingInInput) return;
        e.preventDefault();
        const paths = displayPaths();
        const idx = viewerIndex();
        if (idx >= 0 && idx < paths.length) {
          const newRating = Number(e.key);
          rateItem(paths[idx], newRating).catch(() => {});
        }
      }
    } else {
      if (e.key === "Escape") {
        exitSelectionMode();
      }
      if ((e.ctrlKey || e.metaKey) && e.key === "a" && galleryPath()) {
        e.preventDefault();
        selectAll(displayPaths());
      }
    }
    if (e.key === "F11" && isTauri()) {
      e.preventDefault();
      const win = getCurrentWindow();
      win.isDecorated().then((decorated) => win.setDecorations(!decorated));
    }
    if (e.key === "F12") {
      setDebugOpen((prev) => !prev);
    }
  };

  window.addEventListener("keydown", handleKeyDown);
  onCleanup(() => window.removeEventListener("keydown", handleKeyDown));

  return (
    <div
      class="min-h-screen w-screen relative"
      style={{ background: settings().display.background_color }}
    >
      {/* Custom resize borders for the frameless window (incl. the welcome
          screen, so it's resizable before a gallery is opened). */}
      <Show when={isTauri() && !isMobile()}>
        <WindowResizeGrips />
      </Show>

      {/* Web only: the grid may be painted entirely from cache while the
          server is unreachable. Say so, and offer the way back. */}
      <Show when={isWeb() && serverUnreachable()}>
        <ConnectionBanner onRetry={loadWebBootState} />
      </Show>

      <Show
        when={galleryPath()}
        fallback={
          <>
            {/* No TopBar on the welcome screen, so give the frameless window a
                static titlebar here for window controls + dragging. */}
            <Show when={isTauri() && !isMobile()}>
              <TitleBar visible={true} />
            </Show>
            <WelcomeScreen onOpen={handleOpenFolder} onOpenPath={openPath} />
          </>
        }
      >
        <TopBar commands={commandHandlers} />
        {/* The gallery scrolls in here rather than in the document. iOS draws
            its own scroll indicator over the page, that indicator is
            interactive (press and hold to scrub), and it cannot be styled away
            on the document scroller — `::-webkit-scrollbar` only reaches
            element scrollers. Owning the scroller is what leaves LightView's
            bar, the one with the date markers, as the only one on screen.
            Being positioned also makes this the grids' `offsetParent`, so their
            `offsetTop` measurements share an origin with `scrollTop`. See
            lib/scrollHost.ts. */}
        <Show when={(viewMode() === "grid" || viewMode() === "justified") && !contentHidden()}>
          <div
            ref={(el) => {
              setScrollHost(el);
              onCleanup(() => setScrollHost(null));
            }}
            class="hide-scrollbar fixed inset-0 overflow-y-auto overflow-x-hidden"
            style={{ "overscroll-behavior-y": "contain" }}
          >
          <Show when={viewMode() === "grid"}>
            <GalleryGrid
              paths={displayPaths()}
              onItemClick={(index) => {
                clearSelection();
                openViewer(index);
              }}
              onItemSelect={(path) => toggleSelection(path)}
              onDragSelect={(paths) => setSelectedPaths(new Set(paths))}
              onBackgroundClick={clearSelection}
              selectedPaths={selectedPaths()}
              selectionMode={selectionMode()}
              onItemContextMenu={(e, path, index) => {
                setContextMenu({ x: e.clientX, y: e.clientY, path, index });
              }}
              loading={loading()}
              onContentHeight={setGalleryContentHeight}
            />
          </Show>
          <Show when={viewMode() === "justified"}>
            <JustifiedGrid
              paths={displayPaths()}
              aspects={aspectByPath()}
              itemMeta={mediaMetaByPath()}
              groupStarts={groups().map((g) => g.start_index)}
              onItemClick={(index) => {
                clearSelection();
                openViewer(index);
              }}
              onItemSelect={(path) => toggleSelection(path)}
              onDragSelect={(paths) => setSelectedPaths(new Set(paths))}
              onBackgroundClick={clearSelection}
              selectedPaths={selectedPaths()}
              selectionMode={selectionMode()}
              onItemContextMenu={(e, path, index) => {
                setContextMenu({ x: e.clientX, y: e.clientY, path, index });
              }}
              loading={loading()}
              onContentHeight={setGalleryContentHeight}
            />
          </Show>
          </div>
          {/* Outside the host on purpose. It is `fixed`, so a fixed element's
              scroll chain is the viewport either way — being inside would not
              give a touch on the rail anything to pan, and it would put an
              overlay inside the scroller for no gain. The 10px rail is
              therefore a strip that jumps on a tap and does not pan on a swipe,
              which is how a scrollbar behaves everywhere else. */}
          <ScrollBar
            contentHeight={galleryContentHeight()}
            indicators={scrollIndicators()}
            getThumbLabel={thumbLabel}
          />
          {/* Also shown at zero selected while selection mode is on — it's the
              mode's only exit, and the empty count tells the user the taps are
              landing somewhere. */}
          <Show when={selectedPaths().size > 0 || selectionMode()}>
            <SelectionBar
              selectedPaths={selectedPaths()}
              selectionMode={selectionMode()}
              onSelectAll={() => selectAll(displayPaths())}
              onClear={exitSelectionMode}
            />
          </Show>
          <ContextMenu
            state={contextMenu()}
            onClose={() => setContextMenu(null)}
            paths={displayPaths()}
            selectedPaths={selectedPaths()}
            hideViewOption={viewerOpen()}
            onFilesRemoved={(removed) => {
              const removedSet = new Set(removed);
              setDisplayPaths(displayPaths().filter((p) => !removedSet.has(p)));
              setSortedItems(sortedItems().filter((item) => !removedSet.has(item.path)));
              clearSelection();
            }}
            onFilesMoved={(moved) => {
              // Files stayed in the gallery — re-key the cells to their new
              // paths instead of removing them, so data and thumbnails persist.
              const rename = new Map(moved.map((m) => [m.from, m.to]));
              setDisplayPaths(displayPaths().map((p) => rename.get(p) ?? p));
              setSortedItems(
                sortedItems().map((item) =>
                  rename.has(item.path)
                    ? { ...item, path: rename.get(item.path)! }
                    : item,
                ),
              );
              setSelectedPaths((prev) => {
                let changed = false;
                const next = new Set<string>();
                for (const p of prev) {
                  const to = rename.get(p);
                  if (to) changed = true;
                  next.add(to ?? p);
                }
                return changed ? next : prev;
              });
            }}
          />
        </Show>
        <Show when={viewMode() === "map" && !contentHidden()}>
          <MapView />
        </Show>
        <Show when={viewerOpen()}>
          <MediaViewer
            paths={displayPaths()}
            currentIndex={viewerIndex()}
            onClose={closeViewer}
            onNext={() => nextImage(displayPaths().length)}
            onPrev={prevImage}
            onContextMenu={(e, path, index) => {
              setContextMenu({ x: e.clientX, y: e.clientY, path, index });
            }}
          />
        </Show>
        <Show when={duplicatesOpen()}>
          <DuplicatesPanel onClose={() => setDuplicatesOpen(false)} />
        </Show>
        <Show when={trashOpen()}>
          <TrashPanel onClose={() => setTrashOpen(false)} />
        </Show>
        <Show when={tagManagerOpen()}>
          {/* Renaming/merging a tag can change what the active filter matches,
              so re-run it after every edit rather than on close. */}
          <TagManagerPanel
            onClose={() => setTagManagerOpen(false)}
            onChanged={() => { void refreshFilteredItems(); }}
          />
        </Show>
        <Show when={autoTagOpen()}>
          <AutoTagPanel onClose={() => setAutoTagOpen(false)} />
        </Show>
        <Show when={isWeb()}>
          <UploadSheet
            open={uploadOpen()}
            onClose={() => setUploadOpen(false)}
            onUploaded={refreshAfterUpload}
          />
        </Show>
      </Show>
      <Show when={debugOpen()}>
        <DebugOverlay />
      </Show>
      <Show when={pluginActivity()}>
        <PluginToast />
      </Show>
      <Show when={thumbGenActivity()}>
        <ThumbnailToast />
      </Show>
      <Show when={isWeb()}>
        <PasswordModal />
      </Show>
    </div>
  );
}

function PluginToast() {
  const activity = () => pluginActivity()!;
  const statusColor = () => {
    switch (activity().status) {
      case "running": return "text-teal-400";
      case "done": return "text-green-400";
      case "error": return "text-red-400";
      case "cancelled": return "text-yellow-400";
    }
  };
  const borderColor = () => {
    switch (activity().status) {
      case "running": return "border-teal-500/30";
      case "done": return "border-green-500/30";
      case "error": return "border-red-500/30";
      case "cancelled": return "border-yellow-500/30";
    }
  };
  const progress = () => {
    const a = activity();
    if (a.total === 0) return 0;
    return Math.round((a.completed / a.total) * 100);
  };

  return (
    <div
      class={`fixed bottom-4 right-4 z-[150] flex flex-col gap-2 px-4 py-2.5 rounded-lg border ${borderColor()}`}
      style={{
        background: "rgba(18, 18, 18, 0.95)",
        "backdrop-filter": "blur(12px)",
        "min-width": "240px",
      }}
    >
      <div class="flex items-center gap-3">
        <Show when={activity().status === "running"}>
          <div class="w-3.5 h-3.5 shrink-0 border-2 border-teal-400 border-t-transparent rounded-full animate-spin" />
        </Show>
        <Show when={activity().status === "done"}>
          <svg class="w-3.5 h-3.5 shrink-0 text-green-400" fill="none" stroke="currentColor" viewBox="0 0 24 24">
            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2.5" d="M5 13l4 4L19 7" />
          </svg>
        </Show>
        <Show when={activity().status === "error"}>
          <svg class="w-3.5 h-3.5 shrink-0 text-red-400" fill="none" stroke="currentColor" viewBox="0 0 24 24">
            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2.5" d="M6 18L18 6M6 6l12 12" />
          </svg>
        </Show>
        <Show when={activity().status === "cancelled"}>
          <svg class="w-3.5 h-3.5 shrink-0 text-yellow-400" fill="none" stroke="currentColor" viewBox="0 0 24 24">
            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2.5" d="M6 18L18 6M6 6l12 12" />
          </svg>
        </Show>
        <div class="flex flex-col flex-1 min-w-0">
          <span class={`text-xs font-medium ${statusColor()}`}>{activity().displayName}</span>
          <span class="text-[11px] text-neutral-400">{activity().message}</span>
        </div>
        <Show when={activity().status === "running"}>
          <button
            onClick={() => {
              // The toast serves both local plugin batches (desktop) and
              // remote worker jobs (web) — cancel whichever is driving it.
              const jobId = activeTaggingJobId();
              if (jobId) {
                cancelTaggingJob(jobId).catch((e) => console.error("Cancel failed:", e));
              } else {
                cancelPluginBatch();
              }
            }}
            class="shrink-0 px-2 py-0.5 text-[10px] rounded cursor-pointer transition-colors bg-neutral-700 text-neutral-400 hover:bg-neutral-600 hover:text-neutral-200"
            title="Cancel"
          >
            Cancel
          </button>
        </Show>
      </div>
      <Show when={activity().status === "running" && activity().total > 0}>
        <div class="w-full h-1.5 bg-neutral-800 rounded-full overflow-hidden">
          <div
            class="h-full bg-teal-500 rounded-full transition-all duration-300"
            style={{ width: `${progress()}%` }}
          />
        </div>
      </Show>
    </div>
  );
}

function ThumbnailToast() {
  const activity = () => thumbGenActivity()!;
  const statusColor = () => {
    switch (activity().status) {
      case "generating": return "text-blue-400";
      case "done": return "text-green-400";
      case "error": return "text-red-400";
    }
  };
  const borderColor = () => {
    switch (activity().status) {
      case "generating": return "border-blue-500/30";
      case "done": return "border-green-500/30";
      case "error": return "border-red-500/30";
    }
  };
  const progress = () => {
    const a = activity();
    if (a.total === 0) return 0;
    return Math.round((a.generated / a.total) * 100);
  };

  return (
    <div
      class={`fixed bottom-14 right-4 z-[150] flex items-center gap-3 px-4 py-2.5 rounded-lg border ${borderColor()}`}
      style={{
        background: "rgba(18, 18, 18, 0.95)",
        "backdrop-filter": "blur(12px)",
      }}
    >
      <Show when={activity().status === "generating"}>
        <div class="w-3.5 h-3.5 border-2 border-blue-400 border-t-transparent rounded-full animate-spin" />
      </Show>
      <Show when={activity().status === "done"}>
        <svg class="w-3.5 h-3.5 text-green-400" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2.5" d="M5 13l4 4L19 7" />
        </svg>
      </Show>
      <Show when={activity().status === "error"}>
        <svg class="w-3.5 h-3.5 text-red-400" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2.5" d="M6 18L18 6M6 6l12 12" />
        </svg>
      </Show>
      <div class="flex flex-col">
        <span class={`text-xs font-medium ${statusColor()}`}>
          Thumbnails{activity().status === "generating" ? ` ${progress()}%` : ""}
        </span>
        <span class="text-[11px] text-neutral-400">{activity().message}</span>
      </div>
    </div>
  );
}

function WelcomeScreen(props: { onOpen: () => void; onOpenPath: (path: string) => void }) {
  const [manualPath, setManualPath] = createSignal("");
  const [error, setError] = createSignal("");
  const [recents, setRecents] = createSignal<RecentGallery[]>([]);

  // Load recent galleries on mount (desktop only — not exposed to the web client)
  if (!isWeb()) {
    getRecentGalleries()
      .then(setRecents)
      .catch(() => {});
  }

  // The web client can't open or pick galleries — it mirrors the desktop's
  // currently-open one. If none is open, there's nothing to show.
  if (isWeb()) {
    return (
      <div class="h-screen w-full flex flex-col items-center justify-center gap-4">
        <h1 class="text-3xl font-light text-neutral-300">LightView</h1>
        <p class="text-sm text-neutral-500">
          No gallery is open. Open one in the LightView desktop app.
        </p>
      </div>
    );
  }

  const handleSubmit = (e: Event) => {
    e.preventDefault();
    const p = manualPath().trim();
    if (p) {
      setError("");
      props.onOpenPath(p);
    }
  };

  const handleRemoveRecent = async (e: MouseEvent, path: string) => {
    e.stopPropagation();
    try {
      await removeRecentGallery(path);
      setRecents((prev) => prev.filter((r) => r.path !== path));
    } catch {}
  };

  /** Extract just the folder name for display, show full path underneath. */
  const folderName = (path: string) => {
    const parts = path.replace(/\/+$/, "").split("/");
    return parts[parts.length - 1] || path;
  };

  const formatDate = (ts: number) => {
    const d = new Date(ts * 1000);
    const now = new Date();
    const diffMs = now.getTime() - d.getTime();
    const diffDays = Math.floor(diffMs / (1000 * 60 * 60 * 24));
    if (diffDays === 0) return "Today";
    if (diffDays === 1) return "Yesterday";
    if (diffDays < 7) return `${diffDays} days ago`;
    return d.toLocaleDateString();
  };

  return (
    <div class="h-screen w-full flex flex-col items-center justify-center gap-6">
      <h1 class="text-3xl font-light text-neutral-300">LightView</h1>
      <p class="text-sm text-neutral-500">Open a folder to browse your media</p>
      <button
        onClick={props.onOpen}
        class="px-6 py-3 bg-neutral-800 hover:bg-neutral-700 text-neutral-200 rounded-lg transition-colors text-sm cursor-pointer"
      >
        Open Folder
      </button>

      <Show when={recents().length > 0}>
        <div class="w-full max-w-md px-8 mt-2">
          <div class="text-neutral-500 text-xs mb-2">Recent</div>
          <div class="flex flex-col gap-1">
            {recents().map((r) => (
              <button
                onClick={() => props.onOpenPath(r.path)}
                class="group flex items-center gap-3 w-full px-3 py-2 rounded hover:bg-neutral-800 transition-colors text-left cursor-pointer"
              >
                <div class="flex-shrink-0 w-8 h-8 rounded bg-neutral-800 group-hover:bg-neutral-700 flex items-center justify-center text-neutral-500 text-xs">
                  <svg class="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                    <path stroke-linecap="round" stroke-linejoin="round" stroke-width="1.5" d="M3 7v10a2 2 0 002 2h14a2 2 0 002-2V9a2 2 0 00-2-2h-6l-2-2H5a2 2 0 00-2 2z" />
                  </svg>
                </div>
                <div class="flex-1 min-w-0">
                  <div class="text-sm text-neutral-200 truncate">{folderName(r.path)}</div>
                  <div class="text-xs text-neutral-500 truncate">{r.path}</div>
                </div>
                <div class="flex-shrink-0 flex items-center gap-2">
                  <span class="text-xs text-neutral-600">{formatDate(r.last_opened)}</span>
                  <span
                    onClick={(e) => handleRemoveRecent(e, r.path)}
                    class="opacity-0 group-hover:opacity-100 text-neutral-600 hover:text-neutral-400 transition-opacity cursor-pointer p-1"
                    title="Remove from recent"
                  >
                    <svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                      <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M6 18L18 6M6 6l12 12" />
                    </svg>
                  </span>
                </div>
              </button>
            ))}
          </div>
        </div>
      </Show>

      <div class="text-neutral-600 text-xs">or enter a path</div>
      <form onSubmit={handleSubmit} class="flex gap-2 w-full max-w-lg px-8">
        <input
          type="text"
          value={manualPath()}
          onInput={(e) => setManualPath(e.currentTarget.value)}
          placeholder="/path/to/photos"
          class="flex-1 px-3 py-2 bg-neutral-800 border border-neutral-700 rounded text-sm text-neutral-200 placeholder-neutral-500 outline-none focus:border-neutral-500"
        />
        <button
          type="submit"
          class="px-4 py-2 bg-neutral-700 hover:bg-neutral-600 text-neutral-200 rounded text-sm cursor-pointer transition-colors"
        >
          Go
        </button>
      </form>
      <Show when={error()}>
        <p class="text-red-400 text-xs">{error()}</p>
      </Show>
    </div>
  );
}
