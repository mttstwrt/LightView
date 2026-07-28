import { Show, For, createSignal } from "solid-js";
import { addUserTagBatch, removeUserTagBatch, setRatingBatch } from "../../lib/ipc";
import { isMobile } from "../../lib/runtime";

interface SelectionBarProps {
  selectedPaths: Set<string>;
  /** True while explicit tap-to-toggle selection mode is on (mobile Select
   *  button). The bar then also shows at zero selected, and "Clear" reads
   *  "Done" because it's what leaves the mode. */
  selectionMode?: boolean;
  onSelectAll?: () => void;
  onClear: () => void;
}

export function SelectionBar(props: SelectionBarProps) {
  const [tagInput, setTagInput] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [showRating, setShowRating] = createSignal(false);

  const count = () => props.selectedPaths.size;
  const paths = () => Array.from(props.selectedPaths);
  const empty = () => count() === 0;

  const handleAddTag = async (e: Event) => {
    e.preventDefault();
    const tag = tagInput().trim();
    if (!tag || busy() || empty()) return;
    setBusy(true);
    try {
      await addUserTagBatch(paths(), tag);
      setTagInput("");
    } catch (err) {
      console.error("Batch tag failed:", err);
    } finally {
      setBusy(false);
    }
  };

  const handleSetRating = async (value: number) => {
    if (busy() || empty()) return;
    setBusy(true);
    try {
      await setRatingBatch(paths(), value);
      setShowRating(false);
    } catch (err) {
      console.error("Batch rating failed:", err);
    } finally {
      setBusy(false);
    }
  };

  // Star popover — identical either way, but it drops *up* from a bottom-edge
  // mobile bar and from the floating desktop pill alike.
  const ratingPopover = () => (
    <Show when={showRating()}>
      <div
        class="absolute bottom-full mb-2 left-1/2 -translate-x-1/2 flex items-center gap-0.5 px-2 py-1.5 rounded"
        style={{
          background: "rgba(20, 20, 20, 0.95)",
          border: "1px solid rgba(255,255,255,0.1)",
        }}
      >
        <For each={[1, 2, 3, 4, 5]}>
          {(star) => (
            <button
              class="cursor-pointer transition-colors hover:text-amber-400 text-neutral-600"
              classList={{ "text-2xl px-1": isMobile(), "text-base": !isMobile() }}
              onClick={() => handleSetRating(star)}
            >
              &#9733;
            </button>
          )}
        </For>
        <button
          class="ml-1 text-neutral-500 hover:text-neutral-300 cursor-pointer text-xs"
          onClick={() => handleSetRating(0)}
        >
          Clear
        </button>
      </div>
    </Show>
  );

  // ── Mobile: a full-width bar pinned to the bottom edge. The desktop pill's
  // single row of controls doesn't come close to fitting a phone, so the
  // count/select-all/done row stacks above the tag + rate row, and every
  // control gets a thumb-sized target.
  const mobileBar = () => (
    <div
      class="fixed left-0 right-0 bottom-0 z-[90] flex flex-col gap-2 px-3 pt-2.5"
      style={{
        background: "rgba(10, 10, 10, 0.95)",
        "backdrop-filter": "blur(12px)",
        "border-top": "1px solid rgba(59, 130, 246, 0.35)",
        "padding-bottom": "calc(env(safe-area-inset-bottom, 0px) + 0.625rem)",
      }}
      onClick={(e) => e.stopPropagation()}
    >
      <div class="flex items-center gap-2">
        <span class="text-blue-400 font-medium text-sm tabular-nums">
          {count()} selected
        </span>
        <div class="flex-1" />
        <Show when={props.onSelectAll}>
          <button
            class="px-3 h-9 text-xs text-neutral-300 bg-neutral-800 rounded cursor-pointer"
            onClick={() => props.onSelectAll!()}
          >
            Select all
          </button>
        </Show>
        <button
          class="px-3 h-9 text-xs font-medium text-white bg-blue-600 rounded cursor-pointer"
          onClick={props.onClear}
        >
          {props.selectionMode ? "Done" : "Clear"}
        </button>
      </div>

      <div class="flex items-center gap-2">
        <form onSubmit={handleAddTag} class="flex items-center gap-2 flex-1 min-w-0">
          <input
            type="text"
            value={tagInput()}
            onInput={(e) => setTagInput(e.currentTarget.value)}
            placeholder="Add tag..."
            // 16px keeps iOS Safari from zooming the page on focus.
            class="flex-1 min-w-0 px-2.5 h-9 bg-neutral-800 border border-neutral-700 rounded text-neutral-200 placeholder-neutral-600 outline-none focus:border-neutral-500"
            style={{ "font-size": "16px" }}
          />
          <button
            type="submit"
            disabled={busy() || empty()}
            class="px-3 h-9 shrink-0 bg-neutral-700 text-neutral-200 rounded text-xs cursor-pointer disabled:opacity-40"
          >
            Tag
          </button>
        </form>
        <div class="relative shrink-0">
          <button
            disabled={empty()}
            class="px-3 h-9 bg-neutral-700 text-neutral-200 rounded text-xs cursor-pointer disabled:opacity-40"
            onClick={() => setShowRating((v) => !v)}
          >
            Rate
          </button>
          {ratingPopover()}
        </div>
      </div>
    </div>
  );

  // ── Desktop: floating pill, single row.
  const desktopBar = () => (
    <div
      class="fixed bottom-4 left-1/2 -translate-x-1/2 z-[90] flex items-center gap-3 px-4 py-2.5 rounded-lg text-xs"
      style={{
        background: "rgba(10, 10, 10, 0.9)",
        "backdrop-filter": "blur(12px)",
        border: "1px solid rgba(59, 130, 246, 0.3)",
      }}
      onClick={(e) => e.stopPropagation()}
    >
      <span class="text-blue-400 font-medium">{count()} selected</span>

      <div class="w-px h-5 bg-neutral-700" />

      {/* Tag input */}
      <form onSubmit={handleAddTag} class="flex items-center gap-1">
        <input
          type="text"
          value={tagInput()}
          onInput={(e) => setTagInput(e.currentTarget.value)}
          placeholder="Add tag..."
          class="w-28 px-2 py-1 bg-neutral-800 border border-neutral-700 rounded text-xs text-neutral-200 placeholder-neutral-600 outline-none focus:border-neutral-500"
        />
        <button
          type="submit"
          disabled={busy() || empty()}
          class="px-2 py-1 bg-neutral-700 hover:bg-neutral-600 text-neutral-300 rounded text-xs cursor-pointer transition-colors disabled:opacity-50"
        >
          Tag
        </button>
      </form>

      <div class="w-px h-5 bg-neutral-700" />

      {/* Rating */}
      <div class="relative">
        <button
          disabled={empty()}
          class="px-2 py-1 bg-neutral-700 hover:bg-neutral-600 text-neutral-300 rounded text-xs cursor-pointer transition-colors disabled:opacity-50"
          onClick={() => setShowRating((v) => !v)}
        >
          Rate
        </button>
        {ratingPopover()}
      </div>

      <div class="w-px h-5 bg-neutral-700" />

      {/* Clear selection */}
      <button
        class="px-2 py-1 text-neutral-400 hover:text-neutral-200 cursor-pointer transition-colors"
        onClick={props.onClear}
      >
        {props.selectionMode ? "Done" : "Clear"}
      </button>
    </div>
  );

  return <Show when={isMobile()} fallback={desktopBar()}>{mobileBar()}</Show>;
}
