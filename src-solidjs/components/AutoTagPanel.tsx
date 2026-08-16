// Auto-tagging: run a tagger plugin over the gallery.
//
// One panel, two very different bodies, because the two runtimes run plugins
// in genuinely different places. The desktop spawns its own installed plugins
// as subprocesses and streams progress back over Tauri events. The web client
// cannot run anything — it enqueues a job for a paired `lightview-worker` (or
// the server itself, which registers as a local worker) and watches the job
// queue. What the user is doing is the same in both cases, so it is one
// command and one panel rather than two of each.
//
// This lived in `SettingsMenu` as two sections until the commands/settings
// split (docs/frontend/chrome.md). Neither collapses to a `{label, handler}`
// command — both are live rosters with per-row actions — so they took the same
// route out that tags, duplicates and trash already had: a command that opens
// a dedicated panel.

import { createSignal, onMount, onCleanup, Show, For } from "solid-js";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { CloseIcon } from "./topbar/icons";
import { displayPaths } from "../stores/galleryStore";
import { capabilities } from "../stores/capabilitiesStore";
import { isWeb } from "../lib/runtime";
import { safeListen as listen } from "../lib/runtime";
import {
  listPlugins,
  installPlugin,
  runPluginBatch,
  enqueueTaggingJob,
  cancelTaggingJob,
  type TaggingJob,
} from "../lib/ipc";
import type { PluginInfo } from "../lib/types";
import { pluginStarted, pluginProgress, pluginFinished, pluginFailed, pluginCancelled } from "../stores/pluginStore";
import { taggingWorkers, taggingJobs, taggingActions, refreshTaggingStatus, trackQueuedJob } from "../stores/taggingStore";

export function AutoTagPanel(props: { onClose: () => void }) {
  const handleKey = (e: KeyboardEvent) => {
    if (e.key === "Escape") {
      e.stopPropagation();
      props.onClose();
    }
  };
  window.addEventListener("keydown", handleKey, true);
  onCleanup(() => window.removeEventListener("keydown", handleKey, true));

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
          <Show when={isWeb()} fallback={<LocalPlugins />}>
            <RemoteTagging />
          </Show>
        </div>
      </div>
    </div>
  );
}

// ── Desktop: plugins installed beside this app, run in-process ──────────────

function LocalPlugins() {
  const [plugins, setPlugins] = createSignal<PluginInfo[]>([]);
  const [running, setRunning] = createSignal<string | null>(null);
  const [status, setStatus] = createSignal("");

  const refresh = async () => {
    // Same capability gate as ContextMenu: the web /api/invoke allowlist
    // rejects list_plugins, so calling it there is a guaranteed 403.
    if (!capabilities().plugins) return;
    try {
      setPlugins(await listPlugins());
    } catch (e) {
      console.warn("Failed to list plugins:", e);
    }
  };
  onMount(refresh);

  const install = async (directory: boolean) => {
    const selected = await openDialog({
      title: directory ? "Select plugin directory" : "Select plugin Python file",
      directory,
      multiple: false,
      filters: directory ? undefined : [{ name: "Python Plugin", extensions: ["py"] }],
    });
    if (!selected) return;
    try {
      await installPlugin(selected as string);
      await refresh();
      setStatus("Plugin installed");
    } catch (e) {
      console.error("Install failed:", e);
      setStatus("Install failed");
    }
    setTimeout(() => setStatus(""), 3000);
  };

  /** Run a plugin over everything the current filter shows. The backend runs
   *  the batch in a background task and reports through `plugin:*` events, so
   *  this returns as soon as the batch is launched — the toast carries the
   *  progress and this panel can be closed. */
  const runOnAll = async (pluginName: string) => {
    const paths = displayPaths();
    if (paths.length === 0) return;
    const plugin = plugins().find((p) => p.name === pluginName);
    const displayName = plugin?.display_name ?? pluginName;
    setRunning(pluginName);
    setStatus(`Running on ${paths.length} photos…`);
    pluginStarted(pluginName, displayName, paths.length);

    const unlistenProgress = await listen<{ completed: number; total: number }>(
      "plugin:progress",
      (event) => pluginProgress(event.payload.completed, event.payload.total),
    );

    const unlistenDone = await listen<{ succeeded: number; failed: number; cancelled: boolean }>(
      "plugin:done",
      (event) => {
        const { succeeded, failed, cancelled } = event.payload;
        if (cancelled) {
          pluginCancelled();
        } else if (failed > 0) {
          const msg = `Done: ${succeeded} tagged, ${failed} failed`;
          setStatus(msg);
          pluginFailed(msg);
        } else {
          const msg = `Tagged ${succeeded} photos`;
          setStatus(msg);
          pluginFinished(msg);
        }
        cleanup();
      },
    );

    const cleanup = () => {
      unlistenProgress();
      unlistenDone();
      setRunning(null);
      setTimeout(() => setStatus(""), 5000);
    };

    try {
      await runPluginBatch(pluginName, paths, "tag");
    } catch (e) {
      console.error("Plugin batch launch failed:", e);
      setStatus("Run failed");
      pluginFailed("Run failed");
      cleanup();
    }
  };

  return (
    <>
      <p class="text-xs text-neutral-500 leading-relaxed">
        Tagger plugins run on this machine and write their tags into the
        companion files. A run covers everything the current filter shows —{" "}
        <span class="text-neutral-400 tabular-nums">{displayPaths().length.toLocaleString()}</span>{" "}
        {displayPaths().length === 1 ? "photo" : "photos"} right now.
      </p>

      <Show
        when={plugins().length > 0}
        fallback={<EmptyState>No plugins installed.</EmptyState>}
      >
        <div class="flex flex-col gap-1.5">
          <For each={plugins()}>
            {(plugin) => (
              <Row
                title={plugin.display_name}
                subtitle={plugin.description}
                action={
                  <ActionButton
                    disabled={running() !== null || displayPaths().length === 0}
                    onClick={() => runOnAll(plugin.name)}
                  >
                    {running() === plugin.name ? "Running…" : "Run"}
                  </ActionButton>
                }
              />
            )}
          </For>
        </div>
      </Show>

      <Show when={status()}>
        <span class="text-xs text-neutral-400">{status()}</span>
      </Show>

      <div class="flex gap-2 pt-1">
        <SecondaryButton onClick={() => install(false)}>Add .py file…</SecondaryButton>
        <SecondaryButton onClick={() => install(true)}>Add folder…</SecondaryButton>
      </div>
    </>
  );
}

// ── Web: jobs claimed by a paired worker (or the server itself) ─────────────

function RemoteTagging() {
  // The SSE stream keeps this current, but a client that missed an event while
  // backgrounded would show a stale roster — re-sync whenever the panel opens.
  onMount(refreshTaggingStatus);

  /** Enqueue "tag everything this plugin hasn't seen yet", pinned to a worker
   *  when the user picked one. The filter resolves at claim time, so re-running
   *  after new uploads only tags the new files. */
  const tagAllUntagged = async (pluginName: string, tagPrefix: string, workerId?: string) => {
    try {
      trackQueuedJob(
        await enqueueTaggingJob(
          pluginName,
          // No `type:image`: the host samples frames from videos now, so
          // restricting this to stills would keep excluding them after the
          // backend stopped doing so.
          { filter: `NOT has::plugin.${tagPrefix}` },
          workerId,
        ),
      );
    } catch (e) {
      console.error("Failed to enqueue tagging job:", e);
      // Surface it through the toast — this panel may already be closed.
      pluginStarted(pluginName, pluginName, 0);
      pluginFailed(String(e));
    }
  };

  /** Newest-first slice of the server's job list for the status readout. */
  const recentJobs = () => taggingJobs().slice(-5).reverse();

  const jobStateColor = (job: TaggingJob) => {
    switch (job.state) {
      case "running": return "text-teal-400";
      case "queued": return "text-neutral-400";
      case "done": return "text-green-400";
      case "failed": return "text-red-400";
      case "cancelled": return "text-yellow-400";
    }
  };

  return (
    <>
      <Show
        when={taggingWorkers().length > 0}
        fallback={
          <EmptyState>
            No tagging worker connected. Run{" "}
            <code class="text-neutral-400">lightview-worker</code> on a machine
            with your plugins installed, or install plugins on the server itself.
          </EmptyState>
        }
      >
        <div class="flex flex-col gap-1.5">
          <For each={taggingWorkers()}>
            {(worker) => (
              <div class="flex items-center gap-2 text-[11px] text-neutral-500">
                <span class={worker.busyJobId ? "text-teal-400" : "text-neutral-600"}>&#9679;</span>
                <span class="text-neutral-400">{worker.workerName}</span>
                <Show when={worker.local}>
                  <span class="px-1 py-px rounded bg-neutral-800 text-[9px] text-neutral-400">server</span>
                </Show>
                {/* Which build that machine is on — the question a stale
                    worker binary or a stale plugin copy makes you ask. */}
                <Show when={worker.workerVersion}>
                  <span class="text-neutral-600">v{worker.workerVersion}</span>
                </Show>
                <span>{worker.busyJobId ? "tagging…" : "idle"}</span>
              </div>
            )}
          </For>
        </div>

        {/* One row per (plugin, place-to-run-it): plugins offered by several
            workers get a pinned row each ("on <worker>"). */}
        <div class="flex flex-col gap-1.5">
          <For each={taggingActions()}>
            {(action) => (
              <Row
                title={
                  <>
                    {action.plugin.display_name}
                    <Show when={action.where}>
                      <span class="text-neutral-500"> {action.where}</span>
                    </Show>
                  </>
                }
                subtitle={action.plugin.description}
                action={
                  <ActionButton
                    onClick={() =>
                      tagAllUntagged(action.plugin.name, action.plugin.tag_prefix, action.workerId)
                    }
                  >
                    Tag untagged
                  </ActionButton>
                }
              />
            )}
          </For>
        </div>
      </Show>

      <Show when={recentJobs().length > 0}>
        <div class="flex flex-col gap-1.5">
          <span class="text-[11px] uppercase tracking-wider text-neutral-500 font-medium">
            Recent jobs
          </span>
          <For each={recentJobs()}>
            {(job) => (
              <div class="flex items-center gap-2 px-3 py-2 rounded-lg bg-neutral-900/60 border border-white/[0.04]">
                <span class={`text-[10px] ${jobStateColor(job)}`}>{job.state}</span>
                <span class="text-[11px] text-neutral-400 truncate flex-1">
                  {job.displayName}
                  {job.total > 0 ? ` — ${job.completed + job.failed}/${job.total}` : ""}
                  {job.failed > 0 ? ` (${job.failed} failed)` : ""}
                </span>
                <Show when={job.state === "queued" || job.state === "running"}>
                  <ActionButton onClick={() => cancelTaggingJob(job.id).catch(() => {})}>
                    Cancel
                  </ActionButton>
                </Show>
              </div>
            )}
          </For>
        </div>
      </Show>
    </>
  );
}

// ── Shared bits ────────────────────────────────────────────────────────────

function Row(props: { title: any; subtitle?: string; action: any }) {
  return (
    <div class="flex items-center justify-between gap-3 px-3 py-2.5 rounded-lg bg-neutral-900/60 border border-white/[0.04]">
      <div class="flex flex-col min-w-0">
        <span class="text-xs text-neutral-200 truncate">{props.title}</span>
        <Show when={props.subtitle}>
          <span class="text-[10px] text-neutral-500 truncate">{props.subtitle}</span>
        </Show>
      </div>
      {props.action}
    </div>
  );
}

function ActionButton(props: { children: any; onClick: () => void; disabled?: boolean }) {
  return (
    <button
      onClick={props.onClick}
      disabled={props.disabled}
      class="shrink-0 px-2.5 py-1.5 text-[11px] rounded-md cursor-pointer transition-colors bg-neutral-800 text-neutral-300 hover:bg-neutral-700 hover:text-neutral-100 disabled:opacity-40 disabled:cursor-not-allowed"
    >
      {props.children}
    </button>
  );
}

function SecondaryButton(props: { children: any; onClick: () => void }) {
  return (
    <button
      onClick={props.onClick}
      class="flex-1 px-3 py-2 text-xs rounded-lg cursor-pointer transition-colors bg-neutral-900/60 border border-white/[0.06] text-neutral-400 hover:bg-neutral-800 hover:text-neutral-200"
    >
      {props.children}
    </button>
  );
}

function EmptyState(props: { children: any }) {
  return (
    <div class="px-4 py-6 rounded-lg border border-dashed border-neutral-800 text-xs text-neutral-500 leading-relaxed text-center">
      {props.children}
    </div>
  );
}
