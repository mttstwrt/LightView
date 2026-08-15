# Geocode: coordinates to place names

[← docs index](../README.md) · [architecture](../architecture.md)

One transform, in one direction: decimal-degree WGS-84 coordinates in, the
country / region / city names a person would type out. It exists so that typing
`Japan` into the filter bar finds the photos taken there, and `Kyoto` narrows
them further — which nothing else in the system could answer, because a photo
carries where it was taken as a number and never as a name.

**Responsible for:** the gazetteer lookup, the distance ceilings that decide
when a match is too far away to mean anything, and the ISO 3166-1 code → country
name table.

**Not responsible for:** getting the coordinates — [`pipeline/`](../pipeline/README.md)
extracts those from EXIF and `commands/gallery.rs` caches them in
`media_meta.gps_lat/gps_lon`. Not responsible for writing tags either; that is
`commands::gallery::backfill_location_tags`, described below. Nothing here
touches image bytes, the database, or the filesystem — it is a pure function
over two floats.

**Public interface:** `geocode::lookup(lat, lon) -> Option<Place>`,
`Place::tags()`, and `geocode::DATASET_VERSION`.

**Depends on:** the `reverse_geocoder` crate and nothing else in the tree.

**Depended on by:** `commands/gallery.rs`, at the one call site below.

**Invariants callers must uphold:** coordinates are decimal degrees, WGS-84 —
the same convention as [`companion/`](../companion/README.md)'s `Location` and
the filter language's `GeoBbox`. A `None` result means "no answer worth having",
not "error"; it is the normal outcome for open ocean and for the polar
interiors, and callers treat it as such.

## Why names go to disk when coordinates do not

`backfill_gps_meta` deliberately writes coordinates into `media_meta` and never
into a companion file, because the coordinate is already in the photo's own EXIF
— exiftool reads it, every other gallery reads it, and mirroring it into a
sidecar would duplicate data the file already carries.

Place names are the opposite case. `35.0116, 135.7681` becomes `Kyoto` only by
consulting a 7.9 MB gazetteer at a particular version. If that name lived only
in the cache, deleting the database would destroy it, and a gallery read by
anything other than LightView could not recover it at all. So the names are
written into companion files, where `grep -rl Kyoto` finds them and the gallery
stays legible without this application. That is the whole reason this subsystem
produces tags rather than a column.

The tags land in the companion's `tags.plugins["location"]` bucket, and so in
the `plugin.location` namespace once indexed. Nothing about this is a plugin —
see [decision 0011](../decisions/0011-location-names-are-companion-tags.md) for
why the plugin *storage* convention is reused while the plugin *execution* path
is not. A `PluginTagEntry` is a named, versioned bucket that a re-run replaces
wholesale, which is what re-geocoding against a newer gazetteer needs, and it
required no change to the companion wire format.

## The gazetteer, and why every answer is approximate

The data is GeoNames cities1000 — every populated place of 1,000 people or
more, about 144,000 of them — embedded in the binary by the `reverse_geocoder`
crate. Matching is nearest-neighbour in a k-d tree over unit-sphere cartesian
coordinates. There are no polygons anywhere in this: the lookup does not know
where Japan *ends*, only which town is closest, and it infers the prefecture and
country from that town's record.

The consequence is that a photo taken away from any town still resolves to some
town, and near a border the nearest town can be across it. Two ceilings keep
that from turning into confident nonsense:

- past **25 km** the nearest place is no longer where the photo was taken, so
  the city tag is dropped while country and region — which are still right at
  that range — are kept;
- past **100 km** even those are a guess, and nothing is emitted at all.

The second ceiling is what makes an ocean crossing or an Antarctic traverse
produce no tags rather than the name of the nearest inhabited rock.

**How accurate this actually is**, measured against twelve well-known landmarks:
the country was right twelve times out of twelve, the region eleven, and the
city four of the eleven that had an obvious expected answer. The failures are
systematic rather than random, and they cluster in exactly the places people
photograph most:

| Landmark | Country | Region | City |
|---|---|---|---|
| Kyoto Station | Japan | Kyoto | Kyoto |
| Colosseum | Italy | Latium | Rome |
| Shibuya Crossing | Japan | Tokyo | Tokyo |
| Eiffel Tower | France | Ile-de-France | **Neuilly-sur-Seine** |
| Big Ben | United Kingdom | England | **City of Westminster** |
| Times Square | United States | **New Jersey** | **Weehawken** |
| Golden Gate Bridge | United States | California | **Sausalito** |

A dense metropolis is subdivided in the gazetteer into communes, boroughs, and
wards, each with its own centroid, so the nearest entry to a landmark is
routinely a neighbour rather than the city itself — Notre-Dame resolves to
`Paris`, but the Eiffel Tower four kilometres away resolves to
`Neuilly-sur-Seine`. Times Square is the worst case in the table and shows the
same effect reaching the region: the closest gazetteer centroid is across the
Hudson, so the photo is tagged `New Jersey`.

Fixing this properly means ranking candidates by prominence rather than taking
the single nearest — of the entries within a few kilometres, prefer the one with
the largest population. The dataset this crate embeds carries no population
column (its fields are exactly latitude, longitude, name, admin1, admin2, and
country code), so that change means owning the gazetteer and the search rather
than delegating both. It has not been done, and until it is, **the country and
region tags are the dependable ones and the city tag is best read as "the
nearest named place"**.

The gazetteer costs about a second to parse into its tree and is built lazily
behind a `OnceLock` on the first lookup, so a gallery with no geotagged media
never pays for it.

Two details of the crate's data shape are worth knowing because they look like
bugs. Its `admin1` field is already a *name* (`Kyoto`, `Alabama`), not a code, so
the region needs no translation — but its `cc` field is a two-letter country
*code*, which is why `geocode/countries.rs` exists. And `admin1` is blank on
about 840 of the 144,000 rows, which is why every field of `Place` is optional.

## When it runs

Inside the background task that `open_gallery` spawns, between the GPS backfill
and the companion index pass — the ordering matters in both directions. It runs
*after* `backfill_gps_meta` because it reads the coordinates that pass writes,
and *before* `index_companions` so the sidecars it writes are picked up in the
same sweep rather than waiting for the next gallery open.

Videos reach the same columns by a different and slower route. Their
coordinates come from the container's ISO 6709 tag via ffprobe
([`pipeline/`](../pipeline/README.md)), which runs when a clip is first
thumbnailed rather than during the open pass — so a newly added video's GPS
usually lands *after* this pass has already run, and its place tags appear on
the next gallery open. Probing every video up front would mean an ffprobe spawn
per clip on the open path, which is exactly the cost the existing design defers.

Work is skipped by a `NOT EXISTS` test against `tag_index` for the
`plugin.location` namespace, so the steady state is one pass and later opens do
nothing. That test reads the index as it stood at the *previous* open, so a
gallery whose sidecars arrived already tagged — copied from another machine — is
re-geocoded once and rewrites identical content before settling. Reading every
companion to avoid that single redundant pass would cost more than the pass.

The dominant cost is not the geocoding, which is a k-d tree probe per file. It
is the companion writes: one atomic write-and-rename per geotagged file, which
on a large library is the entire budget of this pass. That is why it runs in the
background task rather than anywhere near the path that renders the grid.

## Known limits

Re-geocoding after a gazetteer upgrade is not automatic. `DATASET_VERSION` is
recorded in each bucket so a future pass can recognise its own output, but
nothing currently compares it — a dataset change would need the existing
`plugin.location` tags cleared to take effect.

Country names are matched exactly and case-sensitively at query time, like every
other tag (see [`query/`](../query/README.md)), so `japan` does not match
`Japan`. Autocomplete is case-insensitive and fuzzy, which is what covers this
in practice.
