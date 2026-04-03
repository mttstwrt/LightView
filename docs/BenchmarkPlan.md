# LightView Benchmark & Profiling Plan

How to measure, compare, and track performance across iterations of LightView.

---

## Tools

| Layer | Tool | Purpose |
|-------|------|---------|
| Rust hot paths | **Criterion.rs** | Statistical micro-benchmarks with regression detection |
| Rust profiling | **cargo flamegraph** | CPU flamegraphs via `perf` for identifying bottlenecks |
| Frontend logic | **tinybench** | Micro-benchmarks for pure JS/TS computation |
| Frontend render | **Chrome DevTools** | Performance panel, frame timing, memory snapshots |
| Frontend render | **DebugOverlay** | Built-in FPS/frame-time HUD (already in LightView) |

---

## 1. Rust Benchmarks (Criterion.rs)

### Setup

```bash
# Install flamegraph (one-time)
cargo install flamegraph

# Ensure perf is available (Arch/CachyOS)
sudo pacman -S perf
```

### Running benchmarks

```bash
cd src-tauri

# Run all benchmarks
cargo bench

# Run a specific benchmark suite
cargo bench --bench thumbnailer
cargo bench --bench cache_db
cargo bench --bench atlas

# Run a specific benchmark by name filter
cargo bench --bench thumbnailer -- "jpeg_thumbnail"
cargo bench --bench thumbnailer -- "resize_filter_comparison"
```

### Benchmark suites

#### `benches/thumbnailer.rs`
- **jpeg_thumbnail** — JPEG DCT-scaled decode + resize at 1080p, 12MP, 24MP
- **png_thumbnail** — Generic decode path for PNG at various sizes
- **rgba_output** — RGBA (pre-BC7) output path vs JPEG encode
- **decode_and_crop** — Decode + center-crop without resize (isolates decode cost)
- **batch_parallel** — 50-image parallel batch via rayon (measures thread pool scaling)
- **resize_filter_comparison** — Nearest vs Bilinear vs Lanczos3 at 12MP

#### `benches/cache_db.rs`
- **cache_db/open** — SQLite open + WAL pragma overhead
- **cache_db/upsert_thumbnail** — Insert/replace a 30KB thumbnail blob
- **cache_db/get_thumbnail_hit** — Read a cached thumbnail (1000-entry DB)
- **cache_db/get_thumbnail_miss** — Cache miss lookup
- **cache_db/tags/query_tag** — Query paths by namespace+tag (500 files, 3 tags each)
- **cache_db/tags/get_tags_for_file** — Get all tags for a single file

#### `benches/atlas.rs`
- **atlas/open** — Open an existing BC7 atlas (mmap + index deserialize)
- **atlas/upsert** — Encode RGBA to BC7 + append to atlas (includes BC7 encode time)
- **atlas/upsert_bc7_raw** — Append pre-encoded BC7 data (isolates IO from encode)
- **atlas/read_bc7_hit** — Zero-copy mmap read of a BC7 entry
- **atlas/read_bc7_miss** — HashMap miss on atlas lookup
- **atlas/is_valid_hit** — mtime validation check

### Reading results

Criterion outputs to `src-tauri/target/criterion/`. After each run:

- **Terminal**: Shows mean time, standard deviation, and throughput per benchmark
- **HTML reports**: Open `target/criterion/report/index.html` for interactive charts
- **Regression detection**: If a baseline exists, Criterion shows `+X% / -X%` vs the previous run

### Saving baselines for comparison

```bash
# Save a named baseline before making changes
cargo bench --bench thumbnailer -- --save-baseline before-optimization

# Make changes, then compare
cargo bench --bench thumbnailer -- --baseline before-optimization
```

This produces a comparison report showing the delta between the two runs.

---

## 2. Flamegraph Profiling

### Generate a flamegraph for a specific benchmark

```bash
cd src-tauri

# Profile a single benchmark (requires perf)
cargo flamegraph --bench thumbnailer -- --bench "jpeg_thumbnail/4000x3000/nearest"

# The SVG is written to flamegraph.svg — open in a browser
```

### Profile the full application

```bash
cd src-tauri

# Build release with debug symbols
cargo build --release

# Run the app under flamegraph
cargo flamegraph --bin lightview -- [args]

# Or attach to a running process
flamegraph -p <PID> -o gallery-open.svg
```

### Tips

- Use `--release` for production-representative profiles (dev builds have different codegen)
- Filter flamegraphs by function name in the SVG viewer to focus on hot paths
- Compare two SVGs side-by-side (or use `difffolded.pl` from FlameGraph tools) to see what changed
- Key functions to watch:
  - `generate_jpeg_thumbnail_inner` — JPEG decode + DCT scaling
  - `resize_rgb` / `resize_rgba` — fast_image_resize calls
  - `encode_bc7` — BC7 texture compression (intel_tex_2)
  - `generate_batch` / `generate_batch_parallel` — rayon thread pool dispatch

---

## 3. Frontend Benchmarks (tinybench)

### Running

```bash
# From project root
npm run bench

# Or directly
npx tsx src-solidjs/bench/renderer.bench.ts
```

### Benchmark suites (`src-solidjs/bench/renderer.bench.ts`)

- **LRU Cache** — get/set/eviction on a 300-entry cache (mirrors ImageLoader)
- **Grid Layout** — layout calculation for 1K and 50K item grids
- **Hit Test** — click-to-cell-index resolution
- **Viewport Intersection** — visible item range calculation at various gallery sizes

### Output

Prints a table per suite with ops/sec, avg latency (ns), p99 latency, and sample count.

### Adding benchmarks

Add a new function returning a `Bench` instance to `renderer.bench.ts` and register it in the `suites` array at the bottom.

---

## 4. Comparing Iterations

### Workflow for any optimization

1. **Baseline**: Run benchmarks on `main` and save a named baseline:
   ```bash
   git checkout main
   cd src-tauri && cargo bench -- --save-baseline main-baseline
   cd .. && npm run bench > bench-results/main-frontend.txt
   ```

2. **Implement**: Make changes on a feature branch.

3. **Measure**: Run benchmarks and compare:
   ```bash
   cd src-tauri && cargo bench -- --baseline main-baseline
   npm run bench > bench-results/feature-frontend.txt
   ```

4. **Profile** (if results are unexpected):
   ```bash
   cargo flamegraph --bench thumbnailer -- --bench "the_slow_benchmark"
   ```

5. **Document**: Record the before/after numbers in the PR description.

### What to measure per optimization area

| Area | Key benchmarks | Flamegraph targets |
|------|---------------|--------------------|
| Thumbnail decode | `jpeg_thumbnail/*`, `decode_and_crop/*` | `generate_jpeg_thumbnail_inner` |
| Resize quality/speed | `resize_filter_comparison/*` | `resize_rgb`, `fir::Resizer::resize` |
| BC7 atlas | `atlas/upsert`, `atlas/read_bc7_hit` | `encode_bc7`, mmap read paths |
| Cache DB | `cache_db/get_thumbnail_*`, `cache_db/upsert_*` | `rusqlite::*` |
| Batch throughput | `batch_parallel/*` | rayon work-stealing, thread pool |
| Frontend scroll | DebugOverlay FPS | Chrome Performance panel |
| Frontend cache | `lru_cache/*` | N/A (pure JS) |

### Regression detection in CI (future)

Criterion supports machine-readable JSON output:
```bash
cargo bench --bench thumbnailer -- --output-format bencher
```

This can be piped to tools like `github-action-benchmark` or `critcmp` for automated regression detection in CI.

---

## 5. End-to-End Gallery Benchmarks

For full-app performance (not covered by micro-benchmarks):

### Manual test protocol

1. Prepare test galleries of known sizes:
   - **Small**: 100 images (mixed JPEG/PNG)
   - **Medium**: 5,000 images (primarily JPEG)
   - **Large**: 50,000 images (mixed formats, some HEIC)

2. Clear the `.lightview/` cache directory for the test gallery.

3. Measure **cold open** time:
   - Start LightView, open the gallery
   - Record time from "Open Gallery" click to first visible thumbnails
   - Record time to full grid population

4. Measure **warm open** time (cached):
   - Close and re-open the same gallery
   - Record time to first visible thumbnails

5. Measure **scroll performance**:
   - Enable the DebugOverlay (Ctrl+Shift+D)
   - Scroll through the entire gallery at a consistent speed
   - Record min/avg/max FPS and frame time

6. Measure **memory usage**:
   - Open Chrome DevTools > Memory
   - Take a heap snapshot after gallery is fully loaded
   - Record JS heap size and total allocated

### Automated timing (via Rust logs)

LightView already logs timing info. Use `RUST_LOG=info` and grep for:
```bash
RUST_LOG=info cargo tauri dev 2>&1 | grep -E "(thumbnail|gallery|atlas|Hardware)"
```

---

## 6. Quick Reference

```bash
# --- Rust ---
cargo bench                                      # all benchmarks
cargo bench --bench thumbnailer                   # one suite
cargo bench -- --save-baseline v1                 # save baseline
cargo bench -- --baseline v1                      # compare to baseline
cargo flamegraph --bench thumbnailer              # flamegraph a bench

# --- Frontend ---
npm run bench                                     # tinybench suite

# --- Comparison ---
critcmp main-baseline feature-branch              # side-by-side (install: cargo install critcmp)
```
