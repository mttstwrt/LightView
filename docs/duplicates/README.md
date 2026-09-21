# duplicates/

[← docs](../README.md)

**Responsible for** finding near-identical images and folding a group onto one
keeper: perceptual hashing, all-pairs grouping, and the merge.

**Not responsible for** deciding *which* copy to keep — the dialog resolves the
conflicts and the backend applies the answer, without second-guessing it.

**Depends on** [`cache/`](../cache/README.md) for hashes and rows, the
[trash](../storage/README.md#the-trash) for what a merge discards, and
[companion/](../companion/README.md) for what it folds in. **Depended on by** the
duplicates panel and the merge dialog.

## Hashes come from the cached `j` tier, never the original

Those bytes are already decoded and already in the database, so hashing a whole
gallery costs **no source decodes**. The hash lives in a `phash` column on
`thumbs_j`, so it is discarded and recomputed along with the thumbnail it
describes — exactly the lifetime it should have. The idle worker computes them
in the background.

dHash: downscale to 9×8 greyscale, compare each pixel to its right neighbour,
produce 64 bits. Downsampling to 9×8 regardless is why moving the source bytes
from square-cropped to aspect-preserving did not change what this measures.

### `NULL` means "not hashed", never a sentinel `0`

The hasher this replaces matched on a stored codec string and fell through to
`hash.unwrap_or(0)`. It read a JPEG tier; the `j` tier is WebP. **Ported
unchanged, every row would have stored `0`** — every row would have passed
`WHERE phash IS NOT NULL`, `hamming(0, 0)` is `0`, and the all-pairs loop would
have unioned **the entire library into one duplicate group**, with no error, no
log line and no failing test, for the merge to then trash.

A genuinely flat image legitimately hashes to zero, which is why the two cases
cannot share a value.

## Grouping is quadratic, and that is why `threshold` is about precision

An all-pairs Hamming comparison over every hashed file, in Rust, with the
connection released first. **A tighter threshold is not cheaper** — the loop runs
either way — so the control is about how loose a match counts, not about cost.

The version this replaces held the *writer* across the entire comparison, on an
async worker with no `spawn_blocking` at all.

## Sets are what "not a duplicate" means now

Two files sharing any `set::` tag are never offered as a pair. Derived from
co-membership rather than from a table of pairwise verdicts, so a forty-frame
burst costs **forty tag rows instead of 780 pairwise ones**, and the user sees a
name rather than a list of negations. Naming a group is the gesture the panel
offers where "not duplicates" used to be.

The accepted cost, stated: two genuinely identical scans inside a 200-page comic
will not be found, because they share the comic's set.

The suppression check is **index-based, not path-keyed**. Probing a set keyed by
paths meant building an owned `(String, String)` for every near-match just to
ask whether it had been dismissed — two allocations and a string comparison per
candidate. Interned set ids make it two integers and no allocation, and that
matters more here than it did for the table it replaces: dismissed pairs were
rare, so the old check almost never fired, while set co-membership is the
**common** case and is now the inner loop of the one quadratic algorithm in the
tree.

## The merge

The dialog resolves the conflicts; the backend applies the answer. It gathers
what the others contribute, applies everything to the keeper in **one locked
read-modify-write**, stamps the agreed capture time onto the keeper's file, and
trashes the rest as **one trash entry** — so one merge is one undo.

`set::` tags union onto the keeper like user tags. Without that, merging a set
member silently drops that member's set; a keeper ending up in two sets is fine,
since suppression is pairwise.

**The mtime stamp is the one place anything writes one**, and it is an explicit
field of the plan rather than a side effect. Restoring a file with a rewritten
mtime is silent data loss, so the operation that legitimately rewrites one says
so out loud.

**The stamp moves the row too, in the same breath.** `index_one` afterwards
re-reads the *companion*, not the file, so the indexed `mtime` would otherwise
keep its old value — and since the grid's date sort falls back to `mtime` (see
[`query/`](../query/README.md)), the keeper would sit in its old place until the
next open and then move without being asked. Invisible while only `date_taken`
drove the order; a silent reorder on restart once it does not.

There is no "companion location versus EXIF location" choice. The companion's
coordinates are mirrored over the indexed ones at index time, so a file has one
effective location and the real question is *which copy's*.

## Invariants a caller must uphold

- **Never write a sentinel hash.** `NULL` is the only value for "could not
  hash", and the reason is a library-wide false grouping.
- **Load, release, then compare.** The all-pairs loop runs on a blocking thread
  with no connection held.
- **A merge is `Owner`.** It rewrites a companion, stamps an mtime and trashes
  several files at once. Finding duplicates is `Device`; resolving them is not.
