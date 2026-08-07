// The rows both debug surfaces show, in order.
//
// The in-app overlay reads the live ring buffers; the separate devtools window
// renders `MetricSnapshot` payloads instead. Same metrics, same labels, same
// colors, same formatting — so the list lives here once rather than as two
// hand-maintained walls of near-identical JSX that drifted apart (the devtools
// window was already missing the image-load rows).

import { inFlightImageLoads, type MetricName } from "../../lib/perfMonitor";

export interface MetricRowSpec {
  /** Key into the metric registry / snapshot. */
  name: MetricName;
  label: string;
  color: string;
  /** Render the latest sample. */
  format: (last: number) => string;
  /** Hide the row entirely until the metric has produced a sample. Used for
   *  `jsHeapUsed`, which only exists on engines exposing `performance.memory`. */
  hideWhenEmpty?: boolean;
}

const int = (n: number) => Math.round(n).toString();
const rate = (n: number) => `${int(n)}/s`;
const ms = (n: number) => `${n.toFixed(1)} ms`;
const bytes = (kb: number) =>
  kb < 1024 ? `${kb.toFixed(1)} KB/s` : `${(kb / 1024).toFixed(2)} MB/s`;

/** Image-load latency reports NaN when nothing completed in the window (see
 *  `sampleTick`). Distinguish the two reasons for that: nothing was asked for,
 *  versus requests are outstanding and not landing — the stall this metric used
 *  to report as a flat 0 ms. */
const loadLatency = (last: number) => {
  if (Number.isFinite(last)) return ms(last);
  const pending = inFlightImageLoads();
  return pending > 0 ? `stalled (${pending} in flight)` : "idle";
};

export const METRIC_ROWS: readonly MetricRowSpec[] = [
  { name: "fps", label: "FPS", color: "#4ade80", format: int },
  { name: "ipcBandwidthIn", label: "IPC In", color: "#60a5fa", format: bytes },
  { name: "ipcBandwidthOut", label: "IPC Out", color: "#a78bfa", format: bytes },
  { name: "ipcCallRate", label: "IPC Calls", color: "#fbbf24", format: rate },
  { name: "ipcAvgLatency", label: "IPC Latency", color: "#f97316", format: ms },
  { name: "cachedImages", label: "Viewer Cache", color: "#14b8a6", format: int },
  { name: "cacheMisses", label: "Cache Misses", color: "#f43f5e", format: rate },
  { name: "imageLoadLatency", label: "Img Load", color: "#a3e635", format: loadLatency },
  { name: "imageLoadRate", label: "Img Load Rate", color: "#84cc16", format: rate },
  { name: "diskReadRate", label: "Disk Read", color: "#38bdf8", format: bytes },
  { name: "diskWriteRate", label: "Disk Write", color: "#818cf8", format: bytes },
  { name: "domNodes", label: "DOM Nodes", color: "#e879f9", format: int },
  {
    name: "jsHeapUsed",
    label: "JS Heap",
    color: "#fb923c",
    format: (n) => `${n.toFixed(1)} MB`,
    hideWhenEmpty: true,
  },
];
