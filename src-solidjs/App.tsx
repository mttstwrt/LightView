// The application shell: what is on screen, and the boot sequence.
//
// **There is one gallery and the process already has it.** `lightview <dir>`
// binds a root, a cache directory, a lock and a watcher at startup, so the
// welcome screen, the folder picker that fed it and the recent-galleries list
// are all gone: opening a different folder is a different process. What is left
// of boot is fetching what this client is allowed to do and running one query.
//
// **The first request can legitimately fail, and each way means something
// different.** A `503` is the readiness gate — the initial scan has not
// finished and the watcher is not armed, so a file arriving now would be in
// neither — and the answer is to wait and ask again, however long the scan
// takes. A `401` is a credential problem, and which one depends on the bind:
// on a served bind there is a pairing flow to go to, and on a loopback bind
// there is not, because a browser cannot read `instance.json` to find the new
// URL. Anything else is the network, which is what `ConnectionBanner` is for.
//
// Everything below the shell reads stores rather than props. This file wires
// the pieces together and owns the keyboard map — not gallery logic.

import { Show, createSignal, onCleanup, onMount } from "solid-js";

import { PasswordModal } from "./components/auth/PasswordModal";
import { ConnectionBanner } from "./components/ConnectionBanner";
import { ContextMenu, type ContextMenuState } from "./components/shared/ContextMenu";
import { DuplicatesPanel } from "./components/DuplicatesPanel";
import { JustifiedGrid } from "./components/gallery/JustifiedGrid";
import { MediaViewer } from "./components/viewer/MediaViewer";
import { ScrollBar, type ScrollIndicator } from "./components/shared/ScrollBar";
import { SelectionBar } from "./components/gallery/SelectionBar";
import { TagManagerPanel } from "./components/TagManagerPanel";
import { AutoTagPanel } from "./components/AutoTagPanel";
import { TopBar } from "./components/topbar/TopBar";
import { TrashPanel } from "./components/TrashPanel";
import { UploadSheet } from "./components/upload/UploadSheet";
import type { CommandHandlers } from "./components/topbar/CommandMenu";

import { IpcError, onAuthInterruption, subscribe } from "./lib/ipc";
import { createOpenAtBottom } from "./lib/openAtBottom";
import { isMobile } from "./lib/runtime";
import { setScrollHost } from "./lib/scrollHost";
import {
  buildScrollIndicators,
  getThumbLabelForItems,
} from "./lib/scrollIndicators";
import { VIEWER_CLOSE_REQUEST_EVENT } from "./lib/viewerTransition";
import type { SortedItem, SortField } from "./lib/types";

import {
  applyEvent as applyActivityEvent,
  loadPlugins,
  run as pluginRun,
  thumbWork,
} from "./stores/activityStore";
import {
  buildFilterQuery,
  refreshFilteredItems,
  setFilterQuery,
} from "./stores/filterStore";
import {
  aspectByPath,
  applyEvent as applyGalleryEvent,
  clearSelection,
  displayPaths,
  exitSelectionMode,
  groups,
  items,
  loading,
  mediaMetaByPath,
  rateItem,
  selectAll,
  selectedPaths,
  selectionMode,
  setItems,
  setSelectedPaths,
  settingsOpen,
  setSettingsOpen,
  toggleSelection,
} from "./stores/galleryStore";
import {
  capabilities,
  gallerySettings,
  loadCapabilities,
  loadGallerySettings,
  prefs,
  sortField,
} from "./stores/settingsStore";
import {
  closeViewer,
  infoPanelOpen,
  nextImage,
  openViewer,
  prevImage,
  toggleInfoPanel,
  viewerIndex,
  viewerOpen,
} from "./stores/viewerStore";

/** How long to wait before asking again while the gallery is still opening.
 *  Short enough that a small gallery appears to open instantly, long enough
 *  that a two-minute scan is not two hundred requests. */
const READY_POLL_MS = 400;

/** What the shell is doing, when it is not showing a gallery. */
type BootState =
  | { phase: "opening" }
  | { phase: "ready" }
  /** The network, or the server, is not answering. Retryable. */
  | { phase: "unreachable" }
  /** Loopback, and the credential is gone: the process this tab belonged to
   *  is not the process listening now. There is nothing to retry. */
  | { phase: "ended" };

export function App() {
  const [boot, setBoot] = createSignal<BootState>({ phase: "opening" });
  const [duplicatesOpen, setDuplicatesOpen] = createSignal(false);
  const [trashOpen, setTrashOpen] = createSignal(false);
  const [tagManagerOpen, setTagManagerOpen] = createSignal(false);
  const [autoTagOpen, setAutoTagOpen] = createSignal(false);
  const [uploadOpen, setUploadOpen] = createSignal(false);
  const [contextMenu, setContextMenu] = createSignal<ContextMenuState | null>(null);
  const [galleryContentHeight, setGalleryContentHeight] = createSignal(0);

  // "Start at bottom" — land at the end of the grid rather than its start.
  //
  // Keyed on the filter rather than on a gallery path: there is one gallery for
  // the life of the process, so the thing that makes a *new* document to land
  // in is a new result set. Not keyed on the sort, where "reverse the order,
  // then jump to the end" would undo itself.
  createOpenAtBottom({
    enabled: () => prefs().start_at_bottom,
    galleryKey: () => buildFilterQuery(),
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
  // on every item change even for the many sessions that never touch the
  // scrollbar. The signal reads stay in the caller's tracking scope, so
  // consumers still update exactly as before.
  let indicatorCache:
    | { items: SortedItem[]; field: SortField; value: ScrollIndicator[] }
    | null = null;
  const scrollIndicators = (): ScrollIndicator[] => {
    const list = items();
    const field = sortField();
    if (!indicatorCache || indicatorCache.items !== list || indicatorCache.field !== field) {
      indicatorCache = { items: list, field, value: buildScrollIndicators(list, field) };
    }
    return indicatorCache.value;
  };
  const thumbLabel = (fraction: number) =>
    getThumbLabelForItems(items(), sortField(), fraction);

  // -------------------------------------------------------------------------
  // Boot
  // -------------------------------------------------------------------------

  /** Fetch what this client may do, what the gallery is set to, and the grid.
   *
   *  Also the reconnect path and the banner's Retry, so it has to be safe to
   *  run repeatedly — every step is a replace rather than an append. */
  const load = async () => {
    try {
      await Promise.all([loadCapabilities(), loadGallerySettings(), loadPlugins()]);

      // The gallery's default filter is user intent stored outside the cache,
      // so it applies to whichever client opens the gallery — including the
      // first one after a cache rebuild. Seeded into the bar rather than
      // applied invisibly: a grid showing a subset with an empty filter box is
      // indistinguishable from a gallery that lost photos.
      const stored = gallerySettings().default_filter.trim();
      if (stored && !buildFilterQuery()) setFilterQuery(stored);

      await refreshFilteredItems();
      setBoot({ phase: "ready" });
    } catch (e) {
      if (e instanceof IpcError && e.status === 503) {
        // The initial scan is still running. Ask again — for as long as it
        // takes, because a first open of a large gallery on a spinning disk
        // legitimately takes minutes and a timeout here would be a failure
        // message for a working system.
        setBoot({ phase: "opening" });
        setTimeout(() => void load(), READY_POLL_MS);
        return;
      }
      if (e instanceof IpcError && e.status === 401) {
        // `onAuthInterruption` has already decided what this means and set the
        // phase; don't overwrite it with "unreachable".
        return;
      }
      console.error("Could not open the gallery:", e);
      setBoot({ phase: "unreachable" });
    }
  };

  onMount(() => {
    void load();

    // Auth, absorbed by `ipc.ts` and surfaced here. The password challenge is
    // the modal's own business; the other two are shell states.
    const stopAuth = onAuthInterruption((interruption) => {
      switch (interruption.kind) {
        case "not-paired":
          window.location.replace("/pair");
          break;
        case "session-ended":
          setBoot({ phase: "ended" });
          break;
        default:
          break;
      }
    });
    onCleanup(stopAuth);

    // One stream, one subscription. `onopen` after the first re-runs boot
    // rather than replaying history: `EventSource` reconnects silently, and on
    // a phone that happens constantly — screen lock, Wi-Fi to LTE,
    // backgrounding — so without it the client sits on a confidently wrong
    // grid indefinitely.
    const stopEvents = subscribe(
      (event) => {
        void applyGalleryEvent(event);
        applyActivityEvent(event);
      },
      () => void load(),
    );
    onCleanup(stopEvents);
  });

  // -------------------------------------------------------------------------
  // Commands and keys
  // -------------------------------------------------------------------------

  // What the command list runs. Declared here because this is where the
  // panels' open state lives; the list itself — what is offered, in what
  // order, on which surface — is `CommandMenu.tsx`.
  const commandHandlers: CommandHandlers = {
    openTagManager: () => setTagManagerOpen(true),
    openDuplicates: () => setDuplicatesOpen(true),
    openTrash: () => setTrashOpen(true),
    openAutoTag: () => setAutoTagOpen(true),
    openUpload: () => setUploadOpen(true),
    openSettings: () => setSettingsOpen(true),
  };

  // Throttle held arrow keys to one navigation per frame so the viewer
  // doesn't queue up a backlog of image loads that keep playing after release.
  let navPending = false;
  let navDirection: "left" | "right" | null = null;

  const flushNav = () => {
    navPending = false;
    if (!navDirection || !viewerOpen()) {
      navDirection = null;
      return;
    }
    if (navDirection === "right") nextImage(displayPaths().length);
    else prevImage();
    window.dispatchEvent(
      new CustomEvent("lightview:scroll-to-index", { detail: viewerIndex() }),
    );
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
      } else if (e.key === "ArrowRight" || e.key === "ArrowLeft") {
        if (typingInInput) return;
        if (e.repeat && navPending) return;
        navDirection = e.key === "ArrowRight" ? "right" : "left";
        if (!navPending) {
          navPending = true;
          requestAnimationFrame(flushNav);
        }
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
          rateItem(paths[idx], Number(e.key)).catch(() => {});
        }
      }
    } else {
      if (e.key === "Escape") exitSelectionMode();
      if ((e.ctrlKey || e.metaKey) && e.key === "a") {
        e.preventDefault();
        selectAll(displayPaths());
      }
    }
  };

  window.addEventListener("keydown", handleKeyDown);
  onCleanup(() => window.removeEventListener("keydown", handleKeyDown));

  // -------------------------------------------------------------------------

  return (
    <div
      class="min-h-screen w-screen relative"
      style={{ background: prefs().background_color }}
    >
      <Show when={boot().phase === "unreachable"}>
        <ConnectionBanner onRetry={load} />
      </Show>

      <Show when={boot().phase === "ended"}>
        <SessionEnded />
      </Show>

      <Show when={boot().phase === "opening"}>
        <Opening />
      </Show>

      <Show when={boot().phase !== "ended"}>
        <TopBar commands={commandHandlers} />
        {/* The gallery scrolls in here rather than in the document. iOS draws
            its own scroll indicator over the page, that indicator is
            interactive (press and hold to scrub), and it cannot be styled away
            on the document scroller — `::-webkit-scrollbar` only reaches
            element scrollers. Owning the scroller is what leaves LightView's
            bar, the one with the date markers, as the only one on screen.
            Being positioned also makes this the grid's `offsetParent`, so its
            `offsetTop` measurements share an origin with `scrollTop`. See
            lib/scrollHost.ts. */}
        <Show when={!contentHidden()}>
          <div
            ref={(el) => {
              setScrollHost(el);
              onCleanup(() => setScrollHost(null));
            }}
            class="hide-scrollbar fixed inset-0 overflow-y-auto overflow-x-hidden"
            style={{ "overscroll-behavior-y": "contain" }}
          >
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
              const gone = new Set(removed);
              setItems((list) => list.filter((item) => !gone.has(item.path)));
              clearSelection();
            }}
          />
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
            onChanged={() => void refreshFilteredItems()}
          />
        </Show>
        <Show when={autoTagOpen()}>
          <AutoTagPanel onClose={() => setAutoTagOpen(false)} />
        </Show>
        <Show when={capabilities().upload}>
          <UploadSheet open={uploadOpen()} onClose={() => setUploadOpen(false)} />
        </Show>
      </Show>

      <Show when={pluginRun()}>
        <PluginToast />
      </Show>
      <Show when={thumbWork()}>
        <ThumbnailToast />
      </Show>
      <PasswordModal />
    </div>
  );
}

/** The readiness gate, said out loud.
 *
 *  A first open scans the whole tree before any route answers, because a file
 *  arriving between "scan finished" and "watcher armed" would be in neither and
 *  nothing would ever notice it. On a large gallery that is minutes, and an
 *  unexplained blank grid for minutes reads as a broken app. */
function Opening() {
  return (
    <div class="fixed inset-0 z-[80] flex flex-col items-center justify-center gap-3 bg-neutral-950">
      <div class="w-6 h-6 border-2 border-teal-500 border-t-transparent rounded-full animate-spin" />
      <p class="text-sm text-neutral-400">Opening the gallery…</p>
      <p class="text-xs text-neutral-600">
        Indexing runs once; later opens are immediate.
      </p>
    </div>
  );
}

/** A loopback session whose credential is gone.
 *
 *  Deliberately a dead end rather than a retry or a redirect. There is no
 *  pairing flow on a loopback bind, and the new URL lives in `instance.json`,
 *  which a browser cannot read — that is exactly the filesystem access the
 *  trust model exists to withhold. So the only true thing to say is how to
 *  start again. */
function SessionEnded() {
  return (
    <div class="fixed inset-0 z-[300] flex flex-col items-center justify-center gap-3 bg-neutral-950 px-6 text-center">
      <h1 class="text-lg font-light text-neutral-300">This session has ended</h1>
      <p class="text-sm text-neutral-500 max-w-sm">
        Start LightView again; it will open a new tab.
      </p>
    </div>
  );
}

function PluginToast() {
  const activity = () => pluginRun()!;
  const progress = () => {
    const a = activity();
    return a.total === 0 ? 0 : Math.round((a.done / a.total) * 100);
  };

  return (
    <div
      class="fixed bottom-4 right-4 z-[150] flex flex-col gap-2 px-4 py-2.5 rounded-lg border border-teal-500/30"
      style={{
        background: "rgba(18, 18, 18, 0.95)",
        "backdrop-filter": "blur(12px)",
        "min-width": "240px",
      }}
    >
      <div class="flex items-center gap-3">
        <div class="w-3.5 h-3.5 shrink-0 border-2 border-teal-400 border-t-transparent rounded-full animate-spin" />
        <div class="flex flex-col flex-1 min-w-0">
          <span class="text-xs font-medium text-teal-400">{activity().plugin}</span>
          <span class="text-[11px] text-neutral-400">
            {activity().done} / {activity().total}
          </span>
        </div>
      </div>
      <Show when={activity().total > 0}>
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
  const work = () => thumbWork()!;
  const progress = () => {
    const w = work();
    return w.total === 0 ? 0 : Math.round((w.done / w.total) * 100);
  };

  return (
    <div
      class="fixed bottom-14 right-4 z-[150] flex items-center gap-3 px-4 py-2.5 rounded-lg border border-blue-500/30"
      style={{
        background: "rgba(18, 18, 18, 0.95)",
        "backdrop-filter": "blur(12px)",
      }}
    >
      <div class="w-3.5 h-3.5 border-2 border-blue-400 border-t-transparent rounded-full animate-spin" />
      <div class="flex flex-col">
        <span class="text-xs font-medium text-blue-400">Thumbnails {progress()}%</span>
        <span class="text-[11px] text-neutral-400">
          {work().done} / {work().total}
        </span>
      </div>
    </div>
  );
}
