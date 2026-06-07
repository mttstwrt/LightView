import { Show, createSignal, onCleanup, onMount } from "solid-js";
import { FilterBar } from "./FilterBar";
import { SortMenu } from "./SortMenu";
import { SettingsMenu } from "./SettingsMenu";
import { TitleBar } from "./TitleBar";
import { viewMode, setViewMode } from "../../stores/galleryStore";
import { isMobile, isTauri } from "../../lib/runtime";

interface TopBarProps {
  onOpenFolder: () => void;
  onOpenDuplicates?: () => void;
}

// Pixels the user must scroll past 0 before we'll consider hiding the bar.
const MOBILE_REVEAL_AT_TOP = 40;
// Scroll-delta threshold to trigger a direction change (anti-jitter).
const MOBILE_DIR_THRESHOLD = 6;

export function TopBar(props: TopBarProps) {
  const [hoverVisible, setHoverVisible] = createSignal(false);
  // Mobile: track-by-scroll-direction. Bar starts visible.
  const [scrollHidden, setScrollHidden] = createSignal(false);
  let filterInputRef: HTMLInputElement | undefined;

  // On desktop the bar is hover-gated; on mobile it follows scroll direction.
  const visible = () => (isMobile() ? !scrollHidden() : hoverVisible());

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
      setHoverVisible(true);
      setScrollHidden(false);
      requestAnimationFrame(() => {
        filterInputRef?.focus();
        filterInputRef?.select();
      });
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

    onCleanup(() => {
      window.removeEventListener("keydown", handleGlobalKeyDown);
      window.removeEventListener("scroll", onScroll);
    });
  });

  return (
    <>
      {/* Hover trigger zone — desktop only. On mobile the bar is always
          present (its visibility is driven by scroll direction). */}
      <Show when={!isMobile()}>
        <div
          class="fixed top-0 left-0 right-0 h-15 z-40"
          onMouseEnter={handleMouseEnter}
        />
      </Show>

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
          otherwise SettingsMenu's mobile drawer (`position: fixed; inset…`)
          would be confined to the topbar's 48px box. */}
      <div
        class="fixed left-0 right-0 z-40 h-12 flex items-center px-3 sm:px-4 gap-2 sm:gap-3 transition-[top,opacity] duration-200"
        style={{
          background: "rgba(10, 10, 10, 0.85)",
          "backdrop-filter": "blur(12px)",
          // Sits below the 2rem titlebar row when the native frame is hidden.
          top: visible() ? (frameless() ? "2rem" : "0") : "-3rem",
          opacity: visible() ? "1" : "0",
        }}
        onMouseEnter={handleMouseEnter}
        onMouseLeave={handleMouseLeave}
      >
        <FilterBar onInputRef={(el) => { filterInputRef = el; }} />
        <SortMenu />
        <button
          onClick={() => setViewMode(viewMode() === "grid" ? "map" : "grid")}
          class="shrink-0 px-2.5 py-1 text-xs rounded cursor-pointer transition-colors text-neutral-300 hover:bg-neutral-700"
          classList={{ "bg-neutral-700": viewMode() === "map" }}
          title={viewMode() === "grid" ? "Switch to map view" : "Switch to grid view"}
        >
          {viewMode() === "grid" ? "Map" : "Grid"}
        </button>
        <SettingsMenu onOpenFolder={props.onOpenFolder} onOpenDuplicates={props.onOpenDuplicates} onRequestShow={() => { setHoverVisible(true); setScrollHidden(false); }} />
      </div>
    </>
  );
}
