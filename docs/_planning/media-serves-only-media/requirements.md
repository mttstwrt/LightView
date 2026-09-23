# `/media` serves media, and nothing else

## Background

`GET /media/{*rel}` serves **any existing file under the gallery root**.
`RelPath::new` refuses only empty, `.`, `..` and NUL segments. `Root::resolve`
confines a path without asking what kind of file it is. `media()` reads the
extension only to pick the `?fit=` and HEIC branches, and everything else reaches
`serve_file`, which sends an unknown type as `application/octet-stream`.

Reproduced against the real binary on `--serve`, as a paired `Device`:

| Request | Answer today |
|---|---|
| `/media/.lightview/settings.toml` | 200, the file |
| `/media/2026/notes.txt` (a stray non-media file) | 200, the file |
| `/media/.lightview/trash/<id>/tall.png.lightview.json` | 200, a companion |
| `/media/.lightview/trash/<id>/tall.png` | 200, the trashed photo |
| `/thumb/j/.lightview/trash/<id>/tall.png` | 200 `image/webp`, the trashed photo. At `jh` that is 2560 px, which is the photo |

The upload path already holds the right rule: `sanitize_name` admits only a name
whose extension `MediaType::from_extension` accepts.

## Requirements

### R1: A non-media extension is refused

`/media` answers 404 `not found` for any path whose extension is not a
`MediaType`. It does so **before any branch runs and before the path is
resolved**, which is the same answer a missing file gets. The rule reads only
the path string, so a refusal cannot reveal whether the file exists.

### R2: Nothing under `.lightview/` is served, at any depth

`/media` and `/thumb` answer the same 404 for any path with a component that is
exactly `.lightview`, wherever it appears. That is the component test the
watcher already uses to decide what is "ours". Companions live in a
`.lightview/` beside the media they describe (`2026/january/.lightview/…`), so a
check on the leading segment alone would miss most of them.

**`/thumb` is beyond the brief**, which named `media()`. It is included because
R2 means nothing if the trash is refused at full resolution on one route and
served at 2560 px on the other. See the design's "The `/thumb` decision".

### R3: Everything the grid shows is still served

Every path the index can hold keeps working, on every branch: Range/206,
`?fit=`, the HEIC transcode, every tier, and an upper-case extension such as
`IMG_0001.HEIC`. No client change.

### R4: The docs say what the byte routes will and will not serve

The routes table and invariants in `docs/server/README.md`, the `/media` line in
`docs/architecture.md`, and the route table in the `routes.rs` module doc.

## Done means

- The new `drive.sh` checks pass, and they **fail against today's binary**. That
  is what shows they test the change.
- `cargo clippy --all-targets --all-features` is clean.
- `cargo test` passes.
- `grid.mjs` passes.

## Out of scope, and flagged separately

- **`Device` commands accept `.lightview` paths.** `trash_files` resolves any
  `RelPath` against the gallery root (`services/trash.rs` `move_to_trash`). A
  paired phone can therefore move `.lightview/settings.toml`, a companion, or
  another trash entry into the trash. `regenerate_thumbnail` and
  `precache_thumbnails` can write tier rows for any path, which is how a
  non-gallery path reaches the duplicates panel (`cache/duplicates.rs` reads
  `thumbs_j`, not the index). This is the seam finding, and the design explains
  why it is not folded into this change.
- **The scan and the watcher disagree about dot-directories.** The scan skips
  every dot-prefixed entry (`provider/local.rs`), but the watcher skips only
  `.lightview`. Checked live: a file dropped into `2026/.hidden/` while the
  gallery is open is indexed, and the next open prunes it.
