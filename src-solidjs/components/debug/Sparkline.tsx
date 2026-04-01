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

    // Auto-scale Y axis
    let max = 0;
    for (let i = 0; i < samples.length; i++) {
      if (samples[i] > max) max = samples[i];
    }
    if (max === 0) max = 1;

    const pad = 1;
    const plotH = ch - pad * 2;
    const step = (cw - pad * 2) / (samples.length - 1);

    ctx.beginPath();
    ctx.moveTo(pad, ch - pad - (samples[0] / max) * plotH);
    for (let i = 1; i < samples.length; i++) {
      ctx.lineTo(pad + i * step, ch - pad - (samples[i] / max) * plotH);
    }

    if (props.fill) {
      ctx.lineTo(pad + (samples.length - 1) * step, ch - pad);
      ctx.lineTo(pad, ch - pad);
      ctx.closePath();
      ctx.fillStyle = props.color + "30"; // 30 = ~19% alpha
      ctx.fill();
      // Re-draw the line on top
      ctx.beginPath();
      ctx.moveTo(pad, ch - pad - (samples[0] / max) * plotH);
      for (let i = 1; i < samples.length; i++) {
        ctx.lineTo(pad + i * step, ch - pad - (samples[i] / max) * plotH);
      }
    }

    ctx.strokeStyle = props.color;
    ctx.lineWidth = 1.2;
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
