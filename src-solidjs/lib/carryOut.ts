// Carrying a file out of the window: dragging a cell or the viewer's photo
// into another application. The grid and the viewer are where such a drag
// starts; both arm it and fill it here, so the rule is stated once.
//
// **A plain mouse drag moves what can move in place, and otherwise carries the
// file out.** In a set view the order can move, so reordering owns the plain
// drag; in the viewer a zoomed photo can move, so panning owns it. The caller
// answers that question as `allowed`. Ctrl/Cmd-drag stays selection's. Mouse
// only, like reordering: on touch a drag is a scroll and a long press is the
// context menu.
//
// **A drag out is Download, aimed by the pointer** — the same URL (the file's
// own bytes, past the HEIC transcode) under the same name. It is the browser
// that fetches it, as a download, with the session cookie it already holds.
//
// What it is not, and why:
//  - **One file per drag.** `DownloadURL` carries a single entry, so a drag
//    from a selected cell carries that cell, not the selection.
//  - **Chromium only.** `DownloadURL` is Chromium's; Firefox and Safari have no
//    way for a page to hand out a file, and there a drag starts and drops
//    nothing.
//  - **No `text/uri-list`.** A file manager offered a URL may fetch it itself —
//    without the session cookie — and save the 401 body under the photo's name.
//    A file URL would avoid the fetch, but Chromium strips `file:` URLs from a
//    drag a page starts.

import { fileName, originalUrl } from "./ipc";

/** Make `el` the source of a drag out for this press, or not.
 *
 *  Decided per press — a mouse, the primary button, no Ctrl/Cmd, and `allowed`
 *  — and written to the element directly, because the browser reads
 *  `draggable` when the pointer first moves past its drag threshold, which is
 *  after the press. A cleared `draggable` is also what keeps a touch press, a
 *  Ctrl-drag and a reorder exactly as they were. */
export function armCarryOut(el: HTMLElement, e: PointerEvent, allowed: boolean): void {
  el.draggable =
    allowed && e.pointerType === "mouse" && e.button === 0 && !e.ctrlKey && !e.metaKey;
}

/** Put the file at `path` on a starting drag, so dropping it into another
 *  application saves the file itself.
 *
 *  `DownloadURL` is `mime:name:url`, and Chromium splits it at the first two
 *  colons, so a colon in the name would cut it there — it becomes `_`. The
 *  MIME type is generic because the name's extension is what the receiving
 *  application goes by. `image`, when given, is drawn under the pointer in
 *  place of the dragged element. */
export function carryOut(dt: DataTransfer, path: string, image?: Element | null): void {
  const url = new URL(originalUrl(path), window.location.href).href;
  const name = fileName(path).replaceAll(":", "_");
  dt.setData("DownloadURL", `application/octet-stream:${name}:${url}`);
  dt.effectAllowed = "copy";
  if (image) {
    const box = image.getBoundingClientRect();
    dt.setDragImage(image, box.width / 2, box.height / 2);
  }
}
