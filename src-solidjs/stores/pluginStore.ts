import { createSignal } from "solid-js";

export type PluginStatus = "running" | "done" | "error";

export interface PluginActivity {
  pluginName: string;
  displayName: string;
  status: PluginStatus;
  message: string;
}

const [pluginActivity, setPluginActivity] = createSignal<PluginActivity | null>(null);

export { pluginActivity };

let clearTimer: ReturnType<typeof setTimeout> | undefined;

export function pluginStarted(pluginName: string, displayName: string, message: string) {
  if (clearTimer) clearTimeout(clearTimer);
  setPluginActivity({ pluginName, displayName, status: "running", message });
}

export function pluginFinished(message: string) {
  setPluginActivity((prev) => prev ? { ...prev, status: "done", message } : null);
  clearTimer = setTimeout(() => setPluginActivity(null), 4000);
}

export function pluginFailed(message: string) {
  setPluginActivity((prev) => prev ? { ...prev, status: "error", message } : null);
  clearTimer = setTimeout(() => setPluginActivity(null), 5000);
}
