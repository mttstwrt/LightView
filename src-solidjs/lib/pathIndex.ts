// ---------------------------------------------------------------------------
// Reverse index from path to position in the view's item list.
//
// Every grid needs O(1) "where is this path now?" for eviction and for
// drain-time prioritization, and every grid has to reconcile its per-path
// state when the list changes underneath it (a delete, a filter, a re-sort).
// Both halves were duplicated verbatim; the pruning half is the one worth
// sharing, because getting it wrong is invisible — a stale entry in a `Set`
// keyed by path silently suppresses future work for whatever path reuses it.
//
// The reconciliation stays in each view's own effect. Views hold different
// side sets (JustifiedGrid also prunes its high-tier precache memo and its
// measured aspects), and the *order* matters: reindex first, then compute what
// went, then prune. Passing a set list in is enough sharing; folding the effect
// in would mean parameterizing every view's extra state.
// ---------------------------------------------------------------------------

/** Anything keyed by path that this module can prune. Both `Set<string>` and
 *  `Map<string, T>` satisfy it, so a queue keyed by tier prunes the same way a
 *  plain membership set does. */
export interface PathKeyed {
  keys(): Iterable<string>;
  delete(path: string): unknown;
}

export interface PathIndex {
  /** Position of `path` in the current list, or undefined if it is gone. */
  indexOf(path: string): number | undefined;
  has(path: string): boolean;
  /** Rebuild against a new list. Call before any pruning. */
  reindex(paths: string[]): void;
  /** Paths drawn from `sources` that are no longer in the list, deduplicated —
   *  the set a view must drop URLs, decodes and versions for. */
  absent(...sources: Iterable<string>[]): Set<string>;
  /** Drop every absent key from each collection, in place. */
  pruneAbsent(...collections: PathKeyed[]): void;
}

export function createPathIndex(): PathIndex {
  const index = new Map<string, number>();

  const has = (path: string) => index.has(path);

  return {
    indexOf: (path) => index.get(path),
    has,
    reindex(paths) {
      index.clear();
      for (let i = 0; i < paths.length; i++) index.set(paths[i], i);
    },
    absent(...sources) {
      const gone = new Set<string>();
      for (const source of sources) {
        for (const path of source) if (!has(path)) gone.add(path);
      }
      return gone;
    },
    pruneAbsent(...collections) {
      for (const c of collections) {
        // Deleting from a Set/Map while iterating its own keys is well-defined
        // — a removed entry that has already been visited is simply not
        // revisited — so this needs no intermediate array.
        for (const path of c.keys()) if (!has(path)) c.delete(path);
      }
    },
  };
}
