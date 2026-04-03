// ---------------------------------------------------------------------------
// Image decode worker — off-thread fetch + decode pipeline (Phase 5)
// ---------------------------------------------------------------------------
//
// Runs in a Dedicated Web Worker. Receives thumbnail URLs, fetches them,
// decodes into ImageBitmap objects, and transfers them back to the main thread.
// Maintains an internal priority queue so viewport-visible images are decoded
// before buffer/background images.
//
// Priority tiers (Phase 4.3):
//   0 = viewport (highest)
//   1 = directional buffer
//   2 = background prefetch (lowest)

// --- Message types ---

export interface DecodeRequest {
  type: "decode";
  path: string;
  url: string;
  priority: 0 | 1 | 2;
}

export interface CancelRequest {
  type: "cancel";
  path: string;
}

export interface CancelAllRequest {
  type: "cancelAll";
}

export type WorkerRequest = DecodeRequest | CancelRequest | CancelAllRequest;

export interface DecodeSuccess {
  type: "decoded";
  path: string;
  bitmap: ImageBitmap;
  width: number;
  height: number;
}

export interface DecodeError {
  type: "error";
  path: string;
  error: string;
}

export type WorkerResponse = DecodeSuccess | DecodeError;

// --- Worker implementation (only runs inside the worker context) ---
// Guard: this block only executes when loaded as a Worker (not when imported
// for types on the main thread). In a Worker, `self` has `postMessage` but
// not `document`.

if (typeof self !== "undefined" && "postMessage" in self && typeof (self as any).document === "undefined") {
  const MAX_CONCURRENT = 6;

  /** Pending decode requests, organized by priority tier. */
  const queues: [DecodeRequest[], DecodeRequest[], DecodeRequest[]] = [[], [], []];

  /** Paths currently being fetched/decoded — maps path to AbortController. */
  const inflight = new Map<string, AbortController>();

  /** Paths that have been cancelled and should be discarded on completion. */
  const cancelled = new Set<string>();

  let activeCount = 0;

  function enqueue(req: DecodeRequest) {
    // Deduplicate: don't enqueue if already queued or inflight
    if (inflight.has(req.path)) return;
    for (const q of queues) {
      if (q.some((r) => r.path === req.path)) return;
    }
    queues[req.priority].push(req);
    drain();
  }

  function dequeue(): DecodeRequest | null {
    for (const q of queues) {
      if (q.length > 0) return q.shift()!;
    }
    return null;
  }

  function drain() {
    while (activeCount < MAX_CONCURRENT) {
      const req = dequeue();
      if (!req) break;

      // Skip if cancelled while queued
      if (cancelled.has(req.path)) {
        cancelled.delete(req.path);
        continue;
      }

      activeCount++;
      const controller = new AbortController();
      inflight.set(req.path, controller);

      processRequest(req, controller.signal).finally(() => {
        activeCount--;
        inflight.delete(req.path);
        drain();
      });
    }
  }

  async function processRequest(req: DecodeRequest, signal: AbortSignal) {
    try {
      const response = await fetch(req.url, { signal });
      if (!response.ok) {
        throw new Error(`HTTP ${response.status}`);
      }

      const blob = await response.blob();
      if (signal.aborted) return;

      const bitmap = await createImageBitmap(blob);
      if (signal.aborted) {
        bitmap.close();
        return;
      }

      // Check cancellation one more time before posting
      if (cancelled.has(req.path)) {
        bitmap.close();
        cancelled.delete(req.path);
        return;
      }

      const msg: DecodeSuccess = {
        type: "decoded",
        path: req.path,
        bitmap,
        width: bitmap.width,
        height: bitmap.height,
      };
      (self as unknown as { postMessage(msg: any, transfer: Transferable[]): void }).postMessage(msg, [bitmap]);
    } catch (err: unknown) {
      if (signal.aborted || cancelled.has(req.path)) {
        cancelled.delete(req.path);
        return;
      }
      const msg: DecodeError = {
        type: "error",
        path: req.path,
        error: err instanceof Error ? err.message : String(err),
      };
      (self as unknown as { postMessage(msg: any): void }).postMessage(msg);
    }
  }

  function cancelPath(path: string) {
    cancelled.add(path);
    const controller = inflight.get(path);
    if (controller) {
      controller.abort();
      inflight.delete(path);
    }
    // Remove from queues
    for (const q of queues) {
      const idx = q.findIndex((r) => r.path === path);
      if (idx >= 0) q.splice(idx, 1);
    }
  }

  function cancelAll() {
    for (const [path, controller] of inflight) {
      controller.abort();
    }
    inflight.clear();
    for (const q of queues) q.length = 0;
    cancelled.clear();
    activeCount = 0;
  }

  self.onmessage = (e: MessageEvent<WorkerRequest>) => {
    const msg = e.data;
    switch (msg.type) {
      case "decode":
        enqueue(msg);
        break;
      case "cancel":
        cancelPath(msg.path);
        break;
      case "cancelAll":
        cancelAll();
        break;
    }
  };
}
