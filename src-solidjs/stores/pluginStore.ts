import { createSignal } from "solid-js";

export type PluginStatus = "running" | "done" | "error" | "cancelled";

export interface PluginActivity {
  pluginName: string;
  displayName: string;
  status: PluginStatus;
  message: string;
  completed: number;
  total: number;
}

const [pluginActivity, setPluginActivity] = createSignal<PluginActivity | null>(null);

export { pluginActivity };

let clearTimer: ReturnType<typeof setTimeout> | undefined;

export function pluginStarted(pluginName: string, displayName: string, total: number) {
  if (clearTimer) clearTimeout(clearTimer);
  setPluginActivity({
    pluginName,
    displayName,
    status: "running",
    message: `0 / ${total}`,
    completed: 0,
    total,
  });
}

export function pluginProgress(completed: number, total: number) {
  setPluginActivity((prev) =>
    prev ? { ...prev, completed, total, message: `${completed} / ${total}` } : null,
  );
}

export function pluginFinished(message: string) {
  setPluginActivity((prev) => prev ? { ...prev, status: "done", message } : null);
  clearTimer = setTimeout(() => setPluginActivity(null), 4000);
}

export function pluginFailed(message: string) {
  setPluginActivity((prev) => prev ? { ...prev, status: "error", message } : null);
  clearTimer = setTimeout(() => setPluginActivity(null), 5000);
}

export function pluginCancelled() {
  setPluginActivity((prev) => {
    if (!prev) return null;
    const msg = `Cancelled (${prev.completed} / ${prev.total} done)`;
    return { ...prev, status: "cancelled", message: msg };
  });
  clearTimer = setTimeout(() => setPluginActivity(null), 4000);
}
