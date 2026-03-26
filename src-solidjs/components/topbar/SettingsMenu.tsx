import { createSignal, Show, For, onCleanup, onMount } from "solid-js";
import { settings, setSettings } from "../../stores/settingsStore";
import { displayPaths } from "../../stores/galleryStore";
import type { AppSettings, ResizeFilter, CompanionLocation, PluginInfo } from "../../lib/types";
import { rebuildThumbnails, getThumbnailSettings, updateThumbnailSettings, listPlugins, installPlugin, runPluginBatch } from "../../lib/ipc";
import { pluginStarted, pluginFinished, pluginFailed } from "../../stores/pluginStore";
import type { ThumbnailSettings, ThumbFormat } from "../../lib/ipc";
import { open as openDialog } from "@tauri-apps/plugin-dialog";

const THUMB_PRESETS = [
  { label: "S", value: 120 },
  { label: "M", value: 200 },
  { label: "L", value: 300 },
  { label: "XL", value: 400 },
] as const;

const THUMB_DIM_PRESETS = [
  { label: "256", value: 256 },
  { label: "400", value: 400 },
  { label: "512", value: 512 },
  { label: "800", value: 800 },
] as const;

const GAP_PRESETS = [
  { label: "None", value: 0 },
  { label: "Tight", value: 2 },
  { label: "Normal", value: 4 },
  { label: "Wide", value: 8 },
] as const;

export function SettingsMenu(props: { onOpenFolder?: () => void }) {
  const [open, setOpen] = createSignal(false);

  const toggle = () => setOpen((v) => !v);

  // Close on Escape
  const handleKey = (e: KeyboardEvent) => {
    if (e.key === "Escape" && open()) {
      e.stopPropagation();
      setOpen(false);
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

  const [thumbSettings, setThumbSettings] = createSignal<ThumbnailSettings | null>(null);

  onMount(async () => {
    try {
      const ts = await getThumbnailSettings();
      setThumbSettings(ts);
    } catch {}
  });

  const updateThumb = async (patch: Partial<ThumbnailSettings>) => {
    const current = thumbSettings();
    if (!current) return;
    const updated = { ...current, ...patch };
    setThumbSettings(updated);
    try {
      await updateThumbnailSettings(updated);
      // Invalidate cached thumbnails so the grid re-fetches in the new format/dimensions
      window.dispatchEvent(new CustomEvent("lightview:thumbnails-invalidated"));
    } catch (e) {
      console.error("Failed to update thumbnail settings:", e);
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
    pluginStarted(pluginName, displayName, `Running on ${paths.length} photos...`);
    try {
      const results = await runPluginBatch(pluginName, paths, "tag");
      const succeeded = results.filter((r) => r.success).length;
      const failed = results.length - succeeded;
      const msg = failed > 0 ? `Done: ${succeeded} tagged, ${failed} failed` : `Tagged ${succeeded} photos`;
      setPluginStatus(msg);
      if (failed > 0) {
        pluginFailed(msg);
      } else {
        pluginFinished(msg);
      }
    } catch (e) {
      console.error("Plugin run failed:", e);
      setPluginStatus("Run failed");
      pluginFailed("Run failed");
    } finally {
      setPluginRunning(null);
      setTimeout(() => setPluginStatus(""), 5000);
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

          <div class="px-4 py-3 flex flex-col gap-4 max-h-[70vh] overflow-y-auto">
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
            </Section>

            {/* ── Thumbnails ── */}
            <Show when={thumbSettings()}>
              <Section label="Thumbnails">
                <Field label="Dimensions">
                  <div class="flex gap-1">
                    {THUMB_DIM_PRESETS.map((p) => (
                      <button
                        class={`px-2 py-0.5 text-xs rounded cursor-pointer transition-colors ${
                          thumbSettings()!.width === p.value && thumbSettings()!.height === p.value
                            ? "bg-teal-700/60 text-teal-200"
                            : "bg-neutral-800 text-neutral-400 hover:bg-neutral-700 hover:text-neutral-300"
                        }`}
                        onClick={() => updateThumb({ width: p.value, height: p.value })}
                      >
                        {p.label}
                      </button>
                    ))}
                  </div>
                </Field>

                <Field label="Format">
                  <div class="flex gap-1">
                    {([
                      { value: "rgba" as ThumbFormat, label: "RGBA", hint: "Fast" },
                      { value: "jpeg" as ThumbFormat, label: "JPEG", hint: "Compact" },
                    ]).map((f) => (
                      <button
                        class={`px-2 py-0.5 text-xs rounded cursor-pointer transition-colors ${
                          thumbSettings()!.format === f.value
                            ? "bg-teal-700/60 text-teal-200"
                            : "bg-neutral-800 text-neutral-400 hover:bg-neutral-700 hover:text-neutral-300"
                        }`}
                        onClick={() => updateThumb({ format: f.value })}
                        title={f.hint}
                      >
                        {f.label}
                      </button>
                    ))}
                  </div>
                </Field>

                <Field label="Resize algorithm">
                  <div class="flex gap-1">
                    {(["nearest", "bilinear", "lanczos3"] as const).map((f) => (
                      <button
                        class={`px-2 py-0.5 text-xs rounded cursor-pointer transition-colors ${
                          thumbSettings()!.resize_filter === f
                            ? "bg-teal-700/60 text-teal-200"
                            : "bg-neutral-800 text-neutral-400 hover:bg-neutral-700 hover:text-neutral-300"
                        }`}
                        onClick={() => {
                          updatePerformance("thumbnail_resize_filter", f);
                          updateThumb({ resize_filter: f });
                        }}
                      >
                        {f.charAt(0).toUpperCase() + f.slice(1)}
                      </button>
                    ))}
                  </div>
                </Field>

                <button
                  onClick={handleRebuild}
                  disabled={rebuilding()}
                  class="px-3 py-1.5 text-xs rounded cursor-pointer transition-colors bg-neutral-800 text-neutral-400 hover:bg-neutral-700 hover:text-neutral-300 disabled:opacity-50 disabled:cursor-not-allowed"
                >
                  {rebuilding() ? "Rebuilding..." : "Rebuild All Thumbnails"}
                </button>
              </Section>
            </Show>

            {/* ── Storage ── */}
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

            {/* ── Plugins ── */}
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

            {/* ── Gallery ── */}
            <Show when={props.onOpenFolder}>
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
