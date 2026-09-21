// Auto-tagging: run a tagger plugin over what the current filter shows.
//
// **One roster, one run.** This used to be two panels in a trench coat: the
// desktop spawned its own installed plugins, and the web client enqueued a job
// for a paired worker, watching a queue with a worker registry, liveness TTLs,
// job pinning and two staleness clocks. All of that existed to move bytes
// between two machines over HTTP — and the machine with the GPU can mount the
// gallery, so it does not need a protocol, it needs a path. The distributed
// half is now `lightview tag <dir> --plugin <name>`, run from a shell on the
// machine that can run the models.
//
// What is left is the in-process executor: a list of installed plugins and a
// Run button, with progress arriving on the same event stream as everything
// else, so this panel can be closed while a run continues.
//
// **Under `--serve` this renders nothing.** Plugins are installed beside the
// viewer, not on the server, and the server could not run the models anyway —
// so the list is empty and the command that opens this panel is absent. An
// honest absence beats a disabled control explaining itself in a tooltip.

import { onCleanup, onMount, Show, For } from "solid-js";

import { CloseIcon } from "./topbar/icons";
import { displayPaths } from "../stores/galleryStore";
import { loadPlugins, plugins, run } from "../stores/activityStore";
import { api } from "../lib/ipc";

export function AutoTagPanel(props: { onClose: () => void }) {
  const handleKey = (e: KeyboardEvent) => {
    if (e.key === "Escape") {
      e.stopPropagation();
      props.onClose();
    }
  };
  window.addEventListener("keydown", handleKey, true);
  onCleanup(() => window.removeEventListener("keydown", handleKey, true));

  // The roster changes when a plugin is added to the data directory, which the
  // server notices on its next sweep — so re-read it when the panel opens
  // rather than trusting whatever boot found.
  onMount(() => void loadPlugins());

  /** Run over everything the current filter shows.
   *
   *  The filter, not the whole gallery: "tag these" is the question the grid
   *  is already answering, and a run scoped to it is how re-running after an
   *  upload tags only the new files. */
  const runOnFiltered = async (plugin: string) => {
    const paths = displayPaths();
    if (paths.length === 0) return;
    try {
      // Returns as soon as the run is launched. Progress is throttled to one
      // message a second server-side and the terminal message is never
      // dropped, so the toast finishes even if this panel is long gone.
      await api.runPlugin(plugin, paths);
    } catch (e) {
      console.error("Could not start the plugin run:", e);
    }
  };

  const count = () => displayPaths().length;

  return (
    <div class="fixed inset-0 z-[200] flex flex-col safe-panel" style={{ background: "rgba(10, 10, 10, 0.98)" }}>
      <div class="flex items-center justify-between px-5 sm:px-6 py-4 border-b border-neutral-800/60 shrink-0">
        <span class="text-sm font-medium text-neutral-200">Auto-tagging</span>
        <button
          onClick={props.onClose}
          class="w-9 h-9 -mr-2 flex items-center justify-center text-neutral-400 hover:text-neutral-200 rounded transition-colors cursor-pointer"
          title="Close"
          aria-label="Close auto-tagging"
        >
          <CloseIcon size={16} />
        </button>
      </div>

      <div class="flex-1 overflow-y-auto overscroll-contain px-5 sm:px-6 py-5">
        <div class="max-w-2xl mx-auto flex flex-col gap-5">
          <p class="text-xs text-neutral-500 leading-relaxed">
            Tagger plugins run on this machine and write their tags into the
            companion files beside your photos. A run covers everything the
            current filter shows —{" "}
            <span class="text-neutral-400 tabular-nums">{count().toLocaleString()}</span>{" "}
            {count() === 1 ? "photo" : "photos"} right now.
          </p>

          <Show
            when={plugins().length > 0}
            fallback={
              <div class="px-4 py-6 rounded-lg border border-dashed border-neutral-800 text-xs text-neutral-500 leading-relaxed text-center">
                No plugins installed.
                <br />
                <span class="text-neutral-600">
                  A plugin is a folder under{" "}
                  <code class="text-neutral-500">
                    $XDG_DATA_HOME/lightview/plugins/
                  </code>
                  . Add one there and reopen this panel.
                </span>
              </div>
            }
          >
            <div class="flex flex-col gap-1.5">
              <For each={plugins()}>
                {(plugin) => {
                  const busy = () => run()?.plugin === plugin.name;
                  return (
                    <div class="flex items-center justify-between gap-3 px-3 py-2.5 rounded-lg bg-neutral-900/60 border border-white/[0.04]">
                      <div class="flex flex-col min-w-0">
                        <span class="text-xs text-neutral-200 truncate">
                          {plugin.display_name}
                        </span>
                        <span class="text-[10px] text-neutral-500 truncate">
                          {plugin.description}
                        </span>
                      </div>
                      <button
                        onClick={() => void runOnFiltered(plugin.name)}
                        disabled={run() !== null || count() === 0}
                        class="shrink-0 px-2.5 py-1.5 text-[11px] rounded-md cursor-pointer transition-colors bg-neutral-800 text-neutral-300 hover:bg-neutral-700 hover:text-neutral-100 disabled:opacity-40 disabled:cursor-not-allowed"
                      >
                        {busy() ? `${run()!.done} / ${run()!.total}` : "Run"}
                      </button>
                    </div>
                  );
                }}
              </For>
            </div>
          </Show>

          <p class="text-[11px] text-neutral-600 leading-relaxed">
            Tagging a gallery on another machine — a NAS, say, from a desktop
            that has the GPU — is{" "}
            <code class="text-neutral-500">lightview tag &lt;dir&gt; --plugin &lt;name&gt;</code>{" "}
            over the mount. It writes the same companion files, and the server
            picks them up through its own watcher.
          </p>
        </div>
      </div>
    </div>
  );
}
