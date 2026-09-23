# `/media` serves media, and nothing else

## Background

`GET /media/{*rel}` serves **any existing file under the gallery root**.
`RelPath::new` refuses only empty, `.`, `..` and NUL segments, `Root::resolve`
confines without asking what kind of file it is, and `media()` reads the
extension only to pick the `?fit=` and HEIC branches. Everything else reaches
`serve_file`, which sends an unknown type as `application/octet-stream`.

Reproduced against the real binary on `--serve`, as a paired `Device`:

| Request | Answer today |
|---|---|
| `/media/.lightview/settings.toml` | 200, the file |
| `/media/2026/notes.txt` (a stray non-media file) | 200, the file |
| `/media/.lightview/trash/<id>/tall.png.lightview.json` | 200, a companion |
| `/media/.lightview/trash/<id>/tall.png` | 200, the trashed photo |

The upload path already holds the right rule: `sanitize_name` admits only a name
whose extension `MediaType::from_extension` accepts.

## Requirements

### R1 — A non-media extension is refused

`/media` answers 404 `not found` for any path whose extension is not a
`MediaType`, **before any branch runs and before the path is resolved**. The
answer is byte-identical to the one for a missing file, so a refusal confirms
nothing about what exists.

### R2 — Nothing under `.lightview/` is served, at any depth

`/media` answers the same 404 for any path with a `.lightview` segment,
compared ignoring ASCII case, wherever the segment appears. Companions live in
a `.lightview/` beside the media they describe (`2026/january/.lightview/…`),
so a check on the leading segment alone would miss most of them.

### R3 — Everything the grid shows is still served

Every path the index can hold keeps working, including every branch: Range/206,
`?fit=`, and the HEIC transcode. The grid never asks for anything else, so no
client change is needed.

### R4 — The docs say what `/media` will and will not serve

The routes table and the invariants in `docs/server/README.md`, and the route
table in the `routes.rs` module doc.

## Done means

- The new `drive.sh` checks pass. They must also **fail against today's binary**,
  which is what shows they test the change and not something else.
- `cargo clippy --all-targets --all-features` is clean.
- `cargo test` passes.
- `grid.mjs` passes, since it exercises the viewer's `/media` requests (R3).

## Out of scope, and flagged separately

- **`/thumb/{tier}/{*rel}` renders any path it can resolve.** Checked live:
  `/thumb/j/.lightview/trash/<id>/tall.png` is 200 `image/webp`, and it writes a
  tier row for a path the index does not hold. It cannot leak non-media bytes,
  because it only ever returns a WebP it decoded, so it is not the
  confidentiality problem this plan fixes.
- **The scan and the watcher disagree about dot-directories.** The scan skips
  every dot-prefixed entry (`provider/local.rs`). The watcher skips only
  `.lightview`. Checked live: a file dropped into `2026/.hidden/` while the
  gallery is open is indexed, and the next open prunes it.
