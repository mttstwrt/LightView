# Companion files

[← docs index](../README.md) · [architecture](../architecture.md)

A companion file is a JSON sidecar named `<media>.lightview.json` holding
everything the user has said about a media file: tags, rating, colour label,
notes, location, and whatever plugins have contributed. It is the **record of
intent**. The [cache](../cache/README.md) indexes it so queries are fast, but
the cache is derived — delete it and re-open the gallery and it is rebuilt from
these files.

**Responsible for:** the on-disk format (`schema.rs`), reading and locating it
(`reader.rs`), writing it atomically (`writer.rs`), and version migration
(`migration.rs`). Also the `MediaType` vocabulary, which lives here because
media type is a companion field — extension-to-type inference has exactly one
definition and this is it.

**Not responsible for:** deciding when to write. Callers
(`commands/tags.rs`, `commands/duplicates.rs`, `plugin/runner.rs`) own that,
and all of them go through the shared `modify_companion` helper so a hand-edit
and a plugin write produce identical files. Not responsible for the tag index
either — that is [`cache/`](../cache/README.md), refreshed by the caller after a
write.

**Depends on:** `serde_json`, `chrono`, `uuid`. Nothing else in the tree.

**Depended on by:** [`query/`](../query/README.md) (for `MediaType` and the tag
namespaces), [`plugins/`](../plugins/README.md),
[`duplicates/`](../duplicates/README.md), `commands/tags.rs`, and the indexer.

## The shape of the file

```
{
  "schema_version": 1,
  "file": "sunset.jpg",
  "file_hash": "...",
  "media_type": "image",
  "created": "<rfc3339>",
  "modified": "<rfc3339>",
  "tags":  { "user": [...], "auto": [...], "plugins": { "<name>": {...} } },
  "meta":  { "core":  {...},              "plugins": { "<name>": {...} } }
}
```

Both `tags` and `meta` are split the same way, and the split is the important
part of the design: **`user` is never overwritten by anything but the user.**
A plugin writes only under its own key in `tags.plugins` / `meta.plugins`, so
re-running a tagger replaces that tagger's output and touches nothing else. A
plugin entry carries the plugin `version` alongside its tags, plus arbitrary
flattened extras (confidence scores and the like) that LightView stores and
returns without interpreting.

That namespacing is what the filter language's `user::`, `auto::`, and
`plugin.<name>::` prefixes address — see [`query/`](../query/README.md).

One `tags.plugins` bucket is written by the host rather than by a plugin:
`location`, holding the place names [`geocode/`](../geocode/README.md) derives
from the file's GPS coordinates. It reuses this container because a
`PluginTagEntry` is exactly a named, versioned set of tags that a re-run
replaces wholesale — which is what re-geocoding needs — and doing so kept the
wire format unchanged. Note the asymmetry with `meta.core.location` below: the
*coordinate* is already in the media file's own EXIF and is cached rather than
duplicated here, while the *name* exists nowhere else and so is written to disk.

`meta.core` holds the fields the app itself owns: `rating`, `date_rated`,
`color_label`, `notes`, `media` (dimensions, duration, codec), and `location`
(decimal degrees, WGS-84, optional altitude in metres).

Two of those — `rating` and `color_label` — are also **mirrored into
`media_meta`** columns, because filtering and sorting run in SQLite and never
open a sidecar. The companion stays the source of truth: the mirror is written
by every path that sets the field, and rebuilt from the companion by the
indexing pass at gallery open, so deleting the cache loses nothing. Adding a
third filterable `meta.core` field means adding a column and a mirror in the
same places — see [`query/`](../query/README.md).

## Where the file lives

`CompanionLocation` has two variants: `Alongside` (next to the media file) and
`LightviewFolder` (under `<gallery>/.lightview/companions/`). Reads try the
requested location first and then fall back to the other one, so a gallery
whose preference changed keeps resolving old sidecars without a migration pass.
Writes go to the requested location only — the fallback is a read affordance,
not a two-way sync.

## Writing is atomic

`write_companion_at` serializes to a uniquely-named temp file **in the target
directory** and renames it into place. Same directory, therefore same
filesystem, therefore the rename is atomic: a reader either sees the old file or
the new one, never a truncated one. This matters more than it looks — companion
writes happen during batch tagging while the indexer may be reading the same
tree.

`modified` is stamped by the writer, not by the caller, so every write carries
an accurate timestamp regardless of which path produced it.

## Versioning

`schema_version` is stamped on write and checked on read;
`migration::migrate` upgrades an older file to the current shape. Only version 1
exists today, so `migrate` is an identity function with the extension point
written out. It is called unconditionally on every read, which is what makes
adding version 2 a change to one function rather than an audit of every reader.

Note that this is a *different* version number from the cache's
`SCHEMA_VERSION`: this one describes a file format that lives in the user's
gallery and must be readable by other installations; the cache's describes a
local database that can be rebuilt. They move independently and should not be
conflated.
