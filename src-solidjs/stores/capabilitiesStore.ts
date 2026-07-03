import { createSignal } from "solid-js";
import { isTauri } from "../lib/runtime";
import { getServerCapabilities, type ServerCapabilities } from "../lib/ipc";

const ALL: ServerCapabilities = {
  metadataWrite: true,
  delete: true,
  localFs: true,
  plugins: true,
};

/** Until the server answers, a web client assumes the paired-device baseline
 * (metadata writes only) so nothing destructive flashes in and then vanishes. */
const WEB_BASELINE: ServerCapabilities = {
  metadataWrite: true,
  delete: false,
  localFs: false,
  plugins: false,
};

const [capabilities, setCapabilities] = createSignal<ServerCapabilities>(
  isTauri() ? ALL : WEB_BASELINE,
);

export { capabilities };

/** Fetch what this client may do. Desktop short-circuits to everything; the
 * web client asks the server (which enforces the same flags in api.rs —
 * this only drives UI visibility). Call once at startup. */
export async function loadCapabilities(): Promise<void> {
  if (isTauri()) return;
  try {
    setCapabilities(await getServerCapabilities());
  } catch (e) {
    // Unpaired clients get a 401 here and the global handler redirects to
    // pairing; keep the safe baseline in the meantime.
    console.warn("Failed to load server capabilities:", e);
  }
}
