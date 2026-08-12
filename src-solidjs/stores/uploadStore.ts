// Whether the open gallery accepts uploads from paired devices, and how it
// files them.
//
// This is per-gallery host configuration, so it has to be fetched. It lives in
// a store rather than inside the upload sheet because two things need it and
// they are not in the same subtree: the command list decides whether to offer
// "Upload photos" at all, and the sheet needs `scheme` to know whether to ask
// for an album name. Fetched once on web boot — the host setting changes about
// as often as the gallery does.

import { createSignal } from "solid-js";
import { getUploadConfig, type UploadConfig } from "../lib/ipc";
import { isWeb } from "../lib/runtime";

const [uploadConfig, setUploadConfig] = createSignal<UploadConfig | null>(null);

export { uploadConfig };

/** True once the host has confirmed uploads are on. Null config (not fetched,
 *  or the fetch failed) reads as off, which is the safe direction: the command
 *  is absent rather than present and failing. */
export const uploadsEnabled = () => uploadConfig()?.enabled === true;

/** Fetch the gallery's upload configuration. Desktop is a no-op — the desktop
 *  app writes into the gallery directly and has no upload flow. */
export async function loadUploadConfig(): Promise<void> {
  if (!isWeb()) return;
  try {
    setUploadConfig(await getUploadConfig());
  } catch (e) {
    // Unpaired clients 401 here and the transport redirects to /pair; anything
    // else leaves uploads hidden, so say why rather than failing silently.
    console.warn("Failed to load upload config:", e);
  }
}
