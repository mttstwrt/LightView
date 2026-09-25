// Lock a selection into an ordered set: name it, and the files become one
// block in the Custom order, in the order they were on screen.

import { onCleanup } from "solid-js";
import { api } from "../../lib/ipc";
import { arrange, showNotice } from "../../stores/galleryStore";
import { sortField } from "../../stores/settingsStore";
import { NameSetField } from "./NameSetField";

export function LockSetDialog(props: { paths: string[]; onClose: () => void }) {
  const handleKey = (e: KeyboardEvent) => {
    if (e.key !== "Escape") return;
    e.stopPropagation();
    props.onClose();
  };
  window.addEventListener("keydown", handleKey, true);
  onCleanup(() => window.removeEventListener("keydown", handleKey, true));

  const lock = (name: string) => {
    const paths = props.paths;
    props.onClose();
    void arrange(async () => {
      await api.lockSet(name.trim(), paths);
      // Under any other sort a block is not contiguous, so nothing on screen
      // moved; say where it shows.
      if (sortField() !== "custom") {
        showNotice(`Locked as “${name.trim()}”. Sort by Custom to see it as one block.`);
      }
    });
  };

  const count = () => props.paths.length;

  return (
    <div
      class="fixed inset-0 z-[260] flex items-center justify-center p-6"
      style={{ background: "rgba(0, 0, 0, 0.75)" }}
      onClick={props.onClose}
    >
      <div
        class="flex flex-col gap-3 w-full max-w-sm rounded-xl px-5 py-4"
        style={{ background: "rgb(20, 20, 22)", border: "1px solid rgba(255,255,255,0.08)" }}
        onClick={(e) => e.stopPropagation()}
      >
        <span class="text-sm font-medium text-neutral-200">
          Lock {count() === 1 ? "1 file" : `${count()} files`} as a set
        </span>
        <p class="text-xs text-neutral-400 leading-relaxed">
          Under the Custom sort they stay together as one block, in the order they are on screen
          now. A name that already exists adds them to the end of that set.
        </p>
        <NameSetField initiallyOpen onName={lock} onCancel={props.onClose} />
      </div>
    </div>
  );
}
