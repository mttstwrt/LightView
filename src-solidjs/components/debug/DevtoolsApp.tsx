// The same metrics as DebugOverlay, in a separate devtools window.
//
// A separate window cannot reach the main window's ring buffers, so it receives
// flattened MetricSnapshot messages instead of reading the live buffers. Row
// definitions are shared via metricRows.ts.

import { For, Show, createSignal, onMount, onCleanup } from "solid-js";
import { safeListen as listen } from "../../lib/runtime";
import { getDebugInfo, type DebugInfo } from "../../lib/ipc";
import type { MetricSnapshot, MetricEntry } from "../../lib/perfMonitor";
import { METRIC_ROWS } from "./metricRows";
import { Sparkline } from "./Sparkline";

type Tab = "performance" | "hardware";

const EMPTY: MetricEntry = { history: [], last: 0 };

export function DevtoolsApp() {
  const [tab, setTab] = createSignal<Tab>("performance");
  const [metrics, setMetrics] = createSignal<MetricSnapshot | null>(null);
  const [hwInfo, setHwInfo] = createSignal<DebugInfo | null>(null);

  onMount(() => {
    const unlisten = listen<MetricSnapshot>("devtools:metrics", (event) => {
      setMetrics(event.payload);
    });

    onCleanup(() => {
      unlisten.then((fn) => fn());
    });
  });

  const loadHw = async () => {
    try {
      setHwInfo(await getDebugInfo());
    } catch (e) {
      console.error("Debug info failed:", e);
    }
  };

  const switchTab = (t: Tab) => {
    setTab(t);
    if (t === "hardware" && !hwInfo()) loadHw();
  };

  const m = (key: keyof MetricSnapshot): MetricEntry => metrics()?.[key] ?? EMPTY;

  const tabBtnClass = (t: Tab) =>
    `px-3 py-1 text-xs rounded cursor-pointer transition-colors ${
      tab() === t
        ? "bg-neutral-700 text-neutral-200"
        : "text-neutral-500 hover:text-neutral-300"
    }`;

  return (
    <div class="h-screen w-screen bg-neutral-950 text-neutral-300 font-mono text-xs flex flex-col select-none">
      {/* Tab bar */}
      <div class="flex items-center gap-2 px-4 py-2 border-b border-neutral-800 shrink-0" data-tauri-drag-region>
        <span class="text-neutral-600 text-xs mr-auto">DevTools</span>
        <button class={tabBtnClass("performance")} onClick={() => switchTab("performance")}>
          Performance
        </button>
        <button class={tabBtnClass("hardware")} onClick={() => switchTab("hardware")}>
          Hardware
        </button>
      </div>

      {/* Performance tab */}
      <Show when={tab() === "performance"}>
        <div class="flex-1 overflow-y-auto px-4 py-3 space-y-2">
          <Show when={metrics()} fallback={
            <div class="text-neutral-500 py-8 text-center">Waiting for metrics...</div>
          }>
            <For each={METRIC_ROWS}>
              {(row) => (
                <Show when={!row.hideWhenEmpty || m(row.name).history.length > 0}>
                  <MetricRow
                    label={row.label}
                    value={row.format(m(row.name).last)}
                    data={() => m(row.name).history}
                    color={row.color}
                  />
                </Show>
              )}
            </For>
          </Show>
        </div>
      </Show>

      {/* Hardware tab */}
      <Show when={tab() === "hardware"}>
        <div class="flex-1 overflow-y-auto px-4 py-3">
          <Show when={hwInfo()} fallback={<div class="text-neutral-500 py-2">Loading...</div>}>
            <div class="space-y-1">
              <HwRow label="Storage" value={hwInfo()!.storage_type} />
              <HwRow label="Filesystem" value={hwInfo()!.filesystem} />
              <HwRow label="CPU" value={`${hwInfo()!.cpu_cores} cores`} />
              <HwRow label="RAM" value={`${hwInfo()!.total_ram_mb} MB`} />
              <HwRow label="GPU resize" value={hwInfo()!.gpu_resize_active ? "active" : "inactive"} color={hwInfo()!.gpu_resize_active ? "#4ade80" : "#737373"} />
              <HwRow label="SQLite thumbs" value={hwInfo()!.sqlite_thumbnail_count.toString()} />
              <HwRow label="Thumb threads" value={hwInfo()!.thumbnail_threads.toString()} />
              <HwRow label="Prefetch" value={hwInfo()!.prefetch_count.toString()} />
              <HwRow label="LRU cache" value={hwInfo()!.lru_cache_size.toString()} />
              <HwRow label="Reflink" value={hwInfo()!.supports_reflink ? "yes" : "no"} color={hwInfo()!.supports_reflink ? "#4ade80" : "#737373"} />
            </div>
            <button class="mt-3 text-neutral-500 hover:text-neutral-300 cursor-pointer text-xs" onClick={loadHw}>
              Refresh
            </button>
          </Show>
        </div>
      </Show>
    </div>
  );
}

function MetricRow(props: {
  label: string;
  value: string;
  data: () => number[];
  color: string;
}) {
  return (
    <div class="flex items-center gap-3">
      <div class="w-[90px] text-neutral-500 text-[11px] truncate shrink-0">{props.label}</div>
      <div class="flex-1 min-w-0">
        <Sparkline data={props.data} color={props.color} width={200} height={28} fill />
      </div>
      <div class="w-[80px] text-right text-[11px] tabular-nums shrink-0" style={{ color: props.color }}>
        {props.value}
      </div>
    </div>
  );
}

function HwRow(props: { label: string; value: string; color?: string }) {
  return (
    <div class="flex items-center gap-2 py-0.5">
      <span class="text-neutral-500 w-[120px] shrink-0">{props.label}</span>
      <span style={{ color: props.color ?? "#e5e5e5" }}>{props.value}</span>
    </div>
  );
}
