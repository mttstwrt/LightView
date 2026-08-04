// Last-session grid snapshot for the remote web client.
//
// The web boot is network-bound: gallery info + sorted items are several
// round-trips away. Persisting the last sorted-items payload in IndexedDB
// lets the next open paint the full grid (ThumbHash placeholders + any
// service-worker-cached thumbnails) before the first byte comes back — and
// makes the app open offline. Fresh data replaces the snapshot as soon as it
// arrives; the snapshot is only ever a boot-time stand-in.
//
// IndexedDB is origin-scoped, so the snapshot can't leak across servers.

import type { SortedItem } from "./types";

const DB_NAME = "lightview";
const STORE = "boot-snapshot";
const KEY = "current";

/** How long a snapshot may stand in for live data. Beyond this the grid it
 *  paints is more misleading than useful — a client that hasn't reached the
 *  server in a month (a lapsed certificate exception, a host that moved) would
 *  otherwise open to a confident-looking view of a gallery it can no longer
 *  see. `savedAt` was recorded from the start but never read. */
const MAX_AGE_MS = 30 * 24 * 3600 * 1000;

export interface BootSnapshot {
  galleryPath: string;
  items: SortedItem[];
  savedAt: number;
}

function openDb(): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    const req = indexedDB.open(DB_NAME, 1);
    req.onupgradeneeded = () => {
      if (!req.result.objectStoreNames.contains(STORE)) {
        req.result.createObjectStore(STORE);
      }
    };
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}

/** Read the last session's snapshot, or null when absent, unreadable, or
 *  older than `MAX_AGE_MS` (an expired snapshot is deleted on the way out). */
export async function loadBootSnapshot(): Promise<BootSnapshot | null> {
  try {
    const db = await openDb();
    const snap = await new Promise<BootSnapshot | null>((resolve) => {
      const tx = db.transaction(STORE, "readonly");
      const req = tx.objectStore(STORE).get(KEY);
      req.onsuccess = () => resolve((req.result as BootSnapshot) ?? null);
      req.onerror = () => resolve(null);
    });
    if (!snap) return null;
    if (Date.now() - (snap.savedAt ?? 0) > MAX_AGE_MS) {
      db.transaction(STORE, "readwrite").objectStore(STORE).delete(KEY);
      return null;
    }
    return snap;
  } catch {
    return null;
  }
}

/** Persist the current grid; fire-and-forget (all failures swallowed —
 *  the snapshot is an optimization, never a source of truth). */
export function saveBootSnapshot(galleryPath: string, items: SortedItem[]): void {
  void (async () => {
    try {
      const db = await openDb();
      const snap: BootSnapshot = { galleryPath, items, savedAt: Date.now() };
      db.transaction(STORE, "readwrite").objectStore(STORE).put(snap, KEY);
    } catch {
      // ignore — e.g. private browsing without IDB
    }
  })();
}
