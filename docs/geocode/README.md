# geocode/

[← docs](../README.md)

**Responsible for** turning the coordinates already cached in `media_meta` into
the place names a person types into the filter bar — country, region, city.

**Not responsible for** reading image bytes. The coordinate was extracted by the
EXIF pass and stored; this module is a pure function over two floats.

**Depends on** the `reverse_geocoder` crate and a bundled GeoNames dataset.
**Depended on by** the gallery's open-time location backfill, and through it by
[query/](../query/README.md), since the names arrive as ordinary tags.

## The names are written to companions; the coordinate is not

The coordinate lives in the file's own EXIF and any photo tool can recover it,
so mirroring it into a sidecar would be redundant. The **name** is not in the
file — recovering it needs this gazetteer, at a particular version — so it is
the one part of this that would not survive the gallery being read by anything
other than LightView.

They land as a versioned plugin bucket like any other tagger's, which is what
lets a gallery tagged by an older gazetteer be spotted and re-tagged.

## Matching is approximate, by construction

The gazetteer is GeoNames `cities1000` — every populated place of 1,000 people
or more — and the lookup is nearest-neighbour, so a photo taken away from a town
still resolves to *some* town.

Two ceilings keep that honest rather than confidently wrong. Past the city
ceiling the nearest place is no longer where the photo was taken and the city
tag is dropped, while country and region stay valid much further out. Past the
region ceiling — mid-ocean, deep desert, Antarctic interior — even those are a
guess, and nothing is emitted.

## The gazetteer is built on first use

It costs roughly a second to parse 144k rows into a k-d tree, and it is never
freed. **A gallery with no geotagged media must never touch it**, which is why
it is behind a `OnceLock` rather than constructed at startup.

## Invariants a caller must uphold

- **Bump `TAGGER_VERSION` whenever the emitted tags would change** — a different
  gazetteer, a change to which fields are emitted, or a change to how names are
  spelled. The trailing revision exists for exactly that second kind: the
  dataset was unchanged but names gained underscores, and every gallery tagged
  before that needed its old spellings replaced.
- **The country table stays sorted by ISO code.** `name_for` binary-searches it;
  a test enforces it.
