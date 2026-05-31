import { createSignal, createEffect, Show, For, onCleanup, onMount } from "solid-js";
import { settings, setSettings } from "../../stores/settingsStore";
import { displayPaths } from "../../stores/galleryStore";
import { viewerOpen } from "../../stores/viewerStore";
import type { AppSettings, CompanionLocation, RendererMode, PluginInfo } from "../../lib/types";
import {
  rebuildThumbnails,
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
  type RemoteAuthState,
  type PairingCode,
} from "../../lib/ipc";
import QRCode from "qrcode";
import { pluginStarted, pluginProgress, pluginFinished, pluginFailed, pluginCancelled } from "../../stores/pluginStore";
import { safeListen as listen } from "../../lib/runtime";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { isWeb, isMobile } from "../../lib/runtime";

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

/** Compute a thumbnail_size in CSS px that lands the grid on `targetCols`
 *  columns, given the current viewport width and grid gap. Mirrors the math
 *  in GalleryGrid's ctrl+wheel resize. */
function thumbSizeForCols(targetCols: number, gap: number): number {
  const w = typeof window !== "undefined" ? window.innerWidth : 400;
  // The grid uses `cols = floor((w + g) / (size + g))`. We pick `size` in the
  // middle of the bucket that yields targetCols so a small viewport change
  // doesn't flip the column count.
  const upper = (w + gap) / targetCols - gap;
  const lower = (w + gap) / (targetCols + 1) - gap;
  return Math.max(20, Math.round((upper + lower) / 2));
}

/** Reverse-derive the current effective column count from a saved
 *  thumbnail_size so the matching preset stays highlighted. */
function currentColCount(thumbSize: number, gap: number): number {
  const w = typeof window !== "undefined" ? window.innerWidth : 400;
  return Math.max(1, Math.floor((w + gap) / (thumbSize + gap)));
}

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

  const formatRelative = (ts: number) => {
    if (!ts) return "never";
    const diff = Math.max(0, Date.now() / 1000 - ts);
    if (diff < 60) return "just now";
    if (diff < 3600) return `${Math.floor(diff / 60)}m ago`;
    if (diff < 86400) return `${Math.floor(diff / 3600)}h ago`;
    return `${Math.floor(diff / 86400)}d ago`;
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

      {/* Dropdown panel (desktop) / right-side sheet (mobile) */}
      <Show when={open()}>
        {/* Backdrop — click to close */}
        <div class="fixed inset-0 z-40" onClick={() => setOpen(false)} />

        <div
          class={
            isMobile()
              ? "fixed top-0 right-0 bottom-0 w-[88vw] max-w-sm overflow-hidden shadow-xl z-50 flex flex-col"
              : "absolute top-full right-0 mt-2 w-72 rounded-lg overflow-hidden shadow-xl z-50 flex flex-col max-h-[calc(100vh-5rem)]"
          }
          style={{
            background: "rgba(18, 18, 18, 0.96)",
            "backdrop-filter": "blur(16px)",
            border: "1px solid rgba(255,255,255,0.08)",
          }}
        >
          <div class="px-4 py-3 border-b border-neutral-800/60 flex items-center justify-between shrink-0">
            <span class="text-sm font-medium text-neutral-200">Settings</span>
            <Show when={isMobile()}>
              <button
                onClick={() => setOpen(false)}
                class="w-7 h-7 -mr-1 flex items-center justify-center rounded text-neutral-400 hover:text-neutral-200 hover:bg-neutral-800 cursor-pointer"
                title="Close"
              >
                <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                  <path d="M18 6L6 18M6 6l12 12" />
                </svg>
              </button>
            </Show>
          </div>

          <div
            class="px-4 py-3 flex flex-col gap-4 overflow-y-auto flex-1 min-h-0"
            classList={{
              "hide-scrollbar": isMobile(),
              "dupes-scroll": !isMobile(),
            }}
          >
            {/* ── Display ── */}
            <Section label="Display">
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

            {/* ── Default filter (desktop-managed; LAN clients inherit it) ── */}
            <Show when={!isWeb()}>
            <Section label="Default Filter">
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
