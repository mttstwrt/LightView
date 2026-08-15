# Frontend

[← docs index](../README.md) · [architecture](../architecture.md)

A SolidJS single-page app in `src-solidjs/`. One bundle runs in two very
different places: inside the Tauri webview on the desktop, and as an ordinary
web page served to a phone or laptop over the LAN. Almost every non-obvious
decision below follows from that, or from the fact that WebKitGTK decodes
images on the main thread.

**Responsible for:** all rendering and interaction — the two grids, the
full-resolution viewer, the top bar and its menus, the panels (tags,
duplicates, trash, auto-tagging, map), and the pairing/password flows. Also the client-side
caches: the service worker, the boot snapshot, and the in-memory viewer cache.

**Not responsible for:** anything the backend can do. There is no image
decoding, no path manipulation, and no policy about what a device may access —
that is the [`/api/invoke` allowlist](../remote/README.md), enforced server-side
regardless of what the client asks for.

**Public interface:** `lib/ipc.ts`. Every backend call in the app goes through
it, and it is the only module that knows which transport is in use.

**Depends on:** [`remote/`](../remote/README.md) in the web build, Tauri's
`invoke()` on the desktop, and the shapes in `lib/types.ts` — which must stay in
step with the Rust serde structs on the other side.

## Layout

| Path | What lives there |
|---|---|
| `stores/` | Global signals: `galleryStore`, `viewerStore`, `settingsStore`, `filterStore`, `tagStore`, `pluginStore`, `taggingStore`, `capabilitiesStore`, `uploadStore`, `thumbnailProgressStore` |
| `lib/` | The non-visual machinery: `ipc`, `runtime`, `types`, `scrollHost` (the gallery scrolls an element, not the document — see [`grid-loading.md`](grid-loading.md)), plus the grid primitives both grids share |
| `components/gallery/` | `GalleryGrid`, `JustifiedGrid`, `ThumbnailCell`, `SelectionBar` |
| `components/viewer/` | `MediaViewer`, `VideoPlayer`, `InfoPanel` |
| `components/topbar/` | `TopBar`, `FilterBar`, `SortMenu`, `CommandMenu`, `ViewSwitcher`, `SettingsMenu`, `TitleBar` — how these divide between things you do and things you set is [`chrome.md`](chrome.md) |
| `components/auth/` | `PairApp`, `PasswordModal` — the web-only bootstrap flows |
| `components/shared/` | `ContextMenu`, `ScrollBar` (its touch behaviour, and why the grids care, is in [`grid-loading.md`](grid-loading.md)), `ConfirmButton` |
| `components/debug/` | `DebugOverlay`, `Sparkline`, `DevtoolsApp` |
| `components/map/` | `MapView` — the one component behind a `lazy()` split (see below) |

### Views are native, and the expensive one is split out

The five views — square grid, justified grid, map, and the two unbuilt ones —
are ordinary components, wired directly in `App.tsx`. There is no view
registry or module API, and [decision 0008](../decisions/0008-no-view-module-api.md)
records why: the thing an API was wanted for is that an unused view should cost
nothing, and that is a bundling question. Per-gallery enablement (`views.rs`,
surfaced through `enabledViews` in `galleryStore`) plus a dynamic `import()` on
the views that carry their own libraries delivers it with no public contract.

`MapView` is the only one that qualifies, and by a wide margin: leaflet plus its
CSS is 153 kB of a 445 kB build. It loads through Solid's `lazy()`, so a gallery
browsed in the grids never fetches it, and a gallery with the map disabled never
even triggers the import. Every other view sits on machinery the main bundle
carries regardless — splitting them would save single-digit kilobytes. Split the
next view that brings its own renderer; measure first.

### Reading the build output

The chunk names mislead, and it is worth knowing why before chasing one. There
are two HTML entries — `index.html` and `devtools.html` — so Rollup hoists what
they share into a common chunk and names it after one member, which is neither
stable nor descriptive. Two artifacts that look alarming and are not:

- **`Sparkline-*.css`, ~53 kB.** This is the whole Tailwind stylesheet, not
  anything to do with the sparkline. `styles/global.css` is imported by both
  `index.tsx` and `devtools.tsx`, so it attaches to the shared chunk and inherits
  its name. It belongs on the critical path and is 9.5 kB gzipped.
- **`Sparkline-*.js`, ~37 kB, `modulepreload`ed from `index.html`.** The shared
  chunk itself: the Solid DOM runtime and `lib/ipc.ts` (34 kB of source on its
  own), both of which the main entry needs immediately. Lazy-loading
  `DebugOverlay` — the only genuinely optional thing in it — was measured at
  6.9 kB raw / **2.3 kB gzipped** off first load, and renames the chunk to
  `ipc-*`. Not taken: a lazy boundary is not worth 2 kB, and the name is
  cosmetic. Recorded so the measurement is not repeated.

`solid-devtools` is a devDependency and its Vite plugin compiles out of
production builds; the shipped bundle contains no instrumentation. Checked, for
the same reason.

## The IPC boundary

`lib/ipc.ts` exports one typed function per backend command. Internally it picks
a transport: Tauri's `invoke()` on the desktop, `POST /api/invoke` in a browser.
Nothing above it branches on which one is active.

The bridge keeps that seamless in two places that would otherwise leak:

- **Password challenges.** A `401` carrying `WWW-Authenticate: LV-Password`
  never reaches the caller. The transport raises a challenge, waits for the
  modal, and retries the original request. Concurrent 401s share one pending
  promise, so a grid that fires twenty requests at once produces one prompt
  rather than twenty stacked modals.
- **Unpaired devices.** A `401` *without* that header means the cookie is
  missing or revoked; the transport emits `NOT_PAIRED_EVENT` and the router
  redirects to `/pair`.

`lib/runtime.ts` holds the mode detection this depends on: `isTauri()` /
`isWeb()`, a reactive `isMobile()` (web client at a narrow viewport, threshold
matched to Tailwind's `sm` so CSS and JS agree), `hasTouch()` (a capability, not
a viewport guess — true on touchscreen laptops too), and `safeListen`, a
drop-in for Tauri's event listener that no-ops in the browser so call sites need
no branch.

Commands that exist only on the desktop — file copy/move, plugin execution,
render config — are absent from the allowlist rather than hidden in the UI.
`capabilitiesStore` asks the server what this client may do and the components
render accordingly, but the enforcement is server-side.

### A command absent from the allowlist is not a command the web client may call

`lib/memoryPressure.ts` polled `get_memory_status` from both runtimes. It is not
on the allowlist and never was, so in a browser every five-second cycle `403`'d
into an empty `catch` — the viewer cache's pressure-based eviction simply did
not exist on the web, and nothing said so. `tsc` cannot see this, and neither
can the Rust tests; it turned up by driving the SPA against
`lightview-headless`.

Allowlisting it would have been the wrong repair: it reports the *server's* RAM,
and sizing a phone's image cache from a NAS's free memory is meaningless. Each
runtime now reads a signal it actually has — free host RAM over IPC on the
desktop, `navigator.deviceMemory` in a browser. The browser one is sampled once
rather than polled, because it is a static device class and the live alternative
(`performance.memory`) measures the JS heap, which is not where decoded images
live.

The general shape: an empty `catch` around an IPC call is how a
transport-specific failure stays invisible. Log it, at least once.

## Invariants

**Grid cells are keyed by path, never by index.** Both grids render with a
path-keyed `<For>`, so a cell that stays on screen across a scroll keeps its
exact DOM node and `<img src>` and the webview never re-decodes it. An
index-keyed `<Index>` rewrites every slot's `src` as the window shifts, which is
what caused per-row scroll flicker.

**Prune per-path state surgically, never wholesale.** When `props.paths`
changes, both grids drop only the paths that actually disappeared. The
URL-assignment effect has already run for that update and will not fire again
until the visible range moves, so wiping surviving cells blanks the grid until
the next scroll — visible as the whole grid going grey after deleting one image.

**Speculation is never free.** Landing-zone warms, look-ahead precache, and
background crawls all land on the same bounded rayon pool as the cells the user
is looking at. Both grids gate speculation behind "nothing the viewport is
waiting on is outstanding", and both use a much smaller batch for speculation
than for the visible drain, because nothing can preempt a batch once issued —
the batch size *is* the worst-case delay before a scroll onto cold cells can get
CPU back. Measured: enabling background precache at high zoom in `JustifiedGrid`
more than doubled time-to-sharp for on-screen cells (6.8 s → 15.2 s mean over
four cold runs of a three-viewport scroll).

**Two single-flight slots, not one.** `inFlightFetch` carries the visible drain;
`inFlightWarm` carries speculation. They were one slot once, and a single
64-image background batch could hold it for an entire session, leaving every
subsequent visible request to the one-at-a-time generate-on-serve path. Both
slots reset in `finally`, not as a trailing statement — the early returns for a
stale generation would otherwise wedge the slot permanently. Both live in
`lib/fetchLoop.ts` now, which is also what re-arms the drain when a batch
settles: the drain slot pokes the loop, the warm slot deliberately does not.

## Client-side caches, and how they lie

Three caches persist across a reload on the web client, and every one of them
can make a dead server look alive:

| Cache | Where | Bound |
|---|---|---|
| Thumbnails | service worker Cache Storage (`lv-thumbs-*`) | 2000 entries FIFO, 1 h revalidation, **30-day hard ceiling** |
| Sorted item list | IndexedDB (`loadBootSnapshot()`) | same 30-day ceiling, honoured against its own `savedAt` |
| Decoded viewer images | in memory (`viewerCache.ts`) | evicted under memory pressure |

The boot snapshot exists because the web boot is network-bound: gallery info and
the sorted items are several round-trips away, and painting ThumbHash
placeholders plus service-worker-cached thumbnails from IndexedDB gets a
recognisable grid on screen before the first byte arrives. Fresh data replaces
it as soon as it lands — it is a boot-time stand-in, never a source of truth.

The time bounds are the correction to a real failure. Neither cache was bounded
originally, so with the origin unreachable the service worker's network-first
navigation served the cached shell, the snapshot repainted the whole grid, and
cached thumbnails rendered — while every `/api`, `/thumb`, and `/media` request
failed. The app looked connected. Worse, the navigation never reached the
network, so the browser had nothing to render a certificate interstitial for;
on iOS the only cure was clearing site data, which also drops `lv_device` and
forces a re-pair.

Three changes fixed it, and they belong together:

1. `networkFirstShell` serves the cached shell only when `navigator.onLine` is
   false — genuinely offline. Network-up-but-origin-dead gets a synthetic
   recovery page whose **Reset connection** button unregisters the worker and
   reloads, so the next navigation is uncontrolled, reaches the network, and the
   browser can show its certificate prompt. Cookies and Cache Storage are left
   intact, so the pairing survives. `?lv_offline=1` opts back into the cached
   shell deliberately.
2. Both persisted caches got the 30-day ceiling above. A client that has not
   reached the server in a month would otherwise open onto a confident-looking
   view of a gallery it can no longer see.
3. In-app, a failed `getBootState()` sets `serverUnreachable` and shows
   `ConnectionBanner` with the same two actions, so the cached-shell path is
   never silent either.

## The WebKitGTK constraint

WebKitGTK decodes images on the main thread. That single fact is the premise
behind the decode gate, the staged tier upgrade, and most `isTauri()` branches
in the grids — see [`grid-loading.md`](grid-loading.md).

No harness available in this repository can measure that platform. Where the
source says a win is "reasoned, not demonstrated", that is accurate, and it is
worth preserving as-is rather than tidying into a claim the measurement does not
support. Where a comment *does* cite numbers, they came from a real run and
should not be edited away either.

## Verifying a change

`tsc --noEmit` catches type drift against `lib/types.ts`, which is most of what
goes wrong at the boundary. It cannot cover the grids at all — scroll windows,
tier selection, eviction, and decode timing are all runtime behaviour.
[`build-and-verify.md`](../build-and-verify.md) describes driving the real SPA
against `lightview-headless` in headless Chromium, which is the only way to see
those paths execute.
