# Design

[requirements.md](requirements.md) · [server/](../../server/README.md) ·
[frontend/](../../frontend/README.md)

## How ComfyUI reads a drop

R4 is built to fit this, so it comes first. ComfyUI's frontend
([`src/utils/eventUtils.ts`](https://github.com/Comfy-Org/ComfyUI_frontend/blob/main/src/utils/eventUtils.ts),
`extractFilesFromDragEvent`, read on `main` in September 2026) does three
things, in order:

1. It takes `dataTransfer.files`, minus any `image/bmp`. The BMP filter exists
   because a browser dragging an `<img>` synthesizes a re-encoded bitmap rather
   than the original.
2. If there are none, it takes the first line of `text/uri-list` (or
   `text/x-moz-url`) and calls **`fetch(uri)` with no options**, which means
   CORS mode and no cookies on a cross-origin request.
3. It wraps the response body in a `File`, named from ComfyUI's private
   asset-info type if present and otherwise from the URI itself. A non-OK
   response yields no file.

Everything it then does (loading a workflow from PNG text chunks, WebP EXIF or
a video container, or feeding a LoadImage node) reads that `File`'s bytes.

So there are exactly two ways in. One is **real files in the `DataTransfer`**,
which a page cannot supply for bytes it does not already hold (alternative 13).
The other is **a URL ComfyUI can fetch with no cookie, across origins**.
Everything LightView serves today needs the cookie. R4 therefore needs a URL
that authorizes itself.

## Placement

**Server: [`server/routes.rs`](../../../src-rust/src/server/routes.rs),
[`server/upload.rs`](../../../src-rust/src/server/upload.rs), and one new
`server/drag_links.rs`. Nothing below the server layer changes.**

- **R0.** The fix stays inside the upload route and `StagedUpload`:
  - The `/api/upload` route gets `DefaultBodyLimit::disable()`, on that route
    only.
  - A chunk or field error becomes an error response.
  - `StagedUpload::write` re-checks the free-space margin every 64 MiB written.
- **R1.** `GET /download/{*rel}` joins the `guarded` group beside `/media`. It
  validates a `RelPath`, resolves it, refuses a non-`MediaType` extension, calls
  the existing `serve_file`, and adds `Content-Disposition` to a 200 or 206.
- **R4.** Three pieces:
  - **`server/drag_links.rs`** is an in-memory registry mapping a token to a
    `RelPath` and an expiry. It sits beside the launch-token and session state,
    because it is authorization state and not a service: the server owns who
    may ask.
  - A **`register_drag_link`** arm in the command table (`Device`). Like every
    other arm, it is two lines: a `require` and a call.
  - **`GET /drag/{token}/{name}`**, in a **new route group** that is gated on
    readiness but not on the cookie, because the token *is* the authorization.
    It serves through the same `serve_file` and the same header helper as
    `/download`.

The pipeline is not involved in either route. The pipeline's job is turning a
file into bytes a browser can show, and both routes exist to *not* do that.
Dependencies still point downward: the routes use `path` and `MediaType`, as
`upload.rs` already does.

**Frontend: components call `lib/`, and only `lib/ipc.ts` builds a backend URL
or handles a 401.**

| Where | What |
|---|---|
| `lib/ipc.ts` | Adds `downloadUrl(path)`, `dragUrl(token, path)`, `api.registerDragLink`, and `ensureSession()`, which runs one cheap `invoke` so a lapsed password raises the existing shared challenge. `upload()` calls `ensureSession()` first, sends batches of at most 100, reports one progress fraction, and reads `uploaded` off an error body |
| `lib/mediaExts.ts` | Adds `IMAGE_EXTS` beside `VIDEO_EXTS`, and `isMediaName(name)`, which trims dots the way `sanitize_name` does |
| `lib/fileDrag.ts` (new) | `startFileDrag(event, path)`, described below. It also holds the "our own drag is active" flag, set on `dragstart` and cleared on `dragend` |
| `ThumbnailCell.tsx`, `MediaViewer.tsx` | `draggable` plus `onDragStart → startFileDrag` |
| `lib/fileDrop.ts` (new) | The one set of window `dragover`/`drop` listeners, installed by `index.tsx` before anything renders, `/pair` included. For any drag whose types include `Files` and is not our own, it always calls `preventDefault`. It hands a drop to the registered handler, or refuses it when none is registered |
| `components/upload/DropZone.tsx` (new) | Registers the handler once the app is ready and uploads are enabled, and renders the overlay |
| `components/upload/UploadSheet.tsx` | The pending list (files plus a left-out count) and the busy flag move up into `App` |
| `components/shared/ContextMenu.tsx` | A **Download** entry: single item, any trust level. It awaits `ensureSession()`, then clicks a transient `<a href download>` |

`startFileDrag`, **synchronously inside `dragstart`**:

1. Generates a 32-byte token with `crypto.getRandomValues`.
2. Sets `text/uri-list` to `location.origin + dragUrl(token, path)`.
3. Sets `DownloadURL` to `application/octet-stream:<name>:<same URL>`, for
   Chromium file managers.
4. Fires `api.registerDragLink(token, path)` **without awaiting it**.

A drop cannot happen before the pointer has travelled to another window, and
the registration is one small request, so it has landed long before ComfyUI
fetches the link.

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
| Gating | the `guarded` group: cookie, and 503 until ready |
| Body | the file as it is on disk. `Range`/206 comes through `serve_file`. No validator is sent, so a download cannot resume |
| `Content-Type` | `mime_for(ext)` |
| `Content-Disposition` | 200 and 206 only. `attachment; filename="<ASCII fallback>"; filename*=UTF-8''<RFC 5987>`. Control characters, `"` and `\` are replaced in the fallback. The value is built fallibly, and a name that still cannot form a header gets a bare `attachment` |
| Refusal | 404 for an extension that is not a `MediaType` |

`/media` currently serves **any** existing file under the root. That is flagged
separately. Neither new route inherits it. Both still serve a media-named file
under `.lightview/trash/`, as `/media` does today, which is no wider, because
the trash is `Device`.

**3. `register_drag_link { token, path }` (R4), `Device`.**

- `token` must be exactly 64 lowercase hex characters; anything else is refused.
- A duplicate token is refused.
- `path` is a `RelPath` with a `MediaType` extension that resolves under the
  root.
- A registration lives **60 seconds**.
- At most 256 live entries. Expired ones are purged on each insert, and a full
  registry refuses.

**4. `GET /drag/{token}/{name}` (R4). No cookie.** This is the new trust
surface.

| | |
|---|---|
| Authorization | the token: registered, unexpired, and `name` equal to the registered file's basename. Anything else is 404 |
| Reuse | any number of fetches within the 60 seconds, because a file manager may send a `HEAD` before the `GET` |
| Body and headers | exactly as `/download`: the raw file, the same `Content-Type` and `Content-Disposition` |
| CORS | `Access-Control-Allow-Origin: *` on **every** response of this route, the 404 included, so ComfyUI sees a clean non-OK response rather than a network error. No `Allow-Credentials`: there is no credential to allow |
| Gating | 503 until ready, like everything past the bootstrap group |

**Why a self-authorizing URL is acceptable here, when the first draft
rejected it.** The first draft's objection was a bearer link to a private
photo leaking into a chat app. The bounds now make the exposure exactly what
the gesture already means:

- **One file.** The token names one path.
- **The one being dragged.** It is minted per drag, by a session that already
  passed the cookie and the password.
- **For a minute.** It expires after 60 seconds.
- **Only where the bind is reachable.** It is loopback in local mode and the
  LAN under `--serve`.

A leaked link gives whoever holds it, and can reach the bind, the image the
user was in the act of handing to another application, for sixty seconds.

**5. The drag payload.** `text/uri-list` and `DownloadURL` both carry the
`/drag` URL:

- **Nothing session-bound.** A target fetches the URL without a cookie.
- **No thumbnail.** It would carry no metadata.
- **No `Files`.** That keeps our own drag out of the upload path, and ComfyUI's
  URI branch is the one we want it to take.
- A `:` in the name is replaced with `_` in the `DownloadURL` field only,
  because that format splits on colons.

**Nothing durable changes.** There is no schema change, no `format_version`
bump, no sidecar field and no settings key. The drag registry is process
memory, and a restart forgets it, which costs nothing, since every link in it
would be expiring within a minute anyway.

## Cost in concepts

- **A third route group: authorized by a capability in the URL.** Until now
  there were two: the unauthenticated bootstrap group, and everything else
  behind the cookie. This is the largest cost in the plan. The server README's
  trust section has to name it, its bounds, and the one route in it.
- **One client-generated secret, the only one in the system.** Every other
  token is minted server-side. This one cannot be, because `dragstart` must set
  its data synchronously, before any round trip could return. That is an
  *except*, stated here and in the module comment. It gives a client nothing:
  a `Device` client can already read every file the token could name.
- **One route for the file, beside `/media`:** `/media` is *bytes a browser
  can render*, and `/download` and `/drag` are *the file*. That replaces an
  `except` inside `media()` (alternative 2).
- **One menu entry. One always-on drop guard with a pluggable handler, and one
  overlay. `ensureSession()`**, with two callers.
- **Two mirrored server facts** in the client: the image-extension list, beside
  the video list already mirrored in `mediaExts.ts`, and the number 100.
- **R0 adds nothing a reader must learn.** It makes the upload module's
  existing claims true.

**Could this be met by deleting something?** No. The native menu and native
image drag both hand over the wrong bytes, and those are the only candidates.

## Alternatives

**For R4, the strongest objection first.**

1. **Build no drag-out at all.** With R1, the user downloads the file and drags
   it from the file manager into ComfyUI, and the metadata arrives intact. That
   needs no new trust surface. *Lost only because ComfyUI is the stated main
   target and this makes it two gestures and a detour per image.* If the new
   route group is judged too costly, this is the fallback, and R1 already
   delivers it.
2. **A `text/uri-list` pointing at `/download` or `/media`.** ComfyUI's
   cookie-less fetch gets a 401 and no file. *Lost.*
3. **Server-minted token on `pointerdown`,** so the client never chooses a
   secret. That costs a round trip on every click, including ones that only
   open the viewer, and it races when the drag starts before the response.
   *Lost to the client-generated token's exactness.*
4. **A stateless HMAC token,** with no registry. It still needs a round trip,
   because the client cannot hold the key. It needs a new `hmac` dependency or
   a hand-rolled HMAC, and it cannot be refused once minted. The registry is
   about thirty lines with no dependency. *Lost.*
5. **Single-use tokens.** Tighter, but a file manager's `HEAD` before its
   `GET` would spend the token. The 60-second life is the bound that matters.
   *Lost.*
6. **Put ComfyUI's private `application/x-comfy-asset-info` type in the
   drag,** so ComfyUI names the file properly. That couples to an internal
   format that can change without notice. *Deferred:* adopt it only if the
   manual check shows a LoadImage upload named after the URL is actually a
   problem (C5).

**For R1 and R3, unchanged from the first draft.**

7. **The native context menu.** It saves or copies the thumbnail in the grid,
   and saves JPEG bytes for a HEIC in the viewer. *Lost on correctness.*
8. **A `?download` flag on `/media`.** It would add a mode that switches off
   two of that route's three branches. *Lost on the `except`.*
9. **Frontend-only Download from `/media`.** A HEIC silently arrives as JPEG.
   *Lost.*
10. **Batch download**, either as a server ZIP (a new dependency, a POST-bodied
    download, a bulk-export trust question) or as N anchor clicks (a
    permission prompt; iOS delivers one). *Deferred, and lost, respectively.*
11. **Remove `draggable={false}` from the `<img>`s.** The grid hands over the
    thumbnail, and ComfyUI filters the synthesized bitmap anyway. *Lost.*
12. **Copy and paste into ComfyUI.** Chromium re-encodes `image/png` on a
    clipboard write, which strips the workflow chunks. *Lost for metadata.*
13. **Put a real `File` in the `DataTransfer` at `dragstart`,** which ComfyUI
    would read first. The whole original must already be in memory when the
    drag starts, which in the grid it is not. Chromium also carries drag files
    between pages as filesystem paths, so an in-memory `File` probably does
    not survive into another tab (unmeasured). *Lost.*
14. **Start uploading as soon as files are dropped, with no sheet.** *Lost on
    reusing the one sheet.*
15. **The server skips unsupported parts instead of refusing.** *The prefilter
    is kept.* The server saying what landed on failure is adopted in R0.
16. **A fixed body ceiling instead of disabling the limit.** It is a knob with
    no principled value, and it does not stop a stream running the disk below
    the margin. *Lost.*
17. **Name the upload folder in the overlay.** That is wire contract for
    decoration. *Lost.*
18. **Probe with `HEAD /download/…` instead of `ensureSession()`.** That is a
    second 401 path in `ipc.ts`. *Lost.*

## Assumptions

Each assumption is **unmeasured** unless it says otherwise.

| # | Assumption | If wrong | How it gets measured |
|---|---|---|---|
| A0 | Uploads over 2 MB are truncated today. Established by reading axum 0.8.9 and `upload_route`, not by a live request | R0 shrinks to its error-path half | `drive.sh` uploads a 5 MB file **before** the fix |
| C1 | ComfyUI runs in a browser tab. If it is ComfyUI Desktop (Electron), the drop arrives as an OS-level drag, and `text/uri-list` is standard on every platform, so it probably still works | R4 needs a separate look for the desktop app | Manual |
| C2 | ComfyUI and LightView are on the same machine, so the fetch is loopback to loopback: no TLS, and no Local Network Access prompt from Chrome | Under `--serve`, the browser must trust LightView's certificate (Settings → Connection) or the fetch fails. A ComfyUI page from the LAN fetching a loopback LightView triggers Chrome's local-network permission prompt | Manual, in whichever arrangement you use |
| C3 | Your ComfyUI version has the URI fallback. It was read from current `main`. Older frontends had the same "files, else fetch the first URI" order as far as I recall, but that is not verified | An older ComfyUI ignores the drop | Manual |
| C4 | A drag of a `<div>` carrying only strings puts nothing in `files`, so ComfyUI takes the URI branch | ComfyUI reads a synthesized file instead | Headless: Playwright's real in-page drag, asserting what a `drop` listener sees |
| C5 | A ComfyUI `File` named after the whole URL is harmless. Workflow loading ignores the name. A LoadImage upload may get an odd filename | Adopt alternative 6 | Manual |
| A2 | Chromium lands a `DownloadURL` drop in your file manager. Best effort only: X11 historically yes, Wayland reported broken for some file managers | File-manager drops do nothing. R4 is unaffected | Manual |
| A4 | A same-origin `<a download>` clicked after an `await` still downloads without fresh user activation | Download needs a second tap after a password prompt | Headless Chromium; iOS by hand |
| A5 | `(pointer: fine)` is false on phones and true on laptops | A phone gets a draggable cell that competes with long-press | The 390px run, and a phone |
| A6 | A dropped folder appears in `files` with no media extension | A folder reaches `upload()` and the request fails | Manual |

The first draft's gate is gone. It existed to test whether Chromium's download
manager would carry the cookie, and nothing in R4 depends on the cookie now.
The fetch ComfyUI makes is an ordinary cross-origin `fetch`, which the headless
suite can perform itself (see Verification).

## The two checks

**Second implementation.**

- `fileDrag.ts` has two callers: the grid cell and the viewer.
- `ensureSession()` has two: Download and upload.
- The drop handler slot has two: the pre-ready refusal and `DropZone`.
- The header helper has two: `/download` and `/drag`.
- The drag registry has one consumer, and is concrete: a map and a clock, not
  an interface.

**Seam.** The new route group is the one place this plan pushes on the
architecture, and it is placed where the architecture says authorization
lives: in the server, beside the other tokens, with the command table deciding
who may mint one. The friction that remains is the mirrored upload limits,
which point at a missing client check rather than a wrong seam.

## The work

**Phase 0: Uploads land whole (R0).** This stands alone and ships first.
1. In `drive.sh`, upload a 5 MB file and compare it; watch it fail.
2. `DefaultBodyLimit::disable()` on the upload route.
3. Handle `Err` in both loops, which returns early and lets `StagedUpload`'s
   `Drop` remove the temp file.
4. JSON error bodies carrying `uploaded`.
5. A margin re-check in `write`, every 64 MiB.

**Phase 1: Download (R1).**
1. The `/download` route and the header helper, unit-tested for ASCII,
   non-ASCII, `"`, `\`, `:` and CR/LF.
2. `downloadUrl` and `ensureSession` in `ipc.ts`.
3. The menu entry.

**Phase 2: Drag into ComfyUI (R4).**
1. `drag_links.rs`, with unit tests driven by an injected clock: expiry, the
   256 cap, a duplicate, a malformed token, a basename mismatch.
2. The `register_drag_link` arm.
3. The `/drag` route in its own group, with CORS.
4. `dragUrl` and `registerDragLink` in `ipc.ts`.
5. `lib/fileDrag.ts`. `draggable` on `ThumbnailCell` when `(pointer: fine)`.
   `draggable` on the viewer's image container for stills at zoom 1. Ctrl/Cmd-
   drag stays range-select, and the zoomed pan keeps its drag, because both
   call `preventDefault` on `mousedown`.

**Phase 3: Drop to upload (R3).**
1. `isMediaName`, applied to picked files too.
2. Batch `upload()`, with `ensureSession()` first.
3. Lift the sheet's state into `App`. A drop appends to the list and clears
   the result.
4. `fileDrop.ts`, installed in `index.tsx`, ignoring our own drag.
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
  - **`/drag`**, in both modes:
    - Register a link with the cookie, then fetch it **without** any cookie:
      200, identical bytes, and `Access-Control-Allow-Origin: *`.
    - An unregistered token, or a wrong name, returns 404 and still carries
      the CORS header.
    - A registration with no cookie returns 401.
    - A malformed token is refused.
- **`grid.mjs`**, the built SPA in headless Chromium:
  - **ComfyUI's path, end to end short of ComfyUI itself.** The fixture is a
    PNG carrying a `workflow` text chunk, written by the script.
    1. A real mouse drag of its cell yields a `text/uri-list` and nothing in
       `files` (C4).
    2. From a second page on another origin, the script runs ComfyUI's
       `fetchDroppedAsset` body verbatim against that URL.
    3. Expect identical bytes and the `workflow` chunk intact.
  - **Download:** fires a `download` event with the right name and identical
    bytes.
  - **Drop to upload:** a trusted file drop (CDP `Input.dispatchDragEvent`)
    carrying a PNG and a `.txt` opens the sheet with one file listed and one
    left out, and the upload appears in the grid. With uploads disabled, and
    again mid-upload, the URL is unchanged and nothing is uploaded.
  - No console errors and no failed requests, as now.
- **Manual, with your ComfyUI:**
  - Drag a ComfyUI-generated PNG onto the canvas: the workflow loads.
  - Drag one onto a LoadImage node: the image loads. Check the name it gets
    (C5).
  - Drag a video that carries a workflow.
  - Drag one image into your file manager (A2, best effort).
  - Download once on a phone, and once after the password window has lapsed.

## On completion

- **`server/README.md`:**
  - The trust section gains the capability group, with its bounds and why it
    exists.
  - `/download` and `/drag` go in the routes table.
  - A section on `/media` versus the file routes.
  - Uploads gains the body limit, the mid-stream margin, the JSON error and
    batching.
- **`architecture.md`:** "Trust is a property of the bind" gains one paragraph
    on the capability group, and the request diagram gains both routes.
- **`frontend/README.md`:** under Chrome, add:
  - Download, and why it goes through `ensureSession`.
  - The drop guard.
  - Drag-out: why it carries a link rather than a file, and why ComfyUI is
    what shaped that.
- **`build-and-verify.md`:** add the new checks.
- **`upload.rs`'s module comment:** "bounded" now names the mid-stream margin.
- Delete this directory.
