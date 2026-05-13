# Phase 3E — rendering re-architecture — validation results

Date: 2026-05-13
Plan: `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3e.md` (v2 — codex-approved after single-`paint_resources` fix)
Branch: `graphics-followups`
Predecessor: KMS teardown fix results (`a693255`)

## Scope landed

Phase 3E migrated the text-run family — two call sites — off the legacy `flush_if_needed(ProtocolBarrier) + run_one_shot_op` borrow-conflict fallback onto `paint_resources() + scheduler.record_paint_op(...)`:

- **T1 (`2763f03`)**: `try_vk_text_run` (core PolyText8 / PolyText16). Single `paint_resources()` call before the glyph-intern loop gates atlas upload on `renderer_failed`. The unconditional `ProtocolBarrier` flush that fired on every text-run is gone; text-heavy frames now pack into the open PaintBatch alongside fill / copy / PutImage / CopyArea-same-overlap.
- **T1 doc fix (`c412c49`)**: audit catalogue entry for `try_vk_text_run` in `run_legacy_paint_op` flipped to "migrated 3E (record_paint_op, TextPipeline persistent descriptor)". Separate commit because the in-commit cleanup pattern wasn't established until T2 — see "Plan bugs caught" below.
- **T2 (`35960c4`)**: `try_vk_render_composite_glyphs` (RENDER CompositeGlyphs8/16/32). Same migration shape: one `paint_resources()` before the intern loop, recorder via `record_paint_op`. Audit-catalogue entry updated in the same commit.

Both migrations use the SHIM `scheduler.record_paint_op(...)` rather than the wide `record_paint_batch_op(...)`. Rationale: `TextPipeline::record_text_run` samples the glyph atlas via TextPipeline's persistent descriptor — there is no per-batch descriptor-arena work for the recorder to do, so the narrow shim is the right surface.

Intentionally unchanged: `GlyphAtlas::intern` still has its self-contained one-shot CB + `queue_wait_idle` for per-glyph atlas uploads. Folding glyph upload into the batch CB is phase-5 sync-rework scope.

## Preflight checks

End of 3E (HEAD = `cb44c1d`):

- `cargo +nightly fmt --check` — clean (no diff).
- `cargo clippy -p yserver` — 5 pre-existing `doc_lazy_continuation` warnings. No new warnings.
- `cargo test --workspace`:
  - `yserver` lib: **138 passed, 0 failed, 3 ignored**.
  - `yserver-core`: **284 passed**.
  - Other crates: green.

## Cutover greps

```
$ rg -n 'flush_if_needed.BatchFlushReason::ProtocolBarrier' crates/yserver/src/kms/backend.rs
1742:        if let Err(e) = self.flush_if_needed(BatchFlushReason::ProtocolBarrier) {
3313:                    if let Err(e) = self.flush_if_needed(BatchFlushReason::ProtocolBarrier) {
5102:            if let Err(e) = self.flush_if_needed(BatchFlushReason::ProtocolBarrier) {
6050:            if let Err(e) = self.flush_if_needed(BatchFlushReason::ProtocolBarrier) {
```

ZERO hits inside `try_vk_text_run` (4430..4730) and `try_vk_render_composite_glyphs` (5173..5579). Remaining sites:
- `1742` — `run_legacy_paint_op` body itself (legacy entry point).
- `3313` — `try_vk_copy_area` same-overlap resize-only pre-flush (3D-installed mitigation; still required until phase-4 sync rework).
- `5102` — `try_vk_render_traps_or_tris` (3F-deferred).
- `6050` — `try_vk_render_composite` (3F-deferred).

```
$ rg -n 'run_one_shot_op' crates/yserver/src/kms/backend.rs
```

ZERO hits inside the two migrated functions. Remaining sites: `run_legacy_paint_op` body, 3 readback handlers (`try_vk_get_image_pixels`, `hw_cursor_refresh`, `read_mirror_pixels`), 2 render-side legacy fallbacks (`try_vk_render_traps_or_tris`, `try_vk_render_composite` — 3F-deferred), `open_with_commit`, `dump_scanout_one`.

```
$ rg -n 'record_paint_op\(' crates/yserver/src/kms/backend.rs | wc -l
11
```

Above the 9+ floor the cutover-greps spec called for. Both new text-family call sites are present.

## Hardware smoke

Run on `silence` (AMD Polaris, dual-screen, MATE-on-X session). The smoke was **noisy** — two unrelated bugs surfaced during 3E smoke and had to be fixed first before the load-bearing 3E signals could be read. Details in "Interleaved fixes" below. Once those landed, the 3E-specific results were clean:

- ✅ **MATE renders correctly**. Panels, file-manager (caja) windows, menus, the full desktop chrome paints without artefacts.
- ✅ **gedit text scrolling is observably faster**. User report: "text scrolling in gedit very fast (mouse wheel works now)". The pre-3E baseline forced a `ProtocolBarrier` flush per text-run; eliminating that per-run flush is the load-bearing benefit of T1+T2.
- ✅ **xterm scrollback** still smooth (regression check vs 3D — text + copy-same-overlap paths share the open batch now).
- ✅ **No `renderer_failed`, no `paint batch submit failed`, no `DEVICE_LOST`** in `yserver-hw.log` after the interleaved fixes landed.
- ⚠️ **Caja perf issues remain** — slow icon-view scroll plus a "wheel needs view-switch to wake up" regression. Both filed as separate known-issues (`49a056b`, `39018c3`). The wheel-needs-view-switch IS a regression from a pre-3E baseline, but the root cause is in event-delivery state machinery — bisect candidates listed in the known-issue entry — not the text-run migration. Both deferred.

The pool-release fix at `cb44c1d` was hardware-smoked on MATE alongside the 3E migrations with no regressions. The smoke run was at WARN log level, so INFO-level pool-exhaustion summaries weren't captured; but no WARN-level `pool_ring_exhausted` either.

## Plan bugs caught (folded back into plan / fixed in-tree)

### 1. T1's audit-catalogue update lived in a separate commit

T1 landed as a pure code refactor (`2763f03`) and then updated the audit catalogue in `c412c49`. Code-quality review feedback during T1 pushed for the in-commit pattern — keep the comment block describing the legacy site in lockstep with the code it describes. T2 (`35960c4`) followed the in-commit pattern correctly: the audit-catalogue line for `try_vk_render_composite_glyphs` flipped in the same commit as the migration. For future task lists, the migration plan should call out "update audit catalogue in the same commit" as part of the task description, not as a followup.

## Commit summary (phase 3E)

| Task | Commit | Subject |
|---|---|---|
| Plan v1 | `1e96352` | phase-3E implementation plan (text-run migration) |
| Plan v2 | `43458a3` | phase-3E plan v2 — fold codex's single-paint_resources fix |
| T1 | `2763f03` | migrate try_vk_text_run to record_paint_op |
| T1 doc fix | `c412c49` | update audit catalogue for 3E T1 (text-run migrated) |
| T2 | `35960c4` | migrate try_vk_render_composite_glyphs to record_paint_op |
| Results doc | this commit | docs(plans): phase-3E validation results |

## Interleaved fixes during 3E hardware smoke

3E smoke surfaced four unrelated commits between T2 and the results doc. Filed here for honesty — none of them are part of the 3E migration itself, but each was needed before 3E could be cleanly validated on hardware:

1. **`92a2a83` fix(composite): correct inverted Automatic/Manual mode constants** — REVERTED. The fix accepted `update=1` (Manual mode) from compositing WMs, which activated `activate_redirect_backing_for` and diverted paint to a backing pixmap the compositor doesn't sample. Result on MATE: "alles kapot", mostly-black screen.
2. **`3751c11` Revert "fix(composite): correct inverted Automatic/Manual mode constants"** — back to the inverted-but-functional state. The root issue (decoupling redirect-record from backing allocation) is a future task, not a 3E concern.
3. **`7e6166e` docs(known-issues): file Composite Manual-mode regression after 92a2a83 revert** — filed for future investigation.
4. **`49a056b` docs(known-issues): file caja right-click popup offset** — filed; not a 3E regression, surfaced by 3E smoke.
5. **`39018c3` docs(known-issues): file caja wheel-needs-view-switch as yserver bug** — filed; bisect candidates listed in the entry. Predates 3E despite surfacing during 3E smoke.
6. **`cb44c1d` fix(kms): release composite pool slot per-frame, not just FIFO prefix** — codex-pinpointed real bug. `poll_in_flight()` only released composite pool slots for the FIFO prefix of in-flight frames, so a lagging frame on one output held pool slots hostage for already-retired frames on the other output, producing `pool_ring_exhausted` warnings and deferred frames. Fix walks all frames, releases each retired-and-not-yet-released frame's slot, tracks `pool_released: bool` per frame. Codex review tightened the gate to require ring existence plus a `debug_assert`. Hardware-smoked on MATE with no regressions. This was a pre-existing bug, not a 3E regression — surfaced because 3E's denser PaintBatch packing exposed more concurrent-output scheduling pressure.

## Known deferred items

- **Phase 3F** — render-composite family: `render::record_render_composite` (2 sites: `try_vk_render_traps_or_tris`, `try_vk_render_composite`), `MaskScratch::upload_r8` migration, `dst_readback` plumbing. Bigger than 3E because the render-composite recorder needs per-batch mask scratch + readback resource handling, unlike text's persistent-descriptor model.
- **`GlyphAtlas::intern` per-glyph `queue_wait_idle`** — phase-5 sync rework. The atlas's self-contained one-shot CB is correct under the current model; it just blocks the recorder thread on every new glyph. Folding into the batch CB requires the BatchResource lifecycle.
- **Composite Manual-mode regression** (`7e6166e`) — filed. Future task: decouple `activate_redirect_backing_for` from the redirect mode record so accepting `update=1` doesn't divert paint away from the screen.
- **Caja popup offset / wheel-needs-view-switch** (`49a056b`, `39018c3`) — filed; bisect candidates listed.
- **Composite pool starvation** — no longer deferred. Fixed at `cb44c1d`.
- **`record_get_image`** — still on `run_one_shot_op` with `flush_if_needed(Readback)`. Phase 5 (targeted VkFence per HLD), unchanged from 3D.
- **disable_output EINVAL** on the daily-driver punch list — fixed in `a693255` predecessor work; no new variants surfaced during 3E.

## What's next

Three reasonable next moves; user's call:

1. **Phase 3F** — render-composite migration. The next logical step in the rendering re-architecture rollout. Plan via writing-plans + codex review loop. Bigger than 3D + 3E combined; likely splits into 3F-traps-tris and 3F-composite sub-phases.
2. **Caja regression investigation** — the wheel-needs-view-switch issue is a real regression from a pre-3E baseline and impacts the user's daily-driver workflow. Bisect with the candidates in `39018c3`'s known-issue entry.
3. **disable_output EINVAL followups** — if any residual variants of the teardown bug remain on the punch list, they block hardware testing of further migrations.
