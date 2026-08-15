# 0011 — Location names are companion tags, written by the host

## Context

Photos and videos carry GPS coordinates, and LightView already extracts and
caches them: `media_meta.gps_lat/gps_lon` (schema v7), backfilled from EXIF at
gallery open, driving the map view and the `has:geo` / `GeoBbox` filter terms.

What was missing was names. "Show me everything I shot in Japan" and "just the
Kyoto ones" are the questions people actually ask, and a bounding box is both
the wrong shape for a real border and unusable as something to type. The filter
bar's bare-word search compiles to an exact match against `tag_index`, so for
`Japan` to work as typed, `Japan` has to exist as a tag.

Three things had to be decided: whether to store names at all or resolve them at
query time, where the names live if stored, and what produces them.

## Options considered

**Resolve at query time, no storage.** Parse `Kyoto` in the filter bar, look the
name up in a gazetteer, compile it to a bounding box or point-radius term over
the existing GPS columns. Stores nothing new. But a rectangle is not a
prefecture, bare-word search would have to learn a second lookup path that only
place names use, and the names would never appear in autocomplete, `tag_counts`,
group headers, or the tag manager — all of which read `tag_index`.

**Store as a derived namespace in the cache only.** Re-derive place tags from
`gps_lat/lon` on every open into a `geo` namespace that the companion index pass
leaves alone. No sidecars written, nothing to migrate, and a gazetteer upgrade
just recomputes. It also satisfies the "delete the database and lose nothing"
rule, since the coordinates come from EXIF in the file. But it fails the
stronger version of that rule: a gallery read *without* LightView, or without
this gazetteer at this version, cannot recover the names at all.

**Store as tags in companion files.** The names go to disk next to the media,
where the existing indexer picks them up and where `grep -rl Kyoto` finds them.
Costs a sidecar write per geotagged file.

Independently, for what produces the tags: **a plugin** — the tagger system
exists, is optional, and would keep the gazetteer out of the main binary — or
**the host**, in the background task that already backfills GPS.

## Decision

Place names are written into companion files as tags, in the
`tags.plugins["location"]` bucket, by the host — not by a plugin.

The storage question turns on an asymmetry between the two halves of the data.
The *coordinate* is already in the file's EXIF; every photo tool can read it, so
mirroring it into a sidecar would duplicate what the file already carries, and
`backfill_gps_meta` correctly keeps it in the cache. The *name* is not in the
file. Turning `35.0116, 135.7681` into `Kyoto` requires a 7.9 MB gazetteer at a
specific version, so the name is genuinely new information, and it is the part
that would be lost if the cache were deleted or the gallery were read by
anything else. Writing it to disk is what makes the gallery self-describing.

Reusing `PluginTagEntry` as the container — without being a plugin — avoids a
companion wire-format change entirely. It is a named, versioned tag bucket that
a re-run replaces wholesale, which is exactly the shape a re-geocode needs, and
it gives the entries a namespace (`plugin.location`) that is distinguishable
from any other automatic tagger's. Writing into `tags.auto` would have worked
today but leaves nothing to tell our entries apart from the next writer's.

The producer question turns on the plugin protocol's request shape, which is a
path and nothing else: `{"action": "tag", "path": "…"}`. A location plugin would
therefore have to re-extract EXIF itself, duplicating `pipeline/exif.rs` in
another language and inheriting HEIC and RAW parsing along with it; it would get
nothing for videos, whose coordinates come from ffprobe rather than EXIF; and on
a remote `lightview-worker` it would cause the full-size original to be
*downloaded* so the worker could read forty bytes of GPS the server already had
in a column. For the container deployment this is the worst available shape.
Extending the protocol to carry known metadata alongside the path would fix
that, and may be worth doing one day, but building it now would mean a protocol
change, a worker change, and new capability semantics to serve a single
hypothetical caller.

## Consequences

Opening a gallery writes a companion file for every geotagged item that does not
already have location tags. On a large library this is the dominant cost of the
pass — one atomic write-and-rename per file — which is why it runs in the
background task and not near the grid's render path. It is a one-time cost per
item; a `NOT EXISTS` test against `tag_index` skips already-tagged files on
later opens.

The names are exact, case-sensitive tag values like every other tag, so `japan`
does not match `Japan`; fuzzy, case-insensitive autocomplete is what bridges
that.

The gazetteer adds roughly 7.9 MB to the binary and about a second of one-time
parsing, deferred behind a `OnceLock` so galleries without geotagged media never
pay it. This was accepted rather than made optional: the primary deployment is a
container whose base image already dwarfs it, and a Cargo feature would be a
cheaper way to make it optional later than a plugin ever would.

Matching is nearest-neighbour against populated places, never point-in-polygon,
so results are approximate near borders and away from towns. Two distance
ceilings bound the damage, but they do not eliminate it: measured against twelve
landmarks, the country was right every time and the region all but once, while
the city was right four times out of eleven. Dense cities are split into
communes and boroughs in the gazetteer, so a landmark's nearest entry is often a
neighbour — the Eiffel Tower resolves to `Neuilly-sur-Seine`, and Times Square
to `Weehawken, New Jersey`. Ranking nearby candidates by population would fix
it, but the embedded dataset carries no population column, so that means owning
the gazetteer instead of delegating it. See [`geocode/`](../geocode/README.md)
for the full table. The country and region tags are the dependable ones today.

Re-geocoding after a gazetteer upgrade is not automatic. The version is recorded
per bucket but nothing acts on it yet.
