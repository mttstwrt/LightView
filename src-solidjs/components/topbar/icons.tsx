// Inline SVG icons shared across the app chrome — the top bar, the mobile
// floating buttons, the command list and the settings page.
//
// All of them are 24×24 stroke outlines at stroke-width 2, so they stay
// visually one family at the two sizes actually used (16 in the desktop
// dropdown, 20 in the mobile sheet and floating buttons). Anything filled or
// at a different weight reads as a foreign icon set at these sizes.

import type { Component } from "solid-js";

/** Every icon here takes the same props, so the command list can hold one as
 *  data without knowing which. */
export type Icon = Component<{ size: number }>;

/** Shared wrapper — one place for the stroke conventions above. */
function Stroke(props: { size: number; children: any }) {
  return (
    <svg
      width={props.size}
      height={props.size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      stroke-width="2"
      stroke-linecap="round"
      stroke-linejoin="round"
    >
      {props.children}
    </svg>
  );
}

export const GearIcon: Icon = (props) => (
  <Stroke size={props.size}>
    <circle cx="12" cy="12" r="3" />
    <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 0 1-2.83-2.83l.06-.06A1.65 1.65 0 0 0 4.68 15a1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 2.83-2.83l.06.06A1.65 1.65 0 0 0 9 4.68a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 2.83l-.06.06A1.65 1.65 0 0 0 19.4 9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" />
  </Stroke>
);

/** Ticked box — the mobile Select button's glyph. */
export const SelectIcon: Icon = (props) => (
  <Stroke size={props.size}>
    <path d="M20 12v6a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2h9" />
    <path d="M8.5 11.5l3 3L21 5" />
  </Stroke>
);

export const CloseIcon: Icon = (props) => (
  <Stroke size={props.size}>
    <path d="M18 6L6 18M6 6l12 12" />
  </Stroke>
);

export const SearchIcon: Icon = (props) => (
  <Stroke size={props.size}>
    <circle cx="11" cy="11" r="7" />
    <path d="M21 21l-4.3-4.3" />
  </Stroke>
);

/** Overflow glyph for the command list's trigger on both surfaces.
 *
 *  Deliberately "more" rather than a `+`: the list contains Settings and
 *  Trash as readily as Upload, and a plus over-promises create while
 *  under-promising everything else (docs/frontend/chrome.md). */
export const MoreIcon: Icon = (props) => (
  <Stroke size={props.size}>
    <circle cx="5" cy="12" r="1.4" fill="currentColor" stroke="none" />
    <circle cx="12" cy="12" r="1.4" fill="currentColor" stroke="none" />
    <circle cx="19" cy="12" r="1.4" fill="currentColor" stroke="none" />
  </Stroke>
);

export const TagIcon: Icon = (props) => (
  <Stroke size={props.size}>
    <path d="M20.6 13.4l-7.2 7.2a2 2 0 0 1-2.8 0l-7.2-7.2a2 2 0 0 1-.6-1.4V4a1 1 0 0 1 1-1h8a2 2 0 0 1 1.4.6l7.4 7.4a2 2 0 0 1 0 2.8z" />
    <circle cx="7.5" cy="7.5" r="1.2" />
  </Stroke>
);

export const DuplicateIcon: Icon = (props) => (
  <Stroke size={props.size}>
    <rect x="9" y="9" width="11" height="11" rx="2" />
    <path d="M5 15H4a1 1 0 0 1-1-1V5a2 2 0 0 1 2-2h9a1 1 0 0 1 1 1v1" />
  </Stroke>
);

export const TrashIcon: Icon = (props) => (
  <Stroke size={props.size}>
    <path d="M3 6h18M8 6V4a1 1 0 0 1 1-1h6a1 1 0 0 1 1 1v2" />
    <path d="M19 6l-1 13a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 6" />
  </Stroke>
);

/** Wand + sparkle — the auto-tagging command (plugins on the desktop, remote
 *  workers on the web). */
export const AutoTagIcon: Icon = (props) => (
  <Stroke size={props.size}>
    <path d="M15 4l1 2.5L18.5 7.5 16 8.5 15 11l-1-2.5L11.5 7.5 14 6.5z" />
    <path d="M5 12l.7 1.8 1.8.7-1.8.7L5 17l-.7-1.8L2.5 14.5l1.8-.7z" />
    <path d="M20.5 13.5L11 23" />
  </Stroke>
);

export const UploadIcon: Icon = (props) => (
  <Stroke size={props.size}>
    <path d="M12 16V4m0 0L8 8m4-4l4 4" />
    <path d="M4 17v2a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-2" />
  </Stroke>
);

export const FolderIcon: Icon = (props) => (
  <Stroke size={props.size}>
    <path d="M3 7v12a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2V9a2 2 0 0 0-2-2h-6l-2-2H5a2 2 0 0 0-2 2z" />
  </Stroke>
);

// ── View-mode glyphs ────────────────────────────────────────────────────────
// One per `VIEW_CHOICES` entry, so the mobile view switcher's button can show
// which view is current without spending a label on it.

export const GridViewIcon: Icon = (props) => (
  <Stroke size={props.size}>
    <rect x="3" y="3" width="7.5" height="7.5" rx="1" />
    <rect x="13.5" y="3" width="7.5" height="7.5" rx="1" />
    <rect x="3" y="13.5" width="7.5" height="7.5" rx="1" />
    <rect x="13.5" y="13.5" width="7.5" height="7.5" rx="1" />
  </Stroke>
);

export const JustifiedViewIcon: Icon = (props) => (
  <Stroke size={props.size}>
    <rect x="3" y="4" width="11" height="6" rx="1" />
    <rect x="16" y="4" width="5" height="6" rx="1" />
    <rect x="3" y="14" width="5" height="6" rx="1" />
    <rect x="10" y="14" width="11" height="6" rx="1" />
  </Stroke>
);

// Boxes stepping outward around a centre one — the spiral, not a grid.
export const CanvasViewIcon: Icon = (props) => (
  <Stroke size={props.size}>
    <rect x="9.5" y="9.5" width="5" height="5" rx="1" />
    <path d="M17 12.5V7H7v10h6" />
    <path d="M20 15V4H4v16h11" />
  </Stroke>
);

export const MapViewIcon: Icon = (props) => (
  <Stroke size={props.size}>
    <path d="M9 3L3 5.5v15L9 18l6 3 6-2.5v-15L15 6z" />
    <path d="M9 3v15M15 6v15" />
  </Stroke>
);
