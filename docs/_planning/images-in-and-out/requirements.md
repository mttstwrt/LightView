# Images in and out

## Background

Other web galleries leave the browser's own context menu on their images, which
gives "Copy Image" and "Save Image As…" for free, and leave the images
draggable, so an image can be dragged to the desktop or into another app. The
request is for the same abilities here, plus the reverse: drag files from the
desktop onto the window to upload them.

What already exists matters, because two of the three halves are partly built.

- **The context menu is custom.** Grid cells and the viewer both call
  `preventDefault` on `contextmenu` and open
  [`ContextMenu`](../../../src-solidjs/components/shared/ContextMenu.tsx)
  instead. It already has **Copy Image**: it fetches `/media/{path}`, re-encodes
  to PNG through a canvas, and writes a `ClipboardItem`. It has no download.
- **Every `<img>` is `draggable={false}`.** Nothing can be dragged out today. A
  plain mouse drag on a grid cell does nothing. Ctrl/Cmd-drag is range-select,
  and it calls `preventDefault` on `mousedown` only with the modifier held, which
  also suppresses a native drag. In the viewer, a mouse drag pans only when
  zoomed past 1×.
- **Upload exists:** `POST /api/upload` (`Device`), streamed and staged into
  `upload_dir`, then picked up by the watcher, behind a file-picker sheet. There
  is no drop target, so a file dropped on the window hits the browser's default,
  which **navigates the tab to the file and leaves the app**.

Three facts shape everything below.

1. **The bytes under the pointer are not the file.** A grid cell shows a
   downscaled thumbnail. The viewer shows `/media/{path}`, which is the original
   *except* for HEIC/HEIF, where it is a JPEG transcode. Any mechanism that
   hands over "the displayed image" hands over a thumbnail, or a JPEG, under the
   original's name.
2. **Only the browser holds the credential.** The session and device cookies
   are `HttpOnly; SameSite=Strict`. A drop target that is given a URL and
   fetches it itself (a file manager going through GVfs or KIO, a chat app
   unfurling a link) gets a 401, not a photo. **ComfyUI is exactly such a
   target.** Its page is another origin, it fetches a dropped URL itself with a
   plain `fetch`, and a cross-origin `fetch` sends no cookies (and a
   `SameSite=Strict` cookie would not go cross-site anyway).
3. **The upload path truncates anything over 2 MB, and reports success.**
   axum 0.8's `Multipart` limits a request body to 2 MB unless a
   `DefaultBodyLimit` says otherwise (axum 0.8.9 `src/extract/multipart.rs:58`),
   and nothing in this crate says otherwise. `upload_route` reads chunks with
   `while let Ok(Some(chunk))`, so the limit error reads as end-of-file: the
   truncated file is committed, later files are dropped, and the response is a
   200. This is established by reading both sources, not yet by a live upload.
   If it holds, every phone photo uploaded so far is damaged, and a user who
   trusted the green "Uploaded" and deleted the original has lost it.

## Requirements

### R0: An upload lands whole, or says it did not (prerequisite)

A file of any size (a 4 GB clip included) lands byte for byte. A stream that
ends early, for any reason, commits nothing for that file and is reported as a
failure. The failure names what did land in the same request. The disk margin
is enforced throughout the write, not only when a file starts, because lifting
the 2 MB ceiling removes the one thing that currently stops a single stream
from filling the disk.

### R1: Download the original from the context menu

A **Download** entry for a single item, in the grid and in the viewer, for
images and videos, at every trust level. It delivers the file exactly as it is
on disk: a HEIC arrives as HEIC, with its own name. If the gallery's password
window has lapsed, Download raises the password prompt like any other action.
It never fails silently, and never replaces the app with an error page.

### R2: Copy stays as it is

**Copy Image** already does what the native menu's entry does, at the same
fidelity (a PNG bitmap), and works for HEIC because it decodes the `/media`
JPEG. No change is planned. Two weaknesses are named rather than fixed: a
failure is logged to the console and not shown to the user, and on a browser
with a canvas-area ceiling below the image's pixel count (iOS Safari is reported
at about 16.7 MP, which a 24 MP iPhone photo exceeds) the copy fails. Both are
unmeasured here.

### R3: Drop files on the window to upload them

- While files from outside are dragged over the window, an overlay says they
  will be uploaded.
- A drop opens the existing upload sheet with those files listed, **appended**
  to any files already picked and not yet sent. The sheet is the one confirm
  step, the one progress bar and the one result, for picked and dropped files
  alike. A drop clears any previous result or error shown in the sheet.
- Files that are not a media type the server accepts are left out before
  anything is sent, **whether they were dropped or picked**, and the sheet says
  how many were left out. The picker's `accept="image/*,video/*"` does not do
  this: it admits SVG, which the server refuses, and desktop pickers offer
  "All files". A dropped folder arrives as a file-like entry with no media
  extension, so it is left out the same way.
- More than 100 files is sent as several requests, because the server caps a
  request at 100 parts.
- If the password window has lapsed, uploading raises the password prompt
  rather than failing with a bare 401.
- **A file dropped on the window never navigates away from the app**: in any
  boot state (opening, unreachable, session ended, `/pair`), whether or not
  uploads are enabled, and whether or not an upload is already running. When
  upload is unavailable or an upload is in flight, the drop is refused.

### R4: Get an image into ComfyUI with its embedded metadata, and no change to authentication

ComfyUI is the main destination, and LightView runs both on the ComfyUI
machine (`lightview <dir>`) and on a remote server (`--serve`). In both cases
the file must reach ComfyUI **byte for byte**, so whatever it carries arrives
with it:

- the workflow and prompt a ComfyUI PNG keeps in its text chunks;
- the equivalents in WebP EXIF and in video containers;
- camera EXIF.

"Metadata" here means what is **in the file**. LightView's own tags, ratings
and notes live in sidecars. ComfyUI has nowhere to read them, and writing them
into the file would modify an original, which LightView never does.

**R1 is how this is met, in both modes.** Download writes the original.
Dragging the finished download out of the browser's download list, or out of
a file manager, into ComfyUI is an ordinary file drop from another
application. ComfyUI reads a file drop first, so the workflow loads.

On the ComfyUI machine there is a second route that needs no code: an
**Open With** entry in `server.toml` that opens the file manager with the file
selected (see [design.md](design.md#local-mode-show-in-file-manager-no-code)).
It saves making a copy in Downloads.

**Direct drag from LightView into ComfyUI is on hold.** It cannot be done
without a URL that works with no cookie, which is a change to authentication.
The findings are recorded in
[design.md](design.md#on-hold-direct-drag-into-comfyui), so the work can resume
from them rather than from scratch.

## Non-goals

- **The browser's native context menu.** In the grid it would save or copy the
  thumbnail, and it has none of tag, rate or delete. Firefox already opens it on
  Shift+right-click, whatever the page does.
- **Downloading several items at once.** The native menu does not do it either,
  and the options are all worse than nothing yet: see
  [design.md](design.md#alternatives).
- **Resuming an interrupted download.** Range requests work, but browsers only
  resume a download when the response carries a validator (`ETag` or
  `Last-Modified`), and none is sent.
- **Dragging anything out of LightView, for now.** See R4.
- **Copy-and-paste into ComfyUI with metadata** from the browser. Chromium's
  async clipboard decodes and re-encodes `image/png` on write, which drops the
  text chunks the workflow lives in. Copy Image stays a bitmap copy.
- **Saving to the iOS Photos library.** A download on iOS goes to Files. Getting
  it into Photos needs the Web Share API with the whole file already in memory,
  which is a different mechanism.
- **Uploading a folder's contents.** A dropped folder is skipped, and the sheet
  counts it among the files left out.
- **Naming the destination folder, or choosing it per drop.** Where uploads
  land is server configuration, the client is not told it, and the grid has no
  folders to drop onto.
