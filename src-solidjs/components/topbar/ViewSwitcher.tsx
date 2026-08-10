// The phone's view-mode selector.
//
// Grid / Justified / Map is neither a command nor configuration — it is a mode
// selector used constantly, so it belongs neither in the command list nor in
// the settings panel, which is why it had no home when those two were split
// (docs/frontend/chrome.md). It gets the top-right corner, which the gear
// vacated when it folded into the command list, so the phone still carries
// three floating controls rather than four.
//
// The desktop keeps its segmented control in the top-bar row: there is room
// for three labels there and none here, which is why this is a button showing
// the current mode plus a small anchored menu rather than the same widget.

import { Show, For, createSignal, createEffect } from "solid-js";
import { Dynamic } from "solid-js/web";
import { GridViewIcon, JustifiedViewIcon, MapViewIcon, type Icon } from "./icons";
import {
  viewMode,
  setViewMode,
  enabledViews,
  VIEW_CHOICES,
  type ViewMode,
} from "../../stores/galleryStore";

const VIEW_ICONS: Record<ViewMode, Icon> = {
  grid: GridViewIcon,
  justified: JustifiedViewIcon,
  map: MapViewIcon,
};

export function ViewSwitcher(props: { visible: boolean }) {
  const [open, setOpen] = createSignal(false);

  // Only the views this gallery has enabled — a disabled view generates no
  // thumbnails, so offering it would open onto a grid decoding the library on
  // the spot.
  const choices = () => VIEW_CHOICES.filter((v) => enabledViews().includes(v.mode));

  // Nothing to switch between: the control is not merely disabled, it is gone.
  // One less thing floating over the grid on a single-view gallery.
  const show = () => choices().length > 1;

  // The chrome slides away on scroll-down; an open menu has to go with it, or
  // it is left anchored to a button that is no longer on screen.
  createEffect(() => {
    if (!props.visible) setOpen(false);
  });

  return (
    <Show when={show()}>
      {/* Laid out by the caller's floating row, not positioned here — that is
          what keeps the row from leaving a hole when this hides itself. */}
      <button
        class="w-11 h-11 flex items-center justify-center rounded-full text-neutral-100 shadow-lg"
        style={{
          background: "rgba(23, 23, 23, 0.92)",
          border: "1px solid rgba(255, 255, 255, 0.10)",
        }}
        onClick={() => setOpen((v) => !v)}
        title="Change view"
        aria-label="Change view"
        aria-expanded={open()}
      >
        {/* The glyph tracks the mode, so the button says which view is current
            without spending width on a label. */}
        <Dynamic component={VIEW_ICONS[viewMode()]} size={20} />
      </button>

      <Show when={open()}>
        <div class="fixed inset-0 z-40" onClick={() => setOpen(false)} />
        <div
          class="fixed z-50 w-44 rounded-xl overflow-hidden shadow-xl py-1"
          style={{
            right: "calc(env(safe-area-inset-right, 0px) + 12px)",
            // Directly under the button: inset + its 44px height + an 8px gap.
            top: "calc(env(safe-area-inset-top, 0px) + 60px)",
            background: "rgba(20, 20, 20, 0.97)",
            "backdrop-filter": "blur(16px)",
            border: "1px solid rgba(255,255,255,0.08)",
          }}
          role="menu"
        >
          <For each={choices()}>
            {(v) => {
              const active = () => viewMode() === v.mode;
              const ChoiceIcon = VIEW_ICONS[v.mode];
              return (
                <button
                  role="menuitemradio"
                  aria-checked={active()}
                  onClick={() => {
                    setViewMode(v.mode);
                    setOpen(false);
                  }}
                  class="w-full h-11 px-4 flex items-center gap-3 text-left text-sm transition-colors cursor-pointer"
                  classList={{
                    "text-teal-300": active(),
                    "text-neutral-300 active:bg-neutral-800": !active(),
                  }}
                >
                  <span class="shrink-0" classList={{ "text-teal-400": active(), "text-neutral-500": !active() }}>
                    <ChoiceIcon size={18} />
                  </span>
                  {v.label}
                </button>
              );
            }}
          </For>
        </div>
      </Show>
    </Show>
  );
}
