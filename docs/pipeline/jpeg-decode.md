# JPEG decode performance

[← docs index](../README.md) · [pipeline](README.md)

**Status:** research / recommendation. Nothing here is built yet.
**Audience:** maintainers deciding whether to speed up thumbnail decode.

---

## 1. Where the time goes

Thumbnail *generation* is decode-bound, not resize- or encode-bound. On the
criterion `thumbnailer` bench (synthetic JPEGs, this machine):

| Source | Full thumbnail | Decode alone (`decode_image`) |
|---|---|---|
| 4000×3000 JPEG → 512px | ~20 ms | ~16 ms (~80%) |

The resize filter choice (nearest vs bilinear vs lanczos3) moves the total by
<1 ms, confirming decode dominates. So the only way to make thumbnailing
meaningfully faster is to decode faster (or decode fewer pixels).

The current decoder is **`jpeg-decoder`** (pure Rust), chosen specifically
because it supports **DCT-scaled decode** via `.scale()`: for a 4000×3000 source
targeting a 512px thumb it decodes at 1/4 (1000×750) instead of full res. See
`src-tauri/src/pipeline/thumbnailer.rs` (`decode_jpeg_to_rgba_inner`,
`generate_jpeg_thumbnail_inner`).

**Important floor:** DCT scaling only shrinks the IDCT + upsampling work. The
Huffman/entropy decode of the *full* stream cannot be skipped at any scale.
That is why a faster decoder helps (SIMD Huffman + SIMD IDCT), but the realistic
win is ~2–4×, not 10×.

---

## 2. Crates considered

### zune-jpeg — already in the tree, but NOT the answer
Pulled in transitively by `image` 0.25; used by our generic-decode fallback.
Its public API (`decode` / `decode_into` / `DecoderOptions`) has **no scale
factor** — it decodes full resolution only. It is ~1.5–2× faster *per pixel*
than `jpeg-decoder`, but forcing a full 12 MP decode instead of our 1/4-scale
decode means processing ~16× more pixels. For the large camera JPEGs a gallery
is full of, that is a **net loss**. Rejected for the main path.

### libjpeg-turbo (via `turbojpeg` or `mozjpeg-sys`) — the real crate answer
The only option that combines SIMD decode **and** DCT-scaled output
(`scale_num`/`scale_denom`, i.e. 1/8 … 1/1). Expect roughly **2–4× faster** than
`jpeg-decoder` on our scaled path.

- **Cost:** it is a C library — needs `nasm`/`cmake` at build time and adds
  cross-platform friction (Windows/CI especially).
- **Mitigant:** the build already links C libraries — `libheif-rs`, the `webp`
  crate (libwebp), bundled `rusqlite`. The tolerance for a C dependency already
  exists.
- `turbojpeg` has the friendlier API (`Decompressor` + `ScalingFactor`);
  `mozjpeg-sys` is lower-level but gives full control.
- The win is image-dependent, so it must be **measured on real photos**, not
  assumed, before taking on the C-build tax.

### jpegli — not yet viable in Rust
libjxl-team decoder with libjpeg-turbo-class speed and better quality. Rust
bindings are experimental / not production-ready. Revisit later.

---

## 3. The angle to try first (bigger payoff, no new dependency)

The HEIC path already prefers an **embedded thumbnail** over decoding the full
image (`pick_thumbnail_handle` in `thumbnailer.rs`). Most **camera JPEGs embed a
~160–320px JPEG thumbnail in EXIF (IFD1)**, and we already depend on
`kamadak-exif`, which exposes `In::THUMBNAIL` plus the `JPEGInterchangeFormat` /
`JPEGInterchangeFormatLength` tags needed to extract those bytes.

For the **micro (128px)** and **standard (512px)** tiers, decoding that tiny
embedded stream is near-instant versus scaling+decoding the full frame — likely
a larger win than any decoder swap, using only crates already present. It does
**not** help the large/preview/justified tiers (embedded thumbs are too small
and would upscale), so it is a tier-aware fast path, not a wholesale
replacement. Needs a size/quality guard: skip the embedded thumb when it is too
small for the requested tier or when EXIF orientation would require handling.

---

## 4. Recommendation

1. **Embedded-EXIF-thumbnail fast path first** — cheap, no new deps, biggest
   gain on the micro/standard tiers that dominate grid scrolling. Fall back to
   the current `jpeg-decoder` path when no suitable embedded thumb exists.
2. **Then, optionally, `turbojpeg` behind a Cargo feature flag** — A/B it against
   `jpeg-decoder` on real photos before defaulting it on, so the C-build tax is
   only paid if the measured win justifies it.

Both should be validated with the existing `decode_image` / `jpeg_thumbnail`
benches (before/after). Note: running benches on this repo requires building only
the bench target — `cargo bench` fails linking the bins (cargo #6313); use
`cargo build --profile bench --bench thumbnailer` then run the binary with
`--bench`.
