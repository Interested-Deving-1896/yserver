# Test status — latest numbers

Snapshot of the current xts5 (X Test Suite) and rendercheck (RENDER
smoke) pass rates. This file is the headline only; run-by-run history
and debugging notes live in [`xts-baseline.md`](xts-baseline.md) and
`status.md`.

## xts5 — full run #3, yserver/KMS bare-metal (M1 air, 2026-06-06)

`just xts-yserver-hw all 21600` — all 1078 test cases in 54 minutes,
zero crashes, zero hangs, **zero ABORTs**. Journal + summary:
`xts/results/2026-06-06-20:26:54/`.

**3419 / 5987 test purposes PASS (57.1%)** — up from 3370 (56.3%) on
the 2026-06-05 snapshot, driven by Xlib8 (+19), Xlib13 (+17),
Xlib9 (+6), XI (+4) and Xlib4 (+4).

| scenario  | cases | tests | PASS | FAIL | UNRES | UNTST | UNSUP | NOTIU | Δ PASS |
|-----------|------:|------:|-----:|-----:|------:|------:|------:|------:|-------:|
| Xproto    |   122 |   389 |  358 |    7 |     3 |    19 |     2 |     0 |      0 |
| Xlib3     |   109 |   162 |  105 |   18 |     5 |    27 |     6 |     1 |      0 |
| Xlib4     |    29 |   324 |  105 |  175 |    11 |    17 |    11 |     5 |     +4 |
| Xlib5     |    15 |    84 |   59 |   18 |     0 |     5 |     2 |     0 |      0 |
| Xlib6     |     8 |    50 |    6 |   15 |     0 |    29 |     0 |     0 |      0 |
| Xlib7     |    58 |   172 |   83 |   31 |     0 |    13 |    45 |     0 |      0 |
| Xlib8     |    29 |   165 |   88 |   39 |     6 |    22 |    10 |     0 |    +19 |
| Xlib9     |    46 |  1472 |  608 |  526 |    78 |    33 |    23 |   201 |     +6 |
| Xlib10    |    23 |    95 |   25 |   29 |     5 |    35 |     1 |     0 |      0 |
| Xlib11    |    33 |   195 |   50 |   72 |     2 |     4 |    24 |    43 |      0 |
| Xlib12    |    27 |   138 |   91 |   14 |     4 |    15 |     2 |    12 |      0 |
| Xlib13    |    32 |   269 |   96 |  124 |    34 |     9 |     3 |     3 |    +17 |
| Xlib14    |    45 |    58 |   19 |   34 |     0 |     5 |     0 |     0 |      0 |
| Xlib15    |    45 |   159 |  122 |    4 |     0 |    33 |     0 |     0 |     +1 |
| Xlib16    |    30 |   105 |   82 |    0 |     0 |    22 |     1 |     0 |      0 |
| Xlib17    |    55 |   131 |  100 |   10 |     0 |    21 |     0 |     0 |     +1 |
| Xopen     |     8 |   127 |  122 |    3 |     0 |     0 |     2 |     0 |      0 |
| Xt3       |    21 |    73 |   73 |    0 |     0 |     0 |     0 |     0 |      0 |
| Xt4       |    33 |   192 |   94 |    0 |     0 |    98 |     0 |     0 |      0 |
| Xt5       |    10 |    69 |   26 |    0 |     0 |    41 |     0 |     0 |      0 |
| Xt6       |     7 |    71 |   67 |    4 |     0 |     0 |     0 |     0 |      0 |
| Xt7       |    11 |   106 |   96 |    1 |     0 |     6 |     0 |     3 |      0 |
| Xt8       |     7 |    43 |   35 |    4 |     0 |     4 |     0 |     0 |      0 |
| Xt9       |    33 |   189 |  126 |    0 |     6 |    55 |     2 |     0 |     +1 |
| Xt10      |     8 |    17 |   16 |    0 |     0 |     1 |     0 |     0 |      0 |
| Xt11      |    58 |   285 |  245 |    4 |     0 |    34 |     0 |     0 |      0 |
| Xt12      |    22 |    67 |   55 |    0 |     1 |    11 |     0 |     0 |      0 |
| Xt13      |    39 |   178 |  126 |    5 |     0 |    47 |     0 |     0 |     +1 |
| Xt14      |     2 |    18 |   18 |    0 |     0 |     0 |     0 |     0 |      0 |
| Xt15      |     1 |     2 |    0 |    0 |     0 |     0 |     2 |     0 |      0 |
| XtC       |    29 |   147 |   88 |    1 |     1 |    56 |     1 |     0 |      0 |
| XtE       |     1 |     1 |    1 |    0 |     0 |     0 |     0 |     0 |      0 |
| ShapeExt  |    11 |    11 |   11 |    0 |     0 |     0 |     0 |     0 |      0 |
| XI        |    36 |   316 |  187 |   74 |    15 |    33 |     2 |     5 |     +4 |
| XIproto   |    35 |   107 |   36 |   49 |    13 |     9 |     0 |     0 |     −5 |
| **total** | **1078** | **5987** | **3419** | **1261** | **184** | **704** | **139** | **273** | **+49** |

ShapeExt and Xlib16 remain fully clean. yserver survived the whole
sweep with zero panics in the server log.

Notes:

- **Client resource-ID starvation is fixed** (`fcbd9c4` — recycle the
  client resource-ID base on disconnect): the 15 XIproto tail ABORTs
  from run #2 ("Could not open display" once the 32-bit base overflowed
  at client 4096) are gone; every TCM connected and ran. The ABORT
  column is zero across the board.
- All rows are now from a single same-machine run (air, M1) — the
  mixed M2/air snapshot footnotes from run #2 no longer apply.
- **Regression to investigate: XIproto 41 → 36 PASS** (FAIL 29 → 49,
  UNRES 7 → 13). Partly expected — the formerly-ABORTing tail TCMs now
  execute and FAIL/UNRES instead of not counting — but a few
  previously-PASSing tests flipped. Candidates: ID-recycle interaction,
  or M2 ↔ air variance (the run-#2 XIproto row came from the M2 box).
- Xlib4 recovered 101 → 105 after run #2's −7 regression; high-water
  is 108 (`71451ca` paint-window-background-on-map), so −3 still to
  re-find.
- 2 NORESULTs: `Xt5/XtUnmanageChildren`, `Xt5/XtUnmanageChild`.

Largest FAIL buckets / next targets:
1. **Xlib9 (526)** — remaining drawing/GetImage content semantics.
2. **Xlib4 (175)** — window attributes (incl. the −3 vs high-water).
3. **Xlib13 (124)** — WM/visibility semantics.
4. **XI (74)** — next: Xorg `AllowSome` port (unified core+XI freeze)
   for the AllowDeviceEvents FAILs.
5. **Xlib11 (72)** — residual grab semantics (root-down passive-grab
   ancestor search + NotifyGrab crossings; one over-delivery).

Previous full runs:
- #2 — 2026-06-05 (M2) + 2026-06-06 air XI row: 3370/5987 PASS (56.3%)
  — `xts/results/2026-06-05-13:20:07/` (+ `2026-06-06-00:58:03` for XI).
- #1 — 2026-06-04 (first ever to complete): 2784/5987 PASS (46.5%) —
  `xts/results/2026-06-04-15:48:44/`.

## rendercheck — bare-metal 2026-06-04, rendercheck 1.6, 900 s/test

| category    |  PASS | TOTAL |
|-------------|------:|------:|
| fill        |    64 |    64 |
| dcoords     |     2 |     2 |
| scoords     |     1 |     1 |
| mcoords     |     1 |     1 |
| tscoords    |     2 |     2 |
| tmcoords    |     2 |     2 |
| blend       |     5 |     5 |
| composite   |     5 |     5 |
| cacomposite |     5 |     5 |
| gradients   |  6081 |  6081 |
| repeat      |   380 |   380 |
| triangles   |   570 |   570 |
| bug7366     |     1 |     1 |
| **total**   | **7119** | **7119** |

**100% pass.**

> Use rendercheck ≥ 1.6. Version 1.5 has a bug in
> `gradients::render_to_gradient_test` that trips even against the
> host X server.
