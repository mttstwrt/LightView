import { Show, For, createSignal, createEffect, onCleanup } from "solid-js";
import { addUserTag, removeUserTag, setRating as setRatingIpc, regenerateThumbnail, addUserTagBatch, setRatingBatch, listPlugins, runPlugin, runPluginBatch, openWith } from "../../lib/ipc";
import { pluginStarted, pluginFinished, pluginFailed } from "../../stores/pluginStore";
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
}

type SubMenu = "tag" | "rating" | "openWith" | "plugins" | null;

export function ContextMenu(props: ContextMenuProps) {
  const [subMenu, setSubMenu] = createSignal<SubMenu>(null);
  const [tagInput, setTagInput] = createSignal("");
  const [plugins, setPlugins] = createSignal<PluginInfo[]>([]);
  const [pluginBusy, setPluginBusy] = createSignal(false);

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

  const handleClickOutside = () => {
    props.onClose();
  };

  createEffect(() => {
    if (props.state) {
      setSubMenu(null);
      setTagInput("");
      listPlugins().then(setPlugins).catch(() => setPlugins([]));
      window.addEventListener("keydown", handleKeyDown);
      // Delay to avoid closing from the same right-click event
      setTimeout(() => window.addEventListener("click", handleClickOutside), 0);
    }
  });

  onCleanup(() => {
    window.removeEventListener("keydown", handleKeyDown);
    window.removeEventListener("click", handleClickOutside);
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
        await setRatingIpc(props.state.path, value);
      }
      props.onClose();
    } catch (err) {
      console.error("Failed to set rating:", err);
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
    if (!props.state || pluginBusy()) return;
    const plugin = plugins().find((p) => p.name === pluginName);
    const displayName = plugin?.display_name ?? pluginName;
    const isBatch = isBatchContext();
    const paths = isBatch ? batchPaths() : [props.state.path];
    setPluginBusy(true);
    props.onClose();
    try {
      if (isBatch) {
        pluginStarted(pluginName, displayName, `Running on ${paths.length} files...`);
        const results = await runPluginBatch(pluginName, paths, "tag");
        const failed = results.filter((r) => !r.success);
        if (failed.length > 0) {
          pluginFailed(`${results.length - failed.length} tagged, ${failed.length} failed`);
        } else {
          pluginFinished(`Tagged ${results.length} files`);
        }
      } else {
        pluginStarted(pluginName, displayName, "Running...");
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
    try {
      await regenerateThumbnail(props.state.path);
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
            <Show when={!isBatchContext()}>
              <MenuItem label="View" onClick={handleOpenViewer} />
            </Show>
            <MenuItem
              label={isBatchContext() ? `Tag ${props.selectedPaths!.size} Items...` : "Add Tag..."}
              onClick={() => setSubMenu("tag")}
            />
            <MenuItem
              label={isBatchContext() ? `Rate ${props.selectedPaths!.size} Items` : "Set Rating"}
              onClick={() => setSubMenu("rating")}
            />
            <Divider />
            <Show when={!isBatchContext()}>
              <MenuItem label="Regenerate Thumbnail" onClick={handleRegenerateThumbnail} />
            </Show>
            <MenuItem label="Copy Path" onClick={handleCopyPath} />
            <MenuItem
              label={isBatchContext() ? `Run Plugin on ${props.selectedPaths!.size}...` : "Run Plugin..."}
              onClick={() => setSubMenu("plugins")}
            />
            <Show when={settings().external_apps.length > 0}>
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

          {/* Plugins sub-menu */}
          <Show when={subMenu() === "plugins"}>
            <div class="px-3 py-2 text-neutral-500">Run Plugin</div>
            <Show when={plugins().length > 0} fallback={
              <div class="px-3 py-1.5 text-neutral-600 text-xs">No plugins installed</div>
            }>
              <For each={plugins()}>
                {(plugin) => (
                  <MenuItem
                    label={pluginBusy() ? `${plugin.display_name} (running...)` : plugin.display_name}
                    onClick={() => handleRunPlugin(plugin.name)}
                  />
                )}
              </For>
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

function MenuItem(props: { label: string; onClick: () => void }) {
  return (
    <button
      class="w-full text-left px-3 py-1.5 text-neutral-300 hover:bg-neutral-700/50 cursor-pointer transition-colors"
      onClick={props.onClick}
    >
      {props.label}
    </button>
  );
}

function Divider() {
  return <div class="mx-2 border-t border-neutral-700/50" />;
}
