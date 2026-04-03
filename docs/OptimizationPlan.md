# LightView Performance Optimization Plan 

## Phase 1 — IPC Architecture & Transport Overhead
The current bottleneck involves high-frequency, small-payload IPC calls that saturate the message bridge between the UI and the Rust backend.

### .1 Batching & Debouncing:**
    - Increase metadata fetch batch size from **32 to 128 items**.
    - Implement a **50ms debounce** on scroll-driven metadata requests to prevent "burst" congestion during high-velocity swipes.
### 1.2 Binary-First Transport:**
    - **Audit Path:** Eliminate any instances where image data is converted to Base64 strings.
    - **Implementation:** Utilize Tauri’s custom protocol (e.g., `asset://`) or streaming responses. This allows the browser engine to handle the binary stream directly, bypassing the JSON serialization/deserialization overhead on the IPC bridge.
### 1.3 Result Streaming (Channels):**
    - Instead of waiting for a full batch of 128 items to complete, use a **multi-producer, single-consumer (mpsc)** channel to stream individual metadata objects back to the UI as they become available.
### 1.4 Thumbnail Format Enforcement
- Ensure thumbnails are **compressed formats only**:
  - JPEG (preferred for grid performance)
  - WebP (optional for storage efficiency)
- Explicitly forbid raw RGBA pixel transport across IPC
- Target size:
  - 10–50 KB per thumbnail
### 1.5 Request Coalescing
- If multiple metadata or thumbnail requests are queued:
  - Merge them into a single IPC call
- Prevents redundant overlapping requests during rapid scroll events

---

## Phase 2 — Persistent Backend Pipeline (Tagger)
The "process-per-image" model is the primary source of latency in the tagging workflow due to Python's startup time and ONNX model loading.

### 2.1 Resident Worker Daemon:
    - Launch `wd_tagger.py` once when plugin is invoked.
    - Load the **EVA02-Large** model into VRAM once and keep it resident.
### 2.2 Protocol Definition:
    - Communicate via **Standard I/O (stdin/stdout)** using newline-delimited JSON or a **Unix Domain Socket**.
    - **Request:** `{"id": "uuid", "path": "/media/img.jpg"}`
    - **Response:** `{"id": "uuid", "tags": [...], "confidence": [...]}`
### 2.3 Concurrency Gating:**
    - Limit the Python worker to a single inference thread to avoid VRAM contention with the UI's rendering process, but keep a queue on the Rust side to manage incoming requests.

---

## Phase 3 — Pressure-Aware Memory Management
The goal is to maximize the cache on high-end systems while remaining "invisible" and non-intrusive on systems with limited resources or heavy background loads.

### 3.1 The "Available vs. Total" Logic:
    - **Metric:** Use `System_Available_RAM` (which includes reclaimable buffers/cache) rather than just `Free_RAM`.
    - **Target Capacity:**
        - **Base:** 10% of `Total_RAM`.
        - **Constraint:** Never exceed 40% of `Available_RAM`.
    - **Safety Trigger:** If `Available_RAM` drops below **1.5GB**, trigger an emergency cache purge down to the last 2 visible rows.
### 3.2 ImageBitmap Lifecycle:
    * Explicitly call `.close()` on `ImageBitmap` objects when evicted from the LRU cache to ensure the browser engine releases GPU memory immediately rather than waiting for Garbage Collection.
    
### 3.3 Decoded Image LRU Cache
- Maintain separate cache for decoded images (ImageBitmap or equivalent)
- Target size:
  - 200–500 images depending on memory pressure
- Evict based on:
  - distance from viewport
  - recency of use
---

## Phase 4 — Virtualization & Directional Buffering
### 4.1 Hard Node Cap:
    - Enforce a limit of `< 150` DOM elements in the grid.
### 4.2 Directional Preloading:
    - **Static Buffer:** Maintain 2 rows above and 2 rows below the viewport.
    - **Dynamic Buffer:** If `scroll_velocity > threshold`, extend the "look-ahead" buffer to **6 rows** in the direction of travel while shrinking the trailing buffer to **0 rows**.
### 4.3 Priority-Based Thumbnail Scheduling

Implement strict priority tiers:

1. Visible viewport (highest priority)
2. Directional buffer (based on scroll velocity)
3. Background prefetch (only when idle)

Rules:
- Never process lower-priority items while higher-priority queue is non-empty
- Cancel or deprioritize work when scroll direction changes


---

## Phase 5 — Off-Thread Rendering & GPU Path
### 5.1 Worker-Based Decoding:
    - Move `fetch() -> blob() -> createImageBitmap()` into a Dedicated Web Worker. 
    - This ensures that the main thread's only responsibility is DOM updates and input handling, eliminating "micro-stutter" during rapid scrolling.

### 5.2 Decode Scheduling Strategy
  - All decoding occurs in Web Worker
  - Main thread only receives:
    - already-decoded ImageBitmap
  - Avoid:
    - `<img>` decode on main thread
  - Ensure:
    - decode pipeline respects priority tiers (Phase 4.3)
    
---

## Phase 6 — Measurement & Validation
  - **Frame Budget:**
    - Target **< 8ms** per frame.
  - **Telemetry:**
    - Log `IPC_Bridge_Wait_Time` to identify if the Rust backend or the Webview is the bottleneck.
    - Monitor `VRAM_Resident_Set_Size` (if available via system APIs) to ensure the Python tagger and the UI are not fighting for GPU memory.

## Phase 7 — Rendering Path Optimization

### 7.1 Canvas Renderer
- Replace DOM grid with a single `<canvas>`
- Draw thumbnails manually
- Eliminates DOM overhead

### 7.2 WebGL Renderer
- Use texture atlas
- Batch draw calls
- Figure out webGPU Linux

## Phase 8 — Component-Level Optimization

### 8.1 Minimize Reactive Dependencies
- Reduce per-thumbnail reactive signals

### 8.2 URL Caching
- Avoid regenerating thumbnail URLs repeatedly

### 8.3 Error Debouncing
- Prevent rapid retry loops on failed thumbnails
    
## Summary of Constants
| Constant | Value | Description |
| :--- | :--- | :--- |
| `BATCH_SIZE` | 128 | Items per IPC request |
| `SETTLE_MS` | 150 | Delay before resuming background preloads |
| `MEM_SAFE_ZONE` | 1536 (MB) | Available RAM threshold for cache purge |
| `BUFFER_ROWS_DIR` | 6 | Directional pre-fetch depth |
