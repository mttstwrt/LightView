// The right-click / long-press menu over grid cells and the viewer.
//
// The most trust-sensitive component in the app: it is where file operations,
// plugin runs and deletes are offered. Everything `Owner`-only is hidden rather
// than offered and refused, so a phone never collects a 403 the user caused —
// but that hiding is presentation. The enforcement is one `require(...)` line
// per arm of the command table, which is why a bug here cannot become a
// security hole.
//
// **There is one plugin flavour now.** A run is in-process, started here and
// reported through the same event stream as everything else; the worker roster
// and the enqueued-job path went with the distributed queue. Under `--serve` no
// plugins are installed and the models could not run there anyway, so the entry
// is simply absent.

import { Show, For, createEffect, createSignal, onCleanup, onMount } from "solid-js";
import { hasTouch } from "../../lib/runtime";
import { rateItem, setItemColorLabel, colorLabelByPath } from "../../stores/galleryStore";
import { api, mediaUrl } from "../../lib/ipc";
import { COLOR_LABELS, COLOR_LABEL_HEX } from "../../lib/colorLabels";
import { announceThumbRegenerated } from "../../lib/thumbRegeneration";
import { isVideoPath } from "../../lib/mediaExts";
import { plugins, loadPlugins } from "../../stores/activityStore";
import { capabilities, isOwner } from "../../stores/settingsStore";
import { openViewer } from "../../stores/viewerStore";
import { DirectoryPicker } from "./DirectoryPicker";

export interface ContextMenuState {
  x: number;
  y: number;
  path: string;
  index: number;
}

interface ContextMenuProps {
  state: ContextMenuState | null;
  onClose: () => void;
  paths: string[];
  selectedPaths?: Set<string>;
  onFilesRemoved?: (removed: string[]) => void;
  hideViewOption?: boolean;
}

type SubMenu = "tag" | "rating" | "color" | "openWith" | "plugins" | null;

/** Which transfer the picker is open for, or null when it is closed. */
type Transfer = { kind: "copy" | "move"; paths: string[] } | null;

export function ContextMenu(props: ContextMenuProps) {
  const [subMenu, setSubMenu] = createSignal<SubMenu>(null);
  const [tagInput, setTagInput] = createSignal("");
  const [externalApps, setExternalApps] = createSignal<{ label: string }[]>([]);
  const [transfer, setTransfer] = createSignal<Transfer>(null);
  let menuRef: HTMLDivElement | undefined;

  // Both lists are `Owner`-only and change about as often as the process
  // restarts, so they are fetched once rather than on every open.
  onMount(() => {
    if (!isOwner()) return;
    void loadPlugins();
    api.externalApps().then(setExternalApps).catch(() => setExternalApps([]));
  });

  // Close on click outside or Escape
  const handleKeyDown = (e: KeyboardEvent) => {
    if (e.key === "Escape") {
      if (subMenu()) {
        setSubMenu(null);
      } else {
        props.onClose();
      }
    }
  };

  // Capture-phase, and consumes the click: a click outside should only
  // dismiss the menu, never also activate what's underneath (e.g. open the
  // grid cell it landed on). Clicks inside the menu pass through to items.
  const handleClickOutside = (e: MouseEvent) => {
    if (!props.state) return;
    if (menuRef && e.target instanceof Node && menuRef.contains(e.target)) return;
    e.preventDefault();
    e.stopPropagation();
    props.onClose();
  };

  createEffect(() => {
    if (props.state) {
      setSubMenu(null);
      setTagInput("");
      window.addEventListener("keydown", handleKeyDown);
      // Delay to avoid closing from the same right-click event
      setTimeout(() => window.addEventListener("click", handleClickOutside, true), 0);
    } else {
      // Detach on close — a lingering capture listener would swallow every
      // click in the app.
      window.removeEventListener("keydown", handleKeyDown);
      window.removeEventListener("click", handleClickOutside, true);
    }
  });

  onCleanup(() => {
    window.removeEventListener("keydown", handleKeyDown);
    window.removeEventListener("click", handleClickOutside, true);
  });

  /** True when the right-clicked item is part of a multi-selection. */
  const isBatchContext = () => {
    if (!props.state || !props.selectedPaths) return false;
    return props.selectedPaths.size > 1 && props.selectedPaths.has(props.state.path);
  };

  const batchPaths = () => {
    if (!props.selectedPaths) return [];
    return Array.from(props.selectedPaths);
  };

  /** What this invocation acts on: the selection when the clicked cell is part
   *  of one, otherwise just the clicked cell. */
  const targetPaths = () =>
    isBatchContext() ? batchPaths() : props.state ? [props.state.path] : [];

  const handleAddTag = async (e: Event) => {
    e.preventDefault();
    const tag = tagInput().trim();
    if (!tag || !props.state) return;
    try {
      await api.addTags(targetPaths(), [tag], "user");
      setTagInput("");
    } catch (err) {
      console.error("Failed to add tag:", err);
    }
  };

  const handleSetRating = async (value: number) => {
    if (!props.state) return;
    try {
      if (isBatchContext()) {
        await api.setRating(batchPaths(), value > 0 ? value : null);
      } else {
        // `rateItem` keeps the item list and the info panel in step, not just
        // the database.
        await rateItem(props.state.path, value);
      }
      props.onClose();
    } catch (err) {
      console.error("Failed to set rating:", err);
    }
  };

  const currentColorLabel = () =>
    props.state ? colorLabelByPath().get(props.state.path) ?? null : null;

  const handleSetColorLabel = async (label: string | null) => {
    if (!props.state) return;
    try {
      if (isBatchContext()) {
        await api.setColorLabel(batchPaths(), label);
      } else {
        // `setItemColorLabel` keeps the item list in step, so a `color:` filter
        // re-evaluates without a refetch.
        await setItemColorLabel(props.state.path, label);
      }
      props.onClose();
    } catch (err) {
      console.error("Failed to set colour label:", err);
    }
  };

  const handleCopyPath = () => {
    if (!props.state) return;
    navigator.clipboard.writeText(props.state.path).catch(() => {});
    props.onClose();
  };

  const handleOpenViewer = () => {
    if (!props.state) return;
    openViewer(props.state.index);
    props.onClose();
  };

  const handleRunPlugin = async (pluginName: string) => {
    if (!props.state) return;
    const paths = targetPaths();
    props.onClose();
    try {
      // Fire and forget. A run over a thousand files outlives any request, so
      // the command starts it and progress arrives as `job-progress` events
      // with a terminal `job-finished` — the toast in `App` reads both.
      await api.runPlugin(pluginName, paths);
    } catch (err) {
      console.error("Could not start the plugin run:", err);
    }
  };

  const handleRegenerateThumbnail = async () => {
    if (!props.state) return;
    const path = props.state.path;
    try {
      await api.regenerate([path]);
      // The tier files changed, not the row — so this is a DOM event to the
      // cells in this client rather than a server broadcast. Every other
      // client's URL still works; it just costs one regeneration on next use.
      announceThumbRegenerated(path);
    } catch (err) {
      console.error("Failed to regenerate thumbnail:", err);
    }
    props.onClose();
  };

  /** Hand the file to an application configured on the server.
   *
   *  The argument is an **index** into that configuration, never a program
   *  name: there is no shape of request a client can send that names something
   *  to execute, which is what keeps this file access rather than code
   *  execution. The labels come back from `list_external_apps`; the commands
   *  never leave the server. */
  const handleOpenWith = async (index: number) => {
    if (!props.state) return;
    const path = props.state.path;
    props.onClose();
    try {
      await api.openWith(index, path);
    } catch (err) {
      console.error("Failed to open with external app:", err);
    }
  };

  const handleCopyImage = () => {
    if (!props.state) return;
    const path = props.state.path;
    props.onClose();
    // The clipboard only accepts image/png, and Safari requires the
    // ClipboardItem to be built synchronously in the user gesture — so hand
    // it a promise that fetches and re-encodes.
    const png = fetch(mediaUrl(path)).then(async (res) => {
      if (!res.ok) throw new Error(`media fetch failed: ${res.status}`);
      const blob = await res.blob();
      if (blob.type === "image/png") return blob;
      const bitmap = await createImageBitmap(blob);
      const canvas = document.createElement("canvas");
      canvas.width = bitmap.width;
      canvas.height = bitmap.height;
      canvas.getContext("2d")!.drawImage(bitmap, 0, 0);
      bitmap.close();
      return new Promise<Blob>((resolve, reject) =>
        canvas.toBlob((b) => (b ? resolve(b) : reject(new Error("PNG encode failed"))), "image/png"),
      );
    });
    navigator.clipboard
      .write([new ClipboardItem({ "image/png": png })])
      .catch((err) => console.error("Failed to copy image to clipboard:", err));
  };

  const handleCopyToClipboard = async () => {
    if (!props.state) return;
    const paths = targetPaths();
    props.onClose();
    try {
      await api.clipboardFiles(paths);
    } catch (err) {
      console.error("Failed to copy files to clipboard:", err);
    }
  };

  /** Open the picker for a copy or a move. The transfer runs when the picker
   *  reports a destination — the menu closes immediately, because the picker
   *  is a dialog of its own and a menu hovering behind it is noise. */
  const startTransfer = (kind: "copy" | "move") => {
    if (!props.state) return;
    const paths = targetPaths();
    props.onClose();
    setTransfer({ kind, paths });
  };

  const finishTransfer = async (destination: string) => {
    const pending = transfer();
    setTransfer(null);
    if (!pending) return;
    try {
      if (pending.kind === "copy") {
        await api.copyFiles(pending.paths, destination);
      } else {
        await api.moveFiles(pending.paths, destination);
        // A move out of the gallery removes the items; a move *within* it is
        // the watcher's business, and it reports both halves. Either way the
        // grid drops them here so the cells go at the moment of the action.
        props.onFilesRemoved?.(pending.paths);
      }
    } catch (err) {
      console.error(`${pending.kind} failed:`, err);
    }
  };

  const handleTrash = async () => {
    if (!props.state) return;
    const paths = targetPaths();
    props.onClose();
    try {
      // One delete is one trash entry, which makes undoing it a natural unit.
      await api.trash(paths);
      props.onFilesRemoved?.(paths);
    } catch (err) {
      console.error("Trash failed:", err);
    }
  };

  // Ensure menu stays within viewport
  const menuStyle = () => {
    if (!props.state) return {};
    const x = Math.min(props.state.x, window.innerWidth - 220);
    const y = Math.min(props.state.y, window.innerHeight - 300);
    return {
      position: "fixed" as const,
      left: `${x}px`,
      top: `${y}px`,
      "z-index": "200",
    };
  };

  return (
    <>
      {/* Outside the menu's own `<Show>`: the menu closes the moment a
          transfer starts, and a picker mounted inside it would go with it. */}
      <Show when={transfer()}>
        {(pending) => (
          <DirectoryPicker
            title={pending().kind === "copy" ? "Copy to" : "Move to"}
            confirmLabel={
              pending().kind === "copy"
                ? `Copy ${pending().paths.length} here`
                : `Move ${pending().paths.length} here`
            }
            onPick={(destination) => void finishTransfer(destination)}
            onCancel={() => setTransfer(null)}
          />
        )}
      </Show>

    <Show when={props.state}>
      <div
        ref={menuRef}
        style={menuStyle()}
        class="min-w-[180px] rounded shadow-lg text-xs"
        classList={{ hidden: !props.state }}
        onClick={(e) => e.stopPropagation()}
        onContextMenu={(e) => e.preventDefault()}
      >
        <div
          class="rounded overflow-hidden"
          style={{
            background: "rgba(30, 30, 30, 0.95)",
            "backdrop-filter": "blur(12px)",
            border: "1px solid rgba(255,255,255,0.1)",
          }}
        >
          {/* Main menu */}
          <Show when={subMenu() === null}>
            <Show when={isBatchContext()}>
              <div class="px-3 py-1 text-blue-400 text-xs">
                {props.selectedPaths!.size} selected
              </div>
              <Divider />
            </Show>
            {/* Each group below is gated by what this client may do — the web
                client only sees actions the server's capability report (and
                its allowlist) actually permits. */}
            <Show when={!isBatchContext() && !props.hideViewOption}>
              <MenuItem label="View" onClick={handleOpenViewer} />
            </Show>
            {/* Tags, ratings and colour labels are `Device`: the phone is the
                only UI there is under `--serve`, and a write here is the same
                companion write the local viewer makes. */}
            <>
              <MenuItem
                label={isBatchContext() ? `Tag ${props.selectedPaths!.size} Items...` : "Add Tag..."}
                onClick={() => setSubMenu("tag")}
              />
              <MenuItem
                label={isBatchContext() ? `Rate ${props.selectedPaths!.size} Items` : "Set Rating"}
                onClick={() => setSubMenu("rating")}
              />
              <MenuItem
                label={isBatchContext() ? `Label ${props.selectedPaths!.size} Items` : "Colour Label"}
                onClick={() => setSubMenu("color")}
              />
            </>
            <Divider />
            <Show when={!isBatchContext()}>
              <MenuItem label="Regenerate Thumbnail" onClick={handleRegenerateThumbnail} />
            </Show>
            <MenuItem label="Copy Path" onClick={handleCopyPath} />
            {/* The image bitmap, through the browser's own clipboard. Works
                anywhere; copying the *files* below needs a host to copy them
                on, which is the `Owner` half. */}
            <Show when={!isBatchContext() && !isVideoPath(props.state!.path)}>
              <MenuItem label="Copy Image" onClick={handleCopyImage} />
            </Show>
            {/* Everything below is `Owner`: it acts on the filesystem of the
                machine the server runs on. A runtime question rather than a
                compile-time one for the clipboard, whose X11 backend fails on
                a Wayland session without XWayland and on a process with no
                display at all — so the server reports whether it works. */}
            <Show when={isOwner() && capabilities().clipboard}>
              <MenuItem
                label={isBatchContext() ? `Copy ${props.selectedPaths!.size} to Clipboard` : "Copy to Clipboard"}
                onClick={handleCopyToClipboard}
              />
            </Show>
            <Divider />
            <Show when={isOwner()}>
              <MenuItem
                label={isBatchContext() ? `Copy ${props.selectedPaths!.size} to...` : "Copy to..."}
                onClick={() => startTransfer("copy")}
              />
              <MenuItem
                label={isBatchContext() ? `Move ${props.selectedPaths!.size} to...` : "Move to..."}
                onClick={() => startTransfer("move")}
              />
            </Show>
            {/* Move-to-trash is `Device` — restorable, and the inverse of a
                delete this client was allowed to make. Permanent deletion is
                the trash panel's, and `Owner`. */}
            <MenuItem
              label={isBatchContext() ? `Delete ${props.selectedPaths!.size} Items` : "Delete"}
              onClick={handleTrash}
              danger
            />
            <Show when={plugins().length > 0}>
              <Divider />
              <MenuItem
                label={isBatchContext() ? `Run Plugin on ${props.selectedPaths!.size}...` : "Run Plugin..."}
                onClick={() => setSubMenu("plugins")}
              />
            </Show>
            <Show when={isOwner() && externalApps().length > 0}>
              <MenuItem label="Open With..." onClick={() => setSubMenu("openWith")} />
            </Show>
          </Show>

          {/* Tag sub-menu */}
          <Show when={subMenu() === "tag"}>
            <div class="px-3 py-2 text-neutral-500">Add Tag</div>
            <form onSubmit={handleAddTag} class="px-2 pb-2 flex gap-1">
              <input
                type="text"
                value={tagInput()}
                onInput={(e) => setTagInput(e.currentTarget.value)}
                placeholder="Tag name..."
                autofocus
                class="flex-1 px-2 py-1 bg-neutral-800 border border-neutral-700 rounded text-xs text-neutral-200 placeholder-neutral-600 outline-none focus:border-neutral-500"
              />
              <button
                type="submit"
                class="px-2 py-1 bg-neutral-700 hover:bg-neutral-600 text-neutral-300 rounded text-xs cursor-pointer"
              >
                +
              </button>
            </form>
            <Divider />
            <MenuItem label="Back" onClick={() => setSubMenu(null)} />
          </Show>

          {/* Rating sub-menu */}
          <Show when={subMenu() === "rating"}>
            <div class="px-3 py-2 text-neutral-500">Set Rating</div>
            <For each={[1, 2, 3, 4, 5]}>
              {(star) => (
                <MenuItem
                  label={"★".repeat(star) + "☆".repeat(5 - star)}
                  onClick={() => handleSetRating(star)}
                />
              )}
            </For>
            <MenuItem label="Clear Rating" onClick={() => handleSetRating(0)} />
            <Divider />
            <MenuItem label="Back" onClick={() => setSubMenu(null)} />
          </Show>

          {/* Colour label sub-menu */}
          <Show when={subMenu() === "color"}>
            <div class="px-3 py-2 text-neutral-500">Colour Label</div>
            <For each={COLOR_LABELS}>
              {(name) => (
                <button
                  type="button"
                  class="w-full flex items-center gap-2 px-3 py-1.5 text-left hover:bg-neutral-800"
                  onClick={() => handleSetColorLabel(name)}
                >
                  <span
                    class="w-3 h-3 rounded-full shrink-0"
                    style={{ "background-color": COLOR_LABEL_HEX[name] }}
                  />
                  <span class="capitalize">{name}</span>
                  {/* Only meaningful for a single item; a mixed selection has
                      no one current value to tick. */}
                  <Show when={!isBatchContext() && currentColorLabel() === name}>
                    <span class="ml-auto text-neutral-400">✓</span>
                  </Show>
                </button>
              )}
            </For>
            <MenuItem label="Clear Label" onClick={() => handleSetColorLabel(null)} />
            <Divider />
            <MenuItem label="Back" onClick={() => setSubMenu(null)} />
          </Show>

          {/* Plugins sub-menu */}
          <Show when={subMenu() === "plugins"}>
            <div class="px-3 py-2 text-neutral-500">Run Plugin</div>
            <For each={plugins()}>
              {(plugin) => (
                <MenuItem
                  label={plugin.display_name}
                  onClick={() => handleRunPlugin(plugin.name)}
                />
              )}
            </For>
            <Divider />
            <MenuItem label="Back" onClick={() => setSubMenu(null)} />
          </Show>

          {/* Open With sub-menu. Labels only — the commands never leave the
              server, and what goes back is the index of the row clicked. */}
          <Show when={subMenu() === "openWith"}>
            <div class="px-3 py-2 text-neutral-500">Open With</div>
            <For each={externalApps()}>
              {(app, index) => (
                <MenuItem label={app.label} onClick={() => handleOpenWith(index())} />
              )}
            </For>
            <Divider />
            <MenuItem label="Back" onClick={() => setSubMenu(null)} />
          </Show>
        </div>
      </div>
    </Show>
    </>
  );
}

function MenuItem(props: { label: string; onClick: () => void; danger?: boolean }) {
  return (
    <button
      // Touch: taller rows + larger text so items are comfortable finger
      // targets (the desktop density stays tight for mouse pointers).
      class={`w-full text-left px-3 cursor-pointer transition-colors ${
        hasTouch() ? "py-2.5 text-sm" : "py-1.5"
      } ${
        props.danger
          ? "text-red-400 hover:bg-red-900/30"
          : "text-neutral-300 hover:bg-neutral-700/50"
      }`}
      onClick={props.onClick}
    >
      {props.label}
    </button>
  );
}

function Divider() {
  return <div class="mx-2 border-t border-neutral-700/50" />;
}
