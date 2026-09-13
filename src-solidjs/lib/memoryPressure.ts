// ---------------------------------------------------------------------------
// Memory pressure — how much the viewer cache is allowed to hold
// ---------------------------------------------------------------------------
//
// This module produces one thing: a pressure level. What each level costs is
// PRESSURE_CONFIGS in viewerCache.ts, which is the only consumer.
//
// The signal is `navigator.deviceMemory`, read once. It is a device class (GB,
// quantized, capped at 8), not a live reading.
//
// There used to be a second signal: the host's free RAM, polled over IPC every
// five seconds, on the grounds that the desktop app and the images it decodes
// were on the same machine. There is one runtime now and it is a browser, so
// that branch is gone — and it was never doing anything for a remote client
// anyway: `get_memory_status` was not on the invoke allowlist, so every cycle
// 403'd into an empty `catch` and the level never left "normal". Allowlisting
// it would have been the wrong repair even then, because it reports the
// *server's* RAM, and sizing a phone's image cache from a NAS's free memory
// means nothing.
//
// Why one sample instead of a poll: `deviceMemory` is static, and the live
// alternative — `performance.memory` — measures the JS heap, which is not where
// decoded images live. Polling it would report a number that cannot move in
// response to the thing being bounded. Safari exposes neither, so those clients
// stay at "normal".

/** `navigator.deviceMemory` values (GB) at or below which a browser trims. */
const DEVICE_MEMORY_EMERGENCY_GB = 1;
const DEVICE_MEMORY_WARNING_GB = 2;

export type PressureLevel = "normal" | "warning" | "emergency";

/** Device class from `navigator.deviceMemory`. Absent (Safari, Firefox) means
 *  no signal, not a small device — assume "normal" rather than throttling a
 *  desktop browser on a guess. */
export function pressureLevel(): PressureLevel {
  if (typeof navigator === "undefined") return "normal";
  const gb = (navigator as { deviceMemory?: number }).deviceMemory;
  if (typeof gb !== "number") return "normal";
  if (gb <= DEVICE_MEMORY_EMERGENCY_GB) return "emergency";
  if (gb <= DEVICE_MEMORY_WARNING_GB) return "warning";
  return "normal";
}
