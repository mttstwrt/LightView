import { createSignal, onCleanup } from "solid-js";

/** Destructive-action button requiring a second click; disarms after 3s.
 *  Shared by the panels that delete things for real (trash, tag manager). */
export function ConfirmButton(props: {
  label: string;
  confirmLabel: string;
  onConfirm: () => void;
  disabled?: boolean;
  class?: string;
}) {
  const [armed, setArmed] = createSignal(false);
  let timer: ReturnType<typeof setTimeout> | undefined;
  onCleanup(() => clearTimeout(timer));

  return (
    <button
      disabled={props.disabled}
      onClick={() => {
        if (armed()) {
          setArmed(false);
          props.onConfirm();
        } else {
          setArmed(true);
          clearTimeout(timer);
          timer = setTimeout(() => setArmed(false), 3000);
        }
      }}
      class={`px-2.5 py-1 text-[10px] rounded cursor-pointer transition-colors whitespace-nowrap disabled:opacity-40 disabled:cursor-default ${
        armed()
          ? "bg-red-700/70 text-red-100"
          : "bg-red-900/30 text-red-400 hover:bg-red-800/40 hover:text-red-300"
      } ${props.class ?? ""}`}
    >
      {armed() ? props.confirmLabel : props.label}
    </button>
  );
}
