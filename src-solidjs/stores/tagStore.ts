import { createSignal } from "solid-js";

// Tag editor state
const [tagEditorOpen, setTagEditorOpen] = createSignal(false);
const [tagEditorPath, setTagEditorPath] = createSignal<string | null>(null);

export { tagEditorOpen, setTagEditorOpen, tagEditorPath, setTagEditorPath };

export function openTagEditor(path: string) {
  setTagEditorPath(path);
  setTagEditorOpen(true);
}

export function closeTagEditor() {
  setTagEditorOpen(false);
  setTagEditorPath(null);
}
