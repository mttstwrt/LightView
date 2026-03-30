import { createSignal } from "solid-js";

export type ThumbGenStatus = "generating" | "done" | "error";

export interface ThumbGenActivity {
  status: ThumbGenStatus;
  generated: number;
  total: number;
  message: string;
}

const [thumbGenActivity, setThumbGenActivity] = createSignal<ThumbGenActivity | null>(null);

export { thumbGenActivity };

let clearTimer: ReturnType<typeof setTimeout> | undefined;

export function thumbGenStarted(total: number) {
  if (clearTimer) clearTimeout(clearTimer);
  setThumbGenActivity({ status: "generating", generated: 0, total, message: `Generating 0 / ${total}` });
}

export function thumbGenProgress(generated: number, total: number) {
  setThumbGenActivity({ status: "generating", generated, total, message: `Generating ${generated} / ${total}` });
}

export function thumbGenFinished(total: number) {
  setThumbGenActivity({ status: "done", generated: total, total, message: `${total} thumbnails generated` });
  clearTimer = setTimeout(() => setThumbGenActivity(null), 4000);
}

export function thumbGenFailed(message: string) {
  setThumbGenActivity((prev) => prev ? { ...prev, status: "error", message } : null);
  clearTimer = setTimeout(() => setThumbGenActivity(null), 5000);
}
