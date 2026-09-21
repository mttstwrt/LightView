// The five colour labels, and what they look like.
//
// A presentation constant rather than a wire shape: the server stores and
// filters a lowercase string and has no opinion about which hex it renders as,
// so the palette lives on this side of the boundary and `types.ts` stays a
// mirror of the Rust types.

export const COLOR_LABELS = ["red", "yellow", "green", "blue", "purple"] as const;

export type ColorLabel = (typeof COLOR_LABELS)[number];

export const COLOR_LABEL_HEX: Record<ColorLabel, string> = {
  red: "#ef4444",
  yellow: "#eab308",
  green: "#22c55e",
  blue: "#3b82f6",
  purple: "#a855f7",
};
