// ---------------------------------------------------------------------------
// The generation queue every grid view keeps for its cells.
//
// A cell points its <img> optimistically at a tier URL; a cached thumbnail
// loads instantly, an uncached one 404s and the cell reports a miss. This is
// the bookkeeping between that miss and the bytes arriving: what is queued,
// what is in flight, what has permanently failed, and what a speculative warm
// has already covered. All four sets have to agree, and a path leaking into
// one of them is silent — the cell simply never loads again.
//
// The payload is what a view needs to remember alongside the path. GalleryGrid
// has nothing to remember (every cell wants the same tier) and instantiates it
// at `void`; JustifiedGrid stores the tier that actually 404'd, because a
// cheap-rung cell there is showing the base "j" tier while the detail level
// says the target is "jh" — regenerating the target neither fixes the miss nor
// is cheap. Same structure, different payload, rather than two queues.
// ---------------------------------------------------------------------------

export interface ThumbQueue<T> {
  /**
   * Queue `path` for generation. Returns true only on a *fresh* miss — not
   * already queued, in flight, or failed — which is what the caller counts as
   * a cache miss and reports to the progress meter.
   *
   * Re-queuing an already-queued path re-aims its payload instead: a cell can
   * be upgraded between its 404 and the drain, and the drain has to generate
   * what the cell is showing now, not what it was showing then.
   */
  queue(path: string, payload: T): boolean;
  /** What `path` is queued to generate, or undefined if it is not queued. */
  payloadFor(path: string): T | undefined;
  /** The queued paths, in insertion order. */
  queued(): Iterable<string>;
  queuedCount(): number;
  /** Drop paths from the queue without generating them (stale, or departed). */
  drop(paths: Iterable<string>): void;
  /** Move paths out of the queue and into the in-flight set. */
  take(paths: Iterable<string>): void;
  /**
   * Retire in-flight paths. `failed` marks the ones that must not be retried —
   * a batch that came back without them, or a batch that threw. Paths absent
   * from the result simply never load otherwise, and re-queue on every 404.
   */
  settle(paths: Iterable<string>, failed?: (path: string) => boolean): void;
  inFlight(path: string): boolean;
  hasFailed(path: string): boolean;
  /** Worth spending a speculative warm on: not in flight, not failed, and not
   *  already covered by an earlier warm of the same fling. */
  warmable(path: string): boolean;
  /** Remember that a speculative warm has covered this path. */
  markWarmed(path: string): void;
  /** Nothing queued and nothing in flight — what "idle" means to the progress
   *  meter. Failed paths do not count: they are finished, not pending. */
  idle(): boolean;
  /** Forget everything. Used where the view's whole picture is invalidated: a
   *  jump to a new position, a rebuild, a tier change. */
  reset(): void;
  /** Drop every path `keep` rejects. Returns how many left the *queue*, which
   *  is the number the progress meter has to be told about; the other sets are
   *  not counted anywhere. */
  prune(keep: (path: string) => boolean): number;
}

export function createThumbQueue<T = void>(): ThumbQueue<T> {
  const pending = new Map<string, T>();
  const flying = new Set<string>();
  const failed = new Set<string>();
  const warmed = new Set<string>();

  return {
    queue(path, payload) {
      if (pending.has(path)) {
        if (pending.get(path) !== payload) pending.set(path, payload);
        return false;
      }
      if (flying.has(path) || failed.has(path)) return false;
      pending.set(path, payload);
      return true;
    },
    payloadFor: (path) => pending.get(path),
    queued: () => pending.keys(),
    queuedCount: () => pending.size,
    drop(paths) {
      for (const p of paths) pending.delete(p);
    },
    take(paths) {
      for (const p of paths) {
        pending.delete(p);
        flying.add(p);
      }
    },
    settle(paths, isFailed) {
      for (const p of paths) {
        flying.delete(p);
        if (isFailed?.(p)) failed.add(p);
      }
    },
    inFlight: (path) => flying.has(path),
    hasFailed: (path) => failed.has(path),
    warmable: (path) => !flying.has(path) && !failed.has(path) && !warmed.has(path),
    markWarmed(path) {
      warmed.add(path);
    },
    idle: () => pending.size === 0 && flying.size === 0,
    reset() {
      pending.clear();
      flying.clear();
      failed.clear();
      warmed.clear();
    },
    prune(keep) {
      let dropped = 0;
      for (const p of pending.keys()) {
        if (!keep(p)) {
          pending.delete(p);
          dropped++;
        }
      }
      for (const set of [flying, failed, warmed]) {
        for (const p of set) if (!keep(p)) set.delete(p);
      }
      return dropped;
    },
  };
}
