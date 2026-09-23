# Design

Principle 1's five questions, in order.

## Placement

**`src-rust/src/server/routes.rs`, and nowhere else in the crate.** One private
function, called at the top of both byte routes:

```
servable_extension(&RelPath) -> Option<String>   // the lowercased extension, or None to refuse
```

`media()` calls it immediately after `RelPath::new` and before `Root::resolve`.
It replaces the extension computation `media()` already does, so the `?fit=`
branch, the HEIC branch and `serve_file` read the same value they read today.
`thumb()` calls it beside `ThumbTier::from_segment`, before the thumbnail
service is asked for anything.

Dependencies still point downward. The server already depends on
`companion::schema::MediaType` through `upload.rs`. The `.lightview` name is
spelled out today by each module that touches the directory (`trash.rs`,
`settings.rs`, the watcher's `classify` in `gallery.rs`, the companion reader).
There is no shared constant, and introducing one is not this change.

### The seam finding, and why it is not taken here

The reviewer's seam test holds up. The rule is really *a client may name only
paths the index could hold*, and the byte routes are two of several places a
client names a path. `trash_files`, `regenerate_thumbnail` and
`precache_thumbnails` accept a `.lightview` path too (see the requirements'
out-of-scope list). The natural single home for the `.lightview` half would be
`RelPath::new`, where every wire path enters.

It is not taken here, for two reasons:

- **`RelPath::new` also parses paths read back from the database.**
  `cache/duplicates.rs` builds a `RelPath` from every `thumbs_j` row with `?`,
  and today's `/thumb` has already let a client write `.lightview/trash/…` rows
  there. Refusing `.lightview` in the constructor would make duplicate detection
  fail outright for any gallery holding such a row. That change needs its own
  audit of every `RelPath::new` and `relativize` call site, and a cleanup of the
  existing rows.
- **The extension half cannot live there at all.** Directories are `RelPath`s
  too (`RelPath::parent`, `upload_dir`), so the rule would split across two
  layers.

So this change closes the confidentiality hole on the byte routes, and the
command-side issue stays open as its own change, with `RelPath::new` as the
candidate fix.

## Contract

| What changes | Who is on the other side |
|---|---|
| `/media` and `/thumb` answer 404 for a path they answered 200 before: a non-`MediaType` extension, or a `.lightview` component | **The SPA.** `mediaUrl` callers: `MediaViewer`, `viewerCache`, `JustifiedGrid` (`?fit=`), `ThumbnailCell` (GIF), `ContextMenu` (copy image), `DuplicatesPanel`. `thumbUrl` callers: `JustifiedGrid`, `MediaViewer`, `TagManagerPanel`, `DuplicatesPanel`, `MergeDialog`. The first four of each take paths from the index, which never holds either kind (A1). **`DuplicatesPanel` and `MergeDialog` take paths from `thumbs_j`**, which can hold a `.lightview/…` row written through today's `/thumb`. After this change such a row's image 404s. That is correct: a trashed file offered as a duplicate candidate is the bug, not the 404. **No client change.** |
| `docs/server/README.md` and `docs/architecture.md` state the rule | readers, and the planned `/download` route (below) |

Nothing durable changes: no schema, no `format_version`, no sidecar field, no
setting. Existing polluted `thumbs_j` rows stay until the next cache rebuild, or
until the command-side change cleans them.

**The planned `/download` route** (`docs/_planning/images-in-and-out/` on
`claude/loving-fermi-pqkhdl`) applies the extension half of this rule. It says
it "does still serve a media-named file inside `.lightview/trash/`, exactly as
`/media` does today", and once this lands that sentence is false. `/download`
lives in `routes.rs` too, so it can call the same private function and inherit
both halves. That plan needs a one-line revision. This change does not touch it.

## Cost in concepts

- **One private function, with two clauses and no `except`.** The rule it
  states: *a byte route opens only a file the index could hold.*
- **Nothing is deleted.** `mime_for`'s `_ => application/octet-stream` arm stays
  reachable. `MediaType` admits seven extensions `mime_for` does not name
  (`raw cr2 nef arw dng wmv flv`), and today they are served that way. After this
  change that arm is reachable **only** for those seven. This is noted, not
  changed.

### The `.lightview/` decision: refuse it, spelled exactly

The brief asked for this decision. **Refuse it, at any depth, compared exactly.**

1. **The grid can never ask for it.** The scan skips every dot-prefixed entry.
   The watcher's `classify` skips any component equal to `.lightview`. No
   indexed path contains one, so refusing it cannot break a grid request. That
   holds *because* the comparison is the watcher's own. A case-insensitive
   comparison would 404 a `2026/.LightView/x.png` that the watcher indexed. That
   is the same failure that rules out alternative 2 below.
2. **The trash already decided that clients do not address it by path.** The
   entry id is opaque on purpose (`docs/storage/README.md`, "The entry id is not
   a path"). Serving `.lightview/trash/<id>/<name>` is an undesigned second way
   in, on both byte routes. That is why `/thumb` is included.
3. **It does not depend on the allowlist staying small.** `from_extension`'s own
   doc calls adding an extension "a security change rather than a format one".
   With this rule, whatever `.lightview/` comes to hold stays unserved,
   whatever that list grows to.

**Why not case-insensitive.** On a case-insensitive mount, such as exFAT, NTFS
or an SMB share, `.LIGHTVIEW/trash/…` opens the real trash. ASCII case-folding
would close that one spelling and leave the class open: an 8.3 short name
(`LIGHTV~1`) and exFAT's Unicode up-casing (`ı` becomes `I`) reach the same
directory. What gets through is bounded. The extension half still refuses every
non-media file in `.lightview/`, so the residue is trashed *media*, which
`list_trash` and `restore_trash` already hand to a `Device`. A partial lexical
fix buys nothing that matters, and it costs the watcher agreement in reason 1.
See A4.

### The `/thumb` decision: include it

This goes beyond the brief. The case for it: reason 2 is not achieved while
`/thumb/jh/.lightview/trash/<id>/x.png` returns the photo at 2560 px. It is one
line in a handler with the same shape. It is the in-tree second consumer, which
answers the second-implementation test. And it stops the route writing
`thumbs_j` rows for non-gallery paths. The case against: it widens a change the
brief scoped to `media()`. If `/thumb` is left out, reason 2 should be dropped
from this plan, and the `.lightview` refusal stands on reasons 1 and 3 alone.

## Alternatives

1. **The extension rule only.** It closes every confidentiality hole in the
   table. Rejected, because it leaves `.lightview/`'s safety resting on the
   extension list (reasons 2 and 3).
2. **Refuse every dot-prefixed segment, which is the scan's exact rule.** This
   was the first choice: one rule shared with the scan. **Refuted by
   measurement:** the watcher indexes `2026/.hidden/x.png` (checked live), so the
   grid shows the item and this rule would 404 it in the viewer. It becomes the
   right rule once the watcher adopts the scan's filter, which is flagged
   separately.
3. **Serve only paths the index holds (a DB lookup).** This is the tightest
   contract, and it tracks the scan and the watcher automatically. Rejected. It
   adds a query to every request, including each Range chunk of a video scrub,
   and it ties byte-serving to index freshness.
4. **Refuse `.lightview` in `RelPath::new`.** This is the right seam for the
   command-side issue, but not this change. See "The seam finding".
5. **Check in `serve_file`.** Rejected. The `?fit=` and HEIC branches open the
   file before `serve_file` runs, and the rule concerns what a client may name,
   not how bytes are sent.
6. **Also check the canonical path after `resolve`.** It costs no syscall, and
   it closes A3's symlink case. Rejected for now. A `Device` cannot create a link
   (A3). The scan judges a link by its own name, so judging it by its target
   here would 404 an indexed link whose target has an unusual name. And it is a
   second check that would need its own explanation.

## Assumptions

| # | Taken on faith | If wrong | How it is checked |
|---|---|---|---|
| A1 | The index never holds a non-`MediaType` extension or a `.lightview` component. The scan and the watcher both filter on `MediaType`, and both exclude `.lightview` by the same exact comparison. | A grid item 404s in the viewer | By reading `provider/local.rs` and `classify`, then `grid.mjs` |
| A2 | The SPA's index-sourced callers send only indexed paths. The `thumbs_j`-sourced ones (duplicates, merge) may not, and a 404 there is correct. | A feature 404s | By reading the callers, then `grid.mjs` |
| A3 | The rule judges the **requested** name, not a symlink's target. A link `x.jpg → .lightview/settings.toml` inside the gallery would still serve. Only filesystem access can plant one: upload writes regular files by rename, and no `Device` command creates a link. | A `Device` route to creating links would reopen the hole. The check would then have to judge the resolved path (alternative 6). | By reading `upload.rs` and the command table |
| A4 | On a case-insensitive mount, another spelling of `.lightview` reaches the trash: another case, an 8.3 short name, or a Unicode up-cased letter. **Unmeasured on a real exFAT or SMB gallery.** | A `Device` fetches trashed media by path. No non-media file is exposed, and the trash is already `Device`-listable and `Device`-restorable. | Named, not tested |

## Verification

**Before the fix.** Append the `drive.sh` checks first, run them against the
current binary, and watch each refusal check **fail with 200**. The checks
assert 404, so a 401 (for example after the phone is revoked) would *also* fail
before the fix and pass nothing afterwards. Their placement is what makes the
failure a real one.

**`drive.sh`**, in the `--serve` section, **after pairing and before
`devices revoke`**:

- A fixture `2026/notes.txt`, created with the other fixtures. The scan ignores
  it.
- `sinv set_default_filter '{"filter":""}'` writes `.lightview/settings.toml`
  the ordinary way. `sinv trash_files '{"paths":["2026/dropped.png"]}'` makes a
  real trash entry, and `list_trash` names it. **Not `tall.png`:** its companion
  would travel into the trash, and both the companion check and the control
  below would then test a missing file.
- **Each of these is 404:** `/media/.lightview/settings.toml`,
  `/media/2026/notes.txt`, `/media/.lightview/companions/tall.png.lightview.json`,
  and `/media/.lightview/trash/<id>/2026/dropped.png`.
  `/thumb/jh/.lightview/trash/<id>/2026/dropped.png` is also 404.
- **A refused file answers the way a missing one does:** the same status and
  body as `/media/2026/no-such.png`, a path that *would* be served if it existed.
- **The control:** `/media/tall.png` from the same phone is 200 `image/png`.

**`cargo test`:**

- `routes_and_trust.rs`: a `Device` harness plants files after indexing (the
  route never reads the index): `.lightview/settings.toml`, `2026/notes.txt`, a
  companion under `2026/.lightview/companions/`, and a PNG under
  `.lightview/trash/1_0/`. It asserts 404 on `/media` for each, the same
  `(status, body)` as a missing media path, 404 on `/thumb/j/` for the trashed
  PNG, and 200 for `2026/a.png`.
- A unit test in `routes.rs` pins the predicate:
  - every extension `from_extension` admits, in upper and lower case, is
    admitted and lowercased;
  - no extension, and a non-media one, are refused;
  - `.lightview` is refused at the root and at depth;
  - `2026/.hidden/x.png` and `2026/.LightView/x.png` are **admitted**. That pins
    both deliberate choices, so a later tightening is a decision and not an
    accident.

**`grid.mjs`** covers R3 end to end for PNG. HEIC, video and upper-case
extensions rest on the unit test's table.

## Docs, in the same change

- `docs/server/README.md`: the `/media` and `/thumb` rows in the routes table, a
  short section on what the byte routes will and will not serve, and an
  invariant bullet.
- `docs/architecture.md`: the `/media` line in the data-flow diagram.
- The `routes.rs` module doc table, and a doc comment on `servable_extension`.
- `docs/build-and-verify.md` and `.claude/skills/verify/SKILL.md`: add the new
  checks to their summaries of what `drive.sh` covers.
- On completion, fold anything durable into those pages and delete this
  directory.
