// ---------------------------------------------------------------------------
// What each cell is currently showing, and which rung it is showing it at.
//
// Three structures that only make sense together: the path → URL store the
// render reads, the path → rung map recording which resolution that URL came
// from, and the off-DOM swapper that upgrades a cell without flashing a
// skeleton. Every view that streams thumbnails needs all three, and needs them
// to agree — which is the reason they live here rather than in each view.
//
// Disagreement is silent, and both directions have already been hit:
//
// - drop the URL but keep the rung, and the cell is *permanently un-evictable*,
//   because the rung map is the index eviction walks. GalleryGrid carries a
//   comment about exactly this, from the jump handler that used to clear the
//   rung map and orphan every cell displayed before the jump.
// - drop the rung but leave the swap running, and an abandoned `Image` keeps
//   fetching and decoding with nothing left to commit it to.
//
// So `evict` and `clear` do the whole tail, and a view cannot do half of it.
//
// The rung is the view's own vocabulary: GalleryGrid's is a `ThumbTier`
// (s/m/l), JustifiedGrid's is `"cheap" | "full"` because a cell there may be
// showing a `?fit=` resize of the original, which is not a tier at all.
// ---------------------------------------------------------------------------

import { batch } from "solid-js";
import { createStore, reconcile } from "solid-js/store";
import { createThumbSwapper } from "./thumbSwap";
import { markUrlLoaded } from "./loadedUrls";

export interface CellSources<R> {
  /** The URL this cell should render, or null. A tracked store read — call it
   *  from the JSX so a cell whose URL is unchanged does not re-render. */
  srcOf(path: string): string | null;
  /** Which rung `path` is on, or undefined if it holds no source. */
  rungOf(path: string): R | undefined;
  has(path: string): boolean;
  /** The paths holding a source — the set eviction walks. */
  held(): Iterable<string>;
  /** Move a cell to another rung without touching its URL. */
  setRung(path: string, rung: R): void;
  /**
   * Point a cell straight at `url`, with no swap. For when the *bytes* behind
   * a cell changed and the URL is being re-versioned to match: a swap there
   * would decode the new bytes twice, once off-DOM and once in the cell.
   */
  point(path: string, url: string): void;
  /**
   * Give a cell `url`, choosing how by what it already shows. An empty cell
   * takes it directly — a skeleton while it loads is correct. A cell already
   * showing an image decodes off-DOM first and swaps on success, so the old
   * image stays up with no flash, and a URL that 404s leaves it up and reports
   * a miss instead of blanking the cell.
   */
  assign(path: string, url: string): void;
  /** Abort an in-flight swap without dropping the cell. */
  cancel(path: string): void;
  /** Drop these cells entirely: URL, rung, and any in-flight swap. */
  evict(paths: Iterable<string>): void;
  /**
   * Drop every cell `keep` rejects, and report which went. For an item list
   * that changed underneath the view — the paths are gone for good, unlike an
   * eviction, so the caller usually forgets their URL versions too.
   *
   * This walks the URL store as well as the rung map, and has to: a cell can
   * hold a URL with no rung, which is exactly what `clear({keepUrls: true})`
   * leaves behind, and pruning only the rung map would strand those entries in
   * the store for the rest of the session.
   */
  prune(keep: (path: string) => boolean): string[];
  /** Abort every in-flight swap, leaving the cells themselves alone. */
  cancelAll(): void;
  /**
   * Drop every cell. `keepUrls` retains the URL store so the displayed images
   * stay on screen while the view re-streams behind them — what a zoom change
   * wants, since blanking to skeletons is the thing it is avoiding. A hard
   * invalidation (the bytes changed) must not keep them.
   */
  clear(opts?: { keepUrls?: boolean }): void;
}

export function createCellSources<R>(opts: {
  /** The view's generation counter; a swap from a stale one is dropped. */
  generation: () => number;
  /** A swapped URL 404'd — queue generation for it. */
  onMiss: (path: string) => void;
  /** Extra per-path teardown on eviction. JustifiedGrid releases the cell's
   *  hold on the look-ahead here. Deliberately not called by `clear`, which
   *  the views pair with their own wholesale reset. */
  onEvict?: (path: string) => void;
}): CellSources<R> {
  const [urls, setUrls] = createStore<Record<string, string>>({});
  const rungs = new Map<string, R>();

  const swapper = createThumbSwapper({
    generation: opts.generation,
    // A cell that has been evicted mid-decode must not be resurrected by its
    // own swap landing — the rung map is what says it is still wanted.
    isCurrent: (path, gen) => opts.generation() === gen && rungs.has(path),
    apply: (path, url) => {
      // Already decoded off-DOM, so suppress the cell's fade-in.
      markUrlLoaded(url);
      setUrls(path, url);
    },
    onMiss: opts.onMiss,
  });

  const point = (path: string, url: string) => setUrls(path, url);

  return {
    srcOf: (path) => urls[path] ?? null,
    rungOf: (path) => rungs.get(path),
    has: (path) => rungs.has(path),
    held: () => rungs.keys(),
    setRung(path, rung) {
      rungs.set(path, rung);
    },
    point,
    assign(path, url) {
      const current = urls[path];
      if (!current || current === url) {
        point(path, url);
        return;
      }
      swapper.swap(path, url);
    },
    cancel: (path) => swapper.cancel(path),
    cancelAll: () => swapper.cancelAll(),
    evict(paths) {
      const list = [...paths];
      if (list.length === 0) return;
      batch(() => {
        for (const p of list) {
          setUrls(p, undefined as unknown as string);
          rungs.delete(p);
          swapper.cancel(p);
          opts.onEvict?.(p);
        }
      });
    },
    prune(keep) {
      const gone = new Set<string>();
      for (const p of Object.keys(urls)) if (!keep(p)) gone.add(p);
      for (const p of rungs.keys()) if (!keep(p)) gone.add(p);
      if (gone.size === 0) return [];
      const list = [...gone];
      batch(() => {
        for (const p of list) {
          setUrls(p, undefined as unknown as string);
          rungs.delete(p);
          swapper.cancel(p);
          opts.onEvict?.(p);
        }
      });
      return list;
    },
    clear(o) {
      if (!o?.keepUrls) setUrls(reconcile({}));
      rungs.clear();
      swapper.cancelAll();
    },
  };
}
