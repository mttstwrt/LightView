// ---------------------------------------------------------------------------
// Memory Pressure Monitor — Phase 3 pressure-aware cache management
// ---------------------------------------------------------------------------
//
// Periodically polls system RAM via IPC and computes a target cache capacity.
// Formula (from OptimizationPlan §3.1):
//   - Base:       10% of Total_RAM (in image count)
//   - Constraint: never exceed 40% of Available_RAM
//   - Emergency:  if Available_RAM < 1.5 GB, purge to visible-only
//
// The monitor converts MB budgets into image counts using a per-image estimate.

import { getMemoryStatus } from "./ipc";

/** Estimated memory footprint of one decoded thumbnail (ImageBitmap). */
const BYTES_PER_IMAGE = 40 * 1024; // ~40 KB average JPEG thumbnail decoded

const POLL_INTERVAL_MS = 5_000;
const EMERGENCY_THRESHOLD_MB = 1536; // 1.5 GB
const MIN_CACHE_SIZE = 50;
const MAX_CACHE_SIZE = 500;

export type PressureLevel = "normal" | "warning" | "emergency";

export interface PressureState {
  level: PressureLevel;
  targetCacheSize: number;
  availableMb: number;
  totalMb: number;
}

export type PressureCallback = (state: PressureState) => void;

export class MemoryPressureMonitor {
  private intervalId: ReturnType<typeof setInterval> | null = null;
  private callback: PressureCallback;
  private lastLevel: PressureLevel = "normal";
  private visibleCount = 0;

  constructor(callback: PressureCallback) {
    this.callback = callback;
  }

  /** Update the number of currently visible items (used for emergency purge target). */
  setVisibleCount(count: number) {
    this.visibleCount = count;
  }

  start() {
    if (this.intervalId !== null) return;
    // Poll immediately, then on interval
    this.poll();
    this.intervalId = setInterval(() => this.poll(), POLL_INTERVAL_MS);
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
      const state = this.computeState(status.total_ram_mb, status.available_ram_mb);
      this.lastLevel = state.level;
      this.callback(state);
    } catch {
      // IPC failure — don't crash, just skip this cycle
    }
  }

  private computeState(totalMb: number, availableMb: number): PressureState {
    // Emergency: available RAM dangerously low
    if (availableMb < EMERGENCY_THRESHOLD_MB) {
      // Purge to last 2 visible rows worth of images (or visibleCount as proxy)
      const emergencySize = Math.max(MIN_CACHE_SIZE, this.visibleCount);
      return {
        level: "emergency",
        targetCacheSize: emergencySize,
        availableMb,
        totalMb,
      };
    }

    // Base budget: 10% of total RAM
    const baseBudgetBytes = totalMb * 1024 * 1024 * 0.10;
    const baseCount = Math.floor(baseBudgetBytes / BYTES_PER_IMAGE);

    // Constraint: never exceed 40% of available RAM
    const availBudgetBytes = availableMb * 1024 * 1024 * 0.40;
    const availCount = Math.floor(availBudgetBytes / BYTES_PER_IMAGE);

    const target = Math.min(baseCount, availCount);
    const clamped = Math.max(MIN_CACHE_SIZE, Math.min(MAX_CACHE_SIZE, target));

    // Warning level if we're constrained significantly by available RAM
    const level: PressureLevel =
      availableMb < EMERGENCY_THRESHOLD_MB * 2 ? "warning" : "normal";

    return {
      level,
      targetCacheSize: clamped,
      availableMb,
      totalMb,
    };
  }
}
