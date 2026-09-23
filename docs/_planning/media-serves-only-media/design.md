# Design

Principle 1's five questions, in order.

## Placement

**`src-rust/src/server/routes.rs`, and nowhere else in the crate.** One private
function beside `media()`:

```
servable_extension(&RelPath) -> Option<String>   // the lowercased extension, or None to refuse
```

`media()` calls it immediately after `RelPath::new` and before `Root::resolve`.
It replaces the extension computation `media()` already does, so the `?fit=` and
HEIC branches and `serve_file` read the same value they read today.

Dependencies still point downward. The server already depends on
`companion::schema::MediaType` through `upload.rs`, and the `.lightview` name is
already spelled out by the services that own the directory (`trash.rs`,
`settings.rs`, the watcher's classifier in `gallery.rs`).

**It is a route rule, not a path rule.** `RelPath` is the domain type the trash,
the companion reader and restore use, and those legitimately name paths under
`.lightview/`. Teaching `path.rs` the metadata directory's name would make the
lowest layer know about one of its callers.

## Contract

| What changes | Who is on the other side |
|---|---|
| `/media` answers 404 for a path it answered 200 before: a non-`MediaType` extension, or a `.lightview` segment in any ASCII case | Every `mediaUrl` caller in the SPA: `MediaViewer`, `viewerCache`, `JustifiedGrid` (`?fit=`), `ThumbnailCell` (GIF), `DuplicatesPanel`, and `ContextMenu` (copy image). All of them pass a `path` taken from the index, and the index cannot hold either kind (see A1). **No client change.** |
| `docs/server/README.md` states the rule | readers, and the planned `/download` route (below) |

Nothing durable changes: no schema, no `format_version`, no sidecar field, no
setting.

**The planned `/download` route** (`docs/_planning/images-in-and-out/` on
`claude/loving-fermi-pqkhdl`) applies the extension half of this rule and says it
"does still serve a media-named file inside `.lightview/trash/`, exactly as
`/media` does today." Once this lands, that sentence is false. The better fix is
for `/download` to call `servable_extension` and inherit both halves. That plan
needs a one-line revision. This change does not touch it.

## Cost in concepts

- **One private function, with two clauses and no `except`.** The rule it
  states: *`/media` opens only a file the index could hold.* It needs no other
  sentence.
- **Nothing is deleted.** `mime_for`'s `_ => application/octet-stream` arm stays
  reachable. `MediaType` admits seven extensions `mime_for` does not name
  (`raw cr2 nef arw dng wmv flv`), and they are served that way today. After
  this change that arm is reachable **only** for those seven. That is noted here
  and not changed.

### The `.lightview/` decision: refuse it

The brief asked for this decision. **Refuse it, at any depth.** Four reasons:

1. **The grid can never ask for it.** The scan skips every dot-prefixed entry,
   and the watcher's rule 3 skips any `.lightview` component. So no indexed path
   contains one, and refusing it cannot break a request the grid makes.
2. **The trash already decided that clients do not name trash paths.** The
   entry id is opaque *on purpose* (`docs/storage/README.md`, "The entry id is
   not a path"). Serving `.lightview/trash/<id>/<name>` by path is a side door
   around that. It is no *wider* read, since `list_trash` is `Device`, but it is
   a second way to address the trash, and that was never designed.
3. **Defence that does not depend on the allowlist staying small.**
   `from_extension`'s own doc comment says adding an extension is "a security
   change rather than a format one". With the segment rule in place, whatever
   `.lightview/` holds later, including a media-named cache, stays unserved
   whatever that list grows to.
4. **ASCII case is ignored because the client picks the spelling.** On a
   case-insensitive mount, such as an exFAT or NTFS drive or an SMB share,
   `.LIGHTVIEW/trash/…` opens the real trash. The comparison costs nothing.

## Alternatives

1. **Only the extension rule.** It closes every confidentiality hole in the
   table. It leaves the trash readable by path and makes `.lightview/`'s safety
   depend on the extension list. Rejected, for the reasons above.
2. **Refuse every dot-prefixed segment, the scan's exact rule.** This was the
   first choice: it is one rule shared with the scan, and it needs no case
   question. **Refuted by measurement:** the watcher indexes
   `2026/.hidden/x.png` (checked live), so the grid shows that item and this rule
   would make the viewer 404 it. It becomes the right rule once the watcher
   adopts the scan's filter. That is flagged separately.
3. **Serve only paths the index holds (a DB lookup).** This is the tightest
   contract, and it tracks the scan and the watcher automatically. Rejected. It
   adds a query to every request, including each Range chunk of a video scrub.
   It ties byte-serving to index freshness. It would also state a different
   contract from `/download` and `/thumb` for no gain the two clauses do not
   already give.
4. **Check in `RelPath::new`.** Rejected, because it is the wrong layer (see
   Placement). `Root::nested(".lightview/trash")` would stop constructing.
5. **Check in `serve_file`.** Rejected, because the `?fit=` and HEIC branches
   open the file before `serve_file` runs, and the rule concerns what a client
   may name, not how bytes are sent.
6. **A tower layer over `/media` and `/thumb`.** Rejected: that is a framework
   for two lines, and `/thumb` is out of scope.

## Assumptions

| # | Taken on faith | If wrong | How it is checked |
|---|---|---|---|
| A1 | The index never holds a non-`MediaType` extension or a `.lightview` segment. Both the scan and the watcher filter on `MediaType`, and both exclude `.lightview`. | A grid item 404s in the viewer | By reading `provider/local.rs` and `classify` in `services/gallery.rs`, then `grid.mjs` |
| A2 | Every `/media` request the SPA makes carries an indexed path | A feature 404s | By reading the six `mediaUrl` callers, then `grid.mjs` |
| A3 | The rule checks the **requested** name, not a symlink's target. A symlink `x.jpg → .lightview/settings.toml` inside the gallery would still serve. Only filesystem access can plant one: upload writes regular files by rename, and no `Device` command creates a link. This matches the scan, which also judges a link by its own name. | A `Device` path to creating links would reopen the hole. The rule would then have to judge the resolved path. | By reading `upload.rs` and the command table |
| A4 | ASCII case-folding is the case-insensitivity that matters. exFAT upper-cases through a Unicode table, so `ı` (dotless i) becomes `I`, and `.lıghtvıew` could open `.lightview` there. **Unmeasured.** | A `Device` reads trashed media by path on an exFAT gallery. That is no wider than today, because the trash is `Device`. | Named, not tested |

## Verification

**Before the fix.** Append the `drive.sh` checks first and run them against
the current binary. The four refusal checks must **fail**, returning 200. That
is what shows they test something (build-and-verify.md, "Two ways a check can
pass while testing nothing").

**`drive.sh`**, in the `--serve` section, where the paired phone is:

- A fixture `2026/notes.txt`, created with the other fixtures. The scan ignores
  it.
- `sinv set_default_filter` writes `.lightview/settings.toml` the ordinary way,
  and `sinv trash_files` then `list_trash` produces a real trash entry.
- **Each of these is 404:** `/media/.lightview/settings.toml`,
  `/media/2026/notes.txt`, `/media/.lightview/companions/tall.png.lightview.json`,
  the trashed file's path, and an upper-cased `/media/.LIGHTVIEW/…` spelling.
- **A refused file answers exactly as a missing one:** the same status and body
  as `/media/.lightview/no-such-file.toml`.
- **The control:** `/media/tall.png` from the same phone is 200 `image/png`.

**`cargo test`:**

- `routes_and_trust.rs`: a `Device` harness plants each kind on disk,
  including a `.LightView/` directory, so the case-insensitive refusal is
  exercised on a case-sensitive filesystem. It asserts 404 for each, the same
  `(status, body)` as a missing path, and 200 for `2026/a.png`.
- A unit test in `routes.rs` pins the predicate's edges: case, depth, no
  extension, and **`2026/.hidden/x.png` is allowed**. That pins alternative 2's
  rejection, so a later tightening is a deliberate change and not an accident.

**`grid.mjs`** covers R3 end to end: the viewer's `/media` image and the rest of
the grid.

## Docs, in the same change

- `docs/server/README.md`: the `/media` row in the routes table, a short
  section on what `/media` will and will not serve, and an invariant bullet.
- The `routes.rs` module doc table and a doc comment on `servable_extension`.
- `docs/build-and-verify.md` and `.claude/skills/verify/SKILL.md`: add the new
  checks to their summaries of what `drive.sh` covers.
- On completion, fold anything durable into those pages and delete this
  directory.
