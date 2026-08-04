// LightView service worker — makes the remote web client feel like an
// installed app: instant warm opens and a usable (read-only) grid offline.
//
// Caching policy, by route:
//   /assets/*            cache-first    (hashed filenames — immutable by URL)
//   /thumb/*, /thumbhash/* stale-while-revalidate, capped by entry count and
//                        by a hard staleness ceiling (thumbnail bytes;
//                        revalidation keeps edited sources from sticking
//                        beyond one view)
//   navigations (/)      network-first; falls back to the cached shell only
//                        when the device is genuinely offline (see
//                        `networkFirstShell`)
//   everything else      untouched (API invokes, SSE, /media, uploads)
//
// Auth: data routes require the lv_device cookie; only `ok` responses are
// cached, so 401s never poison the cache. Cache storage is origin-scoped and
// so is the pairing, which keeps cached thumbnails scoped to the paired
// device's browser profile.
//
// Bump VERSION on any policy change — activate drops old lv-* caches.
const VERSION = "v2";
const ASSET_CACHE = `lv-assets-${VERSION}`;
const THUMB_CACHE = `lv-thumbs-${VERSION}`;
const SHELL_CACHE = `lv-shell-${VERSION}`;

// ~18 KB per m-tier thumb → a full cache is on the order of 40 MB.
const THUMB_CACHE_MAX_ENTRIES = 2000;
// Trim is O(keys), so amortize it over many puts instead of running per-put.
const TRIM_EVERY_N_PUTS = 50;
let putsSinceTrim = 0;

// How long a cached thumb serves without any network at all (matches the
// server's max-age). Revalidating on *every* hit — the textbook SWR shape —
// was a measured disaster on scroll: once the browser's HTTP cache expires
// (any phone revisiting after an hour), each cache hit spawned a full 200
// re-download in the background. A 12s scroll re-transferred ~3 MB of thumbs
// the user was already seeing, starving the real tier-upgrade fetches.
const THUMB_FRESH_MS = 3600 * 1000;

// Hard ceiling on how long a cached thumb may be served at all. Between
// THUMB_FRESH_MS and this, bytes are served immediately and refreshed in the
// background; past it the entry is dropped and the request goes to the network
// even if that fails. Without a ceiling, a client that can no longer reach the
// server (expired cert exception, host retired) keeps rendering thumbnails
// indefinitely — entries only ever left via the 2000-entry FIFO trim, so a
// small gallery's thumbs never aged out at all.
const THUMB_MAX_STALE_MS = 30 * 24 * 3600 * 1000;

// Navigating to `/?lv_offline=1` opts into the cached shell without touching
// the network. It's how the unreachable-server page below offers "browse what's
// cached" without that also being the *default* answer to a failed fetch.
const OFFLINE_PARAM = "lv_offline";

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
// (staleness is bounded by revalidation and THUMB_MAX_STALE_MS, not eviction).
async function trimThumbCache(cache) {
  const keys = await cache.keys();
  const excess = keys.length - THUMB_CACHE_MAX_ENTRIES;
  for (let i = 0; i < excess; i++) await cache.delete(keys[i]);
}

async function staleWhileRevalidate(event, request) {
  const cache = await caches.open(THUMB_CACHE);
  const hit = await cache.match(request);
  if (hit) {
    // Age by the Date header captured at put time. A missing/unparseable Date
    // reads as stale, and (being age 0 from the epoch) also as past the
    // ceiling — so junk entries get dropped rather than served forever.
    const storedAt = Date.parse(hit.headers.get("date") ?? "") || 0;
    const age = Date.now() - storedAt;
    if (age < THUMB_FRESH_MS) return hit;
    if (age < THUMB_MAX_STALE_MS) {
      // Stale: serve cached bytes now, refresh in the background. The refresh
      // rides the HTTP cache's conditional machinery when validators exist
      // (the server ETags thumbs), and the put refreshes the stored Date so
      // this URL stays quiet for another freshness window.
      event.waitUntil(
        fetch(request)
          .then(async (res) => {
            if (res.ok) await putAndTrim(cache, request, res);
          })
          .catch(() => {}),
      );
      return hit;
    }
    // Past the ceiling — evict and fall through to the network.
    await cache.delete(request);
  }
  try {
    const res = await fetch(request);
    if (res.ok) await putAndTrim(cache, request, res);
    return res;
  } catch {
    return Response.error();
  }
}

async function putAndTrim(cache, request, res) {
  await cache.put(request, res.clone());
  if (++putsSinceTrim >= TRIM_EVERY_N_PUTS) {
    putsSinceTrim = 0;
    await trimThumbCache(cache);
  }
}

async function cachedShell() {
  const cache = await caches.open(SHELL_CACHE);
  return cache.match("/");
}

async function networkFirstShell(request) {
  // Explicit opt-in from the unreachable page: don't even try the network.
  if (new URL(request.url).searchParams.has(OFFLINE_PARAM)) {
    return (await cachedShell()) ?? unreachableResponse();
  }

  const cache = await caches.open(SHELL_CACHE);
  try {
    const res = await fetch(request);
    // Cache under a fixed key: every SPA route serves the same shell. Only
    // HTML qualifies — a navigation can also land on a non-SPA route (a `/cert`
    // download, the 503 "starting" page), and storing one of those as the
    // shell would serve it back on the next offline open.
    const type = res.headers.get("content-type") ?? "";
    if (res.ok && type.includes("text/html")) cache.put("/", res.clone());
    return res;
  } catch {
    // A failed navigation fetch is ambiguous: it's the same rejection whether
    // the device has no network or the TLS handshake was refused. Answering
    // both from cache was actively harmful for the second case — iOS drops the
    // click-through exception for our self-signed cert readily, and because the
    // navigation never reached the network, Safari had nothing to render its
    // "connection is not private" interstitial for. The app booted from cache,
    // painted the last snapshot, and looked connected while every /api and
    // /thumb request failed. The only way out was clearing site data, which
    // also drops the lv_device cookie and forces a re-pair.
    //
    // navigator.onLine splits the two well enough: false means there is no
    // network interface at all, so the cached shell is exactly right; true
    // means the network is up and it is *this origin* that is unreachable, so
    // say so and offer the escape hatches.
    if (!navigator.onLine) {
      const hit = await cachedShell();
      if (hit) return hit;
    }
    return unreachableResponse();
  }
}

// Synthetic page shown when the server can't be reached but the network is up.
// Three deliberate affordances:
//   Retry            — plain reload, for a host that was just asleep.
//   Reset connection — unregisters this worker and reloads. The next
//                      navigation is uncontrolled, so it hits the network and
//                      the browser can finally render its certificate
//                      interstitial. Cookies and Cache Storage are untouched,
//                      so the device stays paired (unlike "clear site data",
//                      which was previously the only cure).
//   Browse cached    — reloads with ?lv_offline=1 for the read-only view.
function unreachableResponse() {
  const body = `<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>LightView &mdash; can't reach the server</title>
<style>
  :root { color-scheme: dark }
  body { margin:0; min-height:100vh; display:grid; place-items:center;
         background:#0a0a0a; color:#d4d4d4;
         font:400 15px/1.55 system-ui,-apple-system,sans-serif }
  main { max-width:30rem; padding:2rem 1.5rem; text-align:center }
  h1 { font-size:1.25rem; font-weight:500; margin:0 0 .75rem }
  p { color:#a3a3a3; margin:0 0 1rem }
  .hint { font-size:13px; color:#737373 }
  .actions { display:flex; flex-direction:column; gap:.5rem; margin:1.5rem 0 1rem }
  button { font:inherit; padding:.7rem 1rem; border-radius:.5rem; cursor:pointer;
           border:1px solid #404040; background:#262626; color:#e5e5e5 }
  button.primary { background:#404040 }
  button:active { background:#525252 }
</style></head><body><main>
<h1>Can't reach LightView</h1>
<p>The network is up, but this server didn't answer.</p>
<div class="actions">
  <button class="primary" onclick="location.reload()">Retry</button>
  <button onclick="resetConnection()">Reset connection</button>
  <button onclick="location.replace('/?${OFFLINE_PARAM}=1')">Browse cached view</button>
</div>
<p class="hint">If the host is running, its certificate may need approving
again. <b>Reset connection</b> lets the browser show that prompt &mdash; your
pairing is kept. On iOS, do it from Safari rather than the home-screen app,
then install the certificate from Settings to stop it recurring.</p>
</main><script>
async function resetConnection() {
  try {
    const regs = await navigator.serviceWorker.getRegistrations();
    await Promise.all(regs.map((r) => r.unregister()));
  } catch (e) {}
  location.replace('/');
}
</script></body></html>`;
  return new Response(body, {
    status: 503,
    headers: {
      "Content-Type": "text/html; charset=utf-8",
      "Cache-Control": "no-store",
    },
  });
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
