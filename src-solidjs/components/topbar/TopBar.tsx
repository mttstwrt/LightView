import { Show, For, createSignal, createEffect, onCleanup, onMount } from "solid-js";
import { FilterBar } from "./FilterBar";
import { SortMenu } from "./SortMenu";
import { SettingsMenu } from "./SettingsMenu";
import { TitleBar } from "./TitleBar";
import { GearIcon, CloseIcon, SelectIcon } from "./icons";
import { viewMode, setViewMode, enabledViews, VIEW_CHOICES, displayPaths, settingsOpen, setSettingsOpen, selectionMode, toggleSelectionMode } from "../../stores/galleryStore";
import { settings } from "../../stores/settingsStore";
import { isMobile, isTauri } from "../../lib/runtime";

interface TopBarProps {
  onOpenFolder: () => void;
  onOpenDuplicates?: () => void;
  onOpenTrash?: () => void;
  onOpenTagManager?: () => void;
}

// Pixels the user must scroll past 0 before we'll consider hiding the bar.
const MOBILE_REVEAL_AT_TOP = 40;
// Scroll-delta threshold to trigger a direction change (anti-jitter).
const MOBILE_DIR_THRESHOLD = 6;

export function TopBar(props: TopBarProps) {
  const [hoverVisible, setHoverVisible] = createSignal(false);
  // Mobile: track-by-scroll-direction. Bar starts visible.
  const [scrollHidden, setScrollHidden] = createSignal(false);
  // Mobile: the search button expands filter + sort into a sheet.
  const [filterOpen, setFilterOpen] = createSignal(false);
  let filterInputRef: HTMLInputElement | undefined;

  // On desktop the bar is hover-gated; on mobile it follows scroll direction.
  const visible = () => (isMobile() ? !scrollHidden() : hoverVisible());

  // Mobile filter/sort sheet: pinned to the top under the safe-area inset, or a
  // thumb-reachable sheet sliding up from the bottom (user preference).
  const sheetAtBottom = () => settings().display.mobile_filter_sheet === "bottom";
  // Drives the slide-in transition: the sheet mounts offscreen (entered=false),
  // then flips after a paint so the transform transitions instead of snapping.
  const [sheetEntered, setSheetEntered] = createSignal(false);
  const openFilterSheet = (selectText = false) => {
    setScrollHidden(false);
    setFilterOpen(true);
    // Focus synchronously: <Show> mounts the sheet in the same tick, and iOS
    // Safari only raises the keyboard when focus() runs inside the user
    // gesture — deferring to rAF left the input focused but the keyboard shut.
    if (filterInputRef) {
      filterInputRef.focus();
      if (selectText) filterInputRef.select();
    } else {
      requestAnimationFrame(() => {
        filterInputRef?.focus();
        if (selectText) filterInputRef?.select();
      });
    }
  };
  createEffect(() => {
    if (!filterOpen()) { setSheetEntered(false); return; }
    setSheetEntered(false);
    // Double-rAF guarantees one frame paints with the offscreen transform
    // before the transition target is set.
    requestAnimationFrame(() => requestAnimationFrame(() => {
      if (filterOpen()) setSheetEntered(true);
    }));
  });

  // Height of the software keyboard overlapping the layout viewport. iOS
  // Safari overlays the keyboard without resizing the layout viewport, so a
  // `bottom: 0` sheet vanishes behind it the moment the input is tapped; we
  // track visualViewport and translate the sheet up by the overlap instead.
  // On Android `interactive-widget=resizes-content` (index.html) shrinks the
  // layout viewport, so the overlap computes to ~0 and this is a no-op.
  const [kbOffset, setKbOffset] = createSignal(0);
  createEffect(() => {
    if (!(filterOpen() && sheetAtBottom())) { setKbOffset(0); return; }
    const vv = window.visualViewport;
    if (!vv) return;
    const update = () =>
      setKbOffset(Math.max(0, window.innerHeight - vv.height - vv.offsetTop));
    update();
    vv.addEventListener("resize", update);
    vv.addEventListener("scroll", update);
    onCleanup(() => {
      vv.removeEventListener("resize", update);
      vv.removeEventListener("scroll", update);
      setKbOffset(0);
    });
  });

  // Mobile: the filter sheet and the settings page are "screens", so the
  // platform back gesture (Android back button, browser edge-swipe) should
  // close them rather than leave the gallery. Opening one pushes a history
  // entry; the popstate from the back gesture closes them. A UI close (X /
  // backdrop) consumes the pushed entry with history.back() so a later back
  // gesture isn't swallowed by a stale entry.
  let overlayPushed = false;
  createEffect(() => {
    const overlayShown = isMobile() && (filterOpen() || settingsOpen());
    if (overlayShown && !overlayPushed) {
      overlayPushed = true;
      history.pushState({ lvOverlay: true }, "");
    } else if (!overlayShown && overlayPushed) {
      overlayPushed = false;
      if (history.state?.lvOverlay) history.back();
    }
  });

  // Frameless (decorations: false) desktop gets a custom titlebar row above the
  // filter row; both reveal together off the same hover state.
  const frameless = () => isTauri() && !isMobile();

  // Debounce the hide so the pointer can travel across the gap between the
  // titlebar and filter rows without the chrome collapsing mid-move.
  let hideTimer: number | undefined;
  const handleMouseEnter = () => {
    if (isMobile()) return;
    if (hideTimer) { clearTimeout(hideTimer); hideTimer = undefined; }
    setHoverVisible(true);
  };
  const handleMouseLeave = () => {
    if (isMobile()) return;
    if (hideTimer) clearTimeout(hideTimer);
    hideTimer = window.setTimeout(() => setHoverVisible(false), 100);
  };
  onCleanup(() => { if (hideTimer) clearTimeout(hideTimer); });

  const handleGlobalKeyDown = (e: KeyboardEvent) => {
    if ((e.ctrlKey || e.metaKey) && e.key === "f") {
      e.preventDefault();
      if (isMobile()) {
        openFilterSheet(true);
        return;
      }
      setHoverVisible(true);
      setScrollHidden(false);
      requestAnimationFrame(() => {
        filterInputRef?.focus();
        filterInputRef?.select();
      });
    }
    // Hardware-keyboard escape hatch for the sheet (tablets, DeX, etc.).
    if (e.key === "Escape" && isMobile() && filterOpen()) {
      setFilterOpen(false);
    }
  };

  onMount(() => {
    window.addEventListener("keydown", handleGlobalKeyDown);

    // Mobile scroll-direction watcher. Always attached — the handler bails out
    // on desktop so behavior stays purely hover-driven there.
    let lastY = window.scrollY;
    const onScroll = () => {
      if (!isMobile()) return;
      const y = window.scrollY;
      const dy = y - lastY;
      if (y < MOBILE_REVEAL_AT_TOP) {
        setScrollHidden(false);
      } else if (dy > MOBILE_DIR_THRESHOLD) {
        setScrollHidden(true);
      } else if (dy < -MOBILE_DIR_THRESHOLD) {
        setScrollHidden(false);
      }
      lastY = y;
    };
    window.addEventListener("scroll", onScroll, { passive: true });

    // Back gesture / back button closes mobile overlays (see the pushState
    // effect above). When we consume our own entry after a UI close, this
    // fires too, but `overlayPushed` is already false so it's a no-op.
    const onPopState = () => {
      if (overlayPushed) {
        overlayPushed = false;
        setFilterOpen(false);
        setSettingsOpen(false);
      }
    };
    window.addEventListener("popstate", onPopState);

    onCleanup(() => {
      window.removeEventListener("keydown", handleGlobalKeyDown);
      window.removeEventListener("scroll", onScroll);
      window.removeEventListener("popstate", onPopState);
    });
  });

  return (
    <>
      {/* ── Desktop chrome ─────────────────────────────────────────────── */}
      <Show when={!isMobile()}>
        {/* Hover trigger zone — reveals the bar on pointer approach. */}
        <div
          class="fixed top-0 left-0 right-0 h-15 z-40"
          onMouseEnter={handleMouseEnter}
        />

        {/* Custom window titlebar — only when the native frame is hidden. */}
        <Show when={frameless()}>
          <TitleBar
            visible={visible()}
            onMouseEnter={handleMouseEnter}
            onMouseLeave={handleMouseLeave}
          />
        </Show>

        {/* The bar itself. We slide via `top` (not `transform`) so the bar
            doesn't establish a containing block for fixed descendants —
            otherwise SettingsMenu's drawer would be confined to it. */}
        <div
          class="fixed left-0 right-0 z-40 h-12 flex items-center px-3 sm:px-4 gap-2 sm:gap-3 transition-[top,opacity] duration-200"
          style={{
            background: "rgba(10, 10, 10, 0.85)",
            "backdrop-filter": "blur(12px)",
            top: visible() ? (frameless() ? "2rem" : "0") : "-3rem",
            opacity: visible() ? "1" : "0",
          }}
          onMouseEnter={handleMouseEnter}
          onMouseLeave={handleMouseLeave}
        >
          <FilterBar onInputRef={(el) => { filterInputRef = el; }} />
          <SortMenu />
          {/* Image count for the current filter. */}
          <div
            class="shrink-0 text-xs text-neutral-400 tabular-nums whitespace-nowrap px-1"
            title="Images in the current filter"
          >
            {displayPaths().length.toLocaleString()}
          </div>
          {/* View-mode selector. Only the views this gallery has enabled —
              a disabled view generates no thumbnails, so offering it would
              open onto a grid decoding the library on the spot. */}
          <div class="shrink-0 flex items-center gap-0.5 p-0.5 rounded bg-neutral-800/60">
            <For each={VIEW_CHOICES.filter((v) => enabledViews().includes(v.mode))}>
              {(v) => (
                <button
                  onClick={() => setViewMode(v.mode)}
                  class="px-2 py-0.5 text-xs rounded cursor-pointer transition-colors text-neutral-300 hover:bg-neutral-700"
                  classList={{ "bg-neutral-700 text-white": viewMode() === v.mode }}
                  title={v.title}
                >
                  {v.label}
                </button>
              )}
            </For>
          </div>
          <SettingsMenu onOpenFolder={props.onOpenFolder} onOpenDuplicates={props.onOpenDuplicates} onOpenTrash={props.onOpenTrash} onOpenTagManager={props.onOpenTagManager} onRequestShow={() => { setHoverVisible(true); setScrollHidden(false); }} />
        </div>
      </Show>

      {/* ── Mobile chrome ──────────────────────────────────────────────── */}
      {/* Opaque floating buttons over an edge-to-edge grid instead of a
          full-width bar: a search button (left) that expands filter + sort,
          then a Select toggle and a settings gear (right). All sit below the
          safe-area inset so they clear the notch / dynamic island, and all
          slide away on scroll-down (reusing the scroll-direction `visible()`
          signal). */}
      <Show when={isMobile()}>
        {/* Search / filter button */}
        <button
          class="fixed z-40 w-11 h-11 flex items-center justify-center rounded-full text-neutral-100 shadow-lg transition-[transform,opacity] duration-200"
          style={{
            left: "calc(env(safe-area-inset-left, 0px) + 12px)",
            top: "calc(env(safe-area-inset-top, 0px) + 8px)",
            background: "rgba(23, 23, 23, 0.92)",
            border: "1px solid rgba(255, 255, 255, 0.10)",
            transform: visible() ? "translateY(0)" : "translateY(-150%)",
            opacity: visible() ? "1" : "0",
            "pointer-events": visible() ? "auto" : "none",
          }}
          onClick={() => openFilterSheet()}
          title="Filter and sort"
          aria-label="Filter and sort"
        >
          <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <circle cx="11" cy="11" r="7" />
            <path d="M21 21l-4.3-4.3" />
          </svg>
        </button>

        {/* Select-mode toggle. Touch has no Ctrl/Cmd, so this is the only way
            into multi-select on a phone: while it's lit, a tap on a cell
            toggles that cell instead of opening the viewer. Stays put when the
            bar hides on scroll-down like its neighbours. */}
        <button
          class="fixed z-40 w-11 h-11 flex items-center justify-center rounded-full shadow-lg transition-[transform,opacity,background] duration-200"
          style={{
            // Sits one button-width (44px) plus a 8px gap left of the gear.
            right: "calc(env(safe-area-inset-right, 0px) + 64px)",
            top: "calc(env(safe-area-inset-top, 0px) + 8px)",
            background: selectionMode() ? "rgb(37, 99, 235)" : "rgba(23, 23, 23, 0.92)",
            color: selectionMode() ? "#fff" : "rgb(245, 245, 245)",
            border: selectionMode()
              ? "1px solid rgba(255, 255, 255, 0.25)"
              : "1px solid rgba(255, 255, 255, 0.10)",
            transform: visible() ? "translateY(0)" : "translateY(-150%)",
            opacity: visible() ? "1" : "0",
            "pointer-events": visible() ? "auto" : "none",
          }}
          onClick={() => toggleSelectionMode()}
          title={selectionMode() ? "Exit selection" : "Select items"}
          aria-label={selectionMode() ? "Exit selection" : "Select items"}
          aria-pressed={selectionMode()}
        >
          <SelectIcon size={20} />
        </button>

        {/* Settings gear button */}
        <button
          class="fixed z-40 w-11 h-11 flex items-center justify-center rounded-full text-neutral-100 shadow-lg transition-[transform,opacity] duration-200"
          style={{
            right: "calc(env(safe-area-inset-right, 0px) + 12px)",
            top: "calc(env(safe-area-inset-top, 0px) + 8px)",
            background: "rgba(23, 23, 23, 0.92)",
            border: "1px solid rgba(255, 255, 255, 0.10)",
            transform: visible() ? "translateY(0)" : "translateY(-150%)",
            opacity: visible() ? "1" : "0",
            "pointer-events": visible() ? "auto" : "none",
          }}
          onClick={() => setSettingsOpen(true)}
          title="Settings"
          aria-label="Settings"
        >
          <GearIcon size={20} />
        </button>

        {/* Filter + sort sheet */}
        <Show when={filterOpen()}>
          {/* Backdrop — tap to dismiss. touch-action/overscroll stop drag
              gestures from scrolling the gallery behind the open sheet. */}
          <div
            class="fixed inset-0 z-40"
            style={{
              background: "rgba(0, 0, 0, 0.4)",
              "touch-action": "none",
              "overscroll-behavior": "contain",
            }}
            onClick={() => setFilterOpen(false)}
          />
          <div
            class="fixed left-0 right-0 z-50 px-3 py-3 flex items-center gap-2 transition-transform duration-200"
            style={{
              background: "#171717",
              "border-bottom": sheetAtBottom() ? undefined : "1px solid rgba(255,255,255,0.08)",
              "border-top": sheetAtBottom() ? "1px solid rgba(255,255,255,0.08)" : undefined,
              // Pad past the notch (top sheet) or home indicator (bottom
              // sheet) — but not while the keyboard covers the indicator.
              "padding-top": sheetAtBottom() ? "0.75rem" : "calc(env(safe-area-inset-top, 0px) + 0.5rem)",
              "padding-bottom": sheetAtBottom()
                ? kbOffset() > 0
                  ? "0.75rem"
                  : "calc(env(safe-area-inset-bottom, 0px) + 0.75rem)"
                : "0.75rem",
              ...(sheetAtBottom() ? { bottom: "0" } : { top: "0" }),
              // Slide in from the nearest edge; once entered, a bottom sheet
              // also rides up above the iOS keyboard (kbOffset). The idle
              // state must be `none` (not translateY(0)): a live transform
              // makes the sheet the containing block for fixed descendants,
              // which would trap SortMenu's full-screen backdrop inside it.
              transform: !sheetEntered()
                ? sheetAtBottom() ? "translateY(100%)" : "translateY(-100%)"
                : kbOffset() > 0
                  ? `translateY(-${kbOffset()}px)`
                  : "none",
            }}
          >
            <FilterBar
              dropUp={sheetAtBottom()}
              onInputRef={(el) => { filterInputRef = el; }}
              onSubmit={() => setFilterOpen(false)}
            />
            <SortMenu dropUp={sheetAtBottom()} />
            <button
              class="shrink-0 w-10 h-10 -mr-1 flex items-center justify-center rounded text-neutral-400 hover:text-neutral-200 cursor-pointer"
              onClick={() => setFilterOpen(false)}
              title="Close"
              aria-label="Close filter"
            >
              <CloseIcon size={16} />
            </button>
          </div>
        </Show>

        {/* Settings drawer — full-screen page, opened by the gear button. */}
        <SettingsMenu
          hideTrigger
          onOpenFolder={props.onOpenFolder}
          onOpenDuplicates={props.onOpenDuplicates}
          onOpenTrash={props.onOpenTrash}
          onOpenTagManager={props.onOpenTagManager}
          onRequestShow={() => { setScrollHidden(false); }}
        />
      </Show>
    </>
  );
}
