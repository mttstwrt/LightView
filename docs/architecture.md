# Architecture

[← docs](README.md)

LightView is one binary and one SPA. It opens a folder of images and videos,
indexes it into SQLite, generates thumbnails at four resolutions, and serves a
browsable grid and a full-resolution viewer — to a browser on the same machine,
or to phones and laptops on the LAN.

## Three modes, one process, one gallery

```
lightview <dir>                  serve <dir> on a random 127.x.x.x, open a browser
lightview --serve <dir>          serve <dir> on 0.0.0.0 over TLS, with pairing
lightview tag <dir> --plugin <n> run a plugin over the gallery and write tags
```

Plus four administrative verbs that do their work and exit: `pair`, `devices`,
`password`, `cache`.

**A process has exactly one gallery, bound at startup**, and one listener. The
root is canonicalized once; the derived-cache directory, the lock, the watcher
and every relative path key derive from that single value, so they cannot
disagree about what "the same gallery" is. Opening a different folder is a
different process — there is no command that swaps a running process onto
another gallery, and nothing in the UI offers one.

## Trust is a property of the bind

```
loopback bind  ──→  Trust::Owner    the whole command table
0.0.0.0 bind   ──→  Trust::Device   everything except the filesystem half
```

`lightview <dir>` and `--serve` are mutually exclusive, so a process has one
listener and therefore **one trust ceiling, fixed at bind**. No flag widens it,
and there is no request that can. That is stronger than checking the peer
address, which would be wrong anyway: `0.0.0.0` includes `127.0.0.1`, so a
served process asked "is this peer local?" would answer yes to a loopback
connection and hand out `Owner`.

Each command in the one table carries the minimum trust it requires, as a field
rather than a lookup in a second list. See [server/](server/README.md).

## The layers, and which way they point

```
  pure libraries   filter · sort · autocomplete · geocode · companion · util · path
        ↑          take a connection or a struct; know nothing above them
  services         cache · pipeline · plugin
                   media · gallery · tags · files · duplicates · trash · settings
        ↑          take state or pieces of it; no HTTP types
  adapter          server (routes + one command table) · cli
```

The rule that keeps this honest: **`cache/` must not learn what a route is, and
the pure libraries must not learn what application state is.** A module that
needs to know who called it is in the wrong layer.

## Where a request goes

```
browser ─┬─ POST /api/invoke ──→ one command table ──→ services ──→ cache (SQLite)
         │                                          └─→ companion (sidecars on disk)
         ├─ GET  /thumb/{tier}/{path} ─→ pipeline ──→ cache, generating on a miss
         ├─ GET  /media/{path} ────────→ pipeline ──→ the original file, Range/206
         ├─ GET  /api/events ─────────→ one broadcast channel, relayed as SSE
         └─ POST /api/upload ─────────→ staged write, rename, watcher ingests
```

**Paths on the wire are gallery-relative**, matching the database. Two
exceptions, both `Owner`: a copy or move *destination* is absolute by necessity,
and a plugin's temp file path is absolute by protocol.

The encoding rule travels with them: **percent-encode each segment
independently and leave `/` literal**. axum decodes captures but rejects paths
containing raw encoded slashes, so a single `encodeURIComponent` over the whole
path 404s every file in a subdirectory.

## Where a file goes

```
<gallery>/                        your photos, untouched
  2026/january/IMG_0001.jpg
  .lightview/
    companions/IMG_0001.jpg.lightview.json   tags, rating, notes — per directory
    trash/<epoch_ms>_<seq>/…                 one delete is one directory
    settings.toml                            default filter, trash retention

$XDG_CACHE_HOME/lightview/galleries/<sha256 of canonical root>/
  cache.db                        thumbnails, the index, perceptual hashes
  .lock                           what makes one process per gallery true
  instance.json                   the live URL, mode 0600

$XDG_DATA_HOME/lightview/         tls/ · devices.db · plugins/<name>/
$XDG_CONFIG_HOME/lightview/       server.toml
```

**Everything durable is in the gallery; everything derived is not.** The cache
is a hash away from the tree a person greps, rsyncs and backs up. Losing all of
it costs time and nothing else — which is what lets a format change delete and
rebuild rather than migrate. See [storage/](storage/README.md).

## Subsystems

| Page | What it covers |
|---|---|
| [server/](server/README.md) | routes, trust, the command table, pairing, TLS, events, upload |
| [storage/](storage/README.md) | the gallery tree, the XDG directories, the trash, the cache ceiling |
| [cache/](cache/README.md) | the SQLite schema, the four tier tables, `format_version`, the path-keyed sweep |
| [pipeline/](pipeline/README.md) | one render path, four tiers, the coalescer, the byte budget, the idle worker |
| [gallery/](gallery/README.md) | the initial scan, the watcher, how a new file becomes a grid cell |
| [companion/](companion/README.md) | the sidecar format, the lock, and two machines writing one directory |
| [query/](query/README.md) | the filter language, sort, grouping, autocomplete, and sets |
| [duplicates/](duplicates/README.md) | perceptual hashing, grouping, and merging |
| [plugins/](plugins/README.md) | the NDJSON protocol, the executor, input policy, the skip predicate |
| [geocode/](geocode/README.md) | coordinates to place names, written as companion tags |
| [frontend/](frontend/README.md) | the SPA: boot, stores, the grid, the viewer, the chrome |

## Building and verifying

[build-and-verify.md](build-and-verify.md) — prerequisites, the build order that
is not optional, and the two recipes that drive the real binary and the real SPA
with no display.
