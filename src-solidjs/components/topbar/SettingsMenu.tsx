import { createSignal, createEffect, Show, For, onCleanup, onMount } from "solid-js";
import { settings, setSettings } from "../../stores/settingsStore";
import { displayPaths } from "../../stores/galleryStore";
import { viewerOpen } from "../../stores/viewerStore";
import type { AppSettings, CompanionLocation, RendererMode, PluginInfo } from "../../lib/types";
import { rebuildThumbnails, listPlugins, installPlugin, runPluginBatch, cancelPluginBatch, enableRemoteAccess, disableRemoteAccess, getRemoteAccessInfo, type RemoteAccessInfo } from "../../lib/ipc";
import { pluginStarted, pluginProgress, pluginFinished, pluginFailed, pluginCancelled } from "../../stores/pluginStore";
import { safeListen as listen } from "../../lib/runtime";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { isWeb } from "../../lib/runtime";

const THUMB_PRESETS = [
  { label: "S", value: 120 },
  { label: "M", value: 200 },
  { label: "L", value: 300 },
  { label: "XL", value: 400 },
] as const;

const GAP_PRESETS = [
  { label: "None", value: 0 },
  { label: "Tight", value: 2 },
  { label: "Normal", value: 4 },
  { label: "Wide", value: 8 },
] as const;

export function SettingsMenu(props: { onOpenFolder?: () => void; onOpenDuplicates?: () => void; onRequestShow?: () => void }) {
  const [open, setOpen] = createSignal(false);

  const toggle = () => setOpen((v) => !v);

  // Close on Escape, toggle on 'I' in grid view
  const handleKey = (e: KeyboardEvent) => {
    if (e.key === "Escape" && open()) {
      e.stopPropagation();
      setOpen(false);
      return;
    }
    if (
      e.key === "i" &&
      !e.ctrlKey && !e.metaKey && !e.altKey &&
      !viewerOpen() &&
      !(e.target instanceof HTMLInputElement || e.target instanceof HTMLTextAreaElement)
    ) {
      e.preventDefault();
      if (!open()) props.onRequestShow?.();
      toggle();
    }
  };
  window.addEventListener("keydown", handleKey, true);
  onCleanup(() => window.removeEventListener("keydown", handleKey, true));

  const updateDisplay = <K extends keyof AppSettings["display"]>(
    key: K,
    value: AppSettings["display"][K],
  ) => {
    setSettings((prev) => ({
      ...prev,
      display: { ...prev.display, [key]: value },
    }));
  };

  const updatePerformance = <K extends keyof AppSettings["performance"]>(
    key: K,
    value: AppSettings["performance"][K],
  ) => {
    setSettings((prev) => ({
      ...prev,
      performance: { ...prev.performance, [key]: value },
    }));
  };

  const updateStorage = <K extends keyof AppSettings["storage"]>(
    key: K,
    value: AppSettings["storage"][K],
  ) => {
    setSettings((prev) => ({
      ...prev,
      storage: { ...prev.storage, [key]: value },
    }));
  };

  // ── Remote (LAN) web access ──
  const REMOTE_PORT_KEY = "lv_remote_port";
  const DEFAULT_REMOTE_PORT = 8723;
  const [remote, setRemote] = createSignal<RemoteAccessInfo | null>(null);
  const [remoteBusy, setRemoteBusy] = createSignal(false);
  const [remoteError, setRemoteError] = createSignal("");
  const [copied, setCopied] = createSignal(false);

  // Fixed port persisted in localStorage so a firewall rule made for it keeps
  // working across launches. 0 means "let the OS pick" (ephemeral).
  const storedPort = Number(localStorage.getItem(REMOTE_PORT_KEY));
  const [remotePort, setRemotePort] = createSignal<number>(
    Number.isFinite(storedPort) && storedPort > 0 ? storedPort : DEFAULT_REMOTE_PORT,
  );

  const updateRemotePort = (raw: string) => {
    const n = parseInt(raw, 10);
    const port = Number.isFinite(n) ? Math.min(Math.max(n, 0), 65535) : 0;
    setRemotePort(port);
    localStorage.setItem(REMOTE_PORT_KEY, String(port));
  };

  onMount(() => {
    if (isWeb()) return;
    getRemoteAccessInfo().then(setRemote).catch(() => {});
  });

  // While the panel is open and remote access is on, poll status so the
  // reachability indicator (clients_seen) updates as devices connect.
  let pollTimer: ReturnType<typeof setInterval> | undefined;
  createEffect(() => {
    clearInterval(pollTimer);
    if (open() && remote() && !isWeb()) {
      pollTimer = setInterval(() => {
        getRemoteAccessInfo()
          .then((info) => info && setRemote(info))
          .catch(() => {});
      }, 3000);
    }
  });
  onCleanup(() => clearInterval(pollTimer));

  const toggleRemote = async () => {
    if (remoteBusy()) return;
    setRemoteBusy(true);
    setRemoteError("");
    try {
      if (remote()) {
        await disableRemoteAccess();
        setRemote(null);
      } else {
        // 0 → ephemeral (OS-assigned); any other value → fixed port.
        setRemote(await enableRemoteAccess(remotePort() || undefined));
      }
    } catch (e) {
      console.error("Remote access toggle failed:", e);
      const msg = String(e);
      setRemoteError(
        msg.includes("in use") || msg.includes("address")
          ? `Port ${remotePort()} is unavailable — try another.`
          : "Failed to start remote access.",
      );
    } finally {
      setRemoteBusy(false);
    }
  };

  const copyRemoteUrl = async () => {
    const url = remote()?.url;
    if (!url) return;
    try {
      await navigator.clipboard.writeText(url);
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    } catch (e) {
      console.error("Clipboard write failed:", e);
    }
  };

  const [rebuilding, setRebuilding] = createSignal(false);
  const handleRebuild = async () => {
    setRebuilding(true);
    try {
      await rebuildThumbnails();
      window.dispatchEvent(new CustomEvent("lightview:thumbnails-invalidated"));
    } catch (e) {
      console.error("Rebuild failed:", e);
    }
    setRebuilding(false);
  };

  // ── Plugins ──
  const [plugins, setPlugins] = createSignal<PluginInfo[]>([]);
  const [pluginRunning, setPluginRunning] = createSignal<string | null>(null);
  const [pluginStatus, setPluginStatus] = createSignal("");

  const refreshPlugins = async () => {
    try {
      setPlugins(await listPlugins());
    } catch {}
  };

  onMount(refreshPlugins);

  const handleAddPlugin = async () => {
    const selected = await openDialog({
      title: "Select plugin Python file",
      directory: false,
      multiple: false,
      filters: [{ name: "Python Plugin", extensions: ["py"] }],
    });
    if (!selected) return;
    try {
      await installPlugin(selected as string);
      await refreshPlugins();
      setPluginStatus("Plugin installed");
      setTimeout(() => setPluginStatus(""), 3000);
    } catch (e) {
      console.error("Install failed:", e);
      setPluginStatus("Install failed");
      setTimeout(() => setPluginStatus(""), 3000);
    }
  };

  const handleAddPluginDir = async () => {
    const selected = await openDialog({
      title: "Select plugin directory",
      directory: true,
      multiple: false,
    });
    if (!selected) return;
    try {
      await installPlugin(selected as string);
      await refreshPlugins();
      setPluginStatus("Plugin installed");
      setTimeout(() => setPluginStatus(""), 3000);
    } catch (e) {
      console.error("Install failed:", e);
      setPluginStatus("Install failed");
      setTimeout(() => setPluginStatus(""), 3000);
    }
  };

  const handleRunOnAll = async (pluginName: string) => {
    const paths = displayPaths();
    if (paths.length === 0) return;
    const plugin = plugins().find((p) => p.name === pluginName);
    const displayName = plugin?.display_name ?? pluginName;
    setPluginRunning(pluginName);
    setPluginStatus(`Running on ${paths.length} photos...`);
    pluginStarted(pluginName, displayName, paths.length);

    // Listen for per-file progress and completion events from the background task
    const unlistenProgress = await listen<{
      completed: number;
      total: number;
    }>("plugin:progress", (event) => {
      pluginProgress(event.payload.completed, event.payload.total);
    });

    const unlistenDone = await listen<{
      succeeded: number;
      failed: number;
      cancelled: boolean;
    }>("plugin:done", (event) => {
      const { succeeded, failed, cancelled } = event.payload;
      if (cancelled) {
        pluginCancelled();
      } else if (failed > 0) {
        const msg = `Done: ${succeeded} tagged, ${failed} failed`;
        setPluginStatus(msg);
        pluginFailed(msg);
      } else {
        const msg = `Tagged ${succeeded} photos`;
        setPluginStatus(msg);
        pluginFinished(msg);
      }
      cleanup();
    });

    const cleanup = () => {
      unlistenProgress();
      unlistenDone();
      setPluginRunning(null);
      setTimeout(() => setPluginStatus(""), 5000);
    };

    // Fire-and-forget — the backend runs the batch in a background task
    // and reports progress/completion via events.
    try {
      await runPluginBatch(pluginName, paths, "tag");
    } catch (e) {
      console.error("Plugin batch launch failed:", e);
      setPluginStatus("Run failed");
      pluginFailed("Run failed");
      cleanup();
    }
  };

  return (
    <div class="relative">
      {/* Gear button */}
      <button
        onClick={toggle}
        class="shrink-0 w-8 h-8 flex items-center justify-center text-neutral-400 hover:text-neutral-200 bg-neutral-800 hover:bg-neutral-700 rounded transition-colors cursor-pointer"
        title="Settings"
      >
        <svg
          width="16"
          height="16"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          stroke-width="2"
          stroke-linecap="round"
          stroke-linejoin="round"
        >
          <circle cx="12" cy="12" r="3" />
          <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 0 1-2.83-2.83l.06-.06A1.65 1.65 0 0 0 4.68 15a1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 2.83-2.83l.06.06A1.65 1.65 0 0 0 9 4.68a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 2.83l-.06.06A1.65 1.65 0 0 0 19.4 9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" />
        </svg>
      </button>

      {/* Dropdown panel */}
      <Show when={open()}>
        {/* Backdrop — click to close */}
        <div class="fixed inset-0 z-40" onClick={() => setOpen(false)} />

        <div
          class="absolute top-full right-0 mt-2 w-72 rounded-lg overflow-hidden shadow-xl z-50"
          style={{
            background: "rgba(18, 18, 18, 0.96)",
            "backdrop-filter": "blur(16px)",
            border: "1px solid rgba(255,255,255,0.08)",
          }}
        >
          <div class="px-4 py-3 border-b border-neutral-800/60">
            <span class="text-sm font-medium text-neutral-200">Settings</span>
          </div>

          <div class="px-4 py-3 flex flex-col gap-4 max-h-[70vh] overflow-y-auto hide-scrollbar">
            {/* ── Display ── */}
            <Section label="Display">
              {/* Thumbnail size */}
              <Field label="Thumbnail size">
                <div class="flex items-center gap-2">
                  {/* Preset buttons */}
                  <div class="flex gap-1">
                    {THUMB_PRESETS.map((p) => (
                      <button
                        class={`px-2 py-0.5 text-xs rounded cursor-pointer transition-colors ${
                          settings().display.thumbnail_size === p.value
                            ? "bg-teal-700/60 text-teal-200"
                            : "bg-neutral-800 text-neutral-400 hover:bg-neutral-700 hover:text-neutral-300"
                        }`}
                        onClick={() => updateDisplay("thumbnail_size", p.value)}
                      >
                        {p.label}
                      </button>
                    ))}
                  </div>
                </div>
              </Field>

              {/* Grid gap */}
              <Field label="Grid spacing">
                <div class="flex items-center gap-2">
                  <div class="flex gap-1">
                    {GAP_PRESETS.map((p) => (
                      <button
                        class={`px-2 py-0.5 text-xs rounded cursor-pointer transition-colors ${
                          settings().display.grid_gap === p.value
                            ? "bg-teal-700/60 text-teal-200"
                            : "bg-neutral-800 text-neutral-400 hover:bg-neutral-700 hover:text-neutral-300"
                        }`}
                        onClick={() => updateDisplay("grid_gap", p.value)}
                      >
                        {p.label}
                      </button>
                    ))}
                  </div>
                </div>
              </Field>

              {/* Background color */}
              <Field label="Background">
                <div class="flex items-center gap-2">
                  <input
                    type="color"
                    value={settings().display.background_color}
                    onInput={(e) =>
                      updateDisplay("background_color", e.currentTarget.value)
                    }
                    class="w-6 h-6 rounded cursor-pointer border border-neutral-700 bg-transparent"
                  />
                  <span class="text-xs text-neutral-500 font-mono">
                    {settings().display.background_color}
                  </span>
                </div>
              </Field>

              {/* Toggles */}
              <Toggle
                label="GIF autoplay in grid"
                checked={settings().display.gif_autoplay_grid}
                onChange={(v) => updateDisplay("gif_autoplay_grid", v)}
              />
              <Toggle
                label="Video hover preview"
                checked={settings().display.video_hover_preview}
                onChange={(v) => updateDisplay("video_hover_preview", v)}
              />
              <Toggle
                label="Video auto-replay"
                checked={settings().display.video_autoplay_loop}
                onChange={(v) => updateDisplay("video_autoplay_loop", v)}
              />
              <Toggle
                label="Thumbnail fade-in"
                checked={settings().display.scroll_blur}
                onChange={(v) => updateDisplay("scroll_blur", v)}
              />
              <Toggle
                label="Dark map tiles"
                checked={settings().display.map_dark_mode ?? true}
                onChange={(v) => updateDisplay("map_dark_mode", v)}
              />

              {/* Renderer mode */}
              <Field label="Renderer">
                <div class="flex gap-1">
                  {([
                    { value: "dom" as RendererMode, label: "DOM" },
                    { value: "canvas" as RendererMode, label: "Canvas" },
                    { value: "webgl" as RendererMode, label: "WebGL" },
                  ]).map((opt) => (
                    <button
                      class={`px-2 py-0.5 text-xs rounded cursor-pointer transition-colors ${
                        (settings().display.renderer_mode ?? "dom") === opt.value
                          ? "bg-teal-700/60 text-teal-200"
                          : "bg-neutral-800 text-neutral-400 hover:bg-neutral-700 hover:text-neutral-300"
                      }`}
                      onClick={() => updateDisplay("renderer_mode", opt.value)}
                    >
                      {opt.label}
                    </button>
                  ))}
                </div>
              </Field>
            </Section>

            {/* ── Remote access (desktop only) ── */}
            <Show when={!isWeb()}>
              <Section label="Remote Access">
                <Toggle
                  label="Enable LAN web access"
                  checked={remote() !== null}
                  onChange={() => void toggleRemote()}
                />
                {/* Port is editable only while disabled — changing it requires
                    a restart of the server. A fixed port lets a firewall rule
                    stick across launches. */}
                <Show when={!remote()}>
                  <Field label="Port">
                    <input
                      type="number"
                      min="0"
                      max="65535"
                      value={remotePort()}
                      onInput={(e) => updateRemotePort(e.currentTarget.value)}
                      class="w-20 px-2 py-1 bg-neutral-800 border border-neutral-700 rounded text-xs text-neutral-200 outline-none focus:border-neutral-500"
                      title="0 = random port each launch"
                    />
                  </Field>
                </Show>
                <Show when={remoteError()}>
                  <span class="text-xs text-red-400">{remoteError()}</span>
                </Show>
                <Show when={remote()}>
                  {(info) => (
                    <div class="flex flex-col gap-1.5">
                      <span class="text-[10px] text-neutral-500">
                        Open this URL in a browser on another device:
                      </span>
                      <Show
                        when={info().url}
                        fallback={
                          <span class="text-xs text-amber-400">
                            No LAN IP detected — port {info().port}, token {info().token}
                          </span>
                        }
                      >
                        <button
                          onClick={copyRemoteUrl}
                          class="text-left px-2 py-1.5 rounded bg-neutral-800 hover:bg-neutral-700 text-[11px] text-teal-300 font-mono break-all cursor-pointer transition-colors"
                          title="Click to copy"
                        >
                          {info().url}
                        </button>
                      </Show>
                      <span class="text-[10px] text-neutral-600">
                        {copied() ? "Copied!" : "Read-only. Anyone with this link can browse the gallery."}
                      </span>

                      {/* Reachability indicator — the host can't pre-detect a
                          firewall block, but a connection from a remote device
                          proves the port is open. */}
                      <div class="flex items-start gap-1.5 mt-1">
                        <Show
                          when={info().clients_seen > 0}
                          fallback={
                            <>
                              <span class="text-amber-400 leading-tight">&#9679;</span>
                              <span class="text-[10px] text-neutral-500 leading-tight">
                                Waiting for a device to connect. If a device can't
                                load the page, allow the port in your firewall.
                              </span>
                            </>
                          }
                        >
                          <span class="text-teal-400 leading-tight">&#9679;</span>
                          <span class="text-[10px] text-neutral-400 leading-tight">
                            Reachable — a device has connected.
                          </span>
                        </Show>
                      </div>

                      {/* Firewall guidance, shown until a device connects. */}
                      <Show when={info().firewall_hint && info().clients_seen === 0}>
                        <pre class="mt-1 px-2 py-1.5 rounded bg-neutral-900/80 border border-neutral-800 text-[10px] text-neutral-400 whitespace-pre-wrap break-all font-mono">
                          {info().firewall_hint}
                        </pre>
                      </Show>
                    </div>
                  )}
                </Show>
              </Section>
            </Show>

            {/* ── Thumbnails (desktop only — write op) ── */}
            <Show when={!isWeb()}>
              <Section label="Thumbnails">
                <button
                  onClick={handleRebuild}
                  disabled={rebuilding()}
                  class="px-3 py-1.5 text-xs rounded cursor-pointer transition-colors bg-neutral-800 text-neutral-400 hover:bg-neutral-700 hover:text-neutral-300 disabled:opacity-50 disabled:cursor-not-allowed"
                >
                  {rebuilding() ? "Rebuilding..." : "Rebuild All Thumbnails"}
                </button>
              </Section>
            </Show>

            {/* ── Storage (desktop only) ── */}
            <Show when={!isWeb()}>
            <Section label="Storage">
              <Field label="Companion file location">
                <div class="flex gap-1">
                  {([
                    { value: "lightview_folder" as const, label: ".lightview folder" },
                    { value: "alongside" as const, label: "Alongside images" },
                  ]).map((opt) => (
                    <button
                      class={`px-2 py-0.5 text-xs rounded cursor-pointer transition-colors ${
                        settings().storage.companion_location === opt.value
                          ? "bg-teal-700/60 text-teal-200"
                          : "bg-neutral-800 text-neutral-400 hover:bg-neutral-700 hover:text-neutral-300"
                      }`}
                      onClick={() => updateStorage("companion_location", opt.value)}
                    >
                      {opt.label}
                    </button>
                  ))}
                </div>
              </Field>
            </Section>
            </Show>

            {/* ── Plugins (desktop only) ── */}
            <Show when={!isWeb()}>
            <Section label="Plugins">
              <div class="flex flex-col gap-2">
                <For each={plugins()}>
                  {(plugin) => (
                    <div class="flex items-center justify-between gap-2 px-2 py-1.5 rounded bg-neutral-800/50">
                      <div class="flex flex-col min-w-0">
                        <span class="text-xs text-neutral-300 truncate">{plugin.display_name}</span>
                        <span class="text-[10px] text-neutral-500 truncate">{plugin.description}</span>
                      </div>
                      <button
                        disabled={pluginRunning() !== null}
                        onClick={() => handleRunOnAll(plugin.name)}
                        class="shrink-0 px-2 py-1 text-[10px] rounded cursor-pointer transition-colors bg-neutral-700 text-neutral-300 hover:bg-neutral-600 hover:text-neutral-100 disabled:opacity-50 disabled:cursor-not-allowed"
                      >
                        {pluginRunning() === plugin.name ? "Running..." : "Run All"}
                      </button>
                    </div>
                  )}
                </For>
                <Show when={plugins().length === 0}>
                  <span class="text-xs text-neutral-600">No plugins installed</span>
                </Show>
              </div>
              <Show when={pluginStatus()}>
                <span class="text-xs text-neutral-400">{pluginStatus()}</span>
              </Show>

              <div class="flex gap-1">
                <button
                  onClick={handleAddPlugin}
                  class="flex-1 px-3 py-1.5 text-xs rounded cursor-pointer transition-colors bg-neutral-800 text-neutral-400 hover:bg-neutral-700 hover:text-neutral-300"
                >
                  Add .py File...
                </button>
                <button
                  onClick={handleAddPluginDir}
                  class="flex-1 px-3 py-1.5 text-xs rounded cursor-pointer transition-colors bg-neutral-800 text-neutral-400 hover:bg-neutral-700 hover:text-neutral-300"
                >
                  Add Folder...
                </button>
              </div>
            </Section>
            </Show>

            {/* ── Deduplication (desktop only) ── */}
            <Show when={!isWeb()}>
            <Section label="Deduplication">
              <button
                onClick={() => {
                  setOpen(false);
                  props.onOpenDuplicates?.();
                }}
                class="px-3 py-1.5 text-xs rounded cursor-pointer transition-colors bg-neutral-800 text-neutral-400 hover:bg-neutral-700 hover:text-neutral-300"
              >
                Find Duplicates...
              </button>
            </Section>
            </Show>

            {/* ── Gallery ── */}
            <Show when={props.onOpenFolder && !isWeb()}>
              <div class="border-t border-neutral-800/60 pt-3">
                <button
                  onClick={() => { props.onOpenFolder!(); setOpen(false); }}
                  class="w-full px-3 py-2 text-xs text-neutral-300 hover:text-neutral-100 bg-neutral-800 hover:bg-neutral-700 rounded transition-colors cursor-pointer text-left"
                >
                  Open Folder...
                </button>
              </div>
            </Show>
          </div>
        </div>
      </Show>
    </div>
  );
}

// ── Helpers ──

function Section(props: { label: string; children: any }) {
  return (
    <div class="flex flex-col gap-2.5">
      <span class="text-[11px] uppercase tracking-wider text-neutral-500 font-medium">
        {props.label}
      </span>
      {props.children}
    </div>
  );
}

function Field(props: { label: string; children: any }) {
  return (
    <div class="flex flex-col gap-1">
      <span class="text-xs text-neutral-400">{props.label}</span>
      {props.children}
    </div>
  );
}

function Toggle(props: { label: string; checked: boolean; onChange: (v: boolean) => void }) {
  return (
    <label class="flex items-center justify-between cursor-pointer group">
      <span class="text-xs text-neutral-400 group-hover:text-neutral-300 transition-colors">
        {props.label}
      </span>
      <button
        role="switch"
        aria-checked={props.checked}
        onClick={() => props.onChange(!props.checked)}
        class={`relative w-8 h-4.5 rounded-full transition-colors cursor-pointer ${
          props.checked ? "bg-teal-600" : "bg-neutral-700"
        }`}
      >
        <span
          class="absolute top-0.5 left-0.5 w-3.5 h-3.5 rounded-full bg-white transition-transform"
          style={{
            transform: props.checked ? "translateX(14px)" : "translateX(0)",
          }}
        />
      </button>
    </label>
  );
}
