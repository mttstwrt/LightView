// ---------------------------------------------------------------------------
// Drain-time prioritization for the grids' thumbnail-generation queues.
//
// Queued paths accumulate while scrolling (each 404'd cell enqueues itself),
// so by the time a batch is drained the scroll may be somewhere else
// entirely. Priorities are therefore computed at drain time from the current
// windows — never stored at enqueue time — so a batch always serves what the
// user is looking at *now*:
//
//   rank 0: inside the full-res window (viewport + small buffer) — in
//           insertion order, they're all equally urgent
//   rank 1: inside the rendered look-ahead window, nearest rows first
//   rank 2: outside the rendered window — scrolled-past leftovers; they fill
//           spare batch capacity, and whatever still doesn't fit is reported
//           as stale for the caller to drop (a cell re-queues itself via 404
//           if it ever scrolls back)
// ---------------------------------------------------------------------------

export interface PriorityZones {
  /** Item-index range of the full-res window (viewport + small buffer). */
  viewStart: number;
  viewEnd: number;
  /** Item-index range of the rendered window (incl. cheap look-ahead). */
  renderStart: number;
  renderEnd: number;
}

export interface PriorityPick {
  /** Up to `cap` paths, highest priority first. */
  picked: string[];
  /** Unpicked paths outside the rendered window — drop these from the queue. */
  stale: string[];
}

export function pickByPriority(
  queued: Iterable<string>,
  indexOf: (path: string) => number | undefined,
  zones: PriorityZones,
  cap: number,
): PriorityPick {
  const entries: { path: string; rank: number; dist: number }[] = [];
  for (const path of queued) {
    const idx = indexOf(path);
    if (idx === undefined) {
      // No longer in the path list (renamed/removed) — definitionally stale.
      entries.push({ path, rank: 2, dist: Number.MAX_SAFE_INTEGER });
    } else if (idx >= zones.viewStart && idx < zones.viewEnd) {
      entries.push({ path, rank: 0, dist: 0 });
    } else if (idx >= zones.renderStart && idx < zones.renderEnd) {
      const dist = idx < zones.viewStart ? zones.viewStart - idx : idx - zones.viewEnd + 1;
      entries.push({ path, rank: 1, dist });
    } else {
      const dist = idx < zones.renderStart ? zones.renderStart - idx : idx - zones.renderEnd + 1;
      entries.push({ path, rank: 2, dist });
    }
  }
  entries.sort((a, b) => a.rank - b.rank || a.dist - b.dist);
  const picked: string[] = [];
  const stale: string[] = [];
  for (let i = 0; i < entries.length; i++) {
    if (i < cap) picked.push(entries[i].path);
    else if (entries[i].rank === 2) stale.push(entries[i].path);
  }
  return { picked, stale };
}
