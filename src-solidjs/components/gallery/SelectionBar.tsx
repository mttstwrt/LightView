import { Show, For, createSignal } from "solid-js";
import { addUserTagBatch, removeUserTagBatch, setRatingBatch } from "../../lib/ipc";

interface SelectionBarProps {
  selectedPaths: Set<string>;
  onClear: () => void;
}

export function SelectionBar(props: SelectionBarProps) {
  const [tagInput, setTagInput] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [showRating, setShowRating] = createSignal(false);

  const count = () => props.selectedPaths.size;
  const paths = () => Array.from(props.selectedPaths);

  const handleAddTag = async (e: Event) => {
    e.preventDefault();
    const tag = tagInput().trim();
    if (!tag || busy()) return;
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
    if (busy()) return;
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

  return (
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
          disabled={busy()}
          class="px-2 py-1 bg-neutral-700 hover:bg-neutral-600 text-neutral-300 rounded text-xs cursor-pointer transition-colors disabled:opacity-50"
        >
          Tag
        </button>
      </form>

      <div class="w-px h-5 bg-neutral-700" />

      {/* Rating */}
      <div class="relative">
        <button
          class="px-2 py-1 bg-neutral-700 hover:bg-neutral-600 text-neutral-300 rounded text-xs cursor-pointer transition-colors"
          onClick={() => setShowRating((v) => !v)}
        >
          Rate
        </button>
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
                  class="cursor-pointer text-base transition-colors hover:text-amber-400 text-neutral-600"
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
      </div>

      <div class="w-px h-5 bg-neutral-700" />

      {/* Clear selection */}
      <button
        class="px-2 py-1 text-neutral-400 hover:text-neutral-200 cursor-pointer transition-colors"
        onClick={props.onClear}
      >
        Clear
      </button>
    </div>
  );
}
