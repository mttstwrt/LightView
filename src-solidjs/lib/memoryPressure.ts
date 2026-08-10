// ---------------------------------------------------------------------------
// Memory pressure — how much the viewer cache is allowed to hold
// ---------------------------------------------------------------------------
//
// This module produces one thing: a pressure level. What each level costs is
// PRESSURE_CONFIGS in viewerCache.ts, which is the only consumer.
//
// The two runtimes get different signals, because only one of them has a
// meaningful one:
//
//   Desktop — the host's free RAM, polled over IPC. The app and the images are
//   on the same machine, so this is the real constraint.
//
//   Web — `navigator.deviceMemory`, read once. This is a device class (GB,
//   quantized, capped at 8), not a live reading.
//
// The web client used to poll `get_memory_status` as well. That command is not
// on the `/api/invoke` allowlist and never has been, so in a browser every
// cycle 403'd into an empty `catch` and the level never left "normal" —
// pressure-based eviction simply did not exist there. Allowlisting it would
// have been the wrong repair: it reports the *server's* RAM, and sizing a
// phone's image cache from a NAS's free memory means nothing.
//
// Why the web signal is sampled once instead of polled: `deviceMemory` is
// static, and the live alternative — `performance.memory` — measures the JS
// heap, which is not where decoded images live. Polling it would report a
// number that cannot move in response to the thing being bounded. Safari
// exposes neither, so those clients stay at "normal", which is exactly what
// they were already getting from the broken poll.

import { getMemoryStatus } from "./ipc";
import { isWeb } from "./runtime";

const POLL_INTERVAL_MS = 5_000;

/** Host RAM (MB) below which the desktop cache drops to the current image. */
const EMERGENCY_THRESHOLD_MB = 1536;
/** Twice the emergency floor: still comfortable, but worth shrinking for. */
const WARNING_THRESHOLD_MB = EMERGENCY_THRESHOLD_MB * 2;

/** `navigator.deviceMemory` values (GB) at or below which a browser trims. */
const DEVICE_MEMORY_EMERGENCY_GB = 1;
const DEVICE_MEMORY_WARNING_GB = 2;

export type PressureLevel = "normal" | "warning" | "emergency";

export type PressureCallback = (level: PressureLevel) => void;

export class MemoryPressureMonitor {
  private intervalId: ReturnType<typeof setInterval> | null = null;
  private callback: PressureCallback;
  /** Set after the first failed poll so a persistently failing IPC logs once
   *  rather than every five seconds — the old empty `catch` hid the failure
   *  entirely, which is how the web client's dead poll went unnoticed. */
  private reportedFailure = false;

  constructor(callback: PressureCallback) {
    this.callback = callback;
  }

  start() {
    if (isWeb()) {
      this.callback(deviceMemoryLevel());
      return; // static signal — nothing to poll
    }
    if (this.intervalId !== null) return;
    // Poll immediately, then on interval
    void this.poll();
    this.intervalId = setInterval(() => void this.poll(), POLL_INTERVAL_MS);
  }

  stop() {
    if (this.intervalId !== null) {
      clearInterval(this.intervalId);
      this.intervalId = null;
    }
  }

  private async poll() {
    try {
      const status = await getMemoryStatus();
      this.reportedFailure = false;
      this.callback(hostMemoryLevel(status.available_ram_mb));
    } catch (e) {
      if (!this.reportedFailure) {
        this.reportedFailure = true;
        console.warn(
          "Memory status unavailable — viewer cache stays at its current size:",
          e,
        );
      }
    }
  }
}

function hostMemoryLevel(availableMb: number): PressureLevel {
  if (availableMb < EMERGENCY_THRESHOLD_MB) return "emergency";
  if (availableMb < WARNING_THRESHOLD_MB) return "warning";
  return "normal";
}

/** Device class from `navigator.deviceMemory`. Absent (Safari, Firefox) means
 *  no signal, not a small device — assume "normal" rather than throttling a
 *  desktop browser on a guess. */
function deviceMemoryLevel(): PressureLevel {
  const gb = (navigator as { deviceMemory?: number }).deviceMemory;
  if (typeof gb !== "number") return "normal";
  if (gb <= DEVICE_MEMORY_EMERGENCY_GB) return "emergency";
  if (gb <= DEVICE_MEMORY_WARNING_GB) return "warning";
  return "normal";
}
