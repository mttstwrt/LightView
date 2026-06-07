import { createSignal } from "solid-js";
import { getCurrentWindow } from "@tauri-apps/api/window";

interface TitleBarProps {
  visible: boolean;
  onMouseEnter?: () => void;
  onMouseLeave?: () => void;
}

// Custom window titlebar shown when the native frame is hidden
// (decorations: false). The bar itself is a Tauri drag region; the control
// buttons opt out of dragging so their clicks land normally. Reveal/hide is
// driven by the parent (TopBar) so this row and the filter row appear together.
export function TitleBar(props: TitleBarProps) {
  const [maximized, setMaximized] = createSignal(false);

  const win = getCurrentWindow();

  const minimize = () => win.minimize();
  const toggleMaximize = async () => {
    await win.toggleMaximize();
    setMaximized(await win.isMaximized());
  };
  const close = () => win.close();

  return (
    <div
      data-tauri-drag-region
      class="fixed top-0 left-0 right-0 z-50 h-8 flex items-center justify-between pl-3 transition-[top,opacity] duration-200"
      style={{
        background: "rgba(10, 10, 10, 0.92)",
        "backdrop-filter": "blur(12px)",
        top: props.visible ? "0" : "-2rem",
        opacity: props.visible ? "1" : "0",
        "pointer-events": props.visible ? "auto" : "none",
      }}
      onMouseEnter={props.onMouseEnter}
      onMouseLeave={props.onMouseLeave}
    >
      {/* pointer-events:none lets clicks/drags fall through to the bar's drag
          region underneath. */}
      <span class="text-xs font-medium text-neutral-400 select-none pointer-events-none">
        LightView
      </span>

      <div class="flex items-center h-full">
        <button
          onClick={minimize}
          title="Minimize"
          class="h-full w-11 flex items-center justify-center text-neutral-400 hover:bg-neutral-700/60 hover:text-neutral-100 transition-colors cursor-pointer"
        >
          <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
            <rect x="0" y="4.5" width="10" height="1" fill="currentColor" />
          </svg>
        </button>
        <button
          onClick={toggleMaximize}
          title={maximized() ? "Restore" : "Maximize"}
          class="h-full w-11 flex items-center justify-center text-neutral-400 hover:bg-neutral-700/60 hover:text-neutral-100 transition-colors cursor-pointer"
        >
          <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
            {maximized() ? (
              <>
                <rect x="0.5" y="2.5" width="6" height="6" fill="none" stroke="currentColor" stroke-width="1" />
                <path d="M3 2.5 V0.5 H9.5 V7 H7.5" fill="none" stroke="currentColor" stroke-width="1" />
              </>
            ) : (
              <rect x="0.5" y="0.5" width="9" height="9" fill="none" stroke="currentColor" stroke-width="1" />
            )}
          </svg>
        </button>
        <button
          onClick={close}
          title="Close"
          class="h-full w-11 flex items-center justify-center text-neutral-400 hover:bg-red-600 hover:text-white transition-colors cursor-pointer"
        >
          <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
            <path d="M0.5 0.5 L9.5 9.5 M9.5 0.5 L0.5 9.5" stroke="currentColor" stroke-width="1.2" />
          </svg>
        </button>
      </div>
    </div>
  );
}
