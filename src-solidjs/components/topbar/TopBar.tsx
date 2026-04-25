import { createSignal, onCleanup, onMount } from "solid-js";
import { FilterBar } from "./FilterBar";
import { SortMenu } from "./SortMenu";
import { SettingsMenu } from "./SettingsMenu";

interface TopBarProps {
  onOpenFolder: () => void;
  onOpenDuplicates?: () => void;
}

export function TopBar(props: TopBarProps) {
  const [visible, setVisible] = createSignal(false);
  let filterInputRef: HTMLInputElement | undefined;

  const handleMouseEnter = () => setVisible(true);
  const handleMouseLeave = () => setVisible(false);

  const handleGlobalKeyDown = (e: KeyboardEvent) => {
    if ((e.ctrlKey || e.metaKey) && e.key === "f") {
      e.preventDefault();
      setVisible(true);
      requestAnimationFrame(() => {
        filterInputRef?.focus();
        filterInputRef?.select();
      });
    }
  };

  onMount(() => window.addEventListener("keydown", handleGlobalKeyDown));
  onCleanup(() => window.removeEventListener("keydown", handleGlobalKeyDown));

  return (
    <>
      {/* Hover trigger zone */}
      <div
        class="fixed top-0 left-0 right-0 h-15 z-40"
        onMouseEnter={handleMouseEnter}
      />

      {/* The bar itself */}
      <div
        class="fixed top-0 left-0 right-0 z-40 h-12 flex items-center px-4 gap-3 transition-all duration-200"
        style={{
          background: "rgba(10, 10, 10, 0.85)",
          "backdrop-filter": "blur(12px)",
          transform: visible() ? "translateY(0)" : "translateY(-100%)",
          opacity: visible() ? "1" : "0",
        }}
        onMouseEnter={handleMouseEnter}
        onMouseLeave={handleMouseLeave}
      >
        <FilterBar onInputRef={(el) => { filterInputRef = el; }} />
        <SortMenu />
        <SettingsMenu onOpenFolder={props.onOpenFolder} onOpenDuplicates={props.onOpenDuplicates} onRequestShow={() => setVisible(true)} />
      </div>
    </>
  );
}
