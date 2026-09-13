// ThumbHash placeholder lookup for grid cells.
//
// `get_items` inlines each item's ~25-byte ThumbHash (base64) in the payload,
// so the whole grid can paint blurry placeholders the moment the item list
// arrives — zero extra round-trips, which is what makes a phone on the far side
// of a LAN feel instant. This module derives a path → hash map from the
// reactive item list and decodes hashes to tiny PNG data URLs on demand,
// memoized per hash string.

import { items } from "../stores/galleryStore";
import { thumbHashToDataURL } from "thumbhash";
import type { SortedItem } from "./types";

// Decoded data URLs, keyed by the base64 hash itself (identical hashes across
// items — unlikely but possible — share one decode). Each entry is a ~1 KB
// PNG data URL. Coarse overflow reset, same policy as loadedUrls in
// ThumbnailCell.
const DATA_URL_CACHE_CAP = 8192;
const dataUrlCache = new Map<string, string>();

// path → base64 hash, rebuilt only when the sortedItems array identity
// changes. The reactive read of `items()` happens in the caller's
// tracking scope (a cell's JSX), so cells update when fresh items land.
let mapFor: SortedItem[] | null = null;
let hashByPath = new Map<string, string>();

function lookupHash(path: string): string | undefined {
  const list = items();
  if (list !== mapFor) {
    hashByPath = new Map();
    for (const it of list) {
      if (it.thumbhash) hashByPath.set(it.path, it.thumbhash);
    }
    mapFor = list;
  }
  return hashByPath.get(path);
}

function base64ToBytes(b64: string): Uint8Array {
  const bin = atob(b64);
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  return bytes;
}

/** Data URL of the decoded ThumbHash placeholder for `path`, or null when the
 *  item has no hash yet (thumbnail never generated) or decoding fails. Call
 *  from a reactive scope — it reads `items()`. */
export function thumbhashDataUrl(path: string): string | null {
  const hash = lookupHash(path);
  if (!hash) return null;
  let url = dataUrlCache.get(hash);
  if (url) return url;
  try {
    url = thumbHashToDataURL(base64ToBytes(hash));
  } catch {
    return null;
  }
  if (dataUrlCache.size >= DATA_URL_CACHE_CAP) dataUrlCache.clear();
  dataUrlCache.set(hash, url);
  return url;
}
