// LightView service worker — makes the remote web client feel like an
// installed app: instant warm opens and a usable (read-only) grid offline.
//
// Caching policy, by route:
//   /assets/*            cache-first    (hashed filenames — immutable by URL)
//   /thumb/*, /thumbhash/* stale-while-revalidate, capped (thumbnail bytes;
//                        revalidation keeps edited sources from sticking
//                        beyond one view)
//   navigations (/)      network-first with cached-shell fallback, so the
//                        app opens with no connectivity
//   everything else      untouched (API invokes, SSE, /media, uploads)
//
// Auth: data routes require the lv_device cookie; only `ok` responses are
// cached, so 401s never poison the cache. Cache storage is origin-scoped and
// so is the pairing, which keeps cached thumbnails scoped to the paired
// device's browser profile.
//
// Bump VERSION on any policy change — activate drops old lv-* caches.
const VERSION = "v1";
const ASSET_CACHE = `lv-assets-${VERSION}`;
const THUMB_CACHE = `lv-thumbs-${VERSION}`;
const SHELL_CACHE = `lv-shell-${VERSION}`;

// ~18 KB per m-tier thumb → a full cache is on the order of 40 MB.
const THUMB_CACHE_MAX_ENTRIES = 2000;
// Trim is O(keys), so amortize it over many puts instead of running per-put.
const TRIM_EVERY_N_PUTS = 50;
let putsSinceTrim = 0;

self.addEventListener("install", () => {
  self.skipWaiting();
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    (async () => {
      const keep = new Set([ASSET_CACHE, THUMB_CACHE, SHELL_CACHE]);
      for (const key of await caches.keys()) {
        if (key.startsWith("lv-") && !keep.has(key)) await caches.delete(key);
      }
      await self.clients.claim();
    })(),
  );
});

async function cacheFirst(cacheName, request) {
  const cache = await caches.open(cacheName);
  const hit = await cache.match(request);
  if (hit) return hit;
  const res = await fetch(request);
  if (res.ok) cache.put(request, res.clone());
  return res;
}

// Cache.keys() returns entries in insertion order, so deleting from the
// front approximates FIFO eviction — good enough for a thumbnail cache
// (staleness is bounded by revalidation, not eviction).
async function trimThumbCache(cache) {
  const keys = await cache.keys();
  const excess = keys.length - THUMB_CACHE_MAX_ENTRIES;
  for (let i = 0; i < excess; i++) await cache.delete(keys[i]);
}

async function staleWhileRevalidate(event, request) {
  const cache = await caches.open(THUMB_CACHE);
  const hit = await cache.match(request);
  const revalidate = fetch(request)
    .then(async (res) => {
      if (res.ok) {
        await cache.put(request, res.clone());
        if (++putsSinceTrim >= TRIM_EVERY_N_PUTS) {
          putsSinceTrim = 0;
          await trimThumbCache(cache);
        }
      }
      return res;
    })
    .catch(() => undefined);
  if (hit) {
    // Serve cached bytes now; refresh in the background for next time.
    event.waitUntil(revalidate);
    return hit;
  }
  const res = await revalidate;
  if (res) return res;
  return Response.error();
}

async function networkFirstShell(request) {
  const cache = await caches.open(SHELL_CACHE);
  try {
    const res = await fetch(request);
    // Cache under a fixed key: every SPA route serves the same shell.
    if (res.ok) cache.put("/", res.clone());
    return res;
  } catch {
    const hit = await cache.match("/");
    if (hit) return hit;
    throw new Error("offline and no cached shell");
  }
}

self.addEventListener("fetch", (event) => {
  const { request } = event;
  if (request.method !== "GET") return;
  const url = new URL(request.url);
  if (url.origin !== location.origin) return;

  if (url.pathname.startsWith("/assets/")) {
    event.respondWith(cacheFirst(ASSET_CACHE, request));
  } else if (
    url.pathname.startsWith("/thumb/") ||
    url.pathname.startsWith("/thumbhash/")
  ) {
    event.respondWith(staleWhileRevalidate(event, request));
  } else if (request.mode === "navigate") {
    event.respondWith(networkFirstShell(request));
  }
  // Everything else (POST /api/invoke, SSE /api/events, /media, /gif-atlas)
  // goes straight to the network.
});
