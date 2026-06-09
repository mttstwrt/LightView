// Shared media-extension classification. Mirrors the backend's
// MediaType::from_extension video list (companion/schema.rs) — keep in sync.

/** All video container extensions the app indexes. */
export const VIDEO_EXTS = new Set([
  "mp4", "mov", "avi", "mkv", "webm", "m4v", "wmv", "flv",
]);

/** Subset WebKitGTK's <video> can actually decode (H.264/AAC in MP4,
 *  VP8/9 in WebM). The rest go to the external-player fallback. */
export const PLAYABLE_VIDEO_EXTS = new Set(["mp4", "mov", "webm", "m4v"]);

/** Lower-cased extension of a path, or "" when it has none. */
export function extOf(path: string): string {
  const dot = path.lastIndexOf(".");
  return dot >= 0 ? path.slice(dot + 1).toLowerCase() : "";
}

export function isVideoPath(path: string): boolean {
  return VIDEO_EXTS.has(extOf(path));
}
