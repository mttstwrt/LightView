import { For, Show, createSignal, onMount, onCleanup } from "solid-js";
import { getDebugInfo, getPerfSnapshot, type DebugInfo } from "../../lib/ipc";
import {
  startMonitoring,
  stopMonitoring,
  setPerfSnapshotFetcher,
  metrics,
} from "../../lib/perfMonitor";
import { METRIC_ROWS } from "./metricRows";
import { Sparkline } from "./Sparkline";

type Tab = "hardware" | "performance";

export function DebugOverlay() {
  const [tab, setTab] = createSignal<Tab>("performance");
  const [hwInfo, setHwInfo] = createSignal<DebugInfo | null>(null);

  // Tick counter that drives reactive re-reads of the ring buffers.
  // Incremented every second so sparklines and labels update.
  const [tick, setTick] = createSignal(0);
  let tickInterval: ReturnType<typeof setInterval> | null = null;

  onMount(() => {
    // Wire the perf monitor to our IPC fetcher
    setPerfSnapshotFetcher(getPerfSnapshot);
    startMonitoring();

    tickInterval = setInterval(() => setTick((t) => t + 1), 1000);
  });

  onCleanup(() => {
    stopMonitoring();
    if (tickInterval !== null) clearInterval(tickInterval);
  });

  const loadHw = async () => {
    try {
      setHwInfo(await getDebugInfo());
    } catch (e) {
      console.error("Debug info failed:", e);
    }
  };

  // Lazy-load hardware info only when tab switches to it
  const switchTab = (t: Tab) => {
    setTab(t);
    if (t === "hardware" && !hwInfo()) loadHw();
  };

  // -------------------------------------------------------------------
  // Shared styles
  // -------------------------------------------------------------------

  const overlayStyle = {
    background: "rgba(0,0,0,0.9)",
    "backdrop-filter": "blur(8px)",
    border: "1px solid rgba(255,255,255,0.1)",
  };

  const tabBtnClass = (t: Tab) =>
    `px-2 py-0.5 text-[10px] rounded cursor-pointer transition-colors ${
      tab() === t
        ? "bg-neutral-700 text-neutral-200"
        : "text-neutral-500 hover:text-neutral-300"
    }`;

  return (
    <div
      class="fixed bottom-4 left-4 z-[100] rounded text-xs font-mono text-neutral-300"
      style={{ ...overlayStyle, width: "340px" }}
    >
      {/* Tab bar */}
      <div class="flex items-center gap-1 px-3 pt-2 pb-1 border-b border-neutral-800">
        <span class="text-neutral-600 text-[10px] mr-auto">Debug</span>
        <button class={tabBtnClass("performance")} onClick={() => switchTab("performance")}>
          Perf
        </button>
        <button class={tabBtnClass("hardware")} onClick={() => switchTab("hardware")}>
          Hardware
        </button>
      </div>

      {/* Performance tab */}
      <Show when={tab() === "performance"}>
        <div class="px-3 py-2 space-y-2 max-h-[70vh] overflow-y-auto">
          <For each={METRIC_ROWS}>
            {(row) => {
              const buffer = metrics[row.name];
              // `tick()` is read inside each accessor so the whole block
              // re-renders once a second; the ring buffers themselves aren't
              // reactive.
              return (
                <Show when={!row.hideWhenEmpty || (tick(), buffer.count > 0)}>
                  <MetricRow
                    label={row.label}
                    value={() => { void tick(); return row.format(buffer.last); }}
                    data={() => { void tick(); return buffer.toArray(); }}
                    color={row.color}
                  />
                </Show>
              );
            }}
          </For>
        </div>
      </Show>

      {/* Hardware tab */}
      <Show when={tab() === "hardware"}>
        <div class="px-3 py-2">
          <Show when={hwInfo()} fallback={<div class="text-neutral-500 py-2">Loading...</div>}>
            <div class="space-y-0.5">
              <div>Storage: <span class="text-neutral-100">{hwInfo()!.storage_type}</span></div>
              <div>Filesystem: <span class="text-neutral-100">{hwInfo()!.filesystem}</span></div>
              <div>CPU: <span class="text-neutral-100">{hwInfo()!.cpu_cores} cores</span></div>
              <div>RAM: <span class="text-neutral-100">{hwInfo()!.total_ram_mb} MB</span></div>
              <div>GPU resize: <span class={hwInfo()!.gpu_resize_active ? "text-green-400" : "text-neutral-500"}>{hwInfo()!.gpu_resize_active ? "active" : "inactive"}</span></div>
              <div>SQLite thumbs: <span class="text-neutral-100">{hwInfo()!.sqlite_thumbnail_count}</span></div>
              <div>Thumb threads: <span class="text-neutral-100">{hwInfo()!.thumbnail_threads}</span></div>
              <div>Prefetch: <span class="text-neutral-100">{hwInfo()!.prefetch_count}</span></div>
              <div>LRU cache: <span class="text-neutral-100">{hwInfo()!.lru_cache_size}</span></div>
              <div>Reflink: <span class={hwInfo()!.supports_reflink ? "text-green-400" : "text-neutral-500"}>{hwInfo()!.supports_reflink ? "yes" : "no"}</span></div>
              <div>GDK_BACKEND: <span class="text-neutral-100">{hwInfo()!.gdk_backend || "unset"}</span></div>
              <div>DMABUF disabled: <span class={hwInfo()!.webkit_disable_dmabuf ? "text-yellow-400" : "text-neutral-500"}>{hwInfo()!.webkit_disable_dmabuf ? "yes" : "no"}</span></div>
            </div>
            <button class="mt-2 text-neutral-500 hover:text-neutral-300 cursor-pointer" onClick={loadHw}>refresh</button>
          </Show>
        </div>
      </Show>
    </div>
  );
}

// ---------------------------------------------------------------------------
// MetricRow — single metric with label, value, and sparkline
// ---------------------------------------------------------------------------

function MetricRow(props: {
  label: string;
  value: () => string;
  data: () => number[];
  color: string;
}) {
  return (
    <div class="flex items-center gap-2">
      <div class="w-[80px] text-neutral-500 text-[10px] truncate shrink-0">{props.label}</div>
      <div class="flex-1 min-w-0">
        <Sparkline data={props.data} color={props.color} width={140} height={24} fill />
      </div>
      <div class="w-[72px] text-right text-[10px] tabular-nums shrink-0" style={{ color: props.color }}>
        {props.value()}
      </div>
    </div>
  );
}
