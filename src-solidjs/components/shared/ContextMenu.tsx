// The right-click / long-press menu over grid cells and the viewer.
//
// The most capability-sensitive component in the app: it is where file
// operations, plugin runs, and deletes are offered. Every one of those is
// gated on `capabilities()` so the web client never shows an action the server
// will refuse — but that gating is presentation. The enforcement is the
// `/api/invoke` allowlist, which is why a component bug here cannot become a
// security hole.
//
// Plugin actions come in two flavours that look alike and are not: a local run
// (desktop, spawns a subprocess here) and an enqueued tagging job (web, claimed
// by a paired worker). `taggingStore` decides which are available.

import { Show, For, createSignal, createEffect, onCleanup } from "solid-js";
import { open } from "@tauri-apps/plugin-dialog";
import { safeListen as listen, isWeb, hasTouch } from "../../lib/runtime";
import { rateItem, setItemColorLabel, colorLabelByPath } from "../../stores/galleryStore";
import { addUserTag, removeUserTag, regenerateThumbnail, addUserTagBatch, setRatingBatch, setColorLabelBatch, COLOR_LABELS, COLOR_LABEL_HEX, listPlugins, runPlugin, runPluginBatch, cancelPluginBatch, enqueueTaggingJob, openWith, copyFiles, moveFiles, trashFiles, copyFilesToClipboard, mediaUrl, THUMB_REGENERATED_EVENT } from "../../lib/ipc";
import type { MovedFile } from "../../lib/ipc";
import { isVideoPath } from "../../lib/mediaExts";
import { pluginStarted, pluginFinished, pluginFailed, pluginProgress, pluginCancelled } from "../../stores/pluginStore";
import { workerPlugins, taggingActions, refreshTaggingStatus, trackQueuedJob } from "../../stores/taggingStore";
import { capabilities } from "../../stores/capabilitiesStore";
import { settings } from "../../stores/settingsStore";
import { openViewer } from "../../stores/viewerStore";
import type { PluginInfo } from "../../lib/types";

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
  onFilesMoved?: (moved: MovedFile[]) => void;
  hideViewOption?: boolean;
}

type SubMenu = "tag" | "rating" | "color" | "openWith" | "plugins" | null;

export function ContextMenu(props: ContextMenuProps) {
  const [subMenu, setSubMenu] = createSignal<SubMenu>(null);
  const [tagInput, setTagInput] = createSignal("");
  const [plugins, setPlugins] = createSignal<PluginInfo[]>([]);
  const [pluginBusy, setPluginBusy] = createSignal(false);
  let menuRef: HTMLDivElement | undefined;

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
      if (capabilities().plugins) {
        listPlugins().then(setPlugins).catch(() => setPlugins([]));
      } else if (isWeb()) {
        // No host plugins on web — but a connected lightview-worker may offer
        // some. Refresh so the submenu (and its gate) reflect live workers.
        refreshTaggingStatus();
      }
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

  /** Plugins runnable from this menu: the host's own (desktop) or those
   * offered by connected tagging workers (web). */
  const menuPlugins = () => (capabilities().plugins ? plugins() : workerPlugins());

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

  const handleAddTag = async (e: Event) => {
    e.preventDefault();
    const tag = tagInput().trim();
    if (!tag || !props.state) return;
    try {
      if (isBatchContext()) {
        await addUserTagBatch(batchPaths(), tag);
      } else {
        await addUserTag(props.state.path, tag);
      }
      setTagInput("");
    } catch (err) {
      console.error("Failed to add tag:", err);
    }
  };

  const handleSetRating = async (value: number) => {
    if (!props.state) return;
    try {
      if (isBatchContext()) {
        await setRatingBatch(batchPaths(), value);
      } else {
        // rateItem keeps sortedItems + the info panel in sync, not just the DB.
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
        await setColorLabelBatch(batchPaths(), label);
      } else {
        // setItemColorLabel keeps sortedItems in sync, not just the DB — so a
        // `color:` filter re-evaluates without a refetch.
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

  const handleRunPlugin = async (pluginName: string, workerId?: string) => {
    if (!props.state || pluginBusy()) return;
    const plugin = menuPlugins().find((p) => p.name === pluginName);
    const displayName = plugin?.display_name ?? pluginName;
    const isBatch = isBatchContext();
    const paths = isBatch ? batchPaths() : [props.state.path];

    // Web: the host can't run plugins — enqueue a job for a connected worker.
    // Progress arrives via the tagging SSE events → taggingStore → toast.
    if (!capabilities().plugins) {
      props.onClose();
      try {
        const job = await enqueueTaggingJob(pluginName, { paths }, workerId);
        trackQueuedJob(job);
      } catch (err) {
        console.error("Failed to enqueue tagging job:", err);
        pluginStarted(pluginName, displayName, paths.length);
        pluginFailed(String(err));
      }
      return;
    }

    setPluginBusy(true);
    props.onClose();
    try {
      if (isBatch) {
        pluginStarted(pluginName, displayName, paths.length);

        const unlistenProgress = await listen<{ completed: number; total: number; failed: number }>(
          "plugin:progress",
          (event) => pluginProgress(event.payload.completed, event.payload.total),
        );
        const unlistenDone = await listen<{ succeeded: number; failed: number; cancelled: boolean }>(
          "plugin:done",
          (event) => {
            unlistenProgress();
            unlistenDone();
            const { succeeded, failed, cancelled } = event.payload;
            if (cancelled) {
              pluginCancelled();
            } else if (failed > 0) {
              pluginFailed(`${succeeded} tagged, ${failed} failed`);
            } else {
              pluginFinished(`Tagged ${succeeded} files`);
            }
            setPluginBusy(false);
          },
        );

        runPluginBatch(pluginName, paths, "tag").catch((err) => {
          console.error("Plugin batch failed to start:", err);
          unlistenProgress();
          unlistenDone();
          pluginFailed("Failed to start batch");
          setPluginBusy(false);
        });
        return; // pluginBusy cleared by event listener
      } else {
        pluginStarted(pluginName, displayName, 1);
        const result = await runPlugin(pluginName, paths[0], "tag");
        if (result.success) {
          pluginFinished("Done");
        } else {
          pluginFailed(result.error ?? "Failed");
        }
      }
    } catch (err) {
      console.error("Plugin execution failed:", err);
      pluginFailed("Execution failed");
    } finally {
      setPluginBusy(false);
    }
  };

  const handleRegenerateThumbnail = async () => {
    if (!props.state) return;
    const path = props.state.path;
    try {
      await regenerateThumbnail(path);
      // Desktop grids cache-bust via the `thumb:regenerated` Tauri event; the
      // web client has no event channel, so stand in with a DOM event.
      if (isWeb()) {
        window.dispatchEvent(new CustomEvent(THUMB_REGENERATED_EVENT, { detail: { path } }));
      }
    } catch (err) {
      console.error("Failed to regenerate thumbnail:", err);
    }
    props.onClose();
  };

  const handleOpenWith = async (command: string, args: string[]) => {
    if (!props.state) return;
    const resolvedArgs = args.map((a) => a.replace("{file}", props.state!.path));
    try {
      await openWith(command, resolvedArgs);
    } catch (err) {
      console.error("Failed to open with external app:", err);
    }
    props.onClose();
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
    const paths = isBatchContext() ? batchPaths() : [props.state.path];
    props.onClose();
    try {
      await copyFilesToClipboard(paths);
    } catch (err) {
      console.error("Failed to copy files to clipboard:", err);
    }
  };

  const handleCopyTo = async () => {
    if (!props.state) return;
    const dest = await open({ directory: true, multiple: false });
    if (!dest) return;
    const paths = isBatchContext() ? batchPaths() : [props.state.path];
    props.onClose();
    try {
      const result = await copyFiles(paths, dest as string);
      if (result.failed.length > 0) {
        console.error("Copy failures:", result.failed);
      }
    } catch (err) {
      console.error("Copy failed:", err);
    }
  };

  const handleMoveTo = async () => {
    if (!props.state) return;
    const dest = await open({ directory: true, multiple: false });
    if (!dest) return;
    const paths = isBatchContext() ? batchPaths() : [props.state.path];
    props.onClose();
    try {
      const result = await moveFiles(paths, dest as string);
      if (result.moved.length > 0) {
        props.onFilesMoved?.(result.moved);
      }
      if (result.removed.length > 0) {
        props.onFilesRemoved?.(result.removed);
      }
      if (result.failed.length > 0) {
        console.error("Move failures:", result.failed);
      }
    } catch (err) {
      console.error("Move failed:", err);
    }
  };

  const handleTrash = async () => {
    if (!props.state) return;
    const paths = isBatchContext() ? batchPaths() : [props.state.path];
    props.onClose();
    try {
      const result = await trashFiles(paths);
      if (result.succeeded.length > 0) {
        props.onFilesRemoved?.(result.succeeded);
      }
      if (result.failed.length > 0) {
        console.error("Trash failures:", result.failed);
      }
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
            <Show when={capabilities().metadataWrite}>
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
            </Show>
            <Divider />
            <Show when={capabilities().metadataWrite && !isBatchContext()}>
              <MenuItem label="Regenerate Thumbnail" onClick={handleRegenerateThumbnail} />
            </Show>
            <MenuItem label="Copy Path" onClick={handleCopyPath} />
            {/* Web client: copy the image bitmap via the browser clipboard.
                Desktop copies actual files below instead. */}
            <Show when={isWeb() && !isBatchContext() && !isVideoPath(props.state!.path)}>
              <MenuItem label="Copy Image" onClick={handleCopyImage} />
            </Show>
            <Show when={capabilities().localFs}>
              <MenuItem
                label={isBatchContext() ? `Copy ${props.selectedPaths!.size} to Clipboard` : "Copy to Clipboard"}
                onClick={handleCopyToClipboard}
              />
            </Show>
            <Show when={capabilities().localFs || capabilities().delete}>
              <Divider />
            </Show>
            <Show when={capabilities().localFs}>
              <MenuItem
                label={isBatchContext() ? `Copy ${props.selectedPaths!.size} to...` : "Copy to..."}
                onClick={handleCopyTo}
              />
              <MenuItem
                label={isBatchContext() ? `Move ${props.selectedPaths!.size} to...` : "Move to..."}
                onClick={handleMoveTo}
              />
            </Show>
            <Show when={capabilities().delete}>
              <MenuItem
                label={isBatchContext() ? `Delete ${props.selectedPaths!.size} Items` : "Delete"}
                onClick={handleTrash}
                danger
              />
            </Show>
            <Show when={capabilities().plugins || menuPlugins().length > 0}>
              <Divider />
              <MenuItem
                label={isBatchContext() ? `Run Plugin on ${props.selectedPaths!.size}...` : "Run Plugin..."}
                onClick={() => setSubMenu("plugins")}
              />
            </Show>
            <Show when={capabilities().localFs && settings().external_apps.length > 0}>
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
            <div class="px-3 py-2 text-neutral-500">
              {capabilities().plugins ? "Run Plugin" : "Tag via Worker"}
            </div>
            <Show when={menuPlugins().length > 0} fallback={
              <div class="px-3 py-1.5 text-neutral-600 text-xs">No plugins installed</div>
            }>
              <Show
                when={!capabilities().plugins}
                fallback={
                  <For each={menuPlugins()}>
                    {(plugin) => (
                      <MenuItem
                        label={pluginBusy() ? `${plugin.display_name} (running...)` : plugin.display_name}
                        onClick={() => handleRunPlugin(plugin.name)}
                      />
                    )}
                  </For>
                }
              >
                {/* Web: one entry per (plugin, worker) — a plugin offered by
                    several workers (e.g. server + remote) gets pinned entries
                    so the user picks where it runs. */}
                <For each={taggingActions()}>
                  {(action) => (
                    <MenuItem
                      label={action.where ? `${action.plugin.display_name} ${action.where}` : action.plugin.display_name}
                      onClick={() => handleRunPlugin(action.plugin.name, action.workerId)}
                    />
                  )}
                </For>
              </Show>
            </Show>
            <Divider />
            <MenuItem label="Back" onClick={() => setSubMenu(null)} />
          </Show>

          {/* Open With sub-menu */}
          <Show when={subMenu() === "openWith"}>
            <div class="px-3 py-2 text-neutral-500">Open With</div>
            <For each={settings().external_apps}>
              {(app) => (
                <MenuItem
                  label={app.label}
                  onClick={() => handleOpenWith(app.command, app.args)}
                />
              )}
            </For>
            <Divider />
            <MenuItem label="Back" onClick={() => setSubMenu(null)} />
          </Show>
        </div>
      </div>
    </Show>
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
