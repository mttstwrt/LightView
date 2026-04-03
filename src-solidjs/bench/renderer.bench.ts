// ---------------------------------------------------------------------------
// Frontend micro-benchmarks using tinybench
// ---------------------------------------------------------------------------
//
// Run with: npx tsx src-solidjs/bench/renderer.bench.ts
//
// These benchmarks measure pure computation — no DOM or canvas.
// For canvas render benchmarks, use the DebugOverlay's built-in FPS/frame
// timing or the Chrome DevTools Performance panel.

import { Bench } from "tinybench";

// ---------------------------------------------------------------------------
// 1. LRU cache simulation (mirrors ImageLoader's cache eviction logic)
// ---------------------------------------------------------------------------

function lruCacheBench() {
  const bench = new Bench({ time: 2000 });

  class LRUCache<V> {
    private map = new Map<string, { value: V; lastAccess: number }>();
    private counter = 0;
    constructor(private maxSize: number) {}

    get(key: string): V | undefined {
      const entry = this.map.get(key);
      if (entry) {
        entry.lastAccess = ++this.counter;
        return entry.value;
      }
      return undefined;
    }

    set(key: string, value: V): void {
      if (this.map.size >= this.maxSize && !this.map.has(key)) {
        // Evict LRU
        let oldest = Infinity;
        let oldestKey = "";
        for (const [k, v] of this.map) {
          if (v.lastAccess < oldest) {
            oldest = v.lastAccess;
            oldestKey = k;
          }
        }
        if (oldestKey) this.map.delete(oldestKey);
      }
      this.map.set(key, { value, lastAccess: ++this.counter });
    }
  }

  const cache = new LRUCache<number>(300);
  // Pre-fill
  for (let i = 0; i < 300; i++) {
    cache.set(`/path/image_${i}.jpg`, i);
  }

  let idx = 0;

  bench
    .add("lru_cache/get_hit", () => {
      cache.get(`/path/image_${idx++ % 300}.jpg`);
    })
    .add("lru_cache/get_miss", () => {
      cache.get(`/path/nonexistent_${idx++}.jpg`);
    })
    .add("lru_cache/set_evict", () => {
      cache.set(`/path/new_image_${idx++}.jpg`, idx);
    });

  return bench;
}

// ---------------------------------------------------------------------------
// 2. Grid layout calculation (mirrors CanvasGrid's layout math)
// ---------------------------------------------------------------------------

function gridLayoutBench() {
  const bench = new Bench({ time: 2000 });

  function computeLayout(
    containerWidth: number,
    totalItems: number,
    cellSize: number,
    gap: number,
    scrollTop: number,
    viewportHeight: number,
  ) {
    const cols = Math.max(1, Math.floor((containerWidth + gap) / (cellSize + gap)));
    const totalRows = Math.ceil(totalItems / cols);
    const rowHeight = cellSize + gap;
    const startRow = Math.max(0, Math.floor(scrollTop / rowHeight) - 2);
    const visibleRows = Math.ceil(viewportHeight / rowHeight) + 4;
    const endRow = Math.min(totalRows, startRow + visibleRows);
    const startIndex = startRow * cols;
    const endIndex = Math.min(totalItems, endRow * cols);
    return { cols, totalRows, startRow, endRow, startIndex, endIndex };
  }

  bench
    .add("grid_layout/1000_items", () => {
      computeLayout(1920, 1000, 200, 4, 500, 1080);
    })
    .add("grid_layout/50000_items", () => {
      computeLayout(1920, 50000, 200, 4, 25000, 1080);
    })
    .add("grid_layout/scroll_sweep", () => {
      for (let scroll = 0; scroll < 10000; scroll += 100) {
        computeLayout(1920, 10000, 200, 4, scroll, 1080);
      }
    });

  return bench;
}

// ---------------------------------------------------------------------------
// 3. Hit-test calculation (click → cell index)
// ---------------------------------------------------------------------------

function hitTestBench() {
  const bench = new Bench({ time: 2000 });

  function hitTest(
    x: number,
    y: number,
    scrollTop: number,
    cols: number,
    cellSize: number,
    gap: number,
    totalItems: number,
  ): number | null {
    const absY = y + scrollTop;
    const stride = cellSize + gap;
    const col = Math.floor(x / stride);
    const row = Math.floor(absY / stride);
    if (col >= cols) return null;
    // Check if click is in the gap
    if (x - col * stride >= cellSize) return null;
    if (absY - row * stride >= cellSize) return null;
    const idx = row * cols + col;
    return idx < totalItems ? idx : null;
  }

  bench
    .add("hit_test/single", () => {
      hitTest(500, 300, 1000, 9, 200, 4, 10000);
    })
    .add("hit_test/sweep_100", () => {
      for (let i = 0; i < 100; i++) {
        hitTest(i * 19 % 1920, i * 11 % 1080, 1000, 9, 200, 4, 10000);
      }
    });

  return bench;
}

// ---------------------------------------------------------------------------
// 4. Viewport intersection (which items are visible)
// ---------------------------------------------------------------------------

function viewportIntersectionBench() {
  const bench = new Bench({ time: 2000 });

  function visibleItems(
    totalItems: number,
    cols: number,
    cellSize: number,
    gap: number,
    scrollTop: number,
    viewportHeight: number,
    bufferRows: number,
  ): number[] {
    const stride = cellSize + gap;
    const startRow = Math.max(0, Math.floor(scrollTop / stride) - bufferRows);
    const endRow = Math.ceil((scrollTop + viewportHeight) / stride) + bufferRows;
    const start = startRow * cols;
    const end = Math.min(totalItems, endRow * cols);
    const result: number[] = [];
    for (let i = start; i < end; i++) {
      result.push(i);
    }
    return result;
  }

  bench
    .add("viewport_intersection/1000_items", () => {
      visibleItems(1000, 9, 200, 4, 500, 1080, 2);
    })
    .add("viewport_intersection/100000_items", () => {
      visibleItems(100000, 9, 200, 4, 50000, 1080, 2);
    });

  return bench;
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

async function main() {
  const suites = [
    { name: "LRU Cache", bench: lruCacheBench() },
    { name: "Grid Layout", bench: gridLayoutBench() },
    { name: "Hit Test", bench: hitTestBench() },
    { name: "Viewport Intersection", bench: viewportIntersectionBench() },
  ];

  for (const { name, bench } of suites) {
    console.log(`\n${"=".repeat(60)}`);
    console.log(`  ${name}`);
    console.log(`${"=".repeat(60)}`);
    await bench.run();
    console.table(
      bench.tasks.map((t) => ({
        Name: t.name,
        "ops/sec": Math.round(t.result!.hz).toLocaleString(),
        "avg (ns)": Math.round(t.result!.mean * 1e6).toLocaleString(),
        "p99 (ns)": Math.round(t.result!.p99 * 1e6).toLocaleString(),
        samples: t.result!.samples.length,
      })),
    );
  }
}

main().catch(console.error);
