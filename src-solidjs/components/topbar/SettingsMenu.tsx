import { createSignal, createEffect, Show, For, onCleanup, onMount } from "solid-js";
import { Portal, Dynamic } from "solid-js/web";
import { GearIcon, CloseIcon } from "./icons";
import { settings, setSettings } from "../../stores/settingsStore";
import { displayPaths, settingsOpen, setSettingsOpen, viewMode, setViewMode } from "../../stores/galleryStore";
import { viewerOpen } from "../../stores/viewerStore";
import type { AppSettings, CompanionLocation, PluginInfo } from "../../lib/types";
import {
  rebuildThumbnails,
  getSortedItems,
  precacheThumbnails,
  ensureTierThumbnails,
  listPlugins,
  installPlugin,
  runPluginBatch,
  cancelPluginBatch,
  enableRemoteAccess,
  disableRemoteAccess,
  getRemoteAccessInfo,
  type RemoteAccessInfo,
  getRemoteAuthState,
  generatePairingCode,
  revokeRemoteDevice,
  deleteRemoteDevice,
  setRemotePassword,
  clearRemotePassword,
  setRemoteInactivity,
  getUploadConfig,
  setUploadConfig,
  getRemoteDeleteConfig,
  setRemoteDeleteConfig,
  getRenderConfig,
  setRenderConfig,
  type RenderConfig,
  type RemoteAuthState,
  type PairingCode,
  type UploadConfig,
  type UploadScheme,
} from "../../lib/ipc";
import QRCode from "qrcode";
import { pluginStarted, pluginProgress, pluginFinished, pluginFailed, pluginCancelled } from "../../stores/pluginStore";
import { taggingWorkers, taggingJobs, taggingActions, refreshTaggingStatus, trackQueuedJob } from "../../stores/taggingStore";
import { enqueueTaggingJob, cancelTaggingJob, type TaggingJob } from "../../lib/ipc";
import { thumbGenStarted, thumbGenProgress, thumbGenFinished, thumbGenFailed } from "../../stores/thumbnailProgressStore";
import { capabilities } from "../../stores/capabilitiesStore";
import { safeListen as listen } from "../../lib/runtime";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { isWeb, isMobile } from "../../lib/runtime";
import { thumbSizeForCols, currentColCount } from "../../lib/gridLayout";

const THUMB_PRESETS = [
  { label: "S", value: 120 },
  { label: "M", value: 200 },
  { label: "L", value: 300 },
  { label: "XL", value: 400 },
] as const;

// On mobile, the thumbnail size picker switches from fixed-pixel buckets to
// a column-count picker: tapping "3" sets thumbnail_size such that the grid
// renders exactly 3 columns at the current viewport width. The choice of
// presets here matches the user's request: 1 column (biggest) → 5 columns.
const MOBILE_COL_PRESETS = [1, 2, 3, 4, 5] as const;

const GAP_PRESETS = [
  { label: "None", value: 0 },
  { label: "Tight", value: 2 },
  { label: "Normal", value: 4 },
  { label: "Wide", value: 8 },
] as const;

export function SettingsMenu(props: { onOpenFolder?: () => void; onOpenDuplicates?: () => void; onOpenTrash?: () => void; onRequestShow?: () => void; hideTrigger?: boolean }) {
  // Open state lives in the store (`settingsOpen`) so external chrome — e.g. the
  // mobile floating gear button — can open this panel, and so App can hide the
  // grid behind the full-screen mobile settings page. Cleared on unmount.
  const open = settingsOpen;
  const setOpen = setSettingsOpen;
  onCleanup(() => setSettingsOpen(false));

  // Web: re-sync worker/job state whenever the menu opens, so the Remote
  // Tagging section is current even if an SSE event was missed.
  createEffect(() => {
    if (open() && isWeb()) refreshTaggingStatus();
  });

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

  const updateDefaultFilter = <K extends keyof AppSettings["default_filter"]>(
    key: K,
    value: AppSettings["default_filter"][K],
  ) => {
    setSettings((prev) => ({
      ...prev,
      default_filter: { ...prev.default_filter, [key]: value },
    }));
  };

  // ── Remote (LAN) web access ──
  const REMOTE_PORT_KEY = "lv_remote_port";
  const DEFAULT_REMOTE_PORT = 8723;
  const [remote, setRemote] = createSignal<RemoteAccessInfo | null>(null);
  const [remoteBusy, setRemoteBusy] = createSignal(false);
  const [remoteError, setRemoteError] = createSignal("");

  // Per-gallery device list, password state, inactivity threshold.
  const [authState, setAuthState] = createSignal<RemoteAuthState | null>(null);

  // Active short-lived pairing code (one per kind at a time). The QR image is
  // rendered from `pairingQr.url`.
  const [pairing, setPairing] = createSignal<PairingCode | null>(null);
  const [pairingQr, setPairingQr] = createSignal<string>(""); // data URL
  const [pairingError, setPairingError] = createSignal("");

  // Password form state (separate from authState so we can show "saved").
  const [passwordInput, setPasswordInput] = createSignal("");
  const [passwordStatus, setPasswordStatus] = createSignal("");

  // Inactivity selector, in seconds. Keeping the choices coarse so users
  // don't fiddle endlessly.
  const INACTIVITY_PRESETS = [
    { label: "1h", value: 3600 },
    { label: "6h", value: 6 * 3600 },
    { label: "24h", value: 24 * 3600 },
    { label: "Never", value: 365 * 24 * 3600 },
  ] as const;

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

  const refreshAuthState = () => {
    if (isWeb()) return;
    getRemoteAuthState()
      .then(setAuthState)
      .catch(() => {});
  };

  onMount(() => {
    if (isWeb()) return;
    getRemoteAccessInfo().then(setRemote).catch(() => {});
    refreshAuthState();
    refreshUploadCfg();
    refreshRemoteDelete();
  });

  // While the panel is open and remote access is on, poll status so the
  // reachability indicator and device list stay fresh as phones connect.
  let pollTimer: ReturnType<typeof setInterval> | undefined;
  createEffect(() => {
    clearInterval(pollTimer);
    if (open() && remote() && !isWeb()) {
      pollTimer = setInterval(() => {
        getRemoteAccessInfo()
          .then((info) => info && setRemote(info))
          .catch(() => {});
        refreshAuthState();
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
        setPairing(null);
        setPairingQr("");
      } else {
        // 0 → ephemeral (OS-assigned); any other value → fixed port.
        setRemote(await enableRemoteAccess(remotePort() || undefined));
        refreshAuthState();
      }
    } catch (e) {
      console.error("Remote access toggle failed:", e);
      const msg = String(e);
      setRemoteError(
        msg.includes("in use") || msg.includes("address")
          ? `Port ${remotePort()} is unavailable — try another.`
          : msg.includes("No gallery")
            ? "Open a gallery first."
            : "Failed to start remote access.",
      );
    } finally {
      setRemoteBusy(false);
    }
  };

  const newPairingCode = async (kind: "qr" | "pin") => {
    setPairingError("");
    setPairingQr("");
    try {
      const code = await generatePairingCode(kind);
      setPairing(code);
      if (kind === "qr" && code.pairing_url) {
        // Render the QR locally so the desktop never has to round-trip the
        // image bytes through IPC.
        const dataUrl = await QRCode.toDataURL(code.pairing_url, {
          margin: 1,
          width: 220,
          color: { dark: "#e5e5e5", light: "#0a0a0a" },
        });
        setPairingQr(dataUrl);
      }
    } catch (e) {
      setPairingError(String(e));
    }
  };

  const handleRevokeDevice = async (id: string) => {
    try {
      await revokeRemoteDevice(id);
      refreshAuthState();
    } catch (e) {
      console.error("Revoke failed:", e);
    }
  };

  const handleDeleteDevice = async (id: string) => {
    try {
      await deleteRemoteDevice(id);
      refreshAuthState();
    } catch (e) {
      console.error("Delete failed:", e);
    }
  };

  const handleSetPassword = async () => {
    if (!passwordInput()) return;
    try {
      await setRemotePassword(passwordInput());
      setPasswordInput("");
      setPasswordStatus("Saved");
      setTimeout(() => setPasswordStatus(""), 2000);
      refreshAuthState();
    } catch (e) {
      setPasswordStatus(String(e));
    }
  };

  const handleClearPassword = async () => {
    try {
      await clearRemotePassword();
      setPasswordStatus("Cleared");
      setTimeout(() => setPasswordStatus(""), 2000);
      refreshAuthState();
    } catch (e) {
      setPasswordStatus(String(e));
    }
  };

  const handleSetInactivity = async (secs: number) => {
    try {
      await setRemoteInactivity(secs);
      refreshAuthState();
    } catch (e) {
      console.error("Set inactivity failed:", e);
    }
  };

  // ── Device uploads (per-gallery) ──
  const [uploadCfg, setUploadCfg] = createSignal<UploadConfig | null>(null);
  const UPLOAD_SCHEMES: { label: string; value: UploadScheme }[] = [
    { label: "Year", value: "year" },
    { label: "Year & month", value: "year_month" },
    { label: "Year & album", value: "year_album" },
    { label: "Flat", value: "flat" },
  ];

  const refreshUploadCfg = () => {
    if (isWeb()) return;
    getUploadConfig().then(setUploadCfg).catch(() => {});
  };

  const saveUploadCfg = async (next: UploadConfig) => {
    setUploadCfg(next); // optimistic
    try {
      await setUploadConfig(next.enabled, next.scheme);
    } catch (e) {
      console.error("Set upload config failed:", e);
      refreshUploadCfg(); // revert to server truth
    }
  };

  // ── Remote delete (per-gallery) ──
  const [remoteDelete, setRemoteDelete] = createSignal<boolean | null>(null);

  const refreshRemoteDelete = () => {
    if (isWeb()) return;
    getRemoteDeleteConfig().then(setRemoteDelete).catch(() => {});
  };

  const saveRemoteDelete = async (enabled: boolean) => {
    setRemoteDelete(enabled); // optimistic
    try {
      await setRemoteDeleteConfig(enabled);
    } catch (e) {
      console.error("Set remote delete config failed:", e);
      refreshRemoteDelete(); // revert to server truth
    }
  };

  const formatRelative = (ts: number) => {
    if (!ts) return "never";
    const diff = Math.max(0, Date.now() / 1000 - ts);
    if (diff < 60) return "just now";
    if (diff < 3600) return `${Math.floor(diff / 60)}m ago`;
    if (diff < 86400) return `${Math.floor(diff / 3600)}h ago`;
    return `${Math.floor(diff / 86400)}d ago`;
  };

  // Process-level render config (GPU compositing). Global, not per-gallery, so
  // it's fetched directly rather than from the settings store. Changes take
  // effect only on restart.
  const [renderCfg, setRenderCfg] = createSignal<RenderConfig | null>(null);
  const [renderRestart, setRenderRestart] = createSignal(false);
  if (!isWeb()) {
    onMount(async () => {
      try { setRenderCfg(await getRenderConfig()); } catch { /* ignore */ }
    });
  }
  // Default-off (software path) until the user opts in.
  const gpuOn = () => renderCfg()?.gpu_acceleration ?? false;
  const toggleGpu = async (v: boolean) => {
    const cfg = renderCfg();
    try {
      await setRenderConfig(v, cfg?.gtk_backend ?? null);
      setRenderCfg({ gpu_acceleration: v, gtk_backend: cfg?.gtk_backend ?? null });
      setRenderRestart(true);
    } catch (e) {
      console.error("Failed to save render config:", e);
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

  // ── Generate missing thumbnails (all tiers the grids use at normal zoom) ──
  // Sequential bounded batches over the whole gallery: pass 1 the standard
  // grid tier (micro derives with it), pass 2 the justified base tier.
  // Already-cached paths are filtered backend-side, so re-runs are cheap.
  // Running this up front trades one supervised generation session for the
  // burst-generation heat that otherwise happens while scrolling cold
  // regions. Progress drives the same overlay as scroll-driven generation.
  const [precachingAll, setPrecachingAll] = createSignal(false);
  let precacheAllCancel = false;
  const PRECACHE_ALL_BATCH = 48;
  const handlePrecacheAll = async () => {
    if (precachingAll()) {
      precacheAllCancel = true;
      return;
    }
    setPrecachingAll(true);
    precacheAllCancel = false;
    try {
      // Unfiltered listing — the active view filter must not hide paths from
      // a whole-gallery maintenance pass.
      const all = await getSortedItems("name", "asc", { type: "none" });
      const paths = all.items.map((it) => it.path);
      const totalSteps = paths.length * 2;
      let done = 0;
      thumbGenStarted(totalSteps);
      for (const pass of ["standard", "justified"] as const) {
        for (let i = 0; i < paths.length && !precacheAllCancel; i += PRECACHE_ALL_BATCH) {
          const batch = paths.slice(i, i + PRECACHE_ALL_BATCH);
          if (pass === "standard") await precacheThumbnails(batch);
          else await ensureTierThumbnails(batch, "j");
          done += batch.length;
          thumbGenProgress(done, totalSteps);
        }
      }
      thumbGenFinished(done);
    } catch (e) {
      console.error("Generate-missing-thumbnails failed:", e);
      thumbGenFailed(String(e));
    }
    setPrecachingAll(false);
  };

  // ── Plugins ──
  const [plugins, setPlugins] = createSignal<PluginInfo[]>([]);
  const [pluginRunning, setPluginRunning] = createSignal<string | null>(null);
  const [pluginStatus, setPluginStatus] = createSignal("");

  const refreshPlugins = async () => {
    // Same capability gate as ContextMenu: the web /api/invoke allowlist
    // rejects list_plugins, so calling it on boot is a guaranteed 403.
    if (!capabilities().plugins) return;
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

  /** Web: enqueue a "tag everything this plugin hasn't seen" job for a
   * connected worker (pinned to `workerId` when the user picked one). The
   * filter resolves at claim time, so it's idempotent — re-running after new
   * uploads only tags the new files. */
  const handleTagAllUntagged = async (pluginName: string, tagPrefix: string, workerId?: string) => {
    try {
      const job = await enqueueTaggingJob(
        pluginName,
        { filter: `type:image AND NOT has::plugin.${tagPrefix}` },
        workerId,
      );
      trackQueuedJob(job);
    } catch (e) {
      console.error("Failed to enqueue tagging job:", e);
      // The desktop plugin-status line isn't rendered on web; surface the
      // error through the toast instead.
      pluginStarted(pluginName, pluginName, 0);
      pluginFailed(String(e));
    }
  };

  /** Newest-first slice of the server's job list for the status readout. */
  const recentTaggingJobs = () => taggingJobs().slice(-5).reverse();

  const jobStateColor = (job: TaggingJob) => {
    switch (job.state) {
      case "running": return "text-teal-400";
      case "queued": return "text-neutral-400";
      case "done": return "text-green-400";
      case "failed": return "text-red-400";
      case "cancelled": return "text-yellow-400";
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
      {/* Gear button — hidden when an external trigger (the mobile floating
          button) drives the panel instead. */}
      <Show when={!props.hideTrigger}>
      <button
        onClick={toggle}
        class="shrink-0 w-8 h-8 flex items-center justify-center text-neutral-400 hover:text-neutral-200 bg-neutral-800 hover:bg-neutral-700 rounded transition-colors cursor-pointer"
        title="Settings"
        aria-label="Settings"
      >
        <GearIcon size={16} />
      </button>
      </Show>

      {/* Dropdown panel (desktop) / full-screen page (mobile) */}
      <Show when={open()}>
        {/* Backdrop — click to close (desktop only; the mobile page is opaque
            and covers the whole viewport, so there's nothing to click behind). */}
        <Show when={!isMobile()}>
          <div class="fixed inset-0 z-40" onClick={() => setOpen(false)} />
        </Show>

        {/* On mobile the panel is a full-screen, fully-opaque page rather than a
            translucent drawer over the gallery: phones don't reliably honor
            `backdrop-filter`, so the gallery used to bleed through and the panel
            read as empty/cut-off.

            The mobile page is portalled to <body> so its `position: fixed`
            resolves against the viewport. Rendered in place it would be trapped
            by the top bar's `backdrop-filter`, which establishes a containing
            block for fixed descendants and clips the page to the bar's height.
            Desktop stays in place — its `absolute` panel is positioned by the
            gear's `relative` wrapper. */}
        <Dynamic component={isMobile() ? Portal : InPlace}>
        <div
          class={
            isMobile()
              ? "fixed inset-0 z-[60] overflow-hidden flex flex-col"
              : "absolute top-full right-0 mt-2 w-72 rounded-lg overflow-hidden shadow-xl z-50 flex flex-col max-h-[calc(100vh-5rem)]"
          }
          style={
            isMobile()
              ? {
                  background: "#121212",
                  // Clear the notch/dynamic island and the home-indicator area.
                  "padding-top": "env(safe-area-inset-top)",
                  "padding-bottom": "env(safe-area-inset-bottom)",
                }
              : {
                  background: "rgba(18, 18, 18, 0.96)",
                  "backdrop-filter": "blur(16px)",
                  border: "1px solid rgba(255,255,255,0.08)",
                }
          }
        >
          <div class="px-4 py-3 border-b border-neutral-800/60 flex items-center justify-between shrink-0">
            <div class="flex items-baseline gap-2">
              <span class="text-sm font-medium text-neutral-200">Settings</span>
              {/* Image count for the current filter. On desktop this sits in the
                  top bar; on mobile the bar is too cramped, so it lives here. */}
              <Show when={isMobile()}>
                <span class="text-xs text-neutral-500 tabular-nums">
                  {displayPaths().length.toLocaleString()} images
                </span>
              </Show>
            </div>
            <Show when={isMobile()}>
              <button
                onClick={() => setOpen(false)}
                class="w-10 h-10 -mr-2 flex items-center justify-center rounded text-neutral-400 hover:text-neutral-200 hover:bg-neutral-800 cursor-pointer"
                title="Close"
                aria-label="Close settings"
              >
                <CloseIcon size={16} />
              </button>
            </Show>
          </div>

          <div
            class="px-4 py-3 flex flex-col gap-4 overflow-y-auto overscroll-contain flex-1 min-h-0"
            classList={{
              "hide-scrollbar": isMobile(),
              "dupes-scroll": !isMobile(),
            }}
          >
            {/* ── View (mobile only — relocated from the top bar, which is too
                cramped on phones to fit it alongside the search field) ── */}
            <Show when={isMobile()}>
              <Section label="View" order={0}>
                <div class="flex items-center gap-1 p-0.5 rounded bg-neutral-800/60">
                  <For each={[
                    { mode: "grid" as const, label: "Grid" },
                    { mode: "justified" as const, label: "Justified" },
                    { mode: "map" as const, label: "Map" },
                  ]}>
                    {(v) => (
                      <button
                        onClick={() => { setViewMode(v.mode); setOpen(false); }}
                        class="flex-1 px-2 py-1.5 text-sm rounded cursor-pointer transition-colors text-neutral-300 hover:bg-neutral-700"
                        classList={{ "bg-neutral-700 text-white": viewMode() === v.mode }}
                      >
                        {v.label}
                      </button>
                    )}
                  </For>
                </div>
              </Section>
            </Show>

            {/* ── Display ── */}
            <Section label="Display" order={4}>
              {/* Thumbnail size — mobile uses a column-count picker so the
                  presets stay meaningful on a 400px-wide viewport; desktop
                  keeps the fixed-pixel S/M/L/XL buckets. */}
              <Show
                when={isMobile()}
                fallback={
                  <Field label="Thumbnail size">
                    <div class="flex items-center gap-2">
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
                }
              >
                <Field label="Columns">
                  <div class="flex items-center gap-1.5">
                    {MOBILE_COL_PRESETS.map((n) => {
                      const active = () =>
                        currentColCount(
                          settings().display.thumbnail_size,
                          settings().display.grid_gap,
                        ) === n;
                      return (
                        <button
                          class={`min-w-8 h-8 px-2 text-sm rounded cursor-pointer transition-colors ${
                            active()
                              ? "bg-teal-700/60 text-teal-200"
                              : "bg-neutral-800 text-neutral-400 hover:bg-neutral-700 hover:text-neutral-300"
                          }`}
                          onClick={() =>
                            updateDisplay(
                              "thumbnail_size",
                              thumbSizeForCols(n, settings().display.grid_gap),
                            )
                          }
                        >
                          {n}
                        </button>
                      );
                    })}
                  </div>
                </Field>
              </Show>

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

              {/* Zoom range — min/max thumbnail (row) size the zoom allows. */}
              <Field label="Zoom range (px)">
                <div class="flex items-center gap-2">
                  <input
                    type="number"
                    min="40"
                    max="2000"
                    value={settings().display.thumb_size_min ?? 120}
                    onInput={(e) => {
                      const n = parseInt(e.currentTarget.value, 10);
                      if (Number.isFinite(n) && n > 0) {
                        const max = settings().display.thumb_size_max ?? 700;
                        updateDisplay("thumb_size_min", Math.min(n, max - 1));
                      }
                    }}
                    class="w-20 px-2 py-1 bg-neutral-800 border border-neutral-700 rounded text-xs text-neutral-200 outline-none focus:border-neutral-500"
                    title="Smallest thumbnail / row size the zoom control reaches"
                  />
                  <span class="text-xs text-neutral-500">to</span>
                  <input
                    type="number"
                    min="40"
                    max="2000"
                    value={settings().display.thumb_size_max ?? 700}
                    onInput={(e) => {
                      const n = parseInt(e.currentTarget.value, 10);
                      if (Number.isFinite(n) && n > 0) {
                        const min = settings().display.thumb_size_min ?? 120;
                        updateDisplay("thumb_size_max", Math.max(n, min + 1));
                      }
                    }}
                    class="w-20 px-2 py-1 bg-neutral-800 border border-neutral-700 rounded text-xs text-neutral-200 outline-none focus:border-neutral-500"
                    title="Largest thumbnail / row size the zoom control reaches"
                  />
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
                label="Autoplay short videos in grid"
                checked={settings().display.video_autoplay_grid}
                onChange={(v) => updateDisplay("video_autoplay_grid", v)}
              />
              <Show when={settings().display.video_autoplay_grid}>
                <Field label="Max video length (seconds)">
                  <input
                    type="number"
                    min="1"
                    max="3600"
                    value={settings().display.video_autoplay_max_seconds}
                    onInput={(e) => {
                      const n = parseInt(e.currentTarget.value, 10);
                      if (Number.isFinite(n) && n > 0) {
                        updateDisplay("video_autoplay_max_seconds", n);
                      }
                    }}
                    class="w-20 px-2 py-1 bg-neutral-800 border border-neutral-700 rounded text-xs text-neutral-200 outline-none focus:border-neutral-500"
                    title="Videos at or under this length autoplay in the grid"
                  />
                </Field>
              </Show>
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
                label="Autoplay videos in viewer"
                checked={settings().display.video_autoplay_viewer}
                onChange={(v) => updateDisplay("video_autoplay_viewer", v)}
              />
              <p class="text-[10px] text-neutral-500 -mt-1 pl-0.5">
                Starts muted — tap the pill or the speaker button for sound.
              </p>
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
              <Toggle
                label="High-detail justified zoom"
                checked={settings().display.justified_high_detail ?? true}
                onChange={(v) => updateDisplay("justified_high_detail", v)}
              />
              <p class="text-[10px] text-neutral-500 -mt-1 pl-0.5">
                Generates sharper thumbnails when zoomed into the justified view.
                Uses more disk for the images you view zoomed in.
              </p>

              {/* Mobile-only: where the search/sort sheet appears. */}
              <Show when={isMobile()}>
                <div class="flex flex-col gap-1.5">
                  <span class="text-xs text-neutral-300">Filter sheet position</span>
                  <div class="flex items-center gap-1 p-0.5 rounded bg-neutral-800/60">
                    <For each={[
                      { value: "top" as const, label: "Top" },
                      { value: "bottom" as const, label: "Bottom" },
                    ]}>
                      {(opt) => {
                        const active = () =>
                          settings().display.mobile_filter_sheet === opt.value;
                        return (
                          <button
                            class="flex-1 px-2 py-1 text-xs rounded cursor-pointer transition-colors"
                            classList={{
                              "bg-neutral-700 text-white": active(),
                              "text-neutral-300": !active(),
                            }}
                            onClick={() => updateDisplay("mobile_filter_sheet", opt.value)}
                          >
                            {opt.label}
                          </button>
                        );
                      }}
                    </For>
                  </div>
                  <p class="text-[10px] text-neutral-500 pl-0.5">
                    Bottom is easier to reach one-handed on large phones.
                  </p>
                </div>
              </Show>

            </Section>

            {/* ── Default filter (desktop-managed; LAN clients inherit it) ── */}
            <Show when={!isWeb()}>
            <Section label="Default Filter" order={5}>
              <Toggle
                label="Apply on gallery open"
                checked={settings().default_filter?.enabled ?? false}
                onChange={(v) => updateDefaultFilter("enabled", v)}
              />
              <Show when={settings().default_filter?.enabled}>
                <input
                  type="text"
                  value={settings().default_filter?.query ?? ""}
                  onInput={(e) => updateDefaultFilter("query", e.currentTarget.value)}
                  placeholder="e.g. rating>=3 AND NOT auto::indoor"
                  class="w-full px-2 py-1 bg-neutral-800 border border-neutral-700 rounded text-xs text-neutral-200 placeholder-neutral-600 outline-none focus:border-neutral-500"
                />
                <span class="text-[10px] text-neutral-600 leading-snug">
                  Applied automatically the next time this gallery is opened — including
                  from LAN web clients.
                </span>
              </Show>
            </Section>
            </Show>

            {/* ── Remote access (desktop only) ── */}
            <Show when={!isWeb()}>
              <Section label="Remote Access" order={2}>
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
                    <div class="flex flex-col gap-3">
                      <Show
                        when={info().base_url}
                        fallback={
                          <span class="text-xs text-amber-400">
                            No LAN IP detected — port {info().port}
                          </span>
                        }
                      >
                        <span class="text-[10px] text-neutral-500 font-mono">
                          {info().base_url}
                        </span>
                      </Show>

                      {/* ── Pair a new device ── */}
                      <div class="flex flex-col gap-1.5">
                        <span class="text-[11px] text-neutral-400">Pair a device</span>
                        <div class="flex gap-1.5">
                          <button
                            onClick={() => newPairingCode("qr")}
                            class="px-2 py-1 text-[11px] rounded bg-neutral-800 hover:bg-neutral-700 text-neutral-300 cursor-pointer transition-colors"
                          >
                            QR code
                          </button>
                          <button
                            onClick={() => newPairingCode("pin")}
                            class="px-2 py-1 text-[11px] rounded bg-neutral-800 hover:bg-neutral-700 text-neutral-300 cursor-pointer transition-colors"
                          >
                            PIN
                          </button>
                          <Show when={pairing()}>
                            <button
                              onClick={() => { setPairing(null); setPairingQr(""); }}
                              class="px-2 py-1 text-[11px] rounded text-neutral-500 hover:text-neutral-300 cursor-pointer"
                            >
                              Clear
                            </button>
                          </Show>
                        </div>
                        <Show when={pairingError()}>
                          <span class="text-[10px] text-red-400">{pairingError()}</span>
                        </Show>
                        <Show when={pairing()}>
                          {(code) => (
                            <div class="mt-1 px-3 py-2.5 rounded bg-neutral-900 border border-neutral-800 flex flex-col items-center gap-2">
                              <Show when={code().kind === "qr"}>
                                <Show when={pairingQr()} fallback={
                                  <span class="text-[10px] text-neutral-500">Rendering&hellip;</span>
                                }>
                                  <img src={pairingQr()} alt="Pairing QR" class="w-44 h-44" />
                                </Show>
                                <span class="text-[10px] text-neutral-500 text-center">
                                  Scan with the phone's camera. Single-use; expires in 10&nbsp;min.
                                </span>
                              </Show>
                              <Show when={code().kind === "pin"}>
                                <div class="text-3xl font-mono tracking-[0.4em] text-teal-300 pl-[0.4em]">
                                  {code().code}
                                </div>
                                <Show when={code().pairing_url}>
                                  <span class="text-[10px] text-neutral-500 font-mono break-all text-center">
                                    {code().pairing_url}
                                  </span>
                                </Show>
                                <span class="text-[10px] text-neutral-500 text-center">
                                  Open the URL above and enter the PIN. Single-use; expires in 10&nbsp;min.
                                </span>
                              </Show>
                            </div>
                          )}
                        </Show>
                      </div>

                      {/* ── Optional gallery password ── */}
                      <Show when={authState()}>
                        {(s) => (
                          <div class="flex flex-col gap-1.5">
                            <div class="flex items-center justify-between">
                              <span class="text-[11px] text-neutral-400">Gallery password</span>
                              <Show when={s().password_set}>
                                <span class="text-[10px] text-teal-400">Set</span>
                              </Show>
                            </div>
                            <div class="flex gap-1.5">
                              <input
                                type="password"
                                value={passwordInput()}
                                onInput={(e) => setPasswordInput(e.currentTarget.value)}
                                placeholder={s().password_set ? "Replace password" : "Set password"}
                                class="flex-1 px-2 py-1 bg-neutral-800 border border-neutral-700 rounded text-[11px] text-neutral-200 outline-none focus:border-neutral-500"
                              />
                              <button
                                onClick={handleSetPassword}
                                disabled={!passwordInput()}
                                class="px-2 py-1 text-[11px] rounded bg-neutral-800 hover:bg-neutral-700 text-neutral-300 disabled:opacity-40 cursor-pointer transition-colors"
                              >
                                Save
                              </button>
                              <Show when={s().password_set}>
                                <button
                                  onClick={handleClearPassword}
                                  class="px-2 py-1 text-[11px] rounded text-neutral-500 hover:text-red-400 cursor-pointer"
                                  title="Remove password"
                                >
                                  Clear
                                </button>
                              </Show>
                            </div>
                            <Show when={passwordStatus()}>
                              <span class="text-[10px] text-neutral-500">{passwordStatus()}</span>
                            </Show>
                            <span class="text-[10px] text-neutral-600 leading-snug">
                              When set, paired devices re-enter it after the inactivity window.
                            </span>

                            <div class="flex items-center gap-1.5 mt-1">
                              <span class="text-[10px] text-neutral-500 mr-1">Lock after</span>
                              {INACTIVITY_PRESETS.map((p) => (
                                <button
                                  onClick={() => handleSetInactivity(p.value)}
                                  class={`px-2 py-0.5 text-[10px] rounded cursor-pointer transition-colors ${
                                    s().inactivity_secs === p.value
                                      ? "bg-teal-700/60 text-teal-200"
                                      : "bg-neutral-800 text-neutral-400 hover:bg-neutral-700"
                                  }`}
                                >
                                  {p.label}
                                </button>
                              ))}
                            </div>
                          </div>
                        )}
                      </Show>

                      {/* ── Device uploads ── */}
                      <Show when={uploadCfg()}>
                        {(cfg) => (
                          <div class="flex flex-col gap-1.5">
                            <div class="flex items-center justify-between">
                              <span class="text-[11px] text-neutral-400">Allow uploads from devices</span>
                              <button
                                onClick={() => saveUploadCfg({ ...cfg(), enabled: !cfg().enabled })}
                                class={`relative w-9 h-5 rounded-full transition-colors cursor-pointer ${
                                  cfg().enabled ? "bg-teal-600" : "bg-neutral-700"
                                }`}
                                title="Let paired devices upload photos into this gallery"
                              >
                                <span
                                  class={`absolute top-0.5 w-4 h-4 rounded-full bg-white transition-all ${
                                    cfg().enabled ? "left-[18px]" : "left-0.5"
                                  }`}
                                />
                              </button>
                            </div>
                            {/* ── Remote delete ── */}
                            <Show when={remoteDelete() !== null}>
                              <div class="flex items-center justify-between mt-1">
                                <span class="text-[11px] text-neutral-400">Allow deletes from devices</span>
                                <button
                                  onClick={() => saveRemoteDelete(!remoteDelete())}
                                  class={`relative w-9 h-5 rounded-full transition-colors cursor-pointer ${
                                    remoteDelete() ? "bg-teal-600" : "bg-neutral-700"
                                  }`}
                                  title="Let paired devices move photos to the gallery trash (restorable; auto-purged after the retention period)"
                                >
                                  <span
                                    class={`absolute top-0.5 w-4 h-4 rounded-full bg-white transition-all ${
                                      remoteDelete() ? "left-[18px]" : "left-0.5"
                                    }`}
                                  />
                                </button>
                              </div>
                            </Show>
                            <Show when={cfg().enabled}>
                              <div class="flex items-center gap-2 mt-0.5">
                                <span class="text-[10px] text-neutral-500">Organize into</span>
                                <select
                                  value={cfg().scheme}
                                  onChange={(e) =>
                                    saveUploadCfg({ ...cfg(), scheme: e.currentTarget.value as UploadScheme })
                                  }
                                  class="flex-1 px-2 py-1 text-[11px] rounded bg-neutral-800 text-neutral-300 border border-neutral-700 cursor-pointer focus:outline-none focus:border-teal-600"
                                >
                                  <For each={UPLOAD_SCHEMES}>
                                    {(s) => <option value={s.value}>{s.label}</option>}
                                  </For>
                                </select>
                              </div>
                              <span class="text-[10px] text-neutral-600 leading-snug">
                                Uploads land in <span class="font-mono">Uploads/</span> under the gallery, foldered by capture date.
                              </span>
                            </Show>
                          </div>
                        )}
                      </Show>

                      {/* ── Paired devices ── */}
                      <Show when={authState() && authState()!.devices.length > 0}>
                        <div class="flex flex-col gap-1">
                          <span class="text-[11px] text-neutral-400">Paired devices</span>
                          <div class="flex flex-col gap-1">
                            <For each={authState()!.devices}>
                              {(d) => (
                                <div class="flex items-center gap-2 px-2 py-1 rounded bg-neutral-900/60">
                                  <div class="flex-1 min-w-0">
                                    <div class={`text-xs truncate ${d.revoked_at ? "text-neutral-500 line-through" : "text-neutral-300"}`}>
                                      {d.name}
                                    </div>
                                    <div class="text-[10px] text-neutral-600">
                                      seen {formatRelative(d.last_seen)}
                                    </div>
                                  </div>
                                  <Show
                                    when={!d.revoked_at}
                                    fallback={
                                      <button
                                        onClick={() => handleDeleteDevice(d.id)}
                                        class="text-[10px] text-neutral-500 hover:text-red-400 cursor-pointer"
                                      >
                                        Delete
                                      </button>
                                    }
                                  >
                                    <button
                                      onClick={() => handleRevokeDevice(d.id)}
                                      class="text-[10px] text-neutral-500 hover:text-red-400 cursor-pointer"
                                    >
                                      Revoke
                                    </button>
                                  </Show>
                                </div>
                              )}
                            </For>
                          </div>
                        </div>
                      </Show>

                      {/* Reachability indicator + firewall hint, same as before. */}
                      <div class="flex items-start gap-1.5">
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
                            Reachable &mdash; a device has connected.
                          </span>
                        </Show>
                      </div>
                      <Show when={info().firewall_hint && info().clients_seen === 0}>
                        <pre class="px-2 py-1.5 rounded bg-neutral-900/80 border border-neutral-800 text-[10px] text-neutral-400 whitespace-pre-wrap break-all font-mono">
                          {info().firewall_hint}
                        </pre>
                      </Show>
                    </div>
                  )}
                </Show>
              </Section>
            </Show>

            {/* ── Thumbnails ── */}
            <Section label="Thumbnails" order={6}>
              <button
                onClick={handlePrecacheAll}
                class="px-3 py-1.5 text-xs rounded cursor-pointer transition-colors bg-neutral-800 text-neutral-400 hover:bg-neutral-700 hover:text-neutral-300"
              >
                {precachingAll() ? "Cancel Generation" : "Generate Missing Thumbnails"}
              </button>
              <p class="text-[10px] text-neutral-500 -mt-1 pl-0.5">
                Pre-generates every thumbnail size the gallery views use, so
                they don't burst-generate (CPU spikes) while scrolling new
                areas. Safe to cancel and re-run — already-generated
                thumbnails are skipped.
              </p>
              {/* Rebuild + rendering are desktop-only (destructive / local) */}
              <Show when={!isWeb()}>
                <button
                  onClick={handleRebuild}
                  disabled={rebuilding()}
                  class="px-3 py-1.5 text-xs rounded cursor-pointer transition-colors bg-neutral-800 text-neutral-400 hover:bg-neutral-700 hover:text-neutral-300 disabled:opacity-50 disabled:cursor-not-allowed"
                >
                  {rebuilding() ? "Rebuilding..." : "Rebuild All Thumbnails"}
                </button>
                <Toggle
                  label="GPU acceleration (experimental)"
                  checked={gpuOn()}
                  onChange={toggleGpu}
                />
                <p class="text-[10px] text-neutral-500 -mt-1 pl-0.5">
                  Uses the GPU compositing path for much smoother scrolling and
                  faster image display. Disabled by default because it can crash
                  on some drivers. Takes effect after restarting the app.
                </p>
                <Show when={renderRestart()}>
                  <p class="text-[10px] text-amber-400/80 pl-0.5">
                    Restart LightView to apply the new rendering mode.
                  </p>
                </Show>
              </Show>
            </Section>

            {/* ── Storage (desktop only) ── */}
            <Show when={!isWeb()}>
            <Section label="Storage" order={7}>
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
            <Section label="Plugins" order={3}>
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

            {/* ── Remote Tagging (web only): jobs executed by a paired
                   lightview-worker on a capable machine ── */}
            <Show when={isWeb()}>
            <Section label="Remote Tagging" order={3}>
              <Show
                when={taggingWorkers().length > 0}
                fallback={
                  <span class="text-xs text-neutral-600">
                    No tagging worker connected — run <code class="text-neutral-500">lightview-worker</code> on
                    a machine with your plugins installed, or install plugins on the server itself.
                  </span>
                }
              >
                <div class="flex flex-col gap-1">
                  <For each={taggingWorkers()}>
                    {(worker) => (
                      <span class="text-[11px] text-neutral-500">
                        ● {worker.workerName}
                        <Show when={worker.local}>
                          <span class="ml-1 px-1 py-px rounded bg-neutral-800 text-[9px] text-neutral-400 align-middle">server</span>
                        </Show>
                        {worker.busyJobId ? " (tagging...)" : " (idle)"}
                      </span>
                    )}
                  </For>
                </div>
                <div class="flex flex-col gap-2">
                  {/* One row per (plugin, place-to-run-it): plugins offered by
                      several workers get a pinned row each ("on <worker>"). */}
                  <For each={taggingActions()}>
                    {(action) => (
                      <div class="flex items-center justify-between gap-2 px-2 py-1.5 rounded bg-neutral-800/50">
                        <div class="flex flex-col min-w-0">
                          <span class="text-xs text-neutral-300 truncate">
                            {action.plugin.display_name}
                            <Show when={action.where}>
                              <span class="text-neutral-500"> {action.where}</span>
                            </Show>
                          </span>
                          <span class="text-[10px] text-neutral-500 truncate">{action.plugin.description}</span>
                        </div>
                        <button
                          onClick={() => handleTagAllUntagged(action.plugin.name, action.plugin.tag_prefix, action.workerId)}
                          class="shrink-0 px-2 py-1 text-[10px] rounded cursor-pointer transition-colors bg-neutral-700 text-neutral-300 hover:bg-neutral-600 hover:text-neutral-100"
                        >
                          Tag All Untagged
                        </button>
                      </div>
                    )}
                  </For>
                </div>
              </Show>
              <Show when={recentTaggingJobs().length > 0}>
                <div class="flex flex-col gap-1">
                  <For each={recentTaggingJobs()}>
                    {(job) => (
                      <div class="flex items-center gap-2 px-2 py-1 rounded bg-neutral-800/30">
                        <span class={`text-[10px] ${jobStateColor(job)}`}>{job.state}</span>
                        <span class="text-[11px] text-neutral-400 truncate flex-1">
                          {job.displayName}
                          {job.total > 0 ? ` — ${job.completed + job.failed}/${job.total}` : ""}
                          {job.failed > 0 ? ` (${job.failed} failed)` : ""}
                        </span>
                        <Show when={job.state === "queued" || job.state === "running"}>
                          <button
                            onClick={() => cancelTaggingJob(job.id).catch(() => {})}
                            class="shrink-0 px-1.5 py-0.5 text-[10px] rounded cursor-pointer bg-neutral-700 text-neutral-400 hover:bg-neutral-600 hover:text-neutral-200"
                          >
                            Cancel
                          </button>
                        </Show>
                      </div>
                    )}
                  </For>
                </div>
              </Show>
            </Section>
            </Show>

            {/* ── Deduplication ── */}
            <Show when={props.onOpenDuplicates}>
            <Section label="Deduplication" order={1}>
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

            {/* ── Trash (on web only when the host allows remote delete) ── */}
            <Show when={props.onOpenTrash && capabilities().delete}>
            <Section label="Trash" order={1}>
              <button
                onClick={() => {
                  setOpen(false);
                  props.onOpenTrash?.();
                }}
                class="px-3 py-1.5 text-xs rounded cursor-pointer transition-colors bg-neutral-800 text-neutral-400 hover:bg-neutral-700 hover:text-neutral-300"
              >
                View Trash...
              </button>
            </Section>
            </Show>

            {/* ── Gallery ── */}
            <Show when={props.onOpenFolder && !isWeb()}>
              <div class="border-t border-neutral-800/60 pt-3" style={{ order: 8 }}>
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
        </Dynamic>
      </Show>
    </div>
  );
}

// ── Helpers ──

/** Renders children where they sit (no portal). Paired with `Dynamic` so the
 *  desktop settings dropdown stays in place while the mobile page portals out. */
function InPlace(props: { children: any }) {
  return props.children;
}

function Section(props: { label: string; children: any; order?: number }) {
  // `order` controls visual position within the flex-column settings panel
  // independently of source order, so frequently-used sections (Deduplication,
  // Remote Access, Plugins) can sit at the top while their JSX stays put.
  return (
    <div
      class="flex flex-col gap-2.5"
      style={props.order != null ? { order: props.order } : undefined}
    >
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
