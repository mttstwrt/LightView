import { createEffect, onCleanup, type JSX } from "solid-js";

/**
 * Plays a GIF on a `<canvas>` from a backend-rendered frame atlas (sprite sheet
 * + `X-Gif-*` headers), instead of an animated `<img>`.
 *
 * WebKitGTK 2.52 animates `<img>` GIFs several times too fast and leaks a
 * decoded copy of every frame on each loop (multi-GB growth). Blitting static
 * sprite-sheet frames on our own timer sidesteps both: playback runs at the
 * real per-frame delays and we hold exactly one decoded atlas per GIF.
 */

interface GifMeta {
  frameCount: number;
  frameW: number;
  frameH: number;
  cols: number;
  delays: number[];
}

interface GifEntry {
  bitmap: ImageBitmap;
  meta: GifMeta;
  refs: number;
}

// Module-level cache of decoded atlases, ref-counted so a bitmap is never
// closed while another visible cell is still drawing it, and LRU-capped so
// scrolling a large GIF grid can't grow unbounded.
const MAX_CACHED = 48;
const cache = new Map<string, GifEntry | Promise<GifEntry>>();
const lru: string[] = [];

function touch(url: string) {
  const i = lru.indexOf(url);
  if (i >= 0) lru.splice(i, 1);
  lru.push(url);
}

// Last-displayed frame index per GIF, so scrolling a cell out of view and back
// (which tears down and rebuilds its canvas) resumes the animation where it
// left off instead of restarting from frame zero.
const framePositions = new Map<string, number>();

function evict() {
  while (cache.size > MAX_CACHED) {
    const idx = lru.findIndex((u) => {
      const e = cache.get(u);
      return e !== undefined && !(e instanceof Promise) && e.refs === 0;
    });
    if (idx < 0) break;
    const url = lru[idx];
    lru.splice(idx, 1);
    const e = cache.get(url);
    cache.delete(url);
    if (e && !(e instanceof Promise)) e.bitmap.close();
  }
}

async function load(url: string): Promise<GifEntry> {
  const res = await fetch(url);
  if (!res.ok) throw new Error(`gif atlas ${res.status}`);
  const h = res.headers;
  const num = (k: string, d: number) => {
    const v = parseInt(h.get(k) ?? "", 10);
    return Number.isFinite(v) && v > 0 ? v : d;
  };
  const delays = (h.get("X-Gif-Delays") ?? "")
    .split(",")
    .map((s) => parseInt(s, 10))
    .map((n) => (Number.isFinite(n) && n > 0 ? n : 100));
  const meta: GifMeta = {
    frameCount: num("X-Gif-Frame-Count", 1),
    frameW: num("X-Gif-Frame-Width", 1),
    frameH: num("X-Gif-Frame-Height", 1),
    cols: num("X-Gif-Cols", 1),
    delays,
  };
  const bitmap = await createImageBitmap(await res.blob());
  return { bitmap, meta, refs: 0 };
}

async function acquire(url: string): Promise<GifEntry> {
  let e = cache.get(url);
  if (e === undefined) {
    e = load(url);
    cache.set(url, e);
    // Replace the pending promise with the resolved entry; drop on failure.
    e.then(
      (entry) => cache.set(url, entry),
      () => cache.delete(url),
    );
  }
  const entry = await e;
  entry.refs++;
  touch(url);
  return entry;
}

function release(url: string) {
  const e = cache.get(url);
  if (e && !(e instanceof Promise)) {
    e.refs = Math.max(0, e.refs - 1);
    evict();
  }
}

export function GifCanvas(props: {
  url: string;
  class?: string;
  style?: JSX.CSSProperties | string;
  onClick?: (e: MouseEvent) => void;
}) {
  let canvas: HTMLCanvasElement | undefined;

  createEffect(() => {
    const url = props.url;
    const c = canvas;
    if (!url || !c) return;

    let stopped = false;
    let timer: number | undefined;
    let acquiredUrl: string | undefined;

    void (async () => {
      let entry: GifEntry;
      try {
        entry = await acquire(url);
      } catch {
        return;
      }
      if (stopped) {
        release(url);
        return;
      }
      acquiredUrl = url;
      const { bitmap, meta } = entry;
      c.width = meta.frameW;
      c.height = meta.frameH;
      const ctx = c.getContext("2d");
      if (!ctx) return;

      let i = framePositions.get(url) ?? 0;
      if (i >= meta.frameCount) i = 0;
      const draw = () => {
        if (stopped) return;
        const sx = (i % meta.cols) * meta.frameW;
        const sy = Math.floor(i / meta.cols) * meta.frameH;
        ctx.clearRect(0, 0, meta.frameW, meta.frameH);
        ctx.drawImage(
          bitmap,
          sx, sy, meta.frameW, meta.frameH,
          0, 0, meta.frameW, meta.frameH,
        );
        framePositions.set(url, i);
        const delay = meta.delays[i] ?? 100;
        i = (i + 1) % meta.frameCount;
        if (meta.frameCount > 1) timer = window.setTimeout(draw, delay);
      };
      draw();
    })();

    onCleanup(() => {
      stopped = true;
      if (timer !== undefined) clearTimeout(timer);
      if (acquiredUrl) release(acquiredUrl);
    });
  });

  return <canvas ref={canvas} class={props.class} style={props.style} onClick={props.onClick} />;
}
