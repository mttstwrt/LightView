# server/

[← docs](../README.md)

**Responsible for** the entire HTTP surface: routing, the two trust levels, one
command table, the launch-token session, device pairing, TLS, the event stream,
and uploads.

**Not responsible for** anything a command *does*. Every arm of the table is two
lines — a trust check and a call into [services](../architecture.md#the-layers-and-which-way-they-point).
The server owns who may ask; the services own what happens.

**Depends on** every service, plus [`cache/`](../cache/README.md) and
[`pipeline/`](../pipeline/README.md) through them. **Depended on by** the CLI,
which builds application state and hands it to the listener.

## Trust is a property of the bind, not of the peer

| Level | Reachable from | Covers |
|---|---|---|
| `Device` | any paired client | browse · items · filter · autocomplete · media and thumbnails · tags, ratings, colour labels, notes, sets · `record_view` · `set_default_filter` · thumbnail generation · **trash: move-to-trash, list, restore** · upload · duplicate detection and merge candidates · start a plugin run |
| `Owner` | **a loopback bind only** | copy · move · clipboard · open-with · list a directory (the picker) · **`purge_trash`** · **`merge_duplicates`** |

Because `lightview <dir>` and `--serve` are mutually exclusive, a process has
one listener and therefore one ceiling, fixed at bind. **Nothing widens it.**

Checking the *peer* address instead would be wrong, not merely weaker: a
`0.0.0.0` bind accepts `127.0.0.1`, so a served process asked "is this peer
local?" answers yes to a loopback connection — and anything that can reach the
port from the host gets the filesystem half of the table.

Three placements are decisions rather than accidents:

- **`restore_trash` is `Device`.** It writes a file back to a path the user
  already chose, which is the inverse of a delete the same client was allowed to
  make. What keeps it from being an arbitrary-file-move primitive is the opaque
  entry id — see [storage/](../storage/README.md#the-entry-id-is-not-a-path).
- **`purge_trash` is `Owner`**, because permanent deletion is not
  move-to-trash. A remote client gets the restorable one.
- **`merge_duplicates` is `Owner`**, because its last step trashes several files
  at once after rewriting a companion and stamping an mtime. A phone may find
  duplicates and see the candidates; it may not resolve them.

## The client is told its own trust level

`get_capabilities` returns `{ trust, upload, clipboard }`, and the UI hides what
it cannot do rather than offering it and collecting a 403. **The server enforces
regardless** — this exists only so the UI does not lie.

`clipboard` is a runtime question rather than a compile-time one: the X11
backend fails on a Wayland session without XWayland, and on a process with no
display at all.

## Routes

| Route | Trust | Notes |
|---|---|---|
| `POST /api/invoke` | per command | the one command table |
| `GET /api/events` | `Device` | SSE, one channel, typed events |
| `GET /thumb/{tier}/{*rel}` | `Device` | `js` · `j` · `jm` · `jh` |
| `GET /media/{*rel}` | `Device` | Range/206, HEIC transcode, `?fit=` |
| `POST /api/upload` | `Device` | streamed, staged, renamed |
| `GET /api/dirs?path=` | **`Owner`** | the directory picker: subdirectory names only, never media, never file contents |
| `GET /healthz` · `GET /cert` | bootstrap | unauthenticated |
| `POST /pair/redeem` · `POST /auth/launch` · `POST /auth/password` · `GET /auth/status` | bootstrap | unauthenticated |
| *everything above the bootstrap group* | — | **503 until the initial scan has completed and the watcher is armed** |

The bootstrap group is unauthenticated by necessity — there would otherwise be
no way past the auth layer the first time — and it is a route group, not a trust
level. **No command is reachable unauthenticated.**

`/api/dirs` dispatches *through* the command table rather than checking trust
itself, so the decision is made in exactly one place. A second `require(Owner)`
beside it would be a second thing to keep in step, which is the failure one
table exists to prevent.

### The readiness gate is not politeness

Every guarded route answers 503 until the scan has finished **and** the watcher
is armed. A file arriving between those two moments is in neither, and nothing
would ever notice it. The window is milliseconds when the order is right and the
entire initial scan when it is not, which is why the arming happens before the
gate opens rather than after.

## The launch-token session (loopback)

`lightview <dir>` binds a **random `127.x.x.x`**, not `127.0.0.1`. Cookies are
not port-scoped and `SameSite` is site-scoped, so every other page on
`127.0.0.1` would be same-site with the gallery. The address does the isolation
that `Secure` cannot, because loopback is plaintext.

The token is 32 random bytes, **rotated on every redemption** and single-use,
held in memory and mirrored to `<cache dir>/instance.json` at mode 0600. It is
delivered in the launch URL and **the URL is printed to stdout
unconditionally** — on a headless host `xdg-open` fails and the URL would
otherwise be unknowable, which is also why there is no `--no-browser` flag to
define.

There is no TTL. Single-use plus rotation bounds the exposure, and the file is
readable only by the account that already owns the photos. The named cost: under
a systemd user unit the URL lands in the journal. A rotating single-use token on
a process-private address is an acceptable thing to have in a log; a password
would not be, which is why that is read from stdin instead.

| Case | Behaviour |
|---|---|
| The browser never opened | The bind stays up and callers get 401. Run `lightview <dir>` again: it finds the lock held, reads the live URL from `instance.json`, opens it, and exits |
| The token is redeemed twice | The second attempt is 401. Single-use means single-use |
| A second browser tab | Shares the cookie; no second token needed |
| The process restarted | A **dead end**, deliberately — see below |

### A loopback 401 is a dead end, and says so

On a served bind, a 401 without `WWW-Authenticate` means "not paired" and the
client goes to pairing. On loopback there *is* no pairing flow, and a browser
cannot read `instance.json` to find the new URL — that is precisely the
filesystem access the trust model exists to withhold. So the body says the
session has ended and that starting LightView again will open a new tab, and the
client shows that rather than retrying something that cannot work.

## The `Origin` rule, stated precisely enough to implement

On every state-changing request: **reject when `Origin` is present and is not
byte-equal to this server's scheme + host + port. Allow `Origin` absent.**

Two traps, both of which looser wording walks into. A *site*-level comparison,
or accepting `Sec-Fetch-Site: same-site`, both pass an attacker on another local
port. And *requiring* the header breaks every non-browser client, including the
`curl` recipe — browsers always send `Origin` on a cross-origin POST, so absence
is safe.

Under `--serve` the same check uses `Sec-Fetch-Site: same-origin` instead,
because a `0.0.0.0` bind has no single origin to name.

A `GET` is exempt: a cross-origin `GET` cannot do anything a plain `<img src>`
could not already do, and requiring the header on reads would break the recipe
for no gain.

## Pairing and passwords

A pairing code is minted from a shell — `lightview pair` — because **nothing is
`Owner` under `--serve`**, so a web UI could not be the answer even if one
existed. `lightview devices` lists them and `lightview devices revoke <id>`
undoes one; without that a lost phone would stay paired forever.

**Pairings live in the state directory, not per gallery.** A phone paired to
this machine is paired to every gallery this machine serves, now or later. That
is a real widening, and it is the cost of removing the per-gallery cookie-name
mint; it is consistent with all paired devices being equally trusted.

An optional gallery password is a second factor on an inactivity window. It is
read from **stdin**, never argv, where it would land in shell history and `ps`.
A client past the window gets 401 with `WWW-Authenticate: LV-Password`; the
frontend absorbs that, raises one modal however many requests hit it at once,
and retries.

## Events: one channel, typed lag recovery

There were two channels, because the filesystem channel's subscriber count
doubled as the "is anyone watching?" signal for the idle worker. That signal is
gone — [`Activity`](../pipeline/README.md#the-idle-worker) is the whole of it now
— so the reason is gone and the second channel with it.

But the two carried different lag contracts, and merging them naively collapses
both into the expensive one. So: **every event names the domain it belongs to,
and on `Lagged` the relay emits one `Resync` naming the domains that may have
been missed.** A client re-fetches exactly those.

| Event | Carries |
|---|---|
| `fs-changed` | `{added, removed}` — **what changed**, so a client splices rather than re-fetching. One phone upload used to cost every connected client a full-library payload |
| `items-changed` | `{paths}` — plural, and one per *operation*. Tagging 500 photos is one event, not 500 |
| `tags-indexed` | the vocabulary moved; the item list moved with it only under an active filter |
| `job-progress` | throttled to one a second — the only high-rate producer on the channel |
| `job-finished` | never throttled, or a run appears to stall at 99% |
| `resync` | `{domains}` — you may have missed something in exactly these |

A keep-alive comment goes out every fifteen seconds. A phone's radio and every
intermediary between it and the server will drop an idle connection, and a
silent drop is what turns "reconnect and re-fetch" into "sit on a confidently
wrong grid".

## TLS

Always on for a non-loopback bind, because browsers gate the clipboard and
upload APIs behind a secure context. Self-signed ECDSA, persisted in the data
directory, re-minted when the SAN list changes.

The certificate is `CA:TRUE` with **X.509 `nameConstraints`**, so a device that
installs it to stop the warning is not trusting this key for the whole internet
— only for the names the constraint permits. `GET /cert` serves it
unauthenticated: every handshake hands out the same certificate, so it leaks
nothing, and it has to be reachable *before* the browser trusts the connection
enough to pair.

## Uploads

Four things, each load-bearing, and each a bug the previous implementation had:

- **Temp file in the destination directory, then rename.** Otherwise the watcher
  fires on an empty file and records `file_size = 0` with the upload time as the
  capture time — and because the insert is `INSERT OR IGNORE`, **nothing ever
  corrects them**. The thumbnail self-heals; the metadata does not.
- **Stamp the mtime on the temp file before the rename**, or every uploaded
  photo sorts as "today" forever.
- **Collision dedupe with `RENAME_NOREPLACE`**, so two phones uploading
  `IMG_0001.jpg` cannot lose the race.
- **Remove the temp file on every error path.** A dropped connection or an
  `ENOSPC` otherwise leaves a `.lv-upload-*.tmp` behind permanently — invisible
  to both the scan and the watcher, since it has no media extension, accumulating
  in the one tree this design tells the user is safe to grep and back up.

## Invariants a caller must uphold

- **Never trust a peer address for authorization.** Trust comes from
  `AppState::trust`, decided at bind.
- **Every command arm starts with `require(state, Trust::…)`.** A new arm
  without one is a new hole; there is no default.
- **A path from a client is a `RelPath` before it is anything else.** The
  newtype validates every segment textually, so exactly one spelling of a path
  reaches the database and nothing with a `..` in it reaches the filesystem.
- **Nothing a client sends may name a program.** `open_with` takes an *index*
  into server-side configuration; `run_plugin` takes a plugin *name*, which the
  installer scan either matches or does not.

## A local session ends with its last window

`lightview <dir>` is started by a click nobody associates with a process
lifetime — "Open with LightView" in a file manager. Closing the window used to
leave it serving nothing while holding the gallery lock, its watcher, its idle
worker and its thread pool, until someone went looking for it.

**The signal already existed: the SSE stream.** A browser tears the `/api/events`
connection down when its tab closes, so a count of live streams is a count of
open windows — see [`util/presence`](../../src-rust/src/util/presence.rs), which
lives in `util` rather than here because both layers report into it: this one
counts windows, and the background services count durable work in flight.

Four things decide whether that is right rather than merely clever:

- **Local mode only.** `--serve` passes a future that never resolves. A
  deployment must outlive every client, and a phone locking its screen is not a
  shutdown request.
- **Armed only after a first window has ever opened**, or the process races the
  browser it was started for.
- **Five minutes of continuous zero, re-checked each tick.** Not thirty
  seconds: browsers discard backgrounded tabs under memory pressure, closing
  the socket while the tab stays in the strip. Waiting longer costs nothing,
  because a second `lightview <dir>` inside the window attaches to the running
  process. The *other* direction is safe only because the bind is loopback — a
  suspended laptop does not drop a connection whose endpoints are both local,
  so a closed lid is not a closed window.
- **Never mid-write.** Graceful HTTP shutdown covers requests and *not* the
  writes that matter: the open-time enrichment pass and the hourly companion
  sweep run detached and write sidecars for minutes after the first screen is
  painted. Both hold a busy guard, and the watchdog waits for it.

`instance.json` is removed before the listener stops, so a launch racing the
exit starts its own session rather than opening a tab at a dying port.

A count of streams is not *exactly* a count of windows, and the two places it
differs are both harmless here. `/pair` renders before the main app mounts and
holds no stream, but it belongs to the `--serve` flow, where the watchdog does
not run. A window whose stream has 401'd sits in the "session ended" state
holding none either — but that state only arises across a restart, so it can
never be the last window of the process it would end. Anything transient (a
reload, an `EventSource` reconnect) dips the count and is absorbed by the
grace.

**Presence is not activity.** There is already an idle signal —
[`pipeline::serve::Activity`](../pipeline/README.md), the time of the last
user-driven request — and reusing it here would end a session under someone
reading a page, since a tab parked on a grid makes no requests for hours. That
module's own doc warns against the mirror of the mistake, reading the
subscriber count as activity. Both directions are named where someone would
reach for the wrong one.
