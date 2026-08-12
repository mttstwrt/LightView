// The one list of things the user *does*, and its two renderings.
//
// Everything actionable that used to be a `Section` in the settings drawer
// lives here as data — label, icon, an availability predicate and a handler —
// and is rendered two ways: a dropdown behind an icon in the desktop top bar,
// and a sheet behind a floating button in the phone's thumb zone. The settings
// panel is not a third rendering; it keeps only configuration, and opening it
// is the last entry in this list. See docs/frontend/chrome.md.
//
// Both triggers are *substitutions*: the desktop icon took the gear's place in
// the top-bar row, the mobile button took the upload FAB's. Neither surface
// gained a control, and both containers (dropdown, sheet) already existed.
//
// Two rules this file must keep:
//
//   1. **The trigger and the Settings entry render from local state alone.**
//      Individual entries may be gated on fetched config — Upload is — but the
//      button and Settings may not. Settings → Connection is where "Install
//      server certificate" and "Reset connection" live, so gating the only
//      route to them on a server round-trip deletes the recovery actions in
//      exactly the situation that needs them.
//   2. **Selection-scoped actions stay out.** `SelectionBar` and `ContextMenu`
//      own everything that acts on a selection. It follows that the mobile
//      button hiding while the selection bar is up is correct, not a gap.
//
// Ordering is source order — there is no `order` prop and there should not be
// one. The scheme this replaced let thirteen call sites pick a magic number
// each, four of which collided, because nobody ever saw all thirteen together.
// Here adding an entry means reading the others.

import { Show, For, batch, createSignal, createEffect, onCleanup } from "solid-js";
import { Portal } from "solid-js/web";
import {
  MoreIcon,
  TagIcon,
  DuplicateIcon,
  TrashIcon,
  AutoTagIcon,
  UploadIcon,
  FolderIcon,
  GearIcon,
  type Icon,
} from "./icons";
import { capabilities } from "../../stores/capabilitiesStore";
import { uploadsEnabled } from "../../stores/uploadStore";
import { isWeb } from "../../lib/runtime";
import { hapticTick } from "../../lib/haptics";

/** What each command does. Supplied by `App`, which owns the panels' open
 *  state — this module decides *what* is offered and in what order, never
 *  what a panel is. */
export interface CommandHandlers {
  openTagManager: () => void;
  openDuplicates: () => void;
  openTrash: () => void;
  openAutoTag: () => void;
  openUpload: () => void;
  openFolder: () => void;
  openSettings: () => void;
}

interface Command {
  id: string;
  label: string;
  icon: Icon;
  /** True when this command applies to the current runtime and gallery. */
  available: () => boolean;
  run: () => void;
  /** Settings is separated from the actions above it by a rule. */
  separated?: boolean;
}

/**
 * The list. Order is frequency-first, which on a phone is also
 * reachability-first: the sheet grows downward from the thumb.
 */
function commands(h: CommandHandlers): Command[] {
  return [
    {
      id: "upload",
      label: "Upload photos",
      icon: UploadIcon,
      // The one entry gated on fetched host config — see rule 1 above. Absent
      // rather than present-and-failing when the fetch has not landed.
      available: () => isWeb() && uploadsEnabled(),
      run: h.openUpload,
    },
    {
      id: "autotag",
      label: "Auto-tagging",
      icon: AutoTagIcon,
      // Desktop runs its own plugins; the web client enqueues for a worker and
      // is useful even with none connected (the panel says so).
      available: () => isWeb() || capabilities().plugins,
      run: h.openAutoTag,
    },
    {
      id: "tags",
      label: "Manage tags",
      icon: TagIcon,
      available: () => true,
      run: h.openTagManager,
    },
    {
      id: "duplicates",
      label: "Find duplicates",
      icon: DuplicateIcon,
      available: () => true,
      run: h.openDuplicates,
    },
    {
      id: "trash",
      label: "Trash",
      icon: TrashIcon,
      available: () => capabilities().delete,
      run: h.openTrash,
    },
    {
      id: "folder",
      label: "Open folder…",
      icon: FolderIcon,
      // The native picker exists only on the desktop; the web client mirrors
      // whichever gallery the host has open.
      available: () => !isWeb(),
      run: h.openFolder,
    },
    {
      id: "settings",
      label: "Settings",
      icon: GearIcon,
      available: () => true,
      run: h.openSettings,
      separated: true,
    },
  ];
}

// Open state is module-level rather than per-instance because only one of the
// two renderings is ever mounted (they are on opposite sides of `isMobile()`),
// and `TopBar` needs to read it to wire the mobile back gesture.
const [commandsOpen, setCommandsOpen] = createSignal(false);
export { commandsOpen, setCommandsOpen };

/** Close the list and run the command — in that order, and batched.
 *
 *  The batching is load-bearing. `TopBar` mirrors "is a mobile overlay up?"
 *  into a history entry so the platform back gesture closes it. Unbatched,
 *  closing this list and opening the settings page are two separate reactive
 *  passes, and the instant between them reads as "no overlay up", which pops
 *  the entry — the resulting popstate then lands *after* settings opened and
 *  closes it again. The symptom is that tapping Settings does nothing. */
function activate(c: Command) {
  batch(() => {
    setCommandsOpen(false);
    c.run();
  });
}

/** Desktop: an icon in the top-bar row, opening a dropdown beneath it.
 *  Expects a `position: relative` ancestor to anchor against — `TopBar`
 *  shares one with the settings panel so both drop from the same point. */
export function CommandMenu(props: { handlers: CommandHandlers }) {
  const visible = () => commands(props.handlers).filter((c) => c.available());
  closeOnEscape();

  return (
    <>
      <button
        onClick={() => setCommandsOpen((v) => !v)}
        class="shrink-0 w-8 h-8 flex items-center justify-center text-neutral-400 hover:text-neutral-200 bg-neutral-800 hover:bg-neutral-700 rounded transition-colors cursor-pointer"
        classList={{ "bg-neutral-700 text-neutral-200": commandsOpen() }}
        title="Actions"
        aria-label="Actions"
        aria-expanded={commandsOpen()}
      >
        <MoreIcon size={16} />
      </button>

      <Show when={commandsOpen()}>
        <div class="fixed inset-0 z-40" onClick={() => setCommandsOpen(false)} />
        <div
          class="absolute top-full right-0 mt-2 w-56 rounded-lg overflow-hidden shadow-xl z-50 py-1.5"
          style={{
            background: "rgba(18, 18, 18, 0.96)",
            "backdrop-filter": "blur(16px)",
            border: "1px solid rgba(255,255,255,0.08)",
          }}
          role="menu"
        >
          <For each={visible()}>
            {(c) => (
              <>
                <Show when={c.separated}>
                  <div class="my-1.5 border-t border-neutral-800/80" />
                </Show>
                <button
                  onClick={() => activate(c)}
                  role="menuitem"
                  class="w-full px-3 py-2 flex items-center gap-3 text-left text-xs text-neutral-300 hover:bg-neutral-800 hover:text-neutral-100 transition-colors cursor-pointer"
                >
                  <span class="shrink-0 text-neutral-500">
                    <c.icon size={16} />
                  </span>
                  {c.label}
                </button>
              </>
            )}
          </For>
        </div>
      </Show>
    </>
  );
}

/** Mobile: one floating button in the thumb zone, opening a bottom sheet.
 *
 *  `hidden` carries the caller's visibility policy — the scroll-direction
 *  reveal the rest of the mobile chrome follows, plus the viewer and the
 *  selection bar, both of which own the bottom edge while they are up. All of
 *  it is local state, so the button never disappears on a failed fetch. */
export function CommandFab(props: { handlers: CommandHandlers; hidden: boolean }) {
  const visible = () => commands(props.handlers).filter((c) => c.available());
  closeOnEscape();

  // Slide-in transition: mount offscreen, flip after a paint so the transform
  // animates instead of snapping. Same double-rAF the filter sheet uses.
  const [entered, setEntered] = createSignal(false);
  createEffect(() => {
    if (!commandsOpen()) {
      setEntered(false);
      return;
    }
    requestAnimationFrame(() =>
      requestAnimationFrame(() => {
        if (commandsOpen()) setEntered(true);
      }),
    );
  });

  // A hidden FAB must not leave an open sheet behind it — entering selection
  // mode or opening the viewer takes the bottom edge the sheet occupies.
  createEffect(() => {
    if (props.hidden) setCommandsOpen(false);
  });

  return (
    <>
      <button
        type="button"
        onClick={() => {
          hapticTick();
          setCommandsOpen(true);
        }}
        aria-label="Actions"
        aria-expanded={commandsOpen()}
        class="fixed z-[140] w-14 h-14 rounded-full flex items-center justify-center text-neutral-100 transition-[transform,opacity] duration-200"
        style={{
          right: "calc(env(safe-area-inset-right, 0px) + 20px)",
          bottom: "calc(env(safe-area-inset-bottom, 0px) + 20px)",
          background: "rgba(23, 23, 23, 0.94)",
          "backdrop-filter": "blur(12px)",
          border: "1px solid rgba(255, 255, 255, 0.10)",
          "box-shadow": "0 6px 20px rgba(0,0,0,0.45)",
          transform: props.hidden ? "translateY(150%)" : "translateY(0)",
          opacity: props.hidden ? "0" : "1",
          "pointer-events": props.hidden ? "none" : "auto",
        }}
      >
        <MoreIcon size={22} />
      </button>

      {/* Portalled to <body> so `position: fixed` resolves against the
          viewport rather than any transformed ancestor. */}
      <Show when={commandsOpen()}>
        <Portal>
          <div
            class="fixed inset-0 z-[190] transition-opacity duration-200"
            style={{
              background: "rgba(0,0,0,0.5)",
              "touch-action": "none",
              "overscroll-behavior": "contain",
              opacity: entered() ? "1" : "0",
            }}
            onClick={() => setCommandsOpen(false)}
          />
          <div
            class="fixed left-0 right-0 bottom-0 z-[200] rounded-t-2xl pt-2 transition-transform duration-200"
            style={{
              background: "#141414",
              "border-top": "1px solid rgba(255,255,255,0.08)",
              "padding-bottom": "calc(env(safe-area-inset-bottom, 0px) + 0.75rem)",
              transform: entered() ? "translateY(0)" : "translateY(100%)",
            }}
            role="menu"
          >
            {/* Grab handle — the standard "this sheet came from the bottom
                edge and dismisses downward" affordance. */}
            <div class="mx-auto mb-1 h-1 w-9 rounded-full bg-neutral-700" />
            <For each={visible()}>
              {(c) => (
                <>
                  <Show when={c.separated}>
                    <div class="my-1 mx-4 border-t border-neutral-800" />
                  </Show>
                  <button
                    onClick={() => activate(c)}
                    role="menuitem"
                    // 52px rows: comfortably past the 44px touch-target floor
                    // without turning a seven-item list into a scroller.
                    class="w-full h-13 px-5 flex items-center gap-4 text-left text-[15px] text-neutral-200 active:bg-neutral-800 transition-colors cursor-pointer"
                  >
                    <span class="shrink-0 text-neutral-400">
                      <c.icon size={20} />
                    </span>
                    {c.label}
                  </button>
                </>
              )}
            </For>
          </div>
        </Portal>
      </Show>
    </>
  );
}

/** Escape closes the list on a hardware keyboard — the desktop's dismissal,
 *  and the escape hatch on a tablet with one attached. Capture phase, so it
 *  runs before the viewer's own Escape handling. */
function closeOnEscape() {
  const onKey = (e: KeyboardEvent) => {
    if (e.key === "Escape" && commandsOpen()) {
      e.stopPropagation();
      setCommandsOpen(false);
    }
  };
  window.addEventListener("keydown", onKey, true);
  onCleanup(() => window.removeEventListener("keydown", onKey, true));
}
