# 0008 — No view-module API; per-gallery enablement plus code-splitting

[← docs index](../README.md) · [frontend](../frontend/README.md) · [plugins](../plugins/README.md)

## Context

The gallery has three views today — the square grid, the justified grid, and the
map — and two more wanted: an infinite scrolling canvas and a virtual folder
hierarchy. That is five, which is enough that "should views be modules behind an
API?" stops being idle.

Two separate wishes were sitting behind that question, and they were being
answered as one:

1. **A gallery should offer only the views its owner wants.** Already solved:
   `views.rs` stores the enabled list per gallery, the switcher renders only
   those, and the idle worker pre-warms only the tiers they ask for
   ([`pipeline/`](../pipeline/README.md)).
2. **A view you never use should not cost anything.** This is what a view-module
   API was reached for. The reasoning was that a modular view could be left out
   of the build, or loaded only when selected.

The second wish turns out to be almost entirely about one view. Measured on the
build at the time of writing:

| | size |
|---|---|
| whole SPA bundle | 445.24 kB (135.85 kB gzip) |
| leaflet + `MapView` within it | 153.19 kB (45.10 kB gzip) |
| leaflet CSS within it | 15.61 kB (6.46 kB gzip) |

The map is a third of the bundle because it is the only view that pulls in a
third-party rendering library. The grids, the canvas, and the virtual folder
view all sit on machinery the main bundle carries regardless — the tier ladder,
the windowed loader, the sort/filter stores — so splitting any of them out
saves application code measured in single-digit kilobytes.

## Options considered

**A view-module API: register views against a host contract.** The bytes are
recoverable this way, but the price is set out in
[`plugins/`](../plugins/README.md) §3 and is not small. Expressing even the
justified grid through a contract means publishing — stably, versioned, forever
— the windowed item list, multi-tier thumbnail URLs, per-item aspect ratios, the
selection model, viewer callbacks, scroll and timeline integration, and geo
points. That is most of the internal frontend API. And by the same document's
reasoning you would not route the core views through it anyway, for performance
and privilege, so the native implementations stay and the contract is pure
addition.

**A cargo feature per view.** Ruled out independently: whatever ships must stay
a single Docker image, and a feature flag decided at compile time cannot answer
a per-gallery question at runtime.

**Native dynamic libraries.** Layout runs in a webview; the LAN web client
cannot load a host `.so` at all, and an IPC round trip per scroll frame is in
the one loop that must not have one.

**A dynamic `import()` per expensive view.** No contract at all — the bundler
splits the chunk at the import boundary and the browser fetches it the first
time the view is opened.

## Decision

No view-module API. Views stay native, enablement stays in `views.rs`, and
"costs nothing when unused" is delivered by code-splitting the views that are
actually expensive — today that is the map alone.

`App.tsx` loads `MapView` through Solid's `lazy()`. The effect, verified in
Chromium against `lightview-headless`: first load fetches `main` and nothing
map-related; selecting Map then fetches `MapView-*.js` and `MapView-*.css` on
demand, the leaflet container renders, and its stylesheet applies. `main.js`
drops from 445.24 kB to 291.82 kB (135.85 → 90.87 kB gzip) and leaflet's CSS
leaves the eager path entirely.

The two mechanisms compose without anything joining them. A gallery with the map
disabled never renders `<MapView>`, so the dynamic import never fires and
leaflet is never fetched — enablement and code-splitting together give the whole
payoff of a view API, with no contract in between.

Sharing implementation between views remains a real need and is met the way the
frontend already meets it: shared behaviour lives in `lib/` as factory functions
taking accessors (`scrollDynamics.ts`, `loadPriority.ts`, `thumbSwap.ts`, …),
with each component keeping its own policy. That is internal reuse, revisable at
any time, and is what the canvas should consume — see
[`frontend/grid-loading.md`](../frontend/grid-loading.md).

## Consequences

- The first open of the map pays a chunk fetch. Over loopback or a LAN this is
  imperceptible, and the map needs network tiles to be useful at all, so it was
  never available in the offline path the service worker covers. No `<Suspense>`
  fallback is used; if that stops being true, one belongs at the `<Show>` in
  `App.tsx`.
- On the desktop the saving is parse and evaluation time rather than download —
  the bundle is local. Real, but smaller than the web client's.
- Adding a view is still editing `App.tsx` and the switcher. That is the
  accepted cost: five views is not enough to earn a registry, and a registry
  would not have removed the edit anyway, only moved it.
- The next view that pulls in its own rendering library should be split the same
  way, and the threshold for doing so is a measurement, not a rule.
- This is not a decision about *plugin* views. [`plugins/`](../plugins/README.md)
  Track C — plugin-supplied HTML in a sandboxed iframe — remains open and has a
  different security model. It is not blocked by this, and this does not
  advance it.
- If a third-party view system is ever genuinely wanted, this decision is
  reversed rather than amended: the contract would need designing against real
  external consumers, which do not exist today.
