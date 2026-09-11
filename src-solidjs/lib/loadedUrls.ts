// ---------------------------------------------------------------------------
// URLs whose bytes have already been decoded once in this session.
//
// `ThumbnailCell` fades a thumbnail in the first time it appears and not
// afterwards, so a cell recycled onto a URL the user has already seen must not
// fade again. That has to survive the cell being destroyed and rebuilt, which
// makes it module state rather than component state.
//
// It is also written from outside the cell: a decode-then-swap has already
// decoded the bytes off-DOM by the time the cell is pointed at them, so the
// swapper marks the URL before committing it and the cell appears without a
// fade. That second writer is why this lives in `lib/` — the loading machinery
// depends on it, and `lib/` must not depend back on a component.
// ---------------------------------------------------------------------------

// Capped so a long session browsing huge galleries doesn't accumulate URLs
// forever; on overflow the set resets and some cells fade once more.
const LOADED_URLS_CAP = 4096;
const loadedUrls = new Set<string>();

export function markUrlLoaded(url: string): void {
  if (loadedUrls.size >= LOADED_URLS_CAP) loadedUrls.clear();
  loadedUrls.add(url);
}

export function hasUrlLoaded(url: string): boolean {
  return loadedUrls.has(url);
}
