# Performance optimization guide

This document consolidates every performance strategy discussed for the gallery app, adds new considerations, and analyzes whether GPU-direct storage paths could replace the thumbnail cache entirely.

---

## 1. Gallery open pipeline

The most latency-sensitive moment in the app. The goal is for the user to see content within 1-2 seconds of opening a 20,000-image gallery.

### 1.1 Multi-phase background pipeline

Opening a gallery kicks off five phases that overlap in time. Each phase streams results to the frontend as they complete rather than waiting for the full batch.

| Phase | What it does | Blocking? | Target time (20k files) |
|-------|-------------|-----------|------------------------|
| 1. Directory scan | `walkdir` collects paths + mtimes + file sizes. Sends total count to frontend immediately. | Only the count is blocking (~200ms) | 200ms |
| 2. Cache check | For each file, check SQLite `(path, mtime)`. Cache hits are sent to the frontend in batches of 100. Misses are queued. | Non-blocking, streams results | 500ms–1s |
| 3. Thumbnail generation | Rayon thread pool decodes + resizes + encodes thumbnails for cache misses. Batched SQLite writes (500 per transaction). | Non-blocking, streams results | 1–3 min (first open), near-zero on revisits |
| 4. Companion indexing | Parse companion JSON files where mtime has changed. Update `tag_index`, `tag_counts`, refresh autocomplete cache. | Non-blocking | 2–5s |
| 5. Timeline computation | Query `media_meta` ordered by date, compute month boundaries, send to frontend for scrollbar labels. | Non-blocking | <100ms |

### 1.2 Incremental everything

- **Thumbnail cache:** SQLite stores `(path, mtime, thumbnail_blob)`. On revisit, if the mtime matches, skip regeneration. A gallery where nothing changed reopens in under 1 second.
- **Tag index:** The `index_state` table stores `(path, companion_mtime)`. Only re-parse companion files whose mtime has changed.
- **Gallery meta:** A `gallery_meta` table stores the last scan timestamp and per-directory file counts. Unchanged subdirectories are skipped entirely on subsequent opens.
- **File hashing:** SHA-256 hashing is expensive, especially on large videos. Computed lazily on first access or as a lowest-priority background job — never blocks gallery open.

### 1.3 Batched SQLite writes

Individual inserts are slow because SQLite flushes to disk per-statement outside a transaction. All bulk operations wrap 500 rows in a single `BEGIN`/`COMMIT`, which is 50–100x faster. WAL (write-ahead logging) mode is enabled for concurrent reads during writes.

---

## 2. Scroll-aware thumbnail loading

The thumbnail pipeline is prioritized based on what the user is actually looking at.

### 2.1 Three-tier priority queue

| Priority | Zone | Description |
|----------|------|-------------|
| 0 (immediate) | Visible viewport rows | Always loaded first. Recalculated on every scroll position change. |
| 1 (buffer) | 2–3 rows above and below viewport | Pre-loaded so small scrolls are instant. |
| 2 (background) | Everything else | Filled during idle time, working outward from the viewport. |

### 2.2 Scroll velocity gating

The frontend tracks scroll speed via `requestAnimationFrame` position deltas and adjusts loading behavior:

| Velocity | Behavior |
|----------|----------|
| Stopped or slow (< 500 px/s) | Full loading: all three priority tiers active. |
| Medium (500–3000 px/s) | Viewport only: load priority 0, skip buffer and background. |
| Fast flick (> 3000 px/s) | Suspend entirely. Show skeleton cells. Don't waste decode cycles on rows that'll be offscreen in 200ms. |

Loading resumes immediately when velocity drops below the threshold.

### 2.3 Jump-to cancellation

When the user grabs the scrollbar and drags to a new position:

1. Increment a shared atomic **generation counter**.
2. Worker threads check the counter before writing results — stale work is discarded.
3. Flush the priority queue and rebuild around the new scroll position.
4. Resume loading from priority 0 at the new location.

This makes scrollbar dragging feel responsive even when most thumbnails haven't been generated yet.

---

## 3. Full-resolution image viewing

### 3.1 Progressive loading

When the user clicks image N:

1. Show the cached thumbnail instantly (already a blob URL in frontend memory). No visual gap.
2. Begin async full-resolution decode in background.
3. When decoded, upload to WebGPU texture and swap in the viewer. The transition is a fast opacity crossfade (~150ms).

### 3.2 Prefetching

While viewing image N, decode N+1 and N+2 (and optionally N-1) in background threads. Configurable: 1–10 images, default 3. Adjusted by hardware profile — NVMe systems default to 5, HDD systems default to 1.

### 3.3 LRU memory cache

A bounded least-recently-used cache holds decoded full-resolution images. Default 5, adjusted by available RAM:

| System RAM | LRU size |
|-----------|----------|
| > 32 GB | 10 images |
| > 16 GB | 5 images |
| <= 16 GB | 3 images |

At 50 MP per image (~200 MB decoded RGBA), a 5-image cache uses ~1 GB. Eviction is instant (drop the decoded buffer).

---

## 4. Frontend rendering

### 4.1 Virtual scrolling

The gallery grid only renders DOM nodes for visible thumbnails plus a small buffer. At any gallery size — 100 or 100,000 images — the DOM contains ~50-100 elements. This is non-negotiable for performance.

The scroller calculates total gallery height upfront from `(total_items / columns) * row_height` and uses a single scrollable container with absolutely-positioned children at their correct offsets. SolidJS's fine-grained reactivity means only the changed cells re-render when scrolling, not the entire list.

### 4.2 Thumbnail blob URLs

Thumbnails arrive from the Rust backend as raw WebP bytes. The frontend converts them to blob URLs (`URL.createObjectURL`) and sets them as `<img src>`. Blob URLs are reference-counted by the browser and garbage-collected when revoked. Only visible thumbnails + buffer need active blob URLs.

### 4.3 CSS-only animations

Thumbnail hover effects (brightness boost + subtle scale) use CSS `transition` on `filter` and `transform`, which are GPU-composited properties. No JavaScript runs during hover — the browser's compositor thread handles it.

---

## 5. SQLite query performance

### 5.1 Schema indexing strategy

| Table | Index | Purpose |
|-------|-------|---------|
| `tag_index` | `(namespace, tag)` | Fast "find all images with user:vacation" |
| `tag_index` | `(tag)` | Fast "find all images with 'vacation' in any namespace" |
| `tag_counts` | `(count DESC)` | Fast "top N most popular tags" for autocomplete |
| `media_meta` | `(date_taken DESC)` | Fast date-sorted gallery view |
| `media_meta` | `(file_size DESC)` | Fast size-sorted view |
| `media_meta` | `(media_type)` | Fast "show only videos" filter |
| `file_hashes` | `(hash)` | Fast "find by content hash" for rename detection |

### 5.2 Query selectivity for complex filters

When a filter has multiple AND clauses (`user:vacation AND plugin.face-recognition:person:alice AND rating>=4`), each clause becomes a subquery or join. The query planner works best when the most selective clause (fewest matching rows) is evaluated first. The app checks `tag_counts` to estimate selectivity and reorders the WHERE clause accordingly.

### 5.3 Scale expectations

At 20k images with 50 tags each:

| Table | Rows | Disk size |
|-------|------|-----------|
| `thumbnails` | 20,000 | 300–600 MB (blobs) |
| `tag_index` | ~1,000,000 | 50–100 MB |
| `tag_counts` | ~5,000 | < 1 MB |
| `media_meta` | 20,000 | < 5 MB |

All well within SQLite's comfort zone.

---

## 6. Tag autocomplete performance

### 6.1 In-memory approach

All unique tags (~5,000 entries) are loaded into a Rust `Vec` at ~300 KB. Fuzzy matching runs entirely in memory — no SQLite round-trip per keystroke. Sub-millisecond response times.

### 6.2 Scoring strategy

| Match type | Score | Example: query "vac" |
|-----------|-------|---------------------|
| Exact match | 1000 | "vac" |
| Prefix match | 500+ | "vacation" |
| Substring match | 200+ | "evacuation" |
| Subsequence match | 50 | "volcanic" |
| No match | — | "sunset" |

Ties broken by tag frequency (higher count wins). Debounced at 150ms on the frontend.

### 6.3 Cache refresh

The in-memory cache refreshes whenever the tag index changes: after plugin runs, user tag edits, or full re-index. The refresh is a single `SELECT namespace, tag, count FROM tag_counts` query — fast even at 5,000 rows.

---

## 7. Hardware-specific optimizations

### 7.1 Storage type detection

Detected at startup on Linux via `/sys/block/<dev>/queue/rotational` and `/proc/mounts`.

| Storage | Detection | Adjustments |
|---------|-----------|-------------|
| **NVMe** | `rotational=0`, device name starts with `nvme` | Max thumbnail threads (up to CPU count), parallel companion reads, prefetch 5 images, aggressive random I/O |
| **SSD** | `rotational=0`, non-NVMe | Half CPU threads for thumbnails, prefetch 3 images |
| **HDD** | `rotational=1` | Only 2 thumbnail threads (avoid seek thrashing), serialize directory reads, prefetch 1 image |
| **Network** | Mount type is NFS/CIFS/FUSE | Quarter CPU threads, increase read buffer sizes, aggressive caching, batch companion writes |

### 7.2 Filesystem-specific optimizations

| Filesystem | Optimization |
|------------|-------------|
| **btrfs** | Use `FICLONE` ioctl for atomic companion writes — instant copy-on-write instead of write-tmp + rename. Transparent compression means reads may already be faster; adjust prefetch upward. |
| **ZFS** | Similar COW semantics. Transparent compression benefits. |
| **ext4/XFS** | Standard write-tmp + rename path. |
| **NFS/CIFS** | Increase read buffer sizes, reduce companion write frequency (batch more), cache aggressively. |

### 7.3 GPU compute for thumbnail resizing

If the WebGPU adapter reports compute shader support (`maxComputeWorkgroupsPerDimension > 0`):

1. Decode the image on CPU (JPEG/PNG decode is sequential).
2. Upload the full-resolution decoded buffer to GPU VRAM.
3. Run a compute shader to downsample (bilinear or Lanczos kernel).
4. Read back the resized result.

This is significantly faster than CPU Lanczos for large images (50+ MP) because the GPU parallelizes the resize across thousands of cores. For smaller images (< 10 MP), the upload/readback overhead makes CPU resizing faster — so this is a conditional optimization gated on image dimensions.

### 7.4 GPU texture compression

If the GPU supports BC (Block Compression) or ASTC compressed texture formats, store decoded images in the LRU cache as GPU-compressed textures. This reduces VRAM usage by 4–8x, allowing more images in the cache.

---

## 8. DirectStorage and GPU-direct loading

### 8.1 What is DirectStorage / GPUDirect Storage?

Two related but different technologies:

**Microsoft DirectStorage** is a Windows API (DirectX 12) that enables games to stream compressed assets from NVMe directly into GPU memory, optionally decompressing on the GPU via compute shaders. It bypasses the traditional CPU-mediated I/O path (NVMe → system RAM → CPU decompress → upload to VRAM). It uses GDeflate or Zstandard compression and requires Windows 11. As of early 2026, adoption has been limited — GPU decompression isn't free and competes for compute resources.

**NVIDIA GPUDirect Storage (GDS)** is a Linux technology (part of CUDA) that creates a DMA path directly from NVMe storage to GPU memory, bypassing the CPU bounce buffer entirely. It requires NVIDIA data center GPUs (A100+), the `nvidia-fs.ko` kernel module, and a compatible filesystem. It's designed for HPC/AI data loading, not consumer applications.

### 8.2 Can DirectStorage help this gallery app?

**On Windows: theoretically yes, practically not much.** DirectStorage could eliminate the CPU memory copy when loading images into VRAM. The pipeline would be: NVMe → GPU buffer → compute shader decode → display. But the bottleneck in a photo gallery is image decoding (JPEG/PNG decompression), not raw I/O. DirectStorage's GPU decompression only supports GDeflate and Zstd — it cannot decode JPEG, PNG, WebP, or any image format natively. You'd still need to either:

- Decode on CPU, then upload to GPU (the current approach — DirectStorage saves nothing).
- Pre-convert all images to a GPU-native compressed format (BC7/ASTC) with GDeflate wrapping, store those alongside originals, and stream them with DirectStorage. This would be extremely fast to display but doubles storage requirements and requires a preprocessing step.

**On Linux: not applicable for consumer use.** NVIDIA GPUDirect Storage requires data center GPUs (A100, H100) and kernel-level integration. Consumer GPUs (RTX 4090) don't support GDS. The Vulkan `VK_NV_memory_decompression` extension provides GPU-side GDeflate decompression, but the same image format limitation applies.

### 8.3 Could we skip thumbnails and resize on the GPU on-the-fly?

This is the most interesting question. The idea: instead of pre-generating and caching 20,000 thumbnail files in SQLite, load each full image directly into GPU memory and run a compute shader to produce a thumbnail-sized texture in real-time.

**The math for a single image:**

| Step | Time (NVMe + RTX 4090) |
|------|----------------------|
| Read 8 MB JPEG from NVMe | ~0.5ms |
| CPU JPEG decode to RGBA | ~15–30ms |
| Upload decoded RGBA (~60 MB for 20 MP) to GPU | ~2–5ms |
| GPU compute shader resize to 400px | ~0.1ms |
| Total per image | ~18–36ms |

At 30 visible thumbnails (5 columns × 6 rows), that's **540–1080ms** to fill the initial viewport — acceptable but not instant. But scrolling to a new section means decoding another 30 images on-the-fly. If the user scrolls fast, even with NVMe the CPU JPEG decode bottleneck means you're waiting 0.5–1 second per screenful.

Compare with the cached approach: 30 thumbnail reads from SQLite at ~25 KB each is **<5ms total**. Cache wins by two orders of magnitude for anything that's been seen before.

**Verdict: thumbnails are still necessary.** But there's a hybrid approach worth considering:

### 8.4 Hybrid: GPU-accelerated thumbnail generation + cache

Instead of replacing the cache, use GPU-accelerated decoding and resizing to *generate* thumbnails faster, then cache the results normally:

1. **Batch-read** raw JPEG bytes from NVMe into a large CPU staging buffer (many files at once, sequential I/O).
2. **GPU JPEG decode** using a CUDA/compute shader JPEG decoder (libraries like `nvJPEG` on NVIDIA, or a custom compute shader for baseline JPEG). This can decode 50+ images per second on an RTX 4090.
3. **GPU batch resize** the decoded images to 400px using a compute shader.
4. **GPU encode** to WebP or BC7 compressed format.
5. **Read back** the results and write to SQLite cache.

This could bring first-open thumbnail generation for 20,000 images from ~2 minutes (CPU rayon) down to ~30 seconds. After that, the cache makes all subsequent opens instant.

The catch: `nvJPEG` is CUDA-only (NVIDIA), there's no cross-platform GPU JPEG decoder via WebGPU or Vulkan compute that handles real-world JPEGs well. This would be an NVIDIA-specific code path with CPU fallback.

### 8.5 Future: GPU-native image formats

The cleanest long-term path is a plugin that pre-converts images to BC7 or ASTC compressed GPU textures. These formats:

- Load directly into VRAM with zero CPU decode cost.
- Are 4–8x smaller in VRAM than raw RGBA.
- Can be streamed with DirectStorage (on Windows) or memory-mapped directly.
- A 20 MP BC7 texture is ~20 MB (vs ~60 MB decoded RGBA).

A "GPU texture cache" plugin could run in the background, converting images to `.bc7` sidecar files. The gallery would preferentially load these when available, falling back to JPEG decode otherwise. Combined with a compute shader resize, this could eventually make on-the-fly thumbnailing practical — but it's a significant investment for a marginal gain over the simpler SQLite cache.

### 8.6 BC7 GPU-native thumbnail cache

While converting full-resolution images to GPU-native formats is impractical (doubles storage, requires preprocessing), converting *thumbnails* to BC7 is a much better fit. Thumbnails are already a generated cache artifact — switching their storage format from WebP-in-SQLite to BC7-on-disk changes the display path from CPU-mediated to GPU-direct.

**Current pipeline (WebP in SQLite):**

```
SQLite blob read → heap buffer → CPU WebP decode → RGBA buffer → GPU texture upload → display
```

**Proposed pipeline (BC7 packed atlas):**

```
mmap atlas file → GPU texture upload (raw DMA) → display
```

The CPU decode step is eliminated entirely. BC7 is a fixed-rate compressed texture format that the GPU's texture sampling units read natively — no decompression shader needed.

**Size comparison for 20,000 thumbnails at 400×400:**

| Format | Per thumbnail | 20k total | CPU decode needed? |
|--------|-------------|-----------|-------------------|
| WebP quality 80 (current) | ~25 KB | ~500 MB | Yes |
| BC7 (GPU-native) | ~160 KB | ~3.2 GB | No |
| ASTC 4x4 (GPU-native) | ~160 KB | ~3.2 GB | No |
| Raw RGBA (uncompressed) | ~640 KB | ~12.8 GB | No |

BC7 is 6x larger on disk than WebP, but on NVMe this is a non-issue — reading 160 KB takes ~10 microseconds. The win is eliminating ~50 microseconds of CPU decode per thumbnail. At 30 visible thumbnails per viewport, that's **1.5ms saved per scroll event**, which adds up during fast scrolling.

**Storage architecture: packed texture atlas**

Rather than 20,000 individual `.bc7` files (filesystem metadata overhead), pack all thumbnails into a single atlas file with a small index:

```
.lightview/
  cache.db                    ← SQLite (tag index, media meta — no thumbnail blobs)
  thumb_atlas.bin             ← Packed BC7 thumbnail data (sequential)
  thumb_atlas.idx             ← Index: path → (offset, width, height, byte_size)
```

The index file is a simple binary table or JSON mapping each media path to its position in the atlas. The atlas file is append-only during thumbnail generation and can be memory-mapped for zero-copy reads.

**Display path with mmap:**

```rust
// One-time setup on gallery open
let atlas = memmap2::MmapOptions::new().map(&atlas_file)?;
let index = load_atlas_index(&index_path)?;

// Per-thumbnail display (called from priority queue)
fn get_thumbnail_bc7(path: &str) -> &[u8] {
    let entry = index.get(path).unwrap();
    &atlas[entry.offset..entry.offset + entry.byte_size]
}
// Hand the slice directly to WebGPU/Vulkan as a BC7 texture upload
// No decode, no copy — the mmap'd pointer goes straight to the graphics API
```

**BC7 encoding during thumbnail generation:**

The thumbnail pipeline changes to: decode image → resize to 400px → BC7 encode → append to atlas. BC7 encoding is more expensive than WebP encoding (~10ms vs ~2ms per thumbnail), but this only happens once during initial thumbnail generation. The `intel-tex-rs-2` or `bc7enc_rdo` Rust crates provide fast BC7 encoders, with quality modes that trade encoding speed for compression quality.

**When this makes sense vs when it doesn't:**

| Scenario | BC7 atlas | WebP in SQLite | Winner |
|----------|----------|----------------|--------|
| Fast scrolling through large gallery | Zero CPU decode per cell | CPU decode per visible cell | BC7 |
| First gallery open (generation time) | Slower (BC7 encoding) | Faster (WebP encoding) | WebP |
| Disk usage | 3.2 GB for 20k thumbs | 500 MB for 20k thumbs | WebP |
| Random access (filter results) | mmap + offset lookup | SQLite query + blob read | BC7 (slightly) |
| Remote galleries (SFTP/S3) | Must download larger atlas | Smaller blobs over network | WebP |
| GPU without BC7 support | Fallback needed | Works everywhere | WebP |

**Recommendation:** implement both and select based on hardware profile. Local galleries on NVMe with a discrete GPU use the BC7 atlas. Remote galleries, HDD systems, and integrated GPUs fall back to WebP in SQLite. The atlas format is a pure cache artifact — it can be regenerated from the original images at any time, and the two systems share the same priority queue and scroll-aware loading logic.

**Note on DirectStorage specifically:** on Linux, Microsoft DirectStorage doesn't exist. But the mmap + Vulkan texture upload path achieves the same practical result for this use case — the data goes from NVMe page cache to VRAM with minimal CPU involvement. The CPU's role is reduced to issuing the upload command and managing the priority queue, not decoding image data. NVIDIA GPUDirect Storage (the CUDA/Linux variant) could theoretically eliminate even the page cache step by DMA-ing directly from NVMe to VRAM, but this requires data center GPUs and would save microseconds per thumbnail — not worth the dependency for a consumer app.

---

## 9. Additional optimizations to consider

### 9.1 EXIF thumbnail extraction

Most JPEG files from cameras embed a thumbnail (typically 160×120 or 320×240) in their EXIF data. Reading the EXIF thumbnail is vastly faster than decoding the full image — it requires reading only the first ~50 KB of the file rather than the full 8 MB. On first gallery open, the pipeline could:

1. Extract EXIF thumbnails as a "fast pass" (covers ~70–80% of JPEGs from cameras).
2. Display these immediately at reduced quality.
3. Generate proper 400px thumbnails in the background and swap them in.

This gives the user something to look at within milliseconds of gallery open, even before the rayon pipeline starts.

### 9.2 Memory-mapped I/O for local files

Instead of `std::fs::read()` which copies the entire file into a heap buffer, use `mmap` to map the file directly into the process's address space. The OS kernel handles paging, and if the file is on NVMe, the data arrives in the process's page cache without an explicit copy. For large galleries with many small reads (thumbnails from SQLite, companion files), mmap can reduce memory copies and allocation overhead.

On Linux, the `memmap2` Rust crate provides safe mmap wrappers. SQLite already uses mmap internally for its database file when configured with `PRAGMA mmap_size`.

### 9.3 SIMD-accelerated image resizing

The `fast_image_resize` Rust crate uses SIMD (SSE4.1, AVX2, NEON) intrinsics for image resizing, significantly faster than the generic `image-rs` Lanczos implementation. On an AVX2-capable CPU, resizing a 20 MP image to 400px is ~2–4x faster with SIMD than without. This is a drop-in replacement for the resize step in the thumbnail pipeline.

### 9.4 Parallel EXIF extraction

EXIF date extraction (needed for the timeline and date sort) can be parallelized separately from thumbnail generation since it only needs the first few KB of each file. A dedicated rayon task can extract dates for all 20,000 files in ~2 seconds, populating `media_meta` before thumbnails are ready. This lets the frontend show correct date group headers and timeline labels early.

### 9.5 Connection pooling for remote galleries

When using SFTP or SMB providers, each file read opens a connection. For galleries with thousands of files, use a connection pool (2–4 connections per remote host) and pipeline multiple read requests over each connection. For S3, use multi-part parallel downloads for large files and batch `ListObjects` calls.

### 9.6 Adaptive thumbnail quality

Not all thumbnails need the same quality. The grid view shows images at 200px or smaller — a WebP at quality 60 looks identical to quality 80 at that size but is ~40% smaller. Reserve quality 80+ for the viewer's progressive loading thumbnail (which is displayed briefly at full screen size before the full-res image loads).

### 9.7 Background defragmentation of cache DB

Over time, as thumbnails are regenerated and tags are re-indexed, the SQLite database accumulates fragmentation. A periodic `VACUUM` (triggered manually from settings or automatically after large batch operations) compacts the database file and can improve read performance by 10–20% for large caches.

### 9.8 Precomputed filter results

For filters that the user applies frequently (e.g., "user:favorites"), store the result set in a cache table. When the filter is re-applied, check if the underlying data has changed (via a hash of relevant `index_state` mtimes); if not, return the cached result instantly instead of re-running the SQL query.

### 9.9 Decode format specialization

Use the fastest available decoder for each format rather than routing everything through the generic `image-rs` path:

| Format | Fastest decoder | Speedup vs image-rs |
|--------|----------------|-------------------|
| JPEG | `turbojpeg` (Rust bindings to libjpeg-turbo) | 2–3x |
| PNG | `png` crate (already fast, uses SIMD for filtering) | Marginal |
| WebP | `libwebp` (native C library) | 1.5–2x |
| HEIF/HEIC | Platform-native (libheif) | Required — image-rs doesn't support it |
| RAW | `rawloader` + `imagepipe` or `libraw` | Required |

Using `turbojpeg` for JPEG decoding alone would cut total thumbnail generation time roughly in half, since the majority of photos are JPEG.

### 9.10 Io_uring for async file I/O (Linux)

Linux's `io_uring` interface provides kernel-level async I/O that's significantly faster than the traditional `read()`/`pread()` syscall path for batched operations. The `tokio-uring` Rust crate integrates it with the tokio async runtime. For the gallery open scan (reading 20,000 file metadata entries and companion files), io_uring's batched submission can reduce syscall overhead dramatically compared to per-file reads.

---

## 10. Performance budget summary

Target latencies for key user interactions:

| Action | Target | How |
|--------|--------|-----|
| Gallery open → first thumbnails visible | < 1s | Phase 1 count + Phase 2 cache hits stream immediately |
| Gallery open → all thumbnails ready (revisit) | < 1s | SQLite mtime check skips regeneration |
| Gallery open → all thumbnails ready (first time) | < 2 min | Rayon parallel generation, streamed to frontend |
| Scroll to new section (cached) | < 5ms | SQLite blob reads for visible viewport |
| Scroll to new section (uncached) | < 200ms | Priority queue focuses on viewport rows |
| Fast scrollbar drag | No jank | Skeleton cells shown, stale work cancelled |
| Click thumbnail → image visible | < 50ms | Cached thumbnail shown instantly |
| Click thumbnail → full-res visible | < 300ms | Async decode + WebGPU texture upload |
| Navigate to next image | < 50ms | Prefetched and in LRU cache |
| Type in filter bar → suggestions appear | < 20ms | In-memory fuzzy matching, debounced 150ms |
| Apply complex filter (20k gallery) | < 100ms | SQLite indexed query |
| Add/remove user tag | < 50ms | Atomic companion write + incremental index update |
