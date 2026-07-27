import { onMount, onCleanup, createEffect } from "solid-js";

interface SparklineProps {
  /** Data samples (oldest first). */
  data: () => number[];
  /** Line/fill color. */
  color: string;
  /** Canvas width in CSS pixels. */
  width?: number;
  /** Canvas height in CSS pixels. */
  height?: number;
  /** If true, fill the area under the line. */
  fill?: boolean;
}

export function Sparkline(props: SparklineProps) {
  let canvas: HTMLCanvasElement | undefined;
  const w = () => props.width ?? 120;
  const h = () => props.height ?? 28;

  const draw = () => {
    if (!canvas) return;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;

    const dpr = window.devicePixelRatio || 1;
    const cw = w();
    const ch = h();

    // Resize canvas backing store if needed
    if (canvas.width !== cw * dpr || canvas.height !== ch * dpr) {
      canvas.width = cw * dpr;
      canvas.height = ch * dpr;
      ctx.scale(dpr, dpr);
    }

    ctx.clearRect(0, 0, cw, ch);

    const samples = props.data();
    if (samples.length < 2) return;

    // Auto-scale Y axis. NaN samples mean "no measurement in this window" (see
    // perfMonitor's image-load latency) — they must not scale the axis or be
    // drawn as zero, or a stall reads as a fast, flat line.
    let max = 0;
    for (let i = 0; i < samples.length; i++) {
      if (Number.isFinite(samples[i]) && samples[i] > max) max = samples[i];
    }
    if (max === 0) max = 1;

    const pad = 1;
    const plotH = ch - pad * 2;
    const step = (cw - pad * 2) / (samples.length - 1);
    const x = (i: number) => pad + i * step;
    const y = (v: number) => ch - pad - (v / max) * plotH;

    // Trace each run of finite samples as its own subpath so gaps stay gaps.
    // Filling is per-run too: a single closed path spanning a gap would shade
    // the empty region as if it held data.
    const runs: number[][] = [];
    let run: number[] = [];
    for (let i = 0; i < samples.length; i++) {
      if (Number.isFinite(samples[i])) {
        run.push(i);
      } else if (run.length) {
        runs.push(run);
        run = [];
      }
    }
    if (run.length) runs.push(run);

    if (props.fill) {
      ctx.fillStyle = props.color + "30"; // 30 = ~19% alpha
      for (const r of runs) {
        if (r.length < 2) continue;
        ctx.beginPath();
        ctx.moveTo(x(r[0]), y(samples[r[0]]));
        for (const i of r.slice(1)) ctx.lineTo(x(i), y(samples[i]));
        ctx.lineTo(x(r[r.length - 1]), ch - pad);
        ctx.lineTo(x(r[0]), ch - pad);
        ctx.closePath();
        ctx.fill();
      }
    }

    ctx.strokeStyle = props.color;
    ctx.lineWidth = 1.2;
    ctx.beginPath();
    for (const r of runs) {
      // A lone sample between two gaps has no segment to stroke; dot it so an
      // isolated measurement doesn't vanish entirely.
      if (r.length === 1) {
        const i = r[0];
        ctx.moveTo(x(i) - 0.6, y(samples[i]));
        ctx.lineTo(x(i) + 0.6, y(samples[i]));
        continue;
      }
      ctx.moveTo(x(r[0]), y(samples[r[0]]));
      for (const i of r.slice(1)) ctx.lineTo(x(i), y(samples[i]));
    }
    ctx.stroke();
  };

  createEffect(draw);

  return (
    <canvas
      ref={canvas}
      style={{
        width: `${w()}px`,
        height: `${h()}px`,
        display: "block",
      }}
    />
  );
}
