# storage/

[← docs](../README.md)

**Responsible for** where every byte lives: the gallery tree, the three XDG
directories, the trash, and the ceiling that keeps derived data bounded.

**Not responsible for** the *format* of anything it holds — the sidecar schema
is [companion/](../companion/README.md) and the database schema is
[cache/](../cache/README.md).

**Depends on** nothing but the filesystem. **Depended on by** every service, the
CLI, and the server.

## Everything durable is in the gallery; everything derived is not

```
<gallery>/
  2026/january/IMG_0001.jpg               your photos, untouched
  .lightview/
    companions/IMG_0001.jpg.lightview.json    per directory
    companions/.lock                          the fcntl lock for that directory
    trash/1761350400123_0/2026/january/…      one delete is one directory
    settings.toml                             default filter, trash retention

$XDG_CACHE_HOME/lightview/galleries/<sha256 of the canonical root>/
  cache.db                                  thumbnails, the index, phashes
  .lock                                     one process per gallery
  instance.json                             the live URL, mode 0600

$XDG_DATA_HOME/lightview/
  tls/                                      the self-signed certificate
  devices.db                                pairings, per account
  plugins/<name>/                           installed plugins

$XDG_CONFIG_HOME/lightview/
  server.toml                               port, SANs, password hash, apps
```

**The cache left the gallery, and that is the decision the rest follows from.**
A gallery directory on a share is the one tree a person greps, rsyncs, backs up
and syncs; a SQLite database inside it breaks all four — a WAL on a network
mount is the worst case of the worst case. Keying on the SHA-256 of the
*canonical* root is what makes a symlinked path and its target one gallery
rather than two.

The cost, stated rather than discovered: a desktop tagging a NAS gallery over a
mount builds its **own** derived cache for it, because it cannot read the
server's. That is what the ceiling below is for.

`--data-dir <path>` overrides all three roots with `<path>/{cache,data,config}`.
One flag gives a container one volume and a test its own private machine.

## The lock is what makes "one process per gallery" true

`flock` on `<cache dir>/.lock`, taken when the database opens. A second
`lightview <dir>` on the same folder finds it held, reads the live URL out of
`instance.json`, opens a browser at it and exits — which is also how a user
recovers from a browser that never opened.

`lightview tag` takes it too, so a tagging run cannot start against a gallery
this machine is already serving. Two processes writing one derived cache is
exactly the case the lock exists for.

## The trash

```
.lightview/trash/<epoch_ms>_<seq>/<gallery-relative path>
```

The first segment is the deletion time plus a sequence number and is the
uniqueness key; everything after it is the original path. There is no metadata
file — purge is `read_dir`, parse the numeric name, compare against the
retention window, `remove_dir_all`, with no file reads at all.

**Keep the `_<seq>` suffix.** Two deletes landing in the same millisecond would
otherwise merge into one directory, silently breaking "one delete is one
directory". The suffix is reserved by `create_dir` failing with `AlreadyExists`
and retrying — an atomic `mkdir`, which is why it also holds for two *machines*
trashing over the share in the same millisecond, with no per-machine component
in the name.

**Media first, companion second.** Reversed, a crash between the two moves
leaves the photo in the gallery with its ratings and tags in the trash. Either
order looks arbitrary until you name the failure.

**Restore puts the companion where companions go now** —
`<its directory>/.lightview/companions/<name>.lightview.json` — not alongside
the media where the trash entry keeps it. A naive path-mirroring restore drops
it beside the photo, where the read fallback still finds it, so it *appears* to
work and the next metadata write forks a second sidecar.

Retention is swept once when the gallery opens. `purge_trash` with no entry
means **empty the trash**, all of it: a button called Empty Trash that left last
week's deletions in place would be lying about what it did.

### The entry id is not a path

An earlier design made the client-visible id `<epoch_ms>/<relative path>` — a
string containing slashes — and that creates an **arbitrary-file-move primitive
at `Device` trust**. `valid_entry_id` accepts digits and underscores only,
precisely so a remote client cannot escape the trash directory; a slash-bearing
id cannot pass it, so the layout *forces the check's removal*. The code it
guards normalizes nothing: `root.join(id)` with an absolute argument discards
the base entirely, and pushing `..` components walks out. Restore is `Device`.
The chain runs: upload a file whose bytes are a plugin manifest (the extension
allowlist checks the name, not the content), trash it, restore it to
`/tmp/evil/manifest.json`, run that plugin.

So **the id stays opaque and the destination is rebuilt from validated parts.**
`list_trash` returns `{id, relative_path, file_name, deleted_at, size}` where the
id is the directory name only and the original location travels in its own
field. `restore_trash` takes both, and every component of `relative_path` must
be `Component::Normal` before any join — confined against the **trash root**,
not the gallery root, because `.lightview/trash/` is *inside* the gallery and a
gallery-root check passes a path that has already escaped the trash.

This deletes work as well as risk: an id with no slashes needs no per-segment
percent-encoding on the wire.

## The cache ceiling

Derived data is budgeted, because a directory advertised as safe to delete has
to also be safe to leave alone. Two bounds, at different scales:

- **Per gallery**, a byte budget over the two bounded tiers, enforced by an LRU
  eviction pass — see [pipeline/](../pipeline/README.md#the-byte-budget).
- **Across galleries**, a ceiling enforced at every open: evict whole
  least-recently-opened gallery caches until the total is inside it, never the
  one being opened. Leaving this to `lightview cache --prune` alone would let a
  folder processed once and never reopened leave a cache nothing reclaims.

`lightview cache` prints the directory and its size; `--prune` runs the sweep on
demand.

## Invariants a caller must uphold

- **Never write a derived byte into the gallery.** The gallery holds originals,
  companions, the trash and `settings.toml`. Everything else is regenerable and
  belongs in the cache directory.
- **Resolve against the canonical root.** The database keys, the watcher's
  `strip_prefix` and the cache directory name all derive from one canonicalized
  value. Using the user-supplied path anywhere makes them agree by accident,
  and a symlinked gallery is where the accident stops.
- **A trash entry id is opaque.** Digits and underscores. Anything that would
  make it carry a path is the security bug above, reintroduced.
- **`.lightview` is skipped by the media scan, the companion indexer and the
  watcher's addition branch.** All three, every time.
