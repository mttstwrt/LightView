# Design

[requirements.md](requirements.md) · [server/](../../server/README.md) ·
[frontend/](../../frontend/README.md)

## Placement

**Server: [`server/routes.rs`](../../../src-rust/src/server/routes.rs) and
[`server/upload.rs`](../../../src-rust/src/server/upload.rs), and nothing below
them.**

- **R0.** The fix stays inside the upload route and `StagedUpload`:
  - The `/api/upload` route gets `DefaultBodyLimit::disable()`, on that route
    only.
  - A chunk or field error becomes an error response.
  - `StagedUpload::write` re-checks the free-space margin every 64 MiB written.
    64 MiB is the granularity, so a single write can overshoot the margin by at
    most that much; one `statvfs` per 64 MiB is nothing next to the write
    itself.
- **R1.** `GET /download/{*rel}` joins the `guarded` group beside `/media`, so
  it inherits the auth layer and the readiness gate without restating either.
  It does four things:
  1. validates a `RelPath` and resolves it against the root;
  2. refuses a path whose extension is not a `MediaType`;
  3. calls the existing `serve_file`;
  4. adds `Content-Disposition`, only to a 200 or 206.

  No service or pipeline code is involved. The pipeline's job is turning a
  file into bytes a browser can show, and this route exists to *not* do that,
  so putting it there would make the pipeline learn about a caller that wants
  its work skipped.

The route depends on `path` and `companion::schema::MediaType`, as `upload.rs`
already does. Dependencies still point downward.

**Frontend: components call `lib/`, and only `lib/ipc.ts` builds a backend URL
or handles a 401.**

| Where | What |
|---|---|
| `lib/ipc.ts` | Adds `downloadUrl(path)` beside `mediaUrl`, and `ensureSession()`, which runs one cheap `invoke` so a lapsed password raises the existing shared challenge. `upload()` calls `ensureSession()` first, sends batches of at most 100, reports one progress fraction across all of them, and reads the `uploaded` list off an error body |
| `lib/mediaExts.ts` | Adds `IMAGE_EXTS` beside `VIDEO_EXTS`, and `isMediaName(name)`, which trims dots the way `sanitize_name` does before reading the extension |
| `lib/fileDrop.ts` (new) | The **one** set of window `dragover`/`drop` listeners, installed by `index.tsx` before anything renders, `/pair` included. For any drag whose types include `Files`, it always calls `preventDefault`. It hands a drop to whichever handler is registered and refuses it (`dropEffect = "none"`) when none is |
| `components/upload/DropZone.tsx` (new) | Registers the handler once the app is ready and uploads are enabled, and renders the overlay |
| `components/upload/UploadSheet.tsx` | The pending list (files plus a left-out count) and the busy flag move up into `App`, so the picker and a drop write the same signal and a drop mid-upload is refused |
| `components/shared/ContextMenu.tsx` | A **Download** entry: single item, any trust level, beside Copy Image. It awaits `ensureSession()`, then clicks a transient `<a href download>` |
| `lib/fileDrag.ts` (new, R4 only) | `setFileDrag(dataTransfer, path)` builds the payload from `ipc.downloadUrl`. It also holds a module-level "our own drag is active" flag, set on `dragstart` and cleared on `dragend`, which `fileDrop.ts` checks |
| `ThumbnailCell.tsx`, `MediaViewer.tsx` (R4 only) | `draggable` plus `onDragStart → setFileDrag` |

The drop guard and the drop handler are separate on purpose. The guard is page
policy and must hold before the app exists. Handling a drop needs capabilities
and the sheet, which exist only once the app is ready. One listener set with a
pluggable handler gives both, without two sets of listeners racing over one
event.

## Contract

**1. `/api/upload` behaviour (R0).** The request shape is unchanged. The
response changes in two ways:

| Case | Before | After |
|---|---|---|
| Body over 2 MB | 200, file truncated | the file lands whole |
| Stream ends early, or a later part is refused | 200 with a truncated file, or a plain-text 400/413/507 | the partial file is discarded, and the response is a JSON error, `{ "error": "…", "uploaded": [ … ] }`, listing what landed before the failure |

The only client is `ipc.upload()`, which changes in the same commit.

**2. A new wire route: `GET /download/{*rel}` (R1).** One side is the SPA's
download anchor. The other side (R4 only) is the browser's own download
manager, fulfilling a `DownloadURL` drag outside the page.

| | |
|---|---|
| Trust | `Device`, the same as `/media`, which already serves these bytes for every format except HEIC/HEIF |
| Gating | the `guarded` group: authenticated, and 503 until the scan and the watcher are ready |
| Body | the file as it is on disk. `Range`/206 comes through `serve_file`. It sends no validator, so a download cannot resume (a non-goal) |
| `Content-Type` | `mime_for(ext)`, so HEIC is `image/heic` |
| `Content-Disposition` | 200 and 206 only, so a 404 or 416 body is never saved under a photo's name. `attachment; filename="<ASCII fallback>"; filename*=UTF-8''<RFC 5987>`. Control characters, CR and LF included, and `"` and `\` are replaced in the fallback. The value is built fallibly: a name that still cannot form a header gets a bare `attachment`, never a panic |
| Refusal | 404 for an extension that is not a `MediaType`, the same answer as a missing file |

That refusal is deliberate but narrow. `/media` currently serves **any** existing
file under the root: `RelPath` accepts `.lightview/…`, and nothing between the
route and `serve_file` checks an extension. That is outside this plan and
flagged separately. `/download` does not inherit the non-media half. It does
still serve a media-named file inside `.lightview/trash/`, exactly as `/media`
does today. That is no wider, because the trash is `Device` anyway.

**3. The drag payload (R4):**
`application/octet-stream:<basename>:<location.origin + downloadUrl(path)>`.
The format splits on the first two colons, so a `:` in the basename (legal on
Linux) is replaced with `_` in the *suggested name* only. Nothing else goes into
the `DataTransfer`: no `text/uri-list`, no `text/plain`, no `Files`.

**Nothing durable changes.** There is no schema change, no `format_version`
bump, no sidecar field and no settings key. Uploaded files land in `upload_dir`
exactly as picked ones do today, except that they are now whole.

## Cost in concepts

- **One route, and one distinction to hold:** `/media` is *bytes a browser can
  render*, and `/download` is *the file*. That replaces what would otherwise be
  an `except` inside `media()` (alternative 2). **No `except` case is added
  anywhere.**
- **One menu entry.**
- **One always-on drop guard with a pluggable handler, and one overlay.** The
  rule is that a drop is handled only when its types include `Files`, it is not
  our own drag, and a handler is registered.
- **`ensureSession()`**, with two callers from the start: Download and upload.
- **Two mirrored server facts** in the client: the image-extension list, which
  joins the video list already mirrored in `mediaExts.ts` with a "keep in sync"
  comment, and the number 100. This is a real cost. Publishing both in
  `get_capabilities` would add wire contract for two values that change about
  never.
- **R0 adds nothing a reader must learn.** It makes the upload module's
  existing claims ("streamed", "bounded", "cleaned up on every error path")
  true.
- **R4 only: one helper with two callers**, one flag, and `draggable` on two
  elements.

**Could this be met by deleting something?** Only one candidate: remove the
`preventDefault` calls and let the native menu and native image drag through.
Both hand over the wrong bytes (alternatives 1 and 6), so nothing is deleted.

## Alternatives

1. **The native context menu**, either by dropping the `preventDefault` or by
   passing Shift+right-click through. In the grid, the element under the
   pointer is a thumbnail, so Save Image saves a few hundred pixels under the
   original's name. In the viewer, a HEIC saves as JPEG bytes. It also loses
   tag, rate and delete. *Lost on correctness.*
2. **A `?download` flag on `/media`** instead of a new route. That is one fewer
   route, but `media()` would gain a mode that switches off two of its three
   branches, and every later reader of that handler would have to know which.
   *Lost narrowly, on the `except`.*
3. **Frontend only: `<a download href={mediaUrl(path)}>`.** No server change,
   but an iPhone photo would silently download as a JPEG transcode, under a
   `.heic` name unless renamed. *Lost on correctness.*
4. **Batch download as a server-streamed ZIP.** It needs either a new
   dependency or a hand-written ZIP writer, and a POST-bodied download that a
   plain anchor cannot make. It also raises a question the trust table has
   never had to answer: may a phone bulk-export the library? *Deferred until
   someone needs it.*
5. **Batch download as N anchor clicks.** Chromium asks permission for multiple
   downloads, and iOS delivers only the first. *Lost.*
6. **Drag-out by removing `draggable={false}` from the `<img>`s.** The grid
   would hand over the thumbnail under the original's name, silently. *Lost on
   correctness.*
7. **A `text/uri-list` fallback for browsers without `DownloadURL`.** The target
   fetches the URL without the cookie and saves a 401 body under a photo's
   name. *Lost.*
8. **Signed, cookie-less URLs, so a third party can fetch.** A bearer link to a
   private photo, pasted into a chat app, is a leak. *Lost firmly.*
9. **Put a real `File` into the `DataTransfer` at `dragstart`.** The bytes must
   exist synchronously when the drag starts, and in the grid they do not.
   *Lost.*
10. **Start uploading as soon as files are dropped, with no sheet.** It saves
    one click, but the user never sees what was left out, and it would need a
    second progress and result surface. *Lost on reusing the one sheet.*
11. **The server skips an unsupported part instead of refusing the request.**
    That fixes the refusal for every client, but a rejected file's bytes still
    cross the network first. *The prefilter is kept.* The half of this that
    matters, the server saying what landed when it fails, is adopted in R0.
12. **Raise the body limit to a fixed ceiling** (say 8 GiB) instead of
    disabling it. That is a knob with no principled value, and it still lets a
    stream run the disk below the margin. A margin re-check bounds the thing
    that actually runs out. *Lost.*
13. **Name the upload folder in the overlay** by adding `upload_dir` to
    `get_capabilities`. That is wire contract to decorate a message, and the
    sheet has never named it. *Lost.*
14. **Probe with `HEAD /download/…` instead of `ensureSession()`.** That also
    catches a missing file, but it needs a second 401-handling path in
    `ipc.ts`, beside `invoke`'s. *Lost to the one path.*

## Assumptions

Each assumption is **unmeasured** unless it says otherwise.

| # | Assumption | If wrong | How it gets measured |
|---|---|---|---|
| A0 | Uploads over 2 MB are truncated today. Established by reading axum 0.8.9 and `upload_route`, not by a live request | R0 shrinks to the error-path half, which is still needed | `drive.sh`: upload a 5 MB file and `cmp` it, **before** the fix, to record the failure |
| A1 | Chromium's download manager sends the `SameSite=Strict` cookies when it fulfils a `DownloadURL` drag. It is browser-initiated, like a typed URL | The drag saves nothing useful. R4 is dropped | The gate below |
| A2 | Chromium on your desktop lands a `DownloadURL` drop in your file manager. Public evidence: Chromium implemented this on Linux over X11's direct-save protocol (XDS), while under Wayland drops into some file managers (Dolphin, for one) are reported broken | R4 does nothing on your machine. It is dropped | The gate below |
| A3 | `application/octet-stream` as the `DownloadURL` MIME does not change the saved name or extension | Wrong extension. Fix: mirror `mime_for` too | The gate below |
| A4 | A same-origin `<a download>` clicked **after an `await`** still downloads, without fresh user activation, in Chromium, Firefox and iOS Safari | Download needs a second tap after a password prompt | Headless Chromium via Playwright's `download` event; iOS by hand |
| A5 | `(pointer: fine)` is false on phones and true on laptops, touchscreen laptops included | A phone gets a draggable cell that competes with long-press | The 390px run, plus a phone at the gate |
| A6 | A drop's `dataTransfer.files` lists a folder as an entry with no media extension | A folder reaches `upload()` and the request fails | Manual, at the gate |

The self-drop question (does our own drag expose `Files`?) is not an
assumption any more. The own-drag flag answers it either way, and costs three
lines. `dragend` fires at the source, which for our own drag is this page.

### The gate for R4

This needs no code beyond R1. With R1 deployed, open LightView in the browser
you use, paste the snippet below into the DevTools console, drag a grid cell to
your file manager, and check that a file arrives with the right name, the right
size and the right format. Try a HEIC if you have one.

```js
document.querySelectorAll("[data-vt-path]").forEach((c) => (c.draggable = true));
document.addEventListener("dragstart", (e) => {
  const p = e.target.closest?.("[data-vt-path]")?.dataset.vtPath;
  if (!p) return;
  const name = p.split("/").pop().replaceAll(":", "_");
  const url = `${location.origin}/download/${p.split("/").map(encodeURIComponent).join("/")}`;
  e.dataTransfer.setData("DownloadURL", `application/octet-stream:${name}:${url}`);
}, true);
```

A file with the right bytes means A1 to A3 hold, and Phase 3 goes ahead.
Anything else means R4 is dropped, and the result is recorded in the server
docs so nobody retries it blind.

## The two checks

**Second implementation.** Every new shared piece has two callers from the
start:

- `fileDrag.ts`: the grid cell and the viewer.
- `ensureSession()`: Download and upload.
- `fileDrop.ts`'s pluggable handler: the pre-ready refusal and `DropZone`.

Nothing introduces an interface, a plugin point or a setting.

**Seam.** Placement was not hard. The friction is the two mirrored limits, and
the review showed why they have been invisible: the picker's `accept` looked
like filtering and was not. That points at a missing client check, not at a
wrong seam.

## The work

**Phase 0: Uploads land whole (R0).** This comes first because it stands alone:
it is worth shipping even if nothing else in this plan is approved.
1. In `drive.sh`, upload a 5 MB file and compare the bytes. Watch it fail
   first, to record A0.
2. Add `DefaultBodyLimit::disable()` to the upload route.
3. Handle `Ok(None)`, `Ok(Some)` and `Err` in both loops; `Err` returns early
   and `StagedUpload`'s `Drop` removes the temp file.
4. Make error bodies JSON carrying `uploaded`.
5. Re-check the margin in `write` every 64 MiB.

**Phase 1: Download (R1).**
1. Add the `download` route and a unit-tested header helper, covering ASCII,
   non-ASCII, `"`, `\`, `:`, and CR/LF.
2. Add `downloadUrl` and `ensureSession` in `ipc.ts`.
3. Add the menu entry.

**Phase 2: Drop to upload (R3).**
1. Add `isMediaName`, applied to picked files too.
2. Batch `upload()`, with `ensureSession()` first.
3. Lift the sheet's state into `App`; a drop appends and clears the result.
4. Add `fileDrop.ts`, installed in `index.tsx`.
5. Add `DropZone`. The overlay shows on a `dragover` carrying `Files`. It hides
   on `drop`, on a `dragleave` whose `relatedTarget` is null, and after one
   second without a `dragover`, as a backstop for unbalanced enter/leave
   events. `dragend` does not fire for an external drag, so it cannot be
   relied on.

**Gate:** the console check above.

**Phase 3: Drag out (R4), only if the gate passes.**
1. Add `lib/fileDrag.ts`, and have `fileDrop.ts` check its flag.
2. In `ThumbnailCell`: add `draggable` when `(pointer: fine)`, with
   `onDragStart → setFileDrag`. Ctrl/Cmd-drag stays range-select, because its
   `mousedown` already calls `preventDefault`.
3. In `MediaViewer`: add `draggable` on the image container for stills at
   zoom 1. Past 1×, the pan's `mousedown` `preventDefault` already wins.

## Verification

- `cargo clippy --all-targets --all-features` clean. `cargo test`, including
  the header helper and a margin re-check test.
- `npx tsc --noEmit` clean.
- **`drive.sh`**, against the real binary over curl:
  - A 5 MB upload lands byte for byte.
  - A request cut off mid-part (curl's `--max-time`) leaves no file and no
    `.lv-upload-*.tmp`.
  - `/download` returns the fixture byte for byte, with
    `Content-Disposition: attachment`.
  - A range request returns 206.
  - Both traversal spellings return 404.
  - A non-media file returns 404, with no `Content-Disposition`.
  - Unpaired requests under `--serve` return 401.
- **`grid.mjs`**, the built SPA in headless Chromium:
  - Download fires a `download` event with the right suggested name and
    identical bytes.
  - A **trusted** file drop (CDP `Input.dispatchDragEvent` with real file paths;
    a synthetic `drop` event never triggers the browser's navigation, so it
    could not fail) carrying a PNG and a `.txt` opens the sheet with one file
    listed and one left out, and the upload appears in the grid.
  - The same drop with uploads disabled, and again mid-upload, leaves the URL
    unchanged and nothing uploaded.
  - Phase 3: a real mouse drag of a cell produces a `DownloadURL` payload.
  - No console errors and no failed requests throughout, as now.
- **Manual:** the gate, then one drag from the grid and one from the viewer on
  your desktop, one download on a phone, and one Download after the password
  window has lapsed.

## On completion

- **`server/README.md`:**
  - `/download` in the routes table, and a short section on `/media` versus
    `/download`.
  - Under Uploads: the body limit, the mid-stream margin, the JSON error
    carrying `uploaded`, and batching.
  - If R4 was dropped, the gate's result.
- **`architecture.md`:** add `/download` to the request diagram.
- **`frontend/README.md`:** under Chrome, add:
  - Download, and why it goes through `ensureSession`.
  - The always-on drop guard and its pluggable handler.
  - If built, drag-out, and why it carries nothing on non-Chromium browsers.
- **`build-and-verify.md`:** add the new checks.
- **`upload.rs`'s module comment:** "bounded" now names the mid-stream margin.
- Delete this directory.
