# frontend/

[← docs](../README.md)

**Responsible for** the SolidJS bundle: booting, the stores that hold gallery
state, the justified grid, the full-resolution viewer, and the chrome around
them.

**Not responsible for** deciding what a client may do. It hides what the server
would refuse so the UI does not lie, and that is presentation — the enforcement
is one `require` line per command arm. See
[server/](../server/README.md#the-client-is-told-its-own-trust-level).

**Depends on** the [server](../server/README.md) and nothing else. **Depended on
by** the binary, which embeds the built bundle at compile time.

## One runtime

There used to be two clients — a desktop webview and a browser — and almost
every module carried a branch for the difference. There is one now, so
`isTauri()`, `isWeb()`, the event-shim and the dual-default capabilities store
are gone, along with the workarounds they gated: the canvas GIF renderer for an
engine whose `<img>` animation leaked, the decode gate for an engine that
decoded on the main thread, and the window chrome for a frameless window.

What survived that deletion needed care rather than a mechanical edit.
`isMobile()` was `isWeb() && width < 640`, so removing `isWeb()` silently turns
it into "narrow window" — and a desktop browser dragged narrow would take the
mobile path, whose default cell size is a *two-column* layout. It is now
viewport width **and** touch capability, which is a capability rather than a
guess.

## `lib/ipc.ts` is the only module that talks to the backend

Everything is `POST /api/invoke` plus the media and thumbnail routes. Two
behaviours are absorbed there so they never leak to callers, and each one is a
bug the previous arrangement had:

- A **401 with `WWW-Authenticate: LV-Password`** raises a challenge, waits for
  the modal and retries — and **concurrent 401s share one pending promise**, or
  a grid firing twenty requests produces twenty stacked modals.
- A **401 without it** means the credential is missing or revoked. On a served
  bind that is "not paired" and the client goes to pairing; on a loopback bind
  it is a dead end, because there is no pairing flow and a browser cannot read
  `instance.json` to find the new URL.

The path encoding rule lives here too: **percent-encode each segment, leave `/`
literal.** A single `encodeURIComponent` over the whole path 404s every file in
a subdirectory.

## Boot

```
index.tsx   redeem ?t= and clear it from the address bar
            fetch auth status  (which kind of 401 will this be?)
            mount
App.tsx     capabilities · gallery settings · plugins
            seed the default filter into the bar
            one items query
            subscribe to the event stream
```

**Redemption happens before anything authenticated goes out**, and the query
string is cleared with `history.replaceState` before the round trip completes: a
`?t=` left in place survives into session restore and history, where it is a
spent credential at best.

**Auth status is fetched before mount** because a 401 can arrive with the app's
first request, and learning which kind it is afterwards is a race the user loses
by being shown the wrong dead end.

Three failures are distinguished, because they mean different things. A **503**
is the readiness gate — the scan has not finished — so the answer is to wait and
ask again, however long it takes, behind a screen that says so. A **401** is the
credential. Anything else is the network, which is what the connection banner is
for.

The default filter is **seeded into the bar** rather than applied invisibly: a
grid showing a subset with an empty filter box is indistinguishable from a
gallery that lost photos.

## Stores

| Store | Holds |
|---|---|
| `galleryStore` | the one item list, the groups, the selection, and the event handler |
| `settingsStore` | per-client display preferences, the gallery's two settings, capabilities, sort state |
| `filterStore` | the query text, the rating control, autocomplete state |
| `viewerStore` | whether the viewer is open, which index, the info panel |
| `activityStore` | installed plugins, the current run, outstanding thumbnail work |

**One list, one query.** The filter used to be a client-side pass over a
separate sorted list, kept apart so changing the sort did not re-run it. The
filter now compiles into the same statement the sort orders, so the two signals
collapse into one.

**Display preferences are per client, for every client.** They live in
`localStorage`, not in a file inside the gallery: a file in the gallery is
per-*gallery*, so two desktops mounting one share would fight over thumbnail
size — the exact thing a per-client preference exists to prevent. The gallery's
own settings file holds exactly two keys: the default filter, which is user
intent and must survive a cache rebuild, and trash retention, which is
hand-edited only because it is the one setting that deletes data.

## Events

One `EventSource`, one handler, fanned out to the stores.

**`onopen` after the first re-runs boot** rather than replaying history, and
that is a requirement rather than a nicety: `EventSource` reconnects silently,
and on a phone that happens constantly — screen lock, Wi-Fi to LTE,
backgrounding. Without it the client sits on a confidently wrong grid
indefinitely.

A `fs-changed` removal **splices**; an addition falls back to one refetch,
because the client cannot know where a new item sorts or whether it matches the
active filter. An `items-changed` batch patches row by row up to a dozen and
takes one query beyond that.

## Chrome

Everything you *do* is one command list, rendered two ways: a dropdown in the
desktop top bar, and a sheet behind a floating button in the phone's thumb zone.
Everything you *set* is the settings panel, and opening it is the last entry in
that list.

Ordering is source order. The scheme this replaced let thirteen call sites each
pick a magic number, four of which collided, because nobody ever saw all
thirteen together.

Two rules the command list keeps: the trigger and the Settings entry render from
local state alone — Settings → Connection is where the certificate install
lives, so gating the only route to it on a server round trip deletes the
recovery action in exactly the situation that needs it. And selection-scoped
actions stay out, because the selection bar and the context menu own those.

## The grid

One justified grid. See **[grid-loading.md](grid-loading.md)** for how it
decides what to request and when — the tier ladder, the two-zone render window,
and why a scrub assigns nothing.

## Invariants a caller must uphold

- **Only `lib/ipc.ts` talks to the backend.** A component reaching for `fetch`
  is how the retry, the challenge and the path encoding get reimplemented
  slightly differently.
- **Never assume a capability.** `capabilities()` defaults to the *narrower*
  answer, because a UI that briefly hides an action it turns out to have is a
  flicker, while one that briefly offers an action the server will refuse is a
  403 the user caused.
- **Paths are gallery-relative strings**, encoded per segment at the boundary.

## The item payload carries a sort date, not a capture date

`SortedItem.date` — the field the grid, the group headers and the scrollbar's
date labels all read — is `COALESCE(date_taken, mtime)`, computed server-side.
It is deliberately not named `date_taken`: `MediaMeta.date_taken`, which the
info panel shows, is the camera's timestamp and is frequently null, and the two
must not be confused by a reader of either. A scrubber labelled from a
different value than the list is ordered by is a scrubber that lies, so both
come from the one expression named in
[`query/`](../query/README.md#the-date-a-file-sorts-by-is-not-the-date-it-was-taken).

The info panel prints `Taken …` or `Modified …` accordingly, which is also the
explanation for where a file sits in the scroll.
