import { Show, createSignal, onMount, onCleanup } from "solid-js";
import { safeListen as listen } from "../../lib/runtime";
import { getDebugInfo, type DebugInfo } from "../../lib/ipc";
import type { MetricSnapshot, MetricEntry } from "../../lib/perfMonitor";
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

  const fmt = (n: number, decimals = 1) => n.toFixed(decimals);
  const fmtInt = (n: number) => Math.round(n).toString();
  const fmtBytes = (kb: number) => {
    if (kb < 1024) return `${kb.toFixed(1)} KB/s`;
    return `${(kb / 1024).toFixed(2)} MB/s`;
  };

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
            <MetricRow label="FPS" value={fmtInt(m("fps").last)} data={() => m("fps").history} color="#4ade80" />
            <MetricRow label="IPC In" value={fmtBytes(m("ipcBandwidthIn").last)} data={() => m("ipcBandwidthIn").history} color="#60a5fa" />
            <MetricRow label="IPC Out" value={fmtBytes(m("ipcBandwidthOut").last)} data={() => m("ipcBandwidthOut").history} color="#a78bfa" />
            <MetricRow label="IPC Calls" value={`${fmtInt(m("ipcCallRate").last)}/s`} data={() => m("ipcCallRate").history} color="#fbbf24" />
            <MetricRow label="IPC Latency" value={`${fmt(m("ipcAvgLatency").last)} ms`} data={() => m("ipcAvgLatency").history} color="#f97316" />
            <MetricRow label="Visible Imgs" value={fmtInt(m("visibleImages").last)} data={() => m("visibleImages").history} color="#2dd4bf" />
            <MetricRow label="Cached Imgs" value={fmtInt(m("cachedImages").last)} data={() => m("cachedImages").history} color="#14b8a6" />
            <MetricRow label="Cache Misses" value={`${fmtInt(m("cacheMisses").last)}/s`} data={() => m("cacheMisses").history} color="#f43f5e" />
            <MetricRow label="Disk Read" value={fmtBytes(m("diskReadRate").last)} data={() => m("diskReadRate").history} color="#38bdf8" />
            <MetricRow label="Disk Write" value={fmtBytes(m("diskWriteRate").last)} data={() => m("diskWriteRate").history} color="#818cf8" />
            <MetricRow label="DOM Nodes" value={fmtInt(m("domNodes").last)} data={() => m("domNodes").history} color="#e879f9" />
            <Show when={m("jsHeapUsed").history.length > 0}>
              <MetricRow label="JS Heap" value={`${fmt(m("jsHeapUsed").last)} MB`} data={() => m("jsHeapUsed").history} color="#fb923c" />
            </Show>
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
