// The client's copies of two server facts, checked against the Rust source
// they copy. Each copy exists so the client can stay inside what the upload
// route accepts; a stale one either withholds files the server would take or
// sends a request the server stops partway.

import { describe, it, expect } from "vitest";
import schemaRs from "../../src-rust/src/companion/schema.rs?raw";
import uploadRs from "../../src-rust/src/server/upload.rs?raw";
import { MEDIA_EXTS, isUploadableName } from "./mediaExts";
import { FILES_PER_REQUEST } from "./ipc";

/** The quoted extensions in `MediaType::from_extension`'s match. */
function serverExtensions(): Set<string> {
  const start = schemaRs.indexOf("fn from_extension");
  const end = schemaRs.indexOf("_ => None", start);
  expect(start, "from_extension not found in schema.rs").toBeGreaterThan(-1);
  expect(end, "the match's `_ => None` arm not found").toBeGreaterThan(start);
  return new Set([...schemaRs.slice(start, end).matchAll(/"([a-z0-9]+)"/g)].map((m) => m[1]));
}

describe("the client's copy of the server's media extensions", () => {
  it("is the server's list exactly", () => {
    expect([...MEDIA_EXTS].sort()).toEqual([...serverExtensions()].sort());
  });

  it("judges a name the way sanitize_name does", () => {
    for (const ok of ["a.jpg", "B.JPEG", "clip.MOV", "Café 01.PNG", "dir/x.heic", "C:\\me\\x.dng", " a.png "]) {
      expect(isUploadableName(ok), ok).toBe(true);
    }
    // `.jpg` is all dots and extension: trimmed, it has none — and the server
    // stops the whole request at it.
    for (const no of [".jpg", "notes.txt", "payload.svg", "Holiday", "a.lightview.json", "", "..."]) {
      expect(isUploadableName(no), no).toBe(false);
    }
  });
});

describe("the client's upload batch", () => {
  it("is exactly what one request may carry", () => {
    // Two parts per file: its date, then its bytes.
    const max = uploadRs.match(/pub const MAX_PARTS: usize = (\d+);/);
    expect(max, "MAX_PARTS not found in upload.rs").not.toBeNull();
    expect(FILES_PER_REQUEST).toBe(Number(max![1]) / 2);
  });
});
