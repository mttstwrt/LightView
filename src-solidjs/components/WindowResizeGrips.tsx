import { For } from "solid-js";
import { getCurrentWindow } from "@tauri-apps/api/window";

// Mirrors the (unexported) ResizeDirection union from @tauri-apps/api/window.
type ResizeDirection =
  | "North"
  | "NorthEast"
  | "East"
  | "SouthEast"
  | "South"
  | "SouthWest"
  | "West"
  | "NorthWest";

// Invisible drag handles along the window edges and corners. With the native
// frame hidden (decorations: false) the OS no longer provides resize borders —
// especially on Linux/WebKitGTK — so we recreate them by starting a Tauri
// resize-drag on mousedown. Each grip is a thin fixed strip; corners sit above
// edges and are slightly larger to stay easy to grab.
const EDGE = 6; // px thickness of edge grips
const CORNER = 12; // px size of corner grips

interface Grip {
  dir: ResizeDirection;
  style: Record<string, string>;
  cursor: string;
}

const grips: Grip[] = [
  // Edges
  { dir: "North", cursor: "ns-resize", style: { top: "0", left: "0", right: "0", height: `${EDGE}px` } },
  { dir: "South", cursor: "ns-resize", style: { bottom: "0", left: "0", right: "0", height: `${EDGE}px` } },
  { dir: "West", cursor: "ew-resize", style: { top: "0", bottom: "0", left: "0", width: `${EDGE}px` } },
  { dir: "East", cursor: "ew-resize", style: { top: "0", bottom: "0", right: "0", width: `${EDGE}px` } },
  // Corners
  { dir: "NorthWest", cursor: "nwse-resize", style: { top: "0", left: "0", width: `${CORNER}px`, height: `${CORNER}px` } },
  { dir: "NorthEast", cursor: "nesw-resize", style: { top: "0", right: "0", width: `${CORNER}px`, height: `${CORNER}px` } },
  { dir: "SouthWest", cursor: "nesw-resize", style: { bottom: "0", left: "0", width: `${CORNER}px`, height: `${CORNER}px` } },
  { dir: "SouthEast", cursor: "nwse-resize", style: { bottom: "0", right: "0", width: `${CORNER}px`, height: `${CORNER}px` } },
];

export function WindowResizeGrips() {
  const win = getCurrentWindow();
  return (
    <For each={grips}>
      {(grip) => (
        <div
          class="fixed z-[55]"
          style={{ ...grip.style, cursor: grip.cursor }}
          onMouseDown={(e) => {
            if (e.button !== 0) return;
            e.preventDefault();
            win.startResizeDragging(grip.dir);
          }}
        />
      )}
    </For>
  );
}
