# Remote access

[← docs index](../README.md) · [architecture](../architecture.md)

`http_server/` is an axum server that serves the same SolidJS app to a browser
on the LAN that the desktop webview loads over Tauri IPC. The SPA bundle is
identical; only the transport differs. Everything on this page exists because
that transport crosses a trust boundary the desktop one does not.

**Responsible for:** binding and TLS, serving the SPA and its assets, the
media/thumbnail/GIF/thumbhash routes, the SSE change stream, device pairing and
password re-authentication, uploads, and `/api/invoke` — the allowlist that
decides which backend commands a paired device may reach.

**Not responsible for:** any domain logic. Every `/api/invoke` arm calls the
same `*_impl` function the corresponding `#[tauri::command]` calls, and the
media routes call the same `thumb_serve` / `gif_serve` bodies as the desktop's
custom protocol handler. If a behaviour differs between desktop and web, that
is a bug unless the command is deliberately absent from the allowlist.

**Public interface:** `http_server::start` / `RemoteAccess` (held in
`AppState::remote_server`), `HttpConfig::{local_only, remote}`, and the routes
themselves.

**Invariants callers must uphold:** a device cookie is only ever read through
`middleware::device_cookie` and only ever written through
`middleware::device_cookie_header`, both given the name `devices::cookie_name`
returns for the currently open gallery — a hard-coded `lv_device` re-introduces
the cross-gallery collision described below. Every route that resolves a
filesystem path calls `path_in_gallery` first.

**Depends on:** [`cache/`](../cache/README.md) (device rows, pairings, and
per-gallery settings all live in the gallery's `cache.db`),
[`pipeline/`](../pipeline/README.md) via the serve modules, `commands/*_impl`,
and [`tagging/`](worker-tagging.md).

**Depended on by:** the web build of [`frontend/`](../frontend/README.md), and
`lightview-worker`.

## Two servers, not one

The desktop app runs a *second*, loopback-only instance of this same server with
`HttpConfig::local_only()`: no auth, OS-assigned port, no static files. It exists
for one reason — WebKitGTK refuses non-`http(s)` schemes for `<video>` elements,
so the custom `lightview://` protocol cannot feed the player. Keeping it separate
from the LAN server means enabling remote access never adds an auth layer to the
desktop webview's own requests.

The LAN server (`HttpConfig::remote`) binds `0.0.0.0`, always over TLS, with
per-device cookie auth and the SPA served from the copy compiled into the
binary.

## Where the SPA comes from

`web_assets` embeds the built `dist/` in the executable with `rust-embed`, so a
deployment is one file rather than a binary plus a directory that has to stay in
step with it — a mismatch there showed up as a blank page or a silently stale
bundle. The whole build is a few hundred KB.

`HttpConfig::web_root` (the `--web-root` flag on `lightview-headless`) overrides
it with a directory served by `ServeDir`. That is the development path: point it
at a live Vite output and a frontend rebuild takes effect with no recompile. In
debug builds rust-embed does not embed either — `Assets::get` reads `dist/` off
disk at the path recorded at compile time — so the same is true of a plain debug
build with no flag. Release builds carry the bytes.

Both branches sit behind the same cache-policy middleware, so the two paths
cannot drift: hashed asset filenames get a year of `immutable`, the shell gets
`no-cache`. The embedded handler emits an ETag over each file's content hash,
because "revalidate every load" is only cheap if there is a validator to answer
`304` against.

The folder must exist when the crate is compiled — see
[`build-and-verify.md`](../build-and-verify.md).

## Layers, and why routes are grouped the way they are

```
/healthz                                     no auth (liveness)
/cert  /pair/redeem  /auth/password  /auth/status
                                             bootstrap — cannot be behind auth
/media  /thumb  /gif-atlas  /thumbhash
/api/invoke  /api/events  /api/upload        auth_layer (device cookie)
everything else                              SPA static files + cache policy
```

The bootstrap group is unauthenticated by necessity: there would otherwise be no
way past the auth layer the first time. `/cert` belongs there for the same
reason one step further out — the whole point of downloading the certificate is
to fix a browser that cannot establish trust yet, which is upstream of having a
cookie at all. It is served unauthenticated as
`application/x-x509-ca-cert`; this leaks nothing, since every TLS handshake
hands the same certificate to anyone who connects.

Static assets sit outside the auth layer because the app shell has to load
before the client can pair. The shell contains no gallery data.

## Authentication

A paired device holds a cookie `lv_device_<gallery>=<device_id>.<secret>`. The
server stores only a SHA-256 hash of the secret, keyed by device id, so a leaked
database alone grants nothing.

### Why the cookie name carries a gallery id

Cookies are scoped by host and **not** by port. Two galleries served from one
machine on different ports therefore share a jar entry, and pairing a browser
with the second silently un-paired it from the first — the second `Set-Cookie`
simply overwrote the value the first had issued. iOS home-screen web apps
escaped it only because each gets its own storage container; a desktop browser
has one jar and lost a pairing every time.

`devices::cookie_name` suffixes the name with a random per-gallery id minted on
first use and kept in `gallery_meta`, so it travels with the gallery rather than
with whichever server is currently serving it. Two processes can hold the same
gallery open, so the mint is an `INSERT OR IGNORE` followed by a re-read: both
must agree on the name or each keeps issuing a cookie the other ignores.

A bare `lv_device` is still accepted, for devices paired before this existed;
`auth_layer` re-issues the same value under the gallery's name on the first
request that uses it, so nobody has to pair again. The legacy cookie is
deliberately **not** expired — it is precisely the entry two galleries share, so
clearing it would un-pair whichever gallery has not yet re-issued.

SHA-256 rather than argon2 is deliberate and worth understanding before
"fixing" it: the secret is 32 random bytes, so the brute-force resistance of a
slow hash buys nothing against a search space that size, and verification runs
on **every thumbnail request** — a deliberately slow hash there would be a
self-inflicted denial of service. Comparison is length-checked and
constant-time.

Enrollment goes through a short-lived `remote_pairing` row, redeemed once:
either a 6-digit PIN (typed by hand) or a 32-byte hex token (embedded in a QR
code). The PIN is safe *because* of the 10-minute TTL and single-use
redemption, not because six digits are hard to guess.

An optional gallery-wide password layers on top. When set, a device must
re-prove it after `inactivity_secs` of silence; the server answers `401` with
`WWW-Authenticate: LV-Password`, which the frontend turns into a modal and a
retried request. Both the password hash and the threshold live in
`gallery_meta`, so they travel with the gallery — matching the per-gallery scope
of the device pairings themselves.

## The `/api/invoke` allowlist

Authentication decides *whether* a device may call. The `dispatch` match in
`api.rs` decides *what* it may do, and it is the second boundary:

- Anything not named in the match is `403`, even if the client forges the name.
- Host-level operations are simply absent — file copy/move, plugin execution,
  render config. There is no flag that enables them.
- Delete-shaped commands sit behind an additional per-gallery
  `remote.allow_delete` gate, checked in match guards *before* the arms that
  implement them.
- The reserved worker id `tagging::local::LOCAL_WORKER_ID` identifies the
  server's own in-process executor; `worker_announce` rejects it explicitly so a
  paired device cannot impersonate the server in the worker registry.

That match is long and looks like boilerplate. It is not: **the verbosity is the
security property.** One arm per permitted command, with the argument shape
spelled out locally, is what makes the reachable surface auditable by reading.
Any abstraction that made adding a command easier would make widening the
allowlist easier too. See
[decision 0005](../decisions/0005-remote-invoke-is-an-allowlist.md).

Argument structs use `rename_all = "camelCase"` so the JSON the frontend already
builds for Tauri's `invoke()` works unchanged through this bridge — that is what
lets `lib/ipc.ts` swap transports without changing a single call site.

## Path confinement

Every route that touches a filesystem path calls `path_in_gallery`, which
canonicalizes the candidate per request and compares it against the root
canonicalized once at gallery open. Without it, a non-loopback bind exposes the
entire host filesystem.

It returns **404, not 403**, so "outside the gallery" is indistinguishable from
"missing" — a 403 would confirm the existence of files the caller has no
business knowing about. `/media`, `/thumb`, and `/gif-atlas` all call it;
`/thumbhash` does not need to, because it can only read a blob that is already
in the cache database.

Uploads are the one write channel, and enforce confinement three times over,
because the filename comes from an untrusted device: the name is reduced to its
basename with traversal rejected, the extension must resolve to a known
`MediaType`, and the resolved destination is confirmed inside the gallery root
before any byte is written. Once the file lands, the ordinary fs-watcher ingests
it — uploads do not have their own indexing path.

## TLS

Remote serving is always HTTPS: browsers gate the async Clipboard API and
friends behind a secure context, and half the web client stops working without
one. The certificate is self-signed, ECDSA, persisted at `<exe_dir>/data/tls/`,
and regenerated when the LAN IP changes or expiry nears.

It carries `basicConstraints CA:TRUE` and `keyCertSign` alongside its serverAuth
EKU — not because it signs anything, but because iOS and macOS only offer the
full-trust toggle for CA certificates. Without those bits there is no way to
durably trust the server on an iPhone, and a click-through exception is
per-origin and short-lived. Bumping `CERT_SCHEMA` forces regeneration for
material that predates such a change.

### SANs behind NAT or Docker

`detect_lan_ip()` sees only the interface this process routes through. Inside a
container that is the bridge address, never the host address clients dial, so
the certificate fails hostname verification for everyone. Name the reachable
address explicitly with `--tls-san` (repeatable, comma-separated) or the
`LIGHTVIEW_TLS_SAN` environment variable; `docker-compose.yml` wires the latter
through.

Getting this wrong fails quietly. Desktop browsers keep working on a
click-through exception, but iOS drops that exception readily, and a standalone
PWA cannot render the prompt to re-accept it. Adding a SAN re-mints the
certificate, which re-prompts every paired browser once and breaks any
`lightview-worker` certificate pin — re-pair those with `--trust-new`.

## Change notification

The fs-watcher publishes batches to `AppState::fs_change_tx`. The desktop gets
them as a Tauri event; a browser has none, so `/api/events` subscribes to that
broadcast and relays each batch as SSE. Late subscribers see only changes from
their subscription onward, which is what we want — a reconnecting phone should
re-fetch state, not replay history.

The same SSE stream carries `tagging-job` and `tagging-workers` events, merged
from a *separate* broadcast channel. They are kept separate upstream because
`fs_change_tx`'s subscriber count doubles as the "is any web client connected?"
signal for the idle backfill worker, and tagging traffic must not make the
server think a user is present.

## When the client cannot reach the server

Two client-side caches used to make a dead server look alive: the service
worker's `lv-thumbs-*` Cache Storage and the whole sorted-item list in
IndexedDB. With the origin unreachable, the worker's network-first navigation
fell back to the cached shell, the snapshot repainted the entire grid, and
cached thumbnails rendered — while every `/api`, `/thumb`, and `/media` request
failed. Worse, the navigation never reached the network, so the browser had
nothing to render a certificate interstitial *for*; on iOS the only cure was
clearing site data, which also drops `lv_device` and forces a re-pair.

The fix spans both sides and is described from the client's perspective in
[`frontend/`](../frontend/README.md). The part that matters here: a recovery
page's **Reset connection** button unregisters the service worker and reloads,
so the next navigation is uncontrolled, hits the network, and lets the browser
finally show its certificate prompt — with cookies and Cache Storage left
intact so the pairing survives.

## Testing this without a display

`lightview-headless` boots this entire server with no WebKitGTK, so every route
here can be driven with `curl` and a real browser. See
[`build-and-verify.md`](../build-and-verify.md).
