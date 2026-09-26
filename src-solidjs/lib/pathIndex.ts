// ---------------------------------------------------------------------------
// Reverse index from path to position in the grid's item list, and the pruning
// of per-path state when that list changes underneath it (a delete, a filter,
// a re-sort).
//
// The grid needs O(1) "where is this path now?" for eviction and drain-time
// prioritization. Pruning is the half that is easy to get wrong invisibly: a
// stale entry in a `Set` keyed by path silently suppresses future work for
// whatever path reuses it.
//
// The reconciliation itself stays in the grid's effect, because the *order*
// matters — reindex first, then compute what went, then prune — and the grid
// prunes side state this module does not know about (its high-tier precache
// memo and its measured aspects).
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
  /** Drop every absent key from each collection, in place. */
  pruneAbsent(...collections: PathKeyed[]): void;
}

/** An index from path to position in the current item list, and the pruning of
 *  path-keyed sets when the list changes. */
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
