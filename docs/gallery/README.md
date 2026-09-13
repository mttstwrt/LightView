# gallery/

[← docs](../README.md)

**Responsible for** opening a gallery and keeping it in step with the
filesystem: the initial scan, the enrichment pass, the filesystem watcher, and
the hourly companion sweep. In short, **how a file becomes a grid cell**.

**Not responsible for** what a cell *looks* like ([pipeline/](../pipeline/README.md))
or what a client is told ([server/](../server/README.md)) — it produces rows and
emits events, and stops there.

**Depends on** [`cache/`](../cache/README.md),
[`companion/`](../companion/README.md), `notify`, and the EXIF and video probes.
**Depended on by** the CLI, which calls it in a fixed order at startup.

## The open sequence, and why the order is not negotiable

```
1. take the cache lock          one process per gallery
2. prune cold gallery caches    the ceiling, enforced at open
3. sweep the trash              retention, from the gallery's own settings
4. scan and index               the whole tree; every route is 503 until this ends
5. arm the watcher              ← before the gate opens
6. open the readiness gate
7. enrich in the background     headers, geocoding, companions
8. spawn the idle worker and the hourly sweep
```

**Five before six** is the load-bearing one. A file arriving between "the scan
finished" and "the watcher is armed" is in neither, and nothing ever notices it.
Reversed, that window is the entire initial scan.

**Seven is a resume, not the only pass.** The watcher's ingest reads a new
file's metadata header itself, in the same breath as its companion — otherwise a
batch arriving over rsync or Samba is dateless and placeless until a restart.
Step 7 exists for whatever that missed.

**Which header gets read depends on the file, and only one branch knows it.**
An image's facts come from its EXIF block; a video's come from what its
container declares, which `ffprobe` reads — duration and the display dimensions
with any rotation already applied. Both arrive as the same row, so nothing
downstream of the probe has a video case. A GIF takes the image branch: it has
no EXIF either, and the grid animates it on its own rules rather than on a
duration.

`ffprobe`'s absence is asked about **before** the probe, never inferred from its
failure, because the probe reports a missing binary and an unreadable container
identically. A host with no `ffmpeg` leaves its video rows unmarked rather than
recording a look that never happened, so installing it later fills them in on
the next open — the one case where a row stays a candidate without having been
excluded.

**What decides whether a header still needs reading is `media_meta.exif_read`,
and nothing else.** It is set whenever a header is read, found or not, because
a photo with no GPS and a screenshot with no EXIF block leave identical rows:
any gate phrased over the *result* columns either re-reads them on every open
forever or excludes them forever. The version this replaces chose the second by
accident — it asked whether anything was known about the file yet, and a
thumbnail answered yes, so a file the grid had drawn before its header was read
never got one. See [`cache/`](../cache/README.md) for the column and the partial
index that makes the warm-gallery pass free.

Trash retention comes from the **gallery's own** `settings.toml`, never from
`server.toml` — otherwise a desktop's default would delete a served gallery's
trash.

## The watcher's policy, carried deliberately

`util/fs_watch` is a sixty-line transport whose own doc says the caller decides
what a burst of events means. Everything that decides is here, and it is written
down because none of it is recoverable by reading either half alone:

- **A quiet-period debounce, not a throttle.** The timer is reset by every
  event, so a phone uploading two hundred photos back to back produces no rows
  and no SSE until 500 ms after the last one lands.
- **Three skip filters, in this order.** The `settings.toml` hot-reload branch
  runs **before** the `.lightview` skip, or changing a preference stops reaching
  the running process. Then `.lightview` itself. Then the media-extension
  filter, which is the only reason an upload's own `.lv-upload-*.tmp` is
  invisible.
- **Only `Create` and `Modify(Name(To))` count as additions.** `Modify(Data)` is
  ignored for media — see *a path is immutable* below — and watched for
  companions, which is the whole point of that branch.
- **Armed on the canonical root.** Database paths are relative to the canonical
  root; a watcher armed on the user-supplied one fails `strip_prefix` on every
  event, which looks exactly like "not in this gallery". Concretely:
  `lightview --serve ~/photos` where that is a symlink to `/mnt/nas/photos`
  uploads fine, thumbnails fine, and **never shows the file** until a restart.
- **`notify`'s own errors are surfaced.** inotify watch-limit exhaustion and
  queue overflow arrive through the same channel, and swallowing them makes the
  watcher go *partially* deaf with no log line: some subtrees stop ingesting and
  nothing says so. On a root that disappears the process exits non-zero naming
  it — a serving process can do nothing useful without its gallery, an unmount
  under it is an operator event, and a systemd unit restarts it when the mount
  returns.
- **A newly ingested file gets its companion indexed in the same breath.**
  Otherwise a batch arriving *with* its sidecars over `rsync` or Samba — the
  headline deployment — appears with no tags, no rating and no colour label
  until a restart, and any edit made in that state overwrites a companion the
  index never read.

### The companion branch is how tagging on another machine reaches the phone

A `lightview tag` run on the desktop writes companions over the share. Those
writes arrive at `smbd`, which is an ordinary local process writing to the
server's own disk — and `inotify` watches inodes, so they fire the server's
watcher exactly as a local edit would, bind-mounted container or not. Tags
written on the desktop reach the phone within the debounce window.

(This is true of the *server's* disk and false of a **client-side** mount: a
viewer on the desktop over the same share never sees the server's writes. That
is what the hourly sweep below is for.)

### A path is immutable within a gallery session

Nothing updates a row whose file was replaced, the tier lookup has no `mtime`
predicate, and the ETag is a hash of the cached bytes — so a phone would
revalidate, get a 304, and re-stamp its freshness window on stale bytes.

Upload dedupe guarantees a new upload is a new path. **Replacing a file in place
on the host is outside what this supports**, and saying so is cheaper than
putting an `mtime` comparison in every tier lookup on every request.

## The hourly sweep

`reindex_companions` re-reads every companion whose `(mtime_nanos, size)` has
moved, and it runs on a wall clock for as long as the gallery is open.

**Deliberately not folded into the idle worker**, which skips its units whenever
somebody is touching the grid. That is right for thumbnail backfill, which
competes for the pool the user is waiting on, and wrong here: a companion
written where the watcher cannot see it has to appear whether or not anyone is
looking, and a busy gallery is exactly when somebody is.

What keeps it from blocking the grid is its own two-phase split — a phase that
walks, stats and reads with **no database handle at all**, then a phase that
takes the writer once and runs batched statements. Held as one pass it blocked
every thumbnail the grid was waiting on: tolerable once per open, and not once
it runs every hour beside an hours-long stream of writes from another machine.

## Two machines, one directory: the UID question

Both writers have to be able to **replace** each other's files — rename over a
companion, open `.lock` for writing to take the lock, and on the fallback path
unlink a target. Rename and unlink need write permission on the **directory**;
the lock needs it on the **lock file**. With Samba's default `create mask = 0744`
and two different UIDs, every one of those fails — in both directions, silently
on the desktop and as a logged error on the server — for every directory the
*other* side touched first. A phone could not rate a photo the desktop had
tagged.

Two configurations pass. The one this deployment uses: **the Samba login the
desktop connects as is the same account the container runs as**, so there is one
UID by construction. The alternative, for a share that must stay multi-user, is
a shared group — `force group`, `create mask = 0664`, `directory mask = 0775`,
and a matching umask in the container.

Which applies is a fact about `smb.conf`, not about this design, so **the server
probes at open** rather than trusting either: it writes and replaces a file in
an existing `companions/` directory it does not own, and logs a specific
diagnostic if it cannot.

## Invariants a caller must uphold

- **Arm the watcher before opening the readiness gate.** Every time.
- **Resolve against the canonical root**, everywhere — the database, the
  watcher, and the cache directory name all derive from that one value.
- **A `strip_prefix` failure is a `log::warn`, never a silent `continue`.** It
  means the watcher and the database disagree about the gallery, which is
  invisible otherwise and total in effect.
