# Plan: Xlib9 text/fonts cluster — PCF fonts, font path, spec-correct core text

## Goal

Fix the Xlib9 text/font failure cluster (~150 FAIL+UNRES on vng XTS):
XDrawString 34F, XDrawText/16 31F+38U, XDrawString16 13F+18U,
XDrawImageString/16 22F, plus font functions (XLoadFont, XLoadQueryFont,
XListFonts(WithInfo), XQueryFont, XSetFontPath, XGetFontPath,
XGetFontProperty, XQueryTextExtents/16, XFreeFont, XUnloadFont ≈ 30F).

## Root causes (diagnosed)

1. **SetFontPath (51) silently swallowed** — falls into the unsupported-
   opcode catch-all (`process_request.rs:408`). xts5's `fontstartup`
   fixture calls `XSetFontPath` to point at its private font dir
   (`xts5/fonts/`, PCF files + `fonts.dir`) before every font test.
2. **No bitmap-font support** — `FontLoader::open_font` (kms/core.rs:183)
   resolves every name through fontconfig; `OpenFont("xtfont0")` silently
   substitutes a system monospace instead of returning BadName or loading
   the PCF. (Violates the no-silent-stub rule; latent bug for legacy apps.)
3. **Core text rendered antialiased** — `render_text_chars_v2`
   (v2/backend.rs:5761) renders FreeType A8 bitmaps through a GPU glyph
   atlas with alpha blending. X11 core text is binary: pixels are fg
   (PolyText) or fg/bg (ImageText); PolyText honors all 16 GC functions,
   fill-style, clip, plane-mask. The atlas path applies none of these.
4. **Format gate** — `engine.image_text` (v2/engine.rs:3916) drops
   non-BGRA8 targets, so text into depth-1/depth-8 pixmaps draws nothing
   ("Nothing was drawn with a gc function of GXcopy" → ~56 UNRESOLVED).

## Design

### A. Font path + fonts.dir resolution (FontLoader)

*(revised per codex review 2026-06-07)*

- Add `font_path: Vec<String>` to `FontLoader`; default = Xorg-style
  list filtered to existing dirs (`/usr/share/fonts/{misc,TTF,OTF,
  Type1,100dpi,75dpi}`) + `"built-ins"`. "built-ins" = the existing
  fontconfig catalog **plus the alias set ("fixed", "cursor", "nil2")**
  — it is the compatibility layer that keeps `fixed`/`cursor`
  resolving regardless of path contents (codex blocker #2).
- Parse `fonts.dir` and `fonts.alias` per path dir: fonts.dir =
  count line then `<file> <name>` pairs (names kept **verbatim**);
  fonts.alias = `<alias> <font-name>` pairs with recursive resolution
  capped at 20 hops (Xorg aliascount, dixfonts.c:933). Element
  validation: dir must exist with readable fonts.dir → else the
  SetFontPath element is invalid → BadValue (verified: misc/TTF/OTF
  on this system all carry fonts.dir).
- `SetFontPath` (opcode 51): parse `CARD16 npaths` + `LISTofSTR`;
  validate + install; invalid element → BadValue (old path kept).
  Empty list resets to the default path. New `Backend` trait methods
  `set_font_path`/`font_path`. **ynest does NOT proxy SetFontPath to
  the host** (host font path is server-global state — codex blocker
  #1): ynest stores the list locally for GetFontPath echo; OpenFont
  proxying to the host is unchanged there.
- `GetFontPath` returns the stored path instead of npaths=0.
- `OpenFont` resolution order (first match wins, case-insensitive):
  per path element in order — exact fonts.dir name match, alias
  match (recursive), XLFD wildcard match against the **full
  canonical XLFD strings** in fonts.dir; "built-ins" element = alias
  set + fontconfig catalog. No match after all elements → **BadName**.

### B. PCF faces via FreeType

- FreeType opens PCF natively (`Library::new_face(path)`); no parser.
- Metrics: replace the hardcoded `0x20..=0x7E` loop in
  `compute_font_metrics` with real charmap iteration
  (`FT_Get_First_Char`/`FT_Get_Next_Char` via freetype-sys FFI) so
  xtfont0's encodings {1,2,3} and default_char=0 produce correct
  min/max bounds, `min_char_code`/`max_char_code`, per-char CharInfo,
  and `default_char`.
- Font properties for `XGetFontProperty`/`ListFontsWithInfo`/QueryFont:
  read BDF properties (FONT_ASCENT, FONT_DESCENT, DEFAULT_CHAR, etc.)
  via a local `extern "C" { FT_Get_BDF_Property }` declaration
  (freetype-sys doesn't bind it; same libfreetype, FT_Face is exposed).
  **Best-effort only** (codex #5): scalable faces have no BDF
  properties — keep the synthetic-property path for them; never fail
  an open over properties. Missing glyphs get explicit zero CharInfo
  so QueryTextExtents can apply the "default_char, else zero metrics"
  X11 rule; xtfont0's encoding 0 is deliberately nonexistent.

### C. Spec-correct core text rendering (span path)

- Rasterize core-text glyphs monochrome: PCF gives 1-bpp bitmaps
  natively; scalable faces use `FT_LOAD_RENDER | FT_LOAD_TARGET_MONO`
  (matches Xorg's core-text handling of scalable fonts).
- Convert each glyph bitmap to horizontal runs → `Rectangle16` spans
  at (pen_x + bitmap_left + run_x, pen_y − bitmap_top + row).
- **PolyText8/16**: submit fg spans through the existing
  `fill_solid_rects` path → inherits all 16 GC functions
  (engine.logic_fill VkLogicOp pipelines), plane-mask, clip,
  depth-1/4/8 CPU fallback. Delete the glyph-atlas call for core text.
- **ImageText8/16**: per spec, GC function/fill-style are ignored
  (effective GXcopy): fill ONE bg rect from the run's overall extents
  (port miImageGlyphBlt, mi/miglblt.c:83 — covers overhangs/negative
  bearings, not a per-glyph union), then fg glyph spans via Copy.
- PolyText keeps the existing xTextElt parser (delta + len==255
  font-change sentinel, v2/backend.rs:10689) — spans are emitted per
  item with the item's font (codex #4).
- **Single translation point** (codex #6): span generation stays in
  window-local coords; only `fill_solid_rects`'s `target.offset`
  shift applies. Remove the redirect-resolution from the text layer
  when switching paths; add a redirected-child + clip regression test.
- Text into any drawable depth now works (span path already handles
  non-BGRA8) — removes the format gate for core text entirely.
- The GPU glyph-atlas path remains only for… nothing reachable from the
  core protocol; if it has no other consumer after this, mark it for
  removal in a follow-up rather than deleting in this change.

### D. Request-layer error semantics

- OpenFont: BadName when resolution fails (replaces sentinel-with-empty-
  metrics behavior — keep the sentinel only for host-proxy failures in
  ynest mode).
- ListFonts/ListFontsWithInfo: include font-path (fonts.dir) names —
  xtfont0..6 bare names + the two -vsw- XLFDs — ahead of built-ins,
  with `max_names` as ONE global budget across the ordered path walk
  (codex #7); LFWI opens each match for real metrics+properties.
- QueryTextExtents/16: should fall out of correct per-char metrics (B).

## Validation

- Unit tests: fonts.dir parsing, font-path validation errors, OpenFont
  resolution order incl. BadName, glyph→span conversion (known xtfont0
  10×10 block glyph at encoding 1), ImageText bg-box extents.
- XTS loop (`just xts-yserver Xlib9`, vng): expect the three text cases'
  UNRESOLVED blocks (~56) to flip, plus XDrawString/ImageString FAILs
  and most font-function FAILs. Also rerun Xproto (pOpenFont/pQueryFont
  touch the same code).
- Real-client smoke: xterm under ynest/HW (core fonts user), plus
  e16 headless probe (memory: libX11 reads FONT property — B changes
  property synthesis, must not regress 28f42d1).

## Sequencing (each step compiles + tests green)

1. FontLoader: fonts.dir parsing + font_path state + resolution order
   (incl. BadName) + unit tests. Backend trait + SetFontPath/GetFontPath
   handlers + dispatch.
2. Charmap-driven metrics + BDF properties (FFI) + QueryFont/LFWI wiring.
3. Span-path core text (PolyText/ImageText, both widths) + removal of
   the core-text atlas call + format-gate bypass.
4. XTS Xlib9 run; fix fallout; Xproto + Xlib14(font parts) spot-check.

## Risks

- ynest mode must keep proxying (host owns fonts there) — trait methods
  default to current behavior for the host backend.
- e16/fontset regression risk via FONT property changes (smoke covers).
- `relalldev`/XTS timing unrelated; text tests are synchronous.
- Geometry/copy/image clusters intentionally out of scope.
