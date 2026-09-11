// Build identity baked in at compile time (vite.config.ts `define`). Surfaced
// in Settings so a running client can be matched to a build — the "did the
// container actually update?" check. The build time always changes per build;
// the commit is present for dev/desktop builds and for container builds that
// pass VITE_GIT_SHA.

const APP_VERSION: string = __APP_VERSION__;
export const GIT_SHA: string = __GIT_SHA__;
const BUILD_TIME: string = __BUILD_TIME__;

/** Build time as a compact local string (e.g. "2026-07-23 14:20"), or "" if
 *  the stamp is missing/unparseable. */
function buildTimeLabel(): string {
  if (!BUILD_TIME) return "";
  const d = new Date(BUILD_TIME);
  if (Number.isNaN(d.getTime())) return "";
  const pad = (n: number) => String(n).padStart(2, "0");
  return (
    `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ` +
    `${pad(d.getHours())}:${pad(d.getMinutes())}`
  );
}

/** One-line human summary: "v0.1.0 · a1b2c3d · 2026-07-23 14:20". Omits the
 *  commit segment when no SHA was available at build time. */
export function versionLabel(): string {
  const parts = [`v${APP_VERSION}`];
  if (GIT_SHA) parts.push(GIT_SHA);
  const t = buildTimeLabel();
  if (t) parts.push(t);
  return parts.join(" · ");
}
