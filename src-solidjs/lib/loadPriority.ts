// ---------------------------------------------------------------------------
// Drain-time prioritization for the views' thumbnail-generation queues.
//
// Queued paths accumulate while scrolling (each 404'd cell enqueues itself),
// so by the time a batch is drained the scroll may be somewhere else
// entirely. Priorities are therefore computed at drain time from the current
// windows — never stored at enqueue time — so a batch always serves what the
// user is looking at *now*:
//
//   rank 0: inside the full-res window (viewport + small buffer) — in
//           insertion order, they're all equally urgent
//   rank 1: inside the rendered look-ahead window, nearest first
//   rank 2: outside the rendered window — scrolled-past leftovers; they fill
//           spare batch capacity, and whatever still doesn't fit is reported
//           as stale for the caller to drop (a cell re-queues itself via 404
//           if it ever scrolls back)
//
// `pickByRank` is that schedule with the window test left to the caller;
// `pickByPriority` is the case where the windows are *contiguous ranges of
// item indices*, which is what a view laying items out in reading order down a
// scroller has, and both grids do.
//
// The split exists because the second kind of view arrived. The canvas
// (`CanvasView`) shows an off-centre 2D window of a spiral, which in index
// terms is several disjoint runs rather than one range, and wants ranking by
// distance from the viewport centre. Only the zone→rank mapping differed, so
// only that was lifted out — the ordering, the cap and the staleness rule are
// the same schedule in both views, and should stay one implementation.
// ---------------------------------------------------------------------------

/** The rank meaning "outside everything the view is rendering". Unpicked
 *  entries at this rank are the ones reported stale; a custom ranker must use
 *  it for exactly that case, and lower numbers for anything it wants kept. */
export const OUTSIDE_RANK = 2;

/** Where one queued path sits relative to the view's current windows. */
export interface Rank {
  /** 0 = full-resolution window, 1 = rendered look-ahead, 2 = outside both. */
  rank: number;
  /** Tie-break within a rank: how far from the window the caller wants served
   *  first. Any unit, as long as one ranker is consistent with itself. */
  dist: number;
}

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

/**
 * Order `queued` by the caller's notion of urgency and take the first `cap`.
 *
 * The one obligation on `rankOf`: return [`OUTSIDE_RANK`] for a path the view
 * is not rendering, and only for those. Everything it ranks lower is kept in
 * the queue when it doesn't fit the batch; everything at that rank and beyond
 * the cap is dropped, on the reasoning that a cell nobody is looking at will
 * re-queue itself via a 404 if it ever comes back on screen.
 */
export function pickByRank(
  queued: Iterable<string>,
  rankOf: (path: string) => Rank,
  cap: number,
): PriorityPick {
  const entries: { path: string; rank: number; dist: number }[] = [];
  for (const path of queued) {
    const { rank, dist } = rankOf(path);
    entries.push({ path, rank, dist });
  }
  entries.sort((a, b) => a.rank - b.rank || a.dist - b.dist);
  const picked: string[] = [];
  const stale: string[] = [];
  for (let i = 0; i < entries.length; i++) {
    if (i < cap) picked.push(entries[i].path);
    else if (entries[i].rank >= OUTSIDE_RANK) stale.push(entries[i].path);
  }
  return { picked, stale };
}

/** [`pickByRank`] for a view whose windows are contiguous index ranges. */
export function pickByPriority(
  queued: Iterable<string>,
  indexOf: (path: string) => number | undefined,
  zones: PriorityZones,
  cap: number,
): PriorityPick {
  return pickByRank(
    queued,
    (path) => {
      const idx = indexOf(path);
      // No longer in the path list (renamed/removed) — definitionally stale.
      if (idx === undefined) return { rank: OUTSIDE_RANK, dist: Number.MAX_SAFE_INTEGER };
      if (idx >= zones.viewStart && idx < zones.viewEnd) return { rank: 0, dist: 0 };
      if (idx >= zones.renderStart && idx < zones.renderEnd) {
        const dist = idx < zones.viewStart ? zones.viewStart - idx : idx - zones.viewEnd + 1;
        return { rank: 1, dist };
      }
      const dist =
        idx < zones.renderStart ? zones.renderStart - idx : idx - zones.renderEnd + 1;
      return { rank: OUTSIDE_RANK, dist };
    },
    cap,
  );
}
