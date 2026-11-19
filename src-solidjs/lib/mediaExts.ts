// Shared media-extension classification: a mirror of the backend's
// `MediaType::from_extension` (companion/schema.rs).
//
// A mirror rather than a value on the wire because the client needs it before
// anything is fetched — to classify a cell, and to decide what an upload may
// send. `mediaExts.test.ts` reads the Rust match and fails when the two lists
// differ, so drift breaks `npm test` instead of silently skipping a type the
// server would take, or sending one it refuses.

/** All video container extensions the app indexes. */
export const VIDEO_EXTS = new Set([
  "mp4", "mov", "avi", "mkv", "webm", "m4v", "wmv", "flv",
]);

/** Every extension the gallery holds: stills, GIF and video. */
export const MEDIA_EXTS = new Set([
  "jpg", "jpeg", "png", "webp", "bmp", "tiff", "tif", "heic", "heif",
  "avif", "raw", "cr2", "nef", "arw", "dng",
  "gif",
  ...VIDEO_EXTS,
]);

/** Subset WebKitGTK's <video> can actually decode (H.264/AAC in MP4,
 *  VP8/9 in WebM). The rest go to the external-player fallback. */
export const PLAYABLE_VIDEO_EXTS = new Set(["mp4", "mov", "webm", "m4v"]);

/** Lower-cased extension of a path, or "" when it has none. */
export function extOf(path: string): string {
  const dot = path.lastIndexOf(".");
  return dot >= 0 ? path.slice(dot + 1).toLowerCase() : "";
}

/** Whether the path's extension is a video format. */
export function isVideoPath(path: string): boolean {
  return VIDEO_EXTS.has(extOf(path));
}

/** Whether the upload route will take a file sent under `name`.
 *
 *  The server's rule (`server::upload::sanitize_name`), restated: the basename
 *  under either separator, trimmed of whitespace and then of dots, must carry
 *  a media extension. The trim is what makes `.jpg` a name with no extension
 *  rather than a JPEG — the server stops a whole request at a name it refuses,
 *  so admitting one here costs every file after it. */
export function isUploadableName(name: string): boolean {
  const base = name.slice(Math.max(name.lastIndexOf("/"), name.lastIndexOf("\\")) + 1);
  const trimmed = base.trim().replace(/^\.+|\.+$/g, "");
  const dot = trimmed.lastIndexOf(".");
  return dot > 0 && MEDIA_EXTS.has(trimmed.slice(dot + 1).toLowerCase());
}
