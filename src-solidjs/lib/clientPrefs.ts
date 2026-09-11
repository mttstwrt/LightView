// Per-client (per-browser/device) preferences persisted in localStorage.
//
// The web client has no local gallery `.lightview/settings.toml` to read or
// write — every browser talks to the same host — so display preferences (e.g.
// "play GIFs in the grid") would otherwise reset on every load and be shared
// across all devices. Storing them here keeps each client's settings local to
// that client. Also used to remember the last-selected layout across sessions.

const PREFIX = "lightview.";

export function loadPref<T>(key: string): T | null {
  try {
    const raw = localStorage.getItem(PREFIX + key);
    return raw === null ? null : (JSON.parse(raw) as T);
  } catch {
    return null;
  }
}

export function savePref(key: string, value: unknown): void {
  try {
    localStorage.setItem(PREFIX + key, JSON.stringify(value));
  } catch {}
}
