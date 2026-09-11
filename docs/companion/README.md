# companion/

[← docs](../README.md)

**Responsible for** the sidecar files that hold everything a person put into
the gallery — tags, sets, plugin results, rating, colour label, notes, location
— and for the locking and atomicity that let two machines write one directory.

**Not responsible for** the *index* built from them
([cache/](../cache/README.md)), or for deciding when to write one (the services
do that).

**Depends on** `serde_json` and [`util/`](../architecture.md) for the lock and
the durable write. **Depended on by** every write path, the indexer, and
[plugins/](../plugins/README.md).

## This is the only durable data in the system

Everything else is reconstructable from the photos and these files. That makes
the schema **the largest commitment in the design**: it is written into the
user's gallery and read back by other LightView installations, so changing a
name or a type is a breaking change.

```
<dir>/.lightview/companions/<file name>.lightview.json
```

Per directory, so each subfolder carries its own tree and moving a folder moves
its metadata with it. A sidecar sitting *alongside* the media is still read, for
galleries written by older versions — but never written.

```json
{
  "schema_version": 1,
  "file": "IMG_0001.jpg",
  "file_hash": "…",
  "media_type": "image",
  "created": "2026-01-04T10:22:31Z",
  "modified": "2026-01-04T10:22:31Z",
  "tags": {
    "user":    ["vacation"],
    "set":     ["burst-2026-01-04"],
    "plugins": { "wd": { "version": "2.0.0", "tags": ["beach", "dog"] } }
  },
  "meta": { "core": { "rating": 4, "notes": "…" }, "plugins": {} }
}
```

### Two serde attributes carry the compatibility weight

**`#[serde(default)]` on every field** is what makes an old sidecar without
`set`, and a new one without `auto`, parse rather than fail. Not the schema
version — the version says what to *migrate*, and a file that will not
deserialize never reaches the migration.

**`#[serde(flatten)] extra` on the two collections** is what stops the next
write from erasing what the struct no longer models. Removing the `auto` field
means the first rating change would otherwise silently delete a user's `auto`
tags from the one file that cannot be regenerated. Dropping `auto` from the
*index* is a decision; dropping it from the *file* is data loss, and this is the
line between them.

### Two fields are mirrored from the database on purpose

`date_added` and `last_viewed` live in the index, and are also written here.
Without that, a `format_version` bump would silently empty them — and "a rebuild
loses time and nothing else" would be false. The mirror runs both ways: the
indexer writes them back into sidecars that lack them.

## One read-modify-write, under one lock

**The lock is the point.** Both writers used to read the whole file, mutate, and
serialize with no lock at all, so a rating set from the phone was silently gone
if the desktop's plugin run had read that companion a moment earlier — and over
a network mount "a moment" is the client's attribute cache, one second on `cifs`
by default. The losing write is not the older one; it is whichever reader lost
the race.

So the whole operation happens inside `modify_companion`, and **there is no
public way to write a companion without it.**

### `fcntl(F_OFD_SETLKW)`, and neither of the two obvious alternatives

- **`flock` is invisible to `smbd`.** It would be coherent on one machine and
  decorative across the share — which is the deployment.
- **Classic `F_SETLKW` is process-owned.** Two tasks inside one `--serve`
  process would not contend at all, and closing *any* descriptor for the file
  drops every lock the process holds on it. Open-file-description locks are
  per-descriptor, which is what makes both cases correct.

`rustix` does not expose the OFD commands, which is why this calls `libc`
directly.

### Atomic **and** durable

Write to a temp file in the same directory, `fsync` the file, rename, `fsync`
the parent directory. Atomic alone was the old behaviour and it is not enough
for a file on a NAS: the rename can reach the directory entry before the data
reaches the disk, and the crash window between them is where a companion becomes
zero bytes.

The temp file is removed on **every** error path, or a failed write leaves
litter in the one tree this design tells the user is safe to grep and back up.

## The skip check happens under the lock

`Outcome::Leave` exists for one reason: a tagging run that finds a newer result
already in the file must not overwrite it. A run over twenty thousand files
takes hours, and whatever it decided while planning is a guess by the time it
holds the lock — another process may have written in between. The answer under
the lock is the decision. See
[plugins/](../plugins/README.md#the-skip-predicate-is-version-or-higher).

## Invariants a caller must uphold

- **Never write a companion outside `modify_companion`.** The one exception is
  the trash, which deposits a companion into an entry directory it has just
  created and nobody else can reach.
- **Never remove a field without keeping `extra`.** The struct not modelling
  something is not permission to delete it from a user's file.
- **Bump `CURRENT_SCHEMA_VERSION` for any breaking change**, and add a
  migration. Other installations read these files.
- **Both writers need write permission on the directory and the lock file.**
  See [gallery/](../gallery/README.md#two-machines-one-directory-the-uid-question).
