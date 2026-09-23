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
  - `StagedUpload::write` re-checks the free-space margin every 64 MiB
    written.
- **R1.** `GET /download/{*rel}` joins the `guarded` group beside `/media`, so
  it inherits the cookie check and the readiness gate without restating
  either. It:
  - validates a `RelPath` and resolves it;
  - refuses a non-`MediaType` extension;
  - calls the existing `serve_file`;
  - adds `Content-Disposition` to a 200 or 206.

  The pipeline is not involved. Its job is turning a file into bytes a browser
  can show, and this route exists to *not* do that. The route depends on `path`
  and `MediaType`, as `upload.rs` already does.

**Frontend: components call `lib/`, and only `lib/ipc.ts` builds a backend URL
or handles a 401.**

| Where | What |
|---|---|
| `lib/ipc.ts` | Adds `downloadUrl(path)` beside `mediaUrl`, and `ensureSession()`, which runs one cheap `invoke` so a lapsed password raises the existing shared challenge. `upload()` calls `ensureSession()` first, sends batches of at most 100, reports one progress fraction, and reads `uploaded` off an error body |
| `lib/mediaExts.ts` | Adds `IMAGE_EXTS` beside `VIDEO_EXTS`, and `isMediaName(name)`, which trims dots the way `sanitize_name` does |
| `components/shared/ContextMenu.tsx` | A **Download** entry: single item, any trust level. It awaits `ensureSession()`, then clicks a transient `<a href download>` |
| `lib/fileDrop.ts` (new) | The one set of window `dragover`/`drop` listeners, installed by `index.tsx` before anything renders, `/pair` included. For any drag whose types include `Files`, it always calls `preventDefault`. It hands a drop to the registered handler, or refuses it when none is registered |
| `components/upload/DropZone.tsx` (new) | Registers the handler once the app is ready and uploads are enabled, and renders the overlay |
| `components/upload/UploadSheet.tsx` | The pending list (files plus a left-out count) and the busy flag move up into `App`, so the picker and a drop write the same signal |

The drop guard and the drop handler are separate on purpose. The guard is page
policy and must hold before the app exists. Handling a drop needs capabilities
and the sheet, which exist only once the app is ready.

## Contract

**1. `/api/upload` behaviour (R0).** The request shape is unchanged. The
response changes in two ways:

| Case | Before | After |
|---|---|---|
| Body over 2 MB | 200, file truncated | the file lands whole |
| Stream ends early, or a later part is refused | 200 with a truncated file, or a plain-text 400/413/507 | the partial file is discarded, and the response is a JSON error, `{ "error": "…", "uploaded": [ … ] }`, listing what landed before the failure |

The only client is `ipc.upload()`, which changes in the same commit.

**2. `GET /download/{*rel}` (R1).** The other side is the SPA's download
anchor.

| | |
|---|---|
| Trust | `Device`, the same as `/media`, which already serves these bytes for every format except HEIC/HEIF |
| Gating | the `guarded` group: cookie, and 503 until ready. **No new authentication of any kind** |
| Body | the file as it is on disk, which is what keeps ComfyUI's workflow chunks intact. `Range`/206 comes through `serve_file`. No validator is sent, so a download cannot resume |
| `Content-Type` | `mime_for(ext)`. ComfyUI chooses its reader by type, so this has to stay right, and it currently is |
| `Content-Disposition` | 200 and 206 only. `attachment; filename="<ASCII fallback>"; filename*=UTF-8''<RFC 5987>`. Control characters, `"` and `\` are replaced in the fallback. The value is built fallibly, and a name that still cannot form a header gets a bare `attachment` |
| Refusal | 404 for an extension that is not a `MediaType` |

`/media` currently serves **any** existing file under the root, which is
flagged separately. `/download` does not inherit the non-media half. It still
serves a media-named file under `.lightview/trash/`, as `/media` does today,
which is no wider, because the trash is `Device`.

**3. The client mirrors two server facts** for uploads: the extension
allowlist and the 100-part cap. The server remains the enforcement for both.

**Nothing durable changes.** There is no schema change, no `format_version`
bump, no sidecar field and no settings key.

## Local mode: "Show in file manager", no code

On the ComfyUI machine, the file is already on the disk ComfyUI's browser
reads from. `open_with` (`Owner`) already substitutes `{path}` into configured
arguments ([`services/files.rs`](../../../src-rust/src/services/files.rs),
`ExternalApp`), so a single `server.toml` entry opens the file manager with the
file selected:

```toml
[[external_apps]]
label = "Show in Dolphin"
command = "dolphin"
args = ["--select", "{path}"]
```

`nautilus --select {path}` is the GNOME equivalent. Dragging from
the file manager into ComfyUI is a file drop from another application, which
ComfyUI reads first. This belongs in the user-facing docs as a recipe, not in
code. A built-in "Show in folder" command, over the
`org.freedesktop.FileManager1.ShowItems` D-Bus call, would work with any file
manager, but nobody has asked for it, and the configuration already delivers
it.

## Cost in concepts

- **One route, and one distinction to hold:** `/media` is *bytes a browser can
  render*, and `/download` is *the file*. That replaces an `except` inside
  `media()` (alternative 2). No `except` case is added.
- **One menu entry. One always-on drop guard with a pluggable handler, and one
  overlay. `ensureSession()`**, with two callers: Download and upload.
- **Two mirrored server facts** in the client: the image-extension list, beside
  the video list already mirrored in `mediaExts.ts`, and the number 100.
- **R0 adds nothing a reader must learn.** It makes the upload module's
  existing claims true.
- **The trust model is untouched.** Every route this plan adds sits behind
  the existing cookie.

**Could this be met by deleting something?** No. The only candidates are the
`preventDefault` calls that suppress the native menu and native image drag,
and both of those hand over the wrong bytes.

## Alternatives

1. **The native context menu.** It saves or copies the thumbnail in the grid,
   and saves JPEG bytes for a HEIC in the viewer. *Lost on correctness.*
2. **A `?download` flag on `/media`.** It would add a mode that switches off
   two of that route's three branches. *Lost on the `except`.*
3. **Frontend-only Download from `/media`.** A HEIC silently arrives as JPEG.
   *Lost.*
4. **Batch download**, either as a server ZIP (a new dependency, a POST-bodied
   download, a bulk-export trust question) or as N anchor clicks (a permission
   prompt; iOS delivers one). *Deferred, and lost, respectively.*
5. **Direct drag into ComfyUI.** *On hold, by decision.* See below.
6. **Start uploading as soon as files are dropped, with no sheet.** *Lost on
   reusing the one sheet.*
7. **The server skips unsupported parts instead of refusing.** *The prefilter
   is kept.* The server saying what landed on failure is adopted in R0.
8. **A fixed body ceiling instead of disabling the limit.** It is a knob with
   no principled value, and it does not stop a stream running the disk below
   the margin. *Lost.*
9. **Name the upload folder in the overlay.** That is wire contract for
   decoration. *Lost.*
10. **Probe with `HEAD /download/…` instead of `ensureSession()`.** That is a
    second 401 path in `ipc.ts`. *Lost.*

## Assumptions

Each assumption is **unmeasured** unless it says otherwise.

| # | Assumption | If wrong | How it gets measured |
|---|---|---|---|
| A0 | Uploads over 2 MB are truncated today. Established by reading axum 0.8.9 and `upload_route`, not by a live request | R0 shrinks to its error-path half | `drive.sh` uploads a 5 MB file **before** the fix |
| A1 | A finished download can be dragged from the browser's download list (Chromium's download bubble, Firefox's downloads panel) into a page as a real file | The ComfyUI route is "drag from the file manager" instead, one window further | Manual |
| A2 | A same-origin `<a download>` clicked after an `await` still downloads without fresh user activation | Download needs a second tap after a password prompt | Headless Chromium; iOS by hand |
| A3 | A dropped folder appears in `files` with no media extension | A folder reaches `upload()` and the request fails | Manual |

## On hold: direct drag into ComfyUI

Recorded so this can resume from what is known rather than from scratch.

**How ComfyUI reads a drop.** Its frontend
([`src/utils/eventUtils.ts`](https://github.com/Comfy-Org/ComfyUI_frontend/blob/main/src/utils/eventUtils.ts),
`extractFilesFromDragEvent`, read on `main` in September 2026) does three
things, in order:

1. It takes `dataTransfer.files`, minus any `image/bmp`.
2. If there are none, it takes the first `text/uri-list` line and calls
   `fetch(uri)` with no options: a cross-origin request with no cookies.
3. It wraps the body in a `File`, and a non-OK response yields none.

**Measured: a page cannot hand another page a file through a drag in
Chromium.** Method: headless Chromium through CDP drag interception. Page A, on
one origin, starts a drag. The payload the browser process receives is
replayed as a drop into page B, on another origin, and B's `drop` handler
records what it sees.

| What page A added at `dragstart` | What page B received |
|---|---|
| a script-made `File` (`items.add(new File(…))`) | no file; a `text/plain` item holding the file's *name* |
| a `File` from an `<input type=file>`, backed by a real path | nothing |
| `text/uri-list` set to `file:///…` | the string only, no file. ComfyUI would then `fetch("file:///…")`, which a web page may not do |

Caveat: CDP interception may be lossier than a real desktop drag. Even so, a
real desktop drag goes through the same browser-side drag data, so a file that
is missing there cannot appear later.

**So the only way in is a URL ComfyUI can fetch without the cookie.** That is
true in local mode too. The obstacle is not network reach. It is that ComfyUI
never sends LightView's cookie, which is `SameSite=Strict` besides. The design
that does it, reviewed twice, is in this directory's git history at commit
`5a03122`:

- a per-drag, 60-second, one-file link at `/drag/{token}/{name}`;
- a client-generated token, registered when the mouse button goes down;
- a registry keyed by digest;
- a route group with its own readiness and CORS layer.

Its cost is a third route group in the trust model. Resuming means deciding
that cost is acceptable, nothing else.

## The work

**Phase 0: Uploads land whole (R0).** This stands alone and ships first.
1. In `drive.sh`, upload a 5 MB file and compare it; watch it fail.
2. `DefaultBodyLimit::disable()` on the upload route.
3. Handle `Err` in both loops, which returns early and lets `StagedUpload`'s
   `Drop` remove the temp file.
4. JSON error bodies carrying `uploaded`.
5. A margin re-check in `write`, every 64 MiB.

**Phase 1: Download (R1, and through it R4).**
1. The `/download` route and the header helper, unit-tested for ASCII,
   non-ASCII, `"`, `\`, `:` and CR/LF.
2. `downloadUrl` and `ensureSession` in `ipc.ts`.
3. The menu entry.
4. The file-manager recipe in the user-facing docs.

**Phase 2: Drop to upload (R3).**
1. `isMediaName`, applied to picked files too.
2. Batch `upload()`, with `ensureSession()` first.
3. Lift the sheet's state into `App`. A drop appends to the list and clears
   the result.
4. `fileDrop.ts`, installed in `index.tsx`.
5. `DropZone`. The overlay hides on `drop`, on a `dragleave` with a null
   `relatedTarget`, and after one second without a `dragover`.

## Verification

- `cargo clippy --all-targets --all-features` clean, and `cargo test`.
- `npx tsc --noEmit` clean.
- **`drive.sh`**, the real binary over curl:
  - **Uploads:** a 5 MB upload lands byte for byte. A request cut off mid-part
    leaves no file and no temp file.
  - **`/download`:** the fixture byte for byte, with `attachment`. A range
    returns 206. Traversal and a non-media file return 404. Unpaired requests
    under `--serve` return 401.
- **`grid.mjs`**, the built SPA in headless Chromium:
  - **Download** of a PNG that carries a `workflow` text chunk, written by the
    script, fires a `download` event with the right name. The bytes are
    identical and the chunk is intact. This is ComfyUI's input, checked short
    of ComfyUI.
  - **Drop to upload:** a trusted file drop (CDP `Input.dispatchDragEvent`)
    carrying a PNG and a `.txt` opens the sheet with one file listed and one
    left out, and the upload appears in the grid. With uploads disabled, and
    again mid-upload, the URL is unchanged and nothing is uploaded.
  - No console errors and no failed requests, as now.
- **Manual, with your ComfyUI:**
  - In both modes, download a ComfyUI PNG and drag it from the browser's
    download list onto the canvas: the workflow loads (A1).
  - Locally, run the Open With recipe and drag from the file manager.
  - Download once on a phone, and once after the password window has lapsed.

## On completion

- **`server/README.md`:**
  - `/download` in the routes table.
  - A short section on `/media` versus `/download`.
  - Uploads gains the body limit, the mid-stream margin, the JSON error and
    batching.
  - Record why there is no drag-out: a pointer to the git history of this
    plan, and the measured result.
- **`architecture.md`:** add `/download` to the request diagram.
- **`frontend/README.md`:** under Chrome, add Download (and why it goes through
  `ensureSession`) and the drop guard.
- **`build-and-verify.md`:** add the new checks.
- **The user-facing README:** the ComfyUI route, and the file-manager recipe.
- **`upload.rs`'s module comment:** "bounded" now names the mid-stream margin.
- **The route table in `routes.rs`'s module comment** gains `/download`.
- Delete this directory.
