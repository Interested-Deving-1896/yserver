# Frame-builder Phase B.2 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Port `render_composite` (and the trivial `render_fill_rectangles` delegate that wraps it) into the FrameBuilder. Three back-to-back `render_composite` calls in one frame collapse into a single `vkQueueSubmit2`. Combined with B.1's `composite_glyphs` port, B.2 absorbs ~75 % of bee MATE-drag submits (per the 2026-05-23 ranking: `render_composite` 21854 + `render_fill` 22885 + `composite_glyphs` 8606 out of ~74 k) into per-frame CBs. This is the **smoothness fix** the B.1 close-out flagged: drop bee `queue_submit2/s` from the post-B.1 ~2300 peak to the spec's 200–400/s end-of-B.4 band.

**Architecture:** Three pieces land together because none works alone:

1. **Mechanism 3 — `BatchResource` retire on frame fence + close-reopen-on-grow.** `EngineInner` scratch slots stay as `Option<DstReadback>` / `Option<MaskScratch>` / `Option<SolidColorImage>` (no Arc-wrap; the existing `&mut self`-mutating record APIs stay unchanged). The existing `ensure_returning_old` already returns `Option<Box<dyn BatchResource>>` on growth; B.2 routes the Box to a fence-gated owner: the open frame's `retired_resources` if a frame is open, otherwise the newest `SubmittedOp.retired_resources`, otherwise explicit immediate release only when no work is in flight. **Crucially**: (a) `BatchResource::release(self: Box<Self>, &VkContext)` is explicit (not Drop) — retirement paths must call it; (b) scratch growth mid-frame would mismatch op N's recorded view against op N+1's `record_copy_from(&mut current_scratch)`, so the plan forces a close-reopen before any grow if the frame already has prior ops. Closes the existing retired-scratch leak called out at `engine.rs:529-535`.
2. **Mechanism 2 — descriptor pool ring watermark.** `OpenFrame::frame_generation: u64` is captured on `open_for_paint` from a bumped `acquire_generation`; all per-op descriptor acquisitions during the open frame tag pools with that value; the frame's `SubmittedOp` retires at the same generation, and `DescriptorPoolRing::release_up_to(frame_generation)` recycles only pools whose `high_water_generation ≤ frame_generation`. The existing `release_up_to` API (`descriptor_pool_ring.rs:175`) already does this; B.2 just routes per-frame acquisitions through a single shared generation.
3. **Layout overlay flips to source-of-truth.** B.1's "snapshot for rollback only" model (Pitfall 2 of the B.1 plan) is replaced by "open-frame layout reads consult the overlay first, fall back to `storage.current_layout`." Each `record_layout_transition` during an open frame mutates `overlay.current_in_frame_layout`, not `storage.current_layout`. `commit_close_success` writes `current_in_frame_layout` back to storage on success; rollback writes `pre_frame_layout` back as today. Without this, two `render_composite` calls in the same frame would each see the pre-frame layout and emit duplicate / wrong barriers.

With those in place, `render_composite_via_frame_builder` records a `RecordedOp::RenderComposite` (params + resolved descriptor sets + pinned scratch indices + pinned staging indices). At close, `emit_recorded_op_into_cb` replays the op into the frame CB using existing `vk::ops::render::record_render_composite`. `render_fill_rectangles` continues to delegate to `render_composite` and gets the frame builder for free.

**Tech Stack:** Rust, `ash` Vulkan bindings, existing v2 FrameBuilder (B.1) + `DescriptorPoolRing` + `DstReadback` + `SolidColorImage` + `MaskScratch` + `RenderPipelineCache` + per-op `record_render_composite` infrastructure.

**Reference docs:**
- Phase B spec: `docs/superpowers/specs/2026-05-24-frame-builder-phase-b-design.md` (§ "Op representation", § "Transactional layout state", § "Frame-wide resource pinning")
- Phase A spec: `docs/superpowers/specs/2026-05-23-frame-builder-submit-rate-design.md`
- B.1 implementation plan: `docs/superpowers/plans/2026-05-24-frame-builder-phase-b1.md` (the template — re-use the same task shape, file structure, telemetry pattern)
- B.1 close-out / B.2 trigger: `docs/status.md` § "Phase B sub-phase B.1 — IMPLEMENTED 2026-05-24" + the bee hardware-gate "smoothness — NOT YET" entry

**File structure (locked in before tasks):**

- **Modify**: `crates/yserver/src/kms/v2/frame_builder.rs`
  - Add `RecordedRenderComposite` payload struct + `RecordedOp::RenderComposite` variant.
  - Extend `FramePinSet` with one `retired_resources: Vec<Box<dyn BatchResource>>` slot. Each entry is the boxed retired scratch image returned by `ensure_returning_old` on growth. Vk handles are destroyed by an EXPLICIT `boxed.release(&vk)` call at frame retirement (not Drop — `BatchResource` has no Drop-based teardown per `paint_batch.rs:147`). (Spec § "Frame-wide resource pinning" Mechanism 3; USER-codex R8.F1 + R9.F1 fixes.)
  - Add `OpenFrame::frame_generation: u64` field (Mechanism 2 watermark).
  - Add `frame_generation()` getter on `FrameBuilder`.
  - Add `peek_ops_kinds()` test introspection extending to RenderComposite.
- **Modify**: `crates/yserver/src/kms/v2/engine.rs`
  - **NO field type changes** on `EngineInner` scratch slots — they stay `Option<DstReadback>` / `Option<MaskScratch>` / `Option<SolidColorImage>` exactly as today. The existing `&mut`-mutating record APIs (`record_copy_from`, clear) stay unchanged.
  - Add `adopt_retired_resource_for_gpu_retirement(retired: Option<Box<dyn BatchResource>>)` helper: routes the returned Box to (a) the open frame's `retired_resources` if a frame is open; (b) the newest `submitted` SubmittedOp's `retired_resources` Vec (covers both post-close-on-grow AND legacy fall-through — `submitted.back` is always the newest fence owner); (c) `boxed.release(&self.vk)` only if `submitted` is empty AND no open frame. **Never drop a `Box<dyn BatchResource>` without calling release — `BatchResource::release` is explicit, not a Drop impl (`paint_batch.rs:147`).**
  - Extend `SubmittedOp` with a parallel `retired_resources: Vec<Box<dyn BatchResource>>` field (the existing `Option<ScratchImage>` slot is a concrete RAII type and stays untouched). Initialize as `Vec::new()` at every push site (`close_open_frame`, legacy paths).
  - Re-wire the ~5-8 existing `ensure_returning_old` call sites to call the new helper instead of dropping the returned Box on the floor (the documented leak at `engine.rs:529-535`).
  - (`composite_glyphs_via_frame_builder` needs no changes — B.1's path doesn't touch dst_readback / mask_scratch / src_alias_readback.)
  - Add `render_composite_via_frame_builder` paralleling `composite_glyphs_via_frame_builder`.
  - Keep `render_composite_legacy` as the gate-OFF branch (B.5 deletion target).
  - Add an `emit_recorded_render_composite_into_cb` helper called from `emit_recorded_op_into_cb`.
  - At `FrameBuilder::open_for_paint`, bump `inner.acquire_generation` once and store the result on `OpenFrame::frame_generation`; SubmittedOp at close uses the same value (NO second increment).
  - Wire `acquire_set_for_frame` (or equivalent helper) for the descriptor pool ring acquire path during open-frame ops.
  - Update `close_open_frame` so the SubmittedOp.generation = `open_frame.frame_generation` rather than `inner.acquire_generation += 1`.
- **Modify**: `crates/yserver/src/kms/v2/descriptor_pool_ring.rs` — no API changes; the existing `acquire_set(layout, generation)` and `release_up_to(retired_watermark)` already do what Mechanism 2 needs. The plan just changes the caller's choice of generation.
- **Modify**: `crates/yserver/src/kms/vk/dst_readback.rs` and `crates/yserver/src/kms/vk/mask_scratch.rs` — add a `fits(format, w, h) -> bool` predicate to each so Task 9's peek-before-grow can decide whether to close-reopen. No other changes; existing `ensure_returning_old` API stays.
- **No change to `crates/yserver/src/kms/vk/render_pipeline.rs`** (where `SolidColorImage` lives, lines 576+) — the 1×1 solid scratch doesn't grow and doesn't need pinning under B.2 (see Pitfall 4b).
- **Modify**: `crates/yserver/src/kms/v2/telemetry.rs` — add render-per-frame counters (`frame_builder_renders_per_frame_total` + `_max_in_window`, with avg derived at log time) and `frame_builder_close_reason_scratch_grow` so the new close reason stays counted.
- **Modify**: `crates/yserver/src/kms/v2/backend.rs` — at the `KmsBackendV2::render_composite` / `render_fill_rectangles` wrappers, call `drain_frame_builder_telemetry()` after the engine call (matches B.1's existing pattern at the `composite_glyphs` wrapper).
- **Modify**: `crates/yserver/tests/v2_acceptance.rs` — three new integration tests (`v2_frame_builder_render_composite_collapses_to_one_per_frame`, `v2_frame_builder_render_fill_rectangles_collapses_with_render_composite_in_same_frame`, `v2_frame_builder_mixed_glyphs_and_composite_one_submit`).
- **Modify**: `docs/status.md` — Phase B.2 status entry + bee hardware-smoke gate placeholder for the post-B.2 capture.

**Phased rollout choice.** Same shape as B.1 — HEAD stays green and `KmsBackendV2` runnable at every commit. Two structural commits before the gate flip:

- **Tasks 1–6** build Mechanism 3 + Mechanism 2 + layout-overlay-as-source-of-truth in isolation. No paint path uses the new code yet; existing tests stay green. Task 7 is retired (folded into Task 3 per R3.F7).
- **Tasks 8–12** add the sub-gate, `RecordedOp::RenderComposite`, `render_composite_via_frame_builder` body + emit, all BEHIND `YSERVER_FRAME_BUILDER_RENDER_COMPOSITE` (default OFF). Tests flip the gate on; production reaches the new code only at Task 20.
- **Tasks 13–15** wire M2 narrowing (drop close from render_composite + render_fill), telemetry, drain telemetry at backend wrappers.
- **Tasks 16–18** integration tests (mixed-sequence collapse, render_fill route-through, renderer_failed rollback).
- **Task 19** plain cargo fmt + clippy (NOT pedantic per AGENTS.md).
- **Task 20** flips `YSERVER_FRAME_BUILDER_RENDER_COMPOSITE` default ON. Single bisect-clean commit. This IS the bee smoothness fix.
- **Task 21** status doc update.

The per-port sub-gate keeps the gate-flip narrow. B.3 will follow the same pattern (per-port sub-gate, flip last).

---

## Invariant inventory (load-bearing — every task that adds code must respect)

- **M1 (sub-phase B.1–B.4).** `SubmitGroup::max_size == 1` — inherited unchanged from B.1.
- **M2 (sub-phase B.1–B.3).** Every paint entry point that has NOT been ported to the FrameBuilder closes the open frame BEFORE recording its own CB. B.2 removes `render_composite` and `render_fill_rectangles` from the "non-ported" set; the remaining 8 entry points (`fill_rect`, `fill_rect_batch`, `logic_fill`, `copy_area`, `cow_copy_area`, `put_image`, `image_text`, `render_traps_or_tris`) still flush.
- **M3 (sub-phase B.1–B.3).** Legacy scene compose closes the open frame BEFORE recording its own CB. Unchanged.
- **Drawable ticket-touch.** Every `RecordedOp::RenderComposite` append calls `store.touch_render_fence(id, frame_ticket.clone())` on dst + src (if Drawable) + mask (if Drawable) AND snapshots each touched drawable's pre-frame `last_render_ticket` if this is the first touch in the frame. Mirrors the B.1 composite_glyphs discipline.
- **Atlas ticket-touch.** Unchanged from B.1 — render_composite does not touch the atlas.
- **renderer_failed fatal-after-failure.** Inherited from B.1.
- **NEW: Frame-generation == SubmittedOp.generation.** The OpenFrame captures `frame_generation` once at `open_for_paint`; every descriptor acquisition during the open frame uses that value; the SubmittedOp pushed at close uses the same value. `DescriptorPoolRing::release_up_to(frame_generation)` correctly retires only the frame's pools.
- **NEW: Scratch growth returns its retired image as a `Box<dyn BatchResource>`.** Existing `DstReadback::ensure_returning_old` (`dst_readback.rs:107`) and `MaskScratch::ensure_image_size_returning_old` (`mask_scratch.rs:169`) already do this — B.2's only change is to route the returned Box into the newest fence owner via `adopt_retired_resource_for_gpu_retirement`. At retirement (`poll_retired`'s `submitted` walk drains `op.retired_resources` and calls `release(&inner.vk)` per Box), the Vk handles are destroyed via `BatchResource::release` — NOT via Drop. `BatchResource` has no Drop-based teardown (`paint_batch.rs:147` — `release(self: Box<Self>, &VkContext)`).
- **NEW: Layout overlay is source-of-truth during open frames.** Code paths that read `Drawable::storage.current_layout` while a frame is open MUST go through `frame.current_layout_for_drawable(id)` (returns the overlay value if first-touched, else falls through to storage). B.2 introduces this accessor and the audit covers `record_render_composite` callers.

---

## Close-path correctness pattern (inherits from B.1 — see `2026-05-24-frame-builder-phase-b1.md` § "Close-path correctness pattern")

The three pitfalls codified there (commit-after-submit-Ok, layout overlay model, borrowck pattern) apply verbatim to B.2's `render_composite_via_frame_builder`. The new wrinkle B.2 introduces:

### Pitfall 4 — Pin returned `Box<dyn BatchResource>` on grow; do NOT Arc-wrap mutable scratch

**Rejected approaches** (codex history):

- **Arc-wrap the entire scratch struct** (R3+R4 proposed model) — INCOMPATIBLE with the existing scratch APIs. `DstReadback::record_copy_from` (`dst_readback.rs:197`), `SolidColorImage` clear path (`render_pipeline.rs:706`), and `MaskScratch` resize all take `&mut self` and mutate internal layout state during recording. Wrapping in `Arc` makes `&mut`-access impossible without interior mutability (`Arc<RefCell<...>>` / `Arc<Mutex<...>>`), which complicates close-failure rollback. **User-codex R6 finding 1.**
- **`Arc::make_mut`** — would clone the inner struct (duplicating raw `vk::Image` handles whose Drop never fires for the duplicate). Hard reject.
- **`Arc::strong_count`** branching — wrong test (false positives from transient locals).

**Mandated model: use the existing `BatchResource` trait pattern.**

The scratch modules already implement `BatchResource` for their retired backing — see `dst_readback.rs:57` (`RetiredDstReadbackImage`) and `mask_scratch.rs:65` (`RetiredMaskScratchImage`). The existing `ensure_returning_old` returns `Result<Option<Box<dyn BatchResource>>, …>`; today v2 drops the returned Box on the floor (the documented leak at `engine.rs:529-535`). B.2's only change is: **growth happens before any new frame opens (Phase 9A); the retired Box rides the newest fence owner via the helper — case (a) open frame, case (b) latest `submitted` SubmittedOp (the just-closed frame or legacy per-op), case (c) explicit `release(&vk)` only if both are empty.**

**Crucial: `BatchResource::release(self: Box<Self>, &VkContext)` is EXPLICIT** (`paint_batch.rs:147`). The trait does NOT implement `Drop` for Vk-handle teardown; dropping a `Box<dyn BatchResource>` without calling `release()` LEAKS the underlying Vk handles. (USER-codex R8 finding 1.) Every retirement path must `boxed.release(&inner.vk)` explicitly.

**Mid-frame scratch growth is FORBIDDEN** (USER-codex R8 finding 2). The legacy `record_copy_from(&mut self)` writes commands that target the engine's CURRENT scratch slot (`dst_readback.rs:197`). Under deferred recording, op N's recorded view points at the scratch instance live at OP-APPEND TIME, but op N's emit-time `record_copy_from` would write into the CURRENT scratch — different instance after a grow. To prevent this, the frame MUST be closed before the engine slot is replaced. Implementation: peek the needed size before calling `ensure_returning_old`; if growth would fire AND the frame is open with prior ops, close-reopen first.

```rust
// Existing API (no change):
//   DstReadback::ensure_returning_old(format, w, h) -> Result<Option<Box<dyn BatchResource>>, _>
//   MaskScratch::ensure_image_size_returning_old(w, h) -> Result<Option<Box<dyn BatchResource>>, _>
//
// Caller pattern in render_composite_via_frame_builder (revised per R8.F1+F2):

// (a) PEEK: would growth fire for this op?
let need_grow = inner.dst_readback.as_ref()
    .map(|rb| !rb.fits(format, width, height))
    .unwrap_or(true);

// (b) If growth would replace the engine slot AND a frame is open with
//     prior ops referencing the current scratch, close-reopen FIRST.
//     This guarantees record_copy_from at emit-time targets the SAME
//     scratch instance the recorded views were resolved against.
if need_grow && inner.frame_builder.open.as_ref()
    .is_some_and(|o| !o.ops.is_empty())
{
    self.close_open_frame(store, platform, CloseReason::ScratchGrow)?;
    // Frame is now closed; the next paint append will reopen.
}

// (c) Now ensure can grow safely — no open frame's recorded ops can
//     hold a stale view. Under Phase 9A's grow-before-open rule, either
//     no frame is open, or an open frame has no prior ops.
let retired = inner.dst_readback.as_mut().expect("ensured")
    .ensure_returning_old(format, width, height)?;

// (d) Adopt-or-release: attach to an open frame if one exists; otherwise
//     attach to the newest SubmittedOp; release immediately only when
//     there is no in-flight owner.
inner.adopt_retired_resource_for_gpu_retirement(retired);
```

`CloseReason::ScratchGrow` is a new variant on `super::frame_builder::CloseReason`. Telemetry counts it via the same close_reasons bucket pattern as other variants.

**Engine helper revised to fence-gate retirement** (USER-codex R9 finding 1):

The naive "release immediately when no frame is open" rule is **wrong after a `close_open_frame` returns**: `close_open_frame` SUBMITS the CB but does not wait for GPU retirement. The just-closed frame's CB may still be sampling the to-be-retired scratch when we hit the no-frame branch. The retired Box must always attach to a fence-gated owner.

```rust
impl RenderEngineInner {
    /// Attach a retired scratch BatchResource to the right fence owner:
    /// (a) currently-open frame's pin set if one exists; OR
    /// (b) the back of `submitted` if any CB is still in flight. Under
    ///     B.2, close_open_frame appends the frame's SubmittedOp before
    ///     the post-close grow/adopt path runs, so submitted.back is the
    ///     newest fence owner. Under legacy, it is likewise the newest
    ///     per-op fence owner; OR
    /// (c) release immediately ONLY if the submitted queue is empty AND the
    ///     device's "no in-flight work" invariant holds (engine just
    ///     constructed, or post-drain_all).
    ///
    /// The retired Box rides the chosen owner's fence; on that fence's
    /// signal, the retirement walk drains + releases the BatchResource.
    pub(crate) fn adopt_retired_resource_for_gpu_retirement(
        &mut self,
        retired: Option<Box<dyn crate::kms::scheduler::paint_batch::BatchResource>>,
    ) {
        let Some(boxed) = retired else { return };
        // (a) open frame.
        if let Some(open) = self.frame_builder.open.as_mut() {
            open.pins.adopt_retired(boxed);
            return;
        }
        // (b) latest SubmittedOp — newest in-flight fence owner.
        //     Under B.2, every closed frame appends a SubmittedOp to
        //     `submitted` (in addition to its FrameSubmittedRecord on
        //     `pending_frames`). The SubmittedOp carries the frame's
        //     ticket, so attaching here correctly rides the frame's
        //     fence. Under legacy, per-op SubmittedOps newer than any
        //     pending_frame win — using submitted.back guarantees we
        //     route to the NEWEST owner (USER-codex R10.F2).
        if let Some(submitted) = self.submitted.back_mut() {
            submitted.append_retired_scratch(boxed);
            return;
        }
        // (c) nothing in flight — safe to release immediately.
        boxed.release(&self.vk);
    }
}
```

`SubmittedOp::append_retired_scratch(Box<dyn BatchResource>)` is a new helper. Existing `SubmittedOp.scratch: Option<ScratchImage>` is a concrete RAII type for the legacy `copy_area` self-overlap path and stays unchanged; a NEW parallel field `retired_resources: Vec<Box<dyn BatchResource>>` is added for B.2. See Task 1 Step 3 for the SubmittedOp extension.

**Frame retirement contract** (codex R8.F1 fix). `poll_retired` and `drain_all` MUST explicitly drain the frame's `retired_resources` and call `release(&inner.vk)` on each entry. Today's `pop_front()` drops the `FrameSubmittedRecord` which drops the Vec; under the BatchResource trait that's a LEAK. The fix is at every retirement site:

```rust
// crates/yserver/src/kms/v2/engine.rs (poll_retired, line ~749):

// Existing submitted-queue retire — already calls release_up_to(generation)
// for descriptor pools. Extend with the new B.2 scratch retire:
while let Some(front) = inner.submitted.front() {
    if !front.ticket.poll_signaled(&inner.vk) {
        break;
    }
    let mut op = inner.submitted.pop_front().expect("non-empty");
    unsafe { device.free_command_buffers(pool, &[op.cb]); }
    drop(op.staging.take());
    // NEW (B.2): drain any retired scratch BatchResources attached to
    // this op via the legacy fall-through case (c) of
    // adopt_retired_resource_for_gpu_retirement.
    for r in op.drain_retired_scratch() {
        r.release(&inner.vk);
    }
    inner.descriptor_pool_ring.release_up_to(op.generation);
}

// pending_frames walk (Phase B):
while let Some(front) = inner.pending_frames.front() {
    if !front.ticket.poll_signaled(&inner.vk) {
        break;
    }
    let record = inner.pending_frames.pop_front().expect("non-empty");
    // Explicitly release Mechanism 3 retired resources before the record drops.
    for r in record.pins.retired_resources {
        r.release(&inner.vk);
    }
    // staging_buffers Arc-clones drop here; no explicit release needed.
}
```

`SubmittedOp::drain_retired_scratch()` is a new helper that empties the per-op retired scratch Vec (paired with `append_retired_scratch` above).

**Close-failure rollback** (Task 12 error paths): under B.2's grow-before-open rule, `OpenFrame.pins.retired_resources` is **structurally empty** — every scratch growth happens BEFORE any new frame opens (Phase 9A), so the retired Box rides `submitted.back`, not the open frame's pin set. The rollback walk is defensive (for B.3+ when mid-frame retire becomes possible):

```rust
// engine.rs:close_open_frame error paths — before dropping open_frame:
for r in std::mem::take(&mut open_frame.pins.retired_resources) {
    r.release(&inner.vk);
}
```

**Pending-frames retire walk:** likewise defensive under B.2 (the FrameSubmittedRecord's `pins.retired_resources` is empty for B.2 frames — the submitted.back path is the actual carrier). Keep the walk for B.3+.

**`drain_all` (shutdown):** waits on the deepest ticket, then walks both queues; same release pattern as poll_retired.

**`RenderEngine::Drop`:** today already calls `drain_all` via `KmsBackendV2::shutdown`. With the explicit release pattern wired into `drain_all`, Drop is implicitly covered.

**What about scratches that don't have a `BatchResource`-yielding grow path?** `SolidColorImage` is 1×1, doesn't grow, doesn't need pinning. `MaskScratch` already has `RetiredMaskScratchImage` (`mask_scratch.rs:65`). `src_alias_readback` is a `DstReadback`, same shape. `white_mask_image` is a non-growing 1×1 `SolidColorImage`. Everything that grows has a `BatchResource` impl already.

### Pitfall 4b — Solid-color scratch lifetime

The 1×1 `solid_src_image` / `solid_mask_image` / `white_mask_image` never grow, so there's no `BatchResource` retire path needed. The `&mut`-mutation question is different here: **`record_solid_color_clear` runs per-op at emit time, not once at `ensure_render_assets`** (USER-codex R8 finding 4; see `engine.rs:5622-5629` for the legacy per-op clear).

- `SolidColorImage` is allocated once via `ensure_render_assets`. The struct itself never grows, never gets replaced. The engine slot's `Option<SolidColorImage>` is stable across the entire engine lifetime (until shutdown).
- `record_solid_color_clear(&mut SolidColorImage, color)` records a `cmd_clear_color_image` plus a `to_SHADER_READ_ONLY` barrier into the passed CB; it mutates `SolidColorImage::current_layout` to track the layout. Multiple consecutive calls (same frame or across frames) are fine because the function explicitly transitions from the prior layout.
- **B.2 emit-time:** Task 12's `emit_recorded_render_composite_into_cb` calls `record_solid_color_clear(&mut inner.solid_src_image, c)` per op that needs it. Borrowing `&mut` on the engine's slot is safe because no recorded op holds a reference to the slot or its inner; recorded ops carry only `src_clear_color: Option<[f32; 4]>`.
- **No pin needed.** The engine owns the SolidColorImage for its entire lifetime; nothing in the open frame depends on its identity (only its `image_view()` handle, which is stable because the struct never gets replaced).

So solid_*_image needs NO entry in `retired_resources`.

### Retention bound

`retired_resources` size per frame is bounded by the close-reopen-on-grow rule: at most ONE growth per scratch kind per frame, because the second growth attempt would force another close-reopen. Total: at most 2 entries per frame (one dst_readback grow, one src_alias_readback grow). MaskScratch (not used by render_composite, only by render_traps_or_tris which stays legacy in B.2) doesn't contribute. SolidColorImage doesn't grow.

In practice typical workloads see 0 grow events per frame (steady-state after warm-up). The close-reopen overhead per grow is one extra `vkQueueSubmit2` which is negligible against the per-frame submit savings.

### Pitfall 5 — Layout overlay reads must use `current_in_frame_for_drawable`, not `storage.current_layout`

The existing `record_render_composite_open` (`crates/yserver/src/kms/vk/ops/render.rs:187-212`) emits the dst-side `to_color` barrier internally, reading `CompositeTarget::current_layout()` for the old layout. Under B.1 that read returns `storage.current_layout` and is correct because one CB transitions storage in-place. Under B.2 with two `render_composite` calls in the same frame:

- Op 1 records into the deferred op list. Storage is NOT mutated.
- Op 2 records into the same op list. If `CompositeTarget::current_layout()` reads storage, op 2 sees the pre-frame layout instead of op 1's post-back-transition layout, and emits a duplicate / wrong barrier at close.

**Mandated resolution:** the v2 CompositeTarget adapter (the v2 analog of v1's `DrawableImage`) MUST be frame-aware. At op-append, the engine pre-resolves the dst's old layout via the new helper and stores it on `RecordedRenderComposite`:

```rust
let dst_old_layout = inner.current_layout_for_drawable(store, dst_id);
// ... similar for src/mask if they're Drawable
```

At close-time, the emit helper passes this pre-resolved value into a NEW overload of the recorder that takes an explicit `old_layout` parameter instead of reading it from the target. See § Task 12 for the new recorder overload.

The plan's audit list (every read of a drawable's "current layout" in the open-frame path): dst (always Drawable), src if `ResolvedSource::Drawable`, mask if `ResolvedSource::Drawable`. Solid/Gradient sources are not Drawable and use engine-owned views (no layout transition needed — see § Task 10's gradient note).

### Pitfall 6 — Overlay update is ONCE per op, to the POST-op layout

Codex round 3 finding 4. The earlier plan draft hinted at TWO overlay writes per op (`set_drawable_in_frame(dst_id, COLOR_ATTACHMENT_OPTIMAL)` then `... , SHADER_READ_ONLY_OPTIMAL)`). Both writes happen at append time, before the recorded op executes. The next op-in-frame's append reads the overlay AFTER both writes — so only the SECOND write matters.

**Mandated rule:** at append-end of each ported op, set the overlay to the layout the recorded op will EXIT with. For `render_composite`, that's `SHADER_READ_ONLY_OPTIMAL` (the layout `record_render_composite_close` transitions to). One write per drawable per op, not two.

**Atomicity guard (codex round 4 finding 3):** the overlay write must happen inside the SAME scoped block as `open.ops.push(RecordedOp::RenderComposite(...))` — no intermediate `&mut self` re-borrow, no reentrant engine call, no callback. The plan's Task 11 step 3 places both inside a single `{ let open = inner.frame_builder.open.as_mut().expect("open"); ... }` block. If a future port (B.3) introduces a synchronous close-then-reopen path between op construction and overlay write, the overlay would briefly hold a stale value that a re-entered append could read — same class of bug as a missing barrier in deferred recording. **Debug-assert in `OpenFrame::push_op_and_set_layouts(op, layout_updates)`** to enforce atomicity:

```rust
impl OpenFrame {
    /// Append op + apply overlay updates in one critical section.
    /// Inlined; the debug_assert below proves that the helper is
    /// the only path that mutates ops + layouts in tandem.
    pub(crate) fn push_op_and_set_layouts(
        &mut self,
        op: RecordedOp,
        drawable_layout_updates: &[(DrawableId, vk::ImageLayout)],
    ) {
        self.ops.push(op);
        for (id, layout) in drawable_layout_updates {
            self.layouts.set_drawable_in_frame(*id, *layout);
        }
    }
}
```

Task 11 calls this helper instead of the open `ops.push + set_drawable_in_frame` shape.

---

## Tasks

### Task 1: Plumb `ensure_returning_old`'s `BatchResource` return into the engine (Mechanism 3, foundation)

**Per USER-codex R6 finding 1:** the original Arc-wrap design was incompatible with the existing `&mut`-mutating scratch APIs. The corrected model uses the existing `BatchResource` retire pattern: `ensure_returning_old` already returns `Option<Box<dyn BatchResource>>`; B.2 adds a hook that routes the returned Box to the open frame, the newest `SubmittedOp`, or explicit immediate release when no work is in flight, instead of dropping it.

**No changes to scratch field types.** `EngineInner::dst_readback` stays `Option<DstReadback>` (NOT Arc-wrapped). Same for `src_alias_readback`, `mask_scratch`, `solid_*_image`. Mutation during recording (`record_copy_from`, clear, etc.) continues to use `&mut`.

**Files:**
- Modify: `crates/yserver/src/kms/v2/frame_builder.rs` — add `CloseReason::ScratchGrow` before any Phase 9A code references it; update the exhaustive close-reason test from 8 to 9 variants.
- Modify: `crates/yserver/src/kms/v2/engine.rs` — extend the existing `_legacy` mutation sites to forward the returned `Box<dyn BatchResource>` to a new `inner.adopt_retired_resource_for_gpu_retirement(retired)` helper that routes to the correct fence owner.

- [ ] **Step 1: Add `CloseReason::ScratchGrow` and update the exhaustive variant test**

`CloseReason::ScratchGrow` is referenced by the Phase 9A close-before-grow path, so the enum must grow before that implementation lands. B.1's exhaustive test currently documents eight variants; B.2 changes that to nine:

```rust
// crates/yserver/src/kms/v2/frame_builder.rs
pub(crate) enum CloseReason {
    SceneCompose,
    NonPortedPaintOp,
    LegacyScCompose,
    PresentCompletionSignal,
    SyncWait,
    Timeout,
    Shutdown,
    PinCeiling,
    /// A growable scratch image would be replaced while the open frame
    /// has prior ops that recorded views into the old image. Close first
    /// so the old backing rides the just-submitted frame fence.
    ScratchGrow,
}

#[test]
fn close_reason_has_nine_variants_for_b2() {
    fn _exhaustive(r: CloseReason) -> &'static str {
        match r {
            CloseReason::SceneCompose => "scene_compose",
            CloseReason::NonPortedPaintOp => "non_ported_paint_op",
            CloseReason::LegacyScCompose => "legacy_sc_compose",
            CloseReason::PresentCompletionSignal => "present_completion_signal",
            CloseReason::SyncWait => "sync_wait",
            CloseReason::Timeout => "timeout",
            CloseReason::Shutdown => "shutdown",
            CloseReason::PinCeiling => "pin_ceiling",
            CloseReason::ScratchGrow => "scratch_grow",
        }
    }
    assert_eq!(_exhaustive(CloseReason::ScratchGrow), "scratch_grow");
}
```

- [ ] **Step 2: Add `fits()` accessor to DstReadback + MaskScratch**

(USER-codex R9 finding 4.) Task 9 Phase 9A needs a peek-only "would this op grow the scratch?" predicate. Add `fits(format, width, height) -> bool` on `DstReadback` and `MaskScratch`. One-line predicate body, comparing against the stored format+extent in the per-format slot:

```rust
// crates/yserver/src/kms/vk/dst_readback.rs (alongside the existing ensure_returning_old):
impl DstReadback {
    pub fn fits(&self, format: vk::Format, width: u32, height: u32) -> bool {
        match format {
            vk::Format::B8G8R8A8_UNORM => self.bgra.as_ref()
                .map_or(false, |s| s.extent.width >= width && s.extent.height >= height),
            vk::Format::R8_UNORM => self.r8.as_ref()
                .map_or(false, |s| s.extent.width >= width && s.extent.height >= height),
            _ => false,  // unsupported format → ensure_returning_old will gate
        }
    }
}
```

Same shape for `MaskScratch::fits(width, height) -> bool` (no format arg; mask scratch is R8 only). Audit the actual extent field names at write time.

- [ ] **Step 3: Audit current call sites that drop `BatchResource` on the floor**

```bash
cd /home/jos/Projects/yserver
grep -nE 'ensure_returning_old|ensure_image_size_returning_old' crates/yserver/src/kms/v2/engine.rs
```

Expect ~5-8 sites. Each one currently has the documented leak: returns `Box<dyn BatchResource>` that gets discarded (via `let _retired = ...` or `?;` pattern that drops on Ok). Each becomes:

```rust
let retired = inner.dst_readback.as_mut().expect("ensured")
    .ensure_returning_old(format, w, h)?;
inner.adopt_retired_resource_for_gpu_retirement(retired);
```

- [ ] **Step 4: Add the engine helper**

```rust
// crates/yserver/src/kms/v2/engine.rs:
impl RenderEngineInner {
    /// Route a retired BatchResource to the right fence-gated owner.
    /// Never drops the Box without calling release — BatchResource has
    /// no Drop-based teardown path (`paint_batch.rs:147`).
    ///
    /// Ownership cases (in order of precedence):
    ///   (a) open frame's pin set,
    ///   (b) latest submitted SubmittedOp — this is BOTH the post-close-
    ///       on-grow target (the just-closed frame appended a SubmittedOp
    ///       carrying the frame's ticket) AND the legacy fall-through
    ///       target (per-op SubmittedOps from `_legacy` callers).
    ///       Using submitted.back instead of pending_frames.back
    ///       guarantees we attach to the NEWEST fence owner
    ///       (USER-codex R10.F2 — pending_frames may be older than
    ///       later legacy SubmittedOps).
    ///   (c) immediate release if `submitted` is empty AND no open frame.
    pub(crate) fn adopt_retired_resource_for_gpu_retirement(
        &mut self,
        retired: Option<Box<dyn crate::kms::scheduler::paint_batch::BatchResource>>,
    ) {
        let Some(boxed) = retired else { return };
        if let Some(open) = self.frame_builder.open.as_mut() {
            open.pins.adopt_retired(boxed);
            return;
        }
        if let Some(submitted) = self.submitted.back_mut() {
            submitted.append_retired_scratch(boxed);
            return;
        }
        boxed.release(&self.vk);
    }
}
```

**`SubmittedOp` extension** (USER-codex R10.F3). Current `SubmittedOp.scratch: Option<ScratchImage>` is a concrete RAII type for the legacy `copy_area` self-overlap path; it is NOT `Box<dyn BatchResource>`. Adding a parallel field is the cleanest fix:

```rust
// crates/yserver/src/kms/v2/engine.rs — extend SubmittedOp:
struct SubmittedOp {
    cb: vk::CommandBuffer,
    ticket: FenceTicket,
    staging: Option<Arc<StagingBuffer>>,
    scratch: Option<ScratchImage>,
    atlas_ticket: Option<FenceTicket>,
    generation: u64,
    /// NEW (B.2 Mechanism 3): retired BatchResources adopted via
    /// adopt_retired_resource_for_gpu_retirement case (b). Drained
    /// and released at `poll_retired` time.
    retired_resources: Vec<Box<dyn crate::kms::scheduler::paint_batch::BatchResource>>,
}

impl SubmittedOp {
    pub(crate) fn append_retired_scratch(
        &mut self,
        boxed: Box<dyn crate::kms::scheduler::paint_batch::BatchResource>,
    ) {
        self.retired_resources.push(boxed);
    }

    pub(crate) fn drain_retired_scratch(
        &mut self,
    ) -> std::vec::Drain<'_, Box<dyn crate::kms::scheduler::paint_batch::BatchResource>> {
        self.retired_resources.drain(..)
    }
}
```

All existing `SubmittedOp { ... }` initializers must add `retired_resources: Vec::new()`. The plan covers Task 1 + Task 12's close path init sites. Use `crate::kms::scheduler::paint_batch::BatchResource` directly unless implementation chooses a local `use` alias for readability.

- [ ] **Step 5: Re-wire `_legacy` call sites to call the helper**

For each of the ~5-8 call sites identified in Step 3, replace the drop pattern with:

```rust
// Was:
let _retired = inner.dst_readback.as_mut().expect("ensured")
    .ensure_returning_old(format, w, h)?;
// Now:
let retired = inner.dst_readback.as_mut().expect("ensured")
    .ensure_returning_old(format, w, h)?;
inner.adopt_retired_resource_for_gpu_retirement(retired);
```

Borrowck note: the call must release the `as_mut` borrow on `inner.dst_readback` before the helper takes `&mut inner`. Use a tight `{ }` scope or `let retired = { ... };` to release the borrow.

- [ ] **Step 6: Build + run tests**

```bash
cargo build -p yserver
cargo test -p yserver --lib
```

Expected: clean build, all 1038+ existing tests pass. Behavior equivalent to today for `_legacy`: helper case (b) attaches the BatchResource to the latest SubmittedOp when work is in flight; helper case (c) releases immediately when both queues are empty. The documented leak path is now closed.

- [ ] **Step 7: Commit**

```bash
git add -u
git commit -m "refactor(v2/engine): plumb ensure_returning_old's BatchResource to fence owner (B.2 Mechanism 3)

Existing ensure_returning_old already returns Option<Box<dyn BatchResource>>
on growth; today the Box is dropped on the floor (documented leak at
engine.rs:529-535). B.2 adds adopt_retired_resource_for_gpu_retirement
which routes the Box to the newest fence owner: case (a) open frame's
pin set, case (b) submitted.back's new retired_resources Vec (covers
post-close-on-grow AND legacy fall-through — submitted.back is always
the newest in-flight ticket), case (c) explicit release(&vk) only when
both are empty. Never drop a Box<dyn BatchResource> — BatchResource has
no Drop-based teardown (paint_batch.rs:147 — release(self: Box<Self>,
&VkContext) is explicit).

Adds DstReadback::fits + MaskScratch::fits for Task 9 Phase 9A's
peek-before-grow rule, and adds CloseReason::ScratchGrow plus the
updated exhaustive close-reason test before Phase 9A references it.
Extends SubmittedOp with a parallel retired_resources:
Vec<Box<dyn BatchResource>> field + append_retired_scratch /
drain_retired_scratch helpers; existing scratch: Option<ScratchImage>
stays untouched.

Scratch slot types remain Option<DstReadback> / Option<MaskScratch> /
Option<SolidColorImage> — no Arc-wrap. The existing &mut-mutating APIs
(record_copy_from, clear) stay as-is."
```

---

### Task 2: `FramePinSet::retired_resources: Vec<Box<dyn BatchResource>>` (Mechanism 3 — BatchResource model)

**Per USER-codex R6 finding 1:** the original "four typed scratch pin Vecs of Arc<...>" was incompatible with `&mut`-mutating scratch APIs. Replace with a single `retired_resources` Vec that holds the existing `Box<dyn BatchResource>` returned by `ensure_returning_old`.

**Files:**
- Modify: `crates/yserver/src/kms/v2/frame_builder.rs:453-479` — extend `FramePinSet`.
- Test: same file, new unit tests under `mod tests`.

- [ ] **Step 1: Add the retired-resources slot**

```rust
// crates/yserver/src/kms/v2/frame_builder.rs:453+
use crate::kms::scheduler::paint_batch::BatchResource;

#[derive(Default)]
pub(crate) struct FramePinSet {
    pub(crate) staging_buffers: Vec<Arc<super::engine::StagingBuffer>>,
    /// Mechanism 3: BatchResources retired from scratch growth during
    /// the frame. Each entry is the old image+view+memory of a scratch
    /// (DstReadback / MaskScratch) that was replaced by `ensure_returning_old`.
    /// Dropped on frame retirement (ticket signal); their `Drop` impls
    /// destroy the Vk handles. See `dst_readback.rs:57` and
    /// `mask_scratch.rs:65` for the existing BatchResource impls.
    pub(crate) retired_resources: Vec<Box<dyn BatchResource>>,
}

// Note: FramePinSet can't derive Debug because dyn BatchResource isn't
// Debug-impl-uniform — emit a manual Debug that prints lengths instead.
impl std::fmt::Debug for FramePinSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FramePinSet")
            .field("staging_buffers", &self.staging_buffers.len())
            .field("retired_resources", &self.retired_resources.len())
            .finish()
    }
}
```

Confirm the `BatchResource` trait already satisfies `Send + Debug` (per `paint_batch.rs:146`). If yes, the manual Debug above can be replaced with `#[derive(Debug)]`. Audit at write time.

- [ ] **Step 2: Add `adopt_retired` helper**

```rust
impl FramePinSet {
    pub(crate) fn adopt_retired(&mut self, retired: Box<dyn BatchResource>) {
        self.retired_resources.push(retired);
    }
}
```

No dedupe needed — each `ensure_returning_old` returns a fresh `Box`, never the same instance twice.

- [ ] **Step 3: Update `FramePinSet::len` + `is_empty`**

```rust
pub(crate) fn len(&self) -> usize {
    self.staging_buffers.len() + self.retired_resources.len()
}

pub(crate) fn is_empty(&self) -> bool {
    self.staging_buffers.is_empty() && self.retired_resources.is_empty()
}
```

- [ ] **Step 4: Write failing test — adopt_retired pushes to the Vec**

```rust
// crates/yserver/src/kms/v2/frame_builder.rs (inside `mod tests`):
#[test]
fn adopt_retired_pushes_to_retired_resources() {
    let mut set = FramePinSet::new();
    assert_eq!(set.retired_resources.len(), 0);
    // Use the existing `RetiredDstReadbackImage::for_tests()` shim or
    // construct one via DstReadback::ensure_returning_old in an
    // integration test. For pure-unit-test scope, wrap a no-op
    // BatchResource fake — see `paint_batch.rs:146` for the trait shape.
    struct FakeRetired;
    impl std::fmt::Debug for FakeRetired {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("FakeRetired")
        }
    }
    impl crate::kms::scheduler::paint_batch::BatchResource for FakeRetired {}
    set.adopt_retired(Box::new(FakeRetired));
    assert_eq!(set.retired_resources.len(), 1);
    assert_eq!(set.len(), 1);
}
```

- [ ] **Step 5: Run + verify + commit**

```bash
cargo test -p yserver --lib frame_builder::tests::adopt_retired_pushes_to_retired_resources
```

```bash
git add -u
git commit -m "feat(v2/frame_builder): FramePinSet::retired_resources (B.2 Mechanism 3)

Add a single Vec<Box<dyn BatchResource>> slot on FramePinSet for
retired scratch images (DstReadback / MaskScratch). Under B.2's
grow-before-open rule this Vec is structurally empty (retired
scratch attaches to submitted.back instead), but defensive walks
release every entry via BatchResource::release(&vk) on frame
retirement. B.3+ may populate this Vec when mid-frame retire
becomes possible.

No Arc wrapping — the engine's scratch slots stay as plain
Option<DstReadback> etc., preserving the existing &mut-mutating
record_copy_from API. Task 1's adopt_retired_resource_for_gpu_retirement
hook routes the Box to the appropriate fence owner."
```

---

### Task 3: `OpenFrame::frame_generation` field + descriptor watermark helper (Mechanism 2 — single atomic commit)

**Codex round 3 finding 7:** changing `SubmittedOp.generation` timing (from "close-time `acquire_generation += 1`" → "open-time captured value") while B.1's composite_glyphs path is live could shift the descriptor pool's `release_up_to(generation)` watermark relative to existing acquires. Mitigated by landing the field + the corresponding `acquire_descriptor_set_for_frame_or_op` helper + the close-path consumer in **one commit** (this task absorbs what was draft-Task-7). No intermediate state where some acquires use the old gen scheme and some use the new.

The B.1 composite_glyphs path uses the text pipeline's STATIC descriptor set (per the Phase B spec, "B.1 doesn't need Mechanism 2 — text pipeline uses a static descriptor set"); it does not call `descriptor_pool_ring.acquire_set` per op. So the only descriptor-acquire affected by the timing change today is the legacy render_composite path — which under sub-gate=OFF runs without any frame open, falls through to the legacy `acquire_generation` bump path, and behaves exactly as before. Audit confirms the change is safe to land as a single commit.

**Files:**
- Modify: `crates/yserver/src/kms/v2/frame_builder.rs` — extend `OpenFrame` struct + `FrameBuilder::open_for_paint` signature.
- Modify: `crates/yserver/src/kms/v2/engine.rs` — bump `acquire_generation` at frame-open, route close-time SubmittedOp.generation through `open.frame_generation`, add the `acquire_descriptor_set_for_frame_or_op` helper.
- Test: `crates/yserver/src/kms/v2/frame_builder.rs` + integration test for Mechanism 2 watermark.

- [ ] **Step 1: Locate `OpenFrame` definition + `open_for_paint` signature**

```bash
grep -nE 'struct OpenFrame|fn open_for_paint' crates/yserver/src/kms/v2/frame_builder.rs
```

Expect to find the `OpenFrame` struct (around `frame_builder.rs:1012`) and `FrameBuilder::open_for_paint` (around frame_builder.rs).

- [ ] **Step 2: Add the field + parameter**

```rust
// In OpenFrame:
pub(crate) frame_generation: u64,

// FrameBuilder::open_for_paint signature:
pub(crate) fn open_for_paint(
    &mut self,
    ticket: FenceTicket,
    frame_generation: u64,  // NEW
) {
    debug_assert!(self.open.is_none(), "open_for_paint while already open");
    self.open = Some(Box::new(OpenFrame {
        ticket,
        frame_generation,  // NEW
        ops: Vec::new(),
        pins: FramePinSet::new(),
        layouts: FrameLayoutTable::new(),
        touched: TouchedDrawables::new(),
        pending_glyph_inserts: PendingGlyphInserts::new(),
        atlas_prev_ticket_snapshot: None,
        glyph_uploads_in_frame: 0,
    }));
    self.lifetime_opens = self.lifetime_opens.saturating_add(1);
}
```

- [ ] **Step 3: Update callers — `composite_glyphs_via_frame_builder` (engine.rs:4680)**

```rust
// Before:
inner.frame_builder.open_for_paint(ticket);

// After:
inner.acquire_generation = inner.acquire_generation.saturating_add(1);
let frame_generation = inner.acquire_generation;
inner.frame_builder.open_for_paint(ticket, frame_generation);
```

Same change at the close+reopen path at engine.rs:4795.

- [ ] **Step 4: Update `close_open_frame` (engine.rs:1064-1075)**

```rust
// Before:
{
    let inner = self.inner.as_mut().expect("inner");
    inner.acquire_generation += 1;
    let generation = inner.acquire_generation;
    inner.pending_group_ops.push(SubmittedOp { ... generation });
}

// After:
{
    let inner = self.inner.as_mut().expect("inner");
    let generation = open_frame.frame_generation;  // captured at open
    inner.pending_group_ops.push(SubmittedOp { ... generation });
}
```

- [ ] **Step 5: Write failing test**

```rust
// crates/yserver/src/kms/v2/frame_builder.rs (inside `mod tests`):
#[test]
fn open_for_paint_records_frame_generation() {
    let mut fb = FrameBuilder::new();
    let ticket = FenceTicket::for_tests_stub();
    fb.open_for_paint(ticket.clone(), 42);
    assert_eq!(fb.open.as_ref().unwrap().frame_generation, 42);
}
```

- [ ] **Step 6: Run + verify**

```bash
cargo test -p yserver --lib frame_builder::tests::open_for_paint_records_frame_generation
cargo test -p yserver --lib
```

Both should pass.

- [ ] **Step 7: Add `acquire_descriptor_set_for_frame_or_op` helper (fold of draft Task 7)**

```rust
// crates/yserver/src/kms/v2/engine.rs:
impl RenderEngineInner {
    /// Acquire a descriptor set tagged with the open frame's generation
    /// (Mechanism 2). Falls back to `acquire_generation + 1` when no
    /// frame is open (legacy per-op path).
    pub(crate) fn acquire_descriptor_set_for_frame_or_op(
        &mut self,
        layout: vk::DescriptorSetLayout,
    ) -> Result<vk::DescriptorSet, vk::Result> {
        let generation = if let Some(open) = self.frame_builder.open.as_ref() {
            open.frame_generation
        } else {
            self.acquire_generation = self.acquire_generation.saturating_add(1);
            self.acquire_generation
        };
        self.descriptor_pool_ring.acquire_set(layout, generation)
    }
}
```

**Invariant (codex round 3 finding 3):** `DescriptorPoolRing::acquire_set(layout, generation)` only allocates from pools whose state is `Active` (currently growing — never seen `vkResetDescriptorPool`) OR was just transitioned `Free → Active` via `ensure_active_with_capacity` after the ring's `release_up_to` reset it. The ring's `release_up_to(retired_watermark)` only resets pools whose `high_water_generation <= retired_watermark` (via `vkResetDescriptorPool`), and Vulkan VUID-vkResetDescriptorPool-descriptorPool-00313 mandates that all CBs referencing the pool's sets must have completed execution before reset. Therefore:

- **Active pool case:** allocating from a still-growing pool produces a handle to backing storage that has NEVER been written to by `vkAllocateDescriptorSets` before; no prior CB can possibly reference it.
- **Just-reset pool case:** the reset guarantees no in-flight CB depends on any of the pool's prior sets; the new `vkAllocateDescriptorSets` call produces fresh handles whose backing storage is also CB-independent.

Either way, the descriptor set returned by `acquire_set` has zero in-flight CB dependencies. `vkUpdateDescriptorSets` against it at op-append time is safe per Vulkan host-mutation rules (VUID-vkUpdateDescriptorSets-pDescriptorWrites-06493): the targeted set must not be used by any pending command buffer.

**This invariant is load-bearing for B.2.** If a future refactor changes the ring to recycle descriptor sets without going through reset (e.g. a hypothetical "fast-reuse" path), `vkUpdateDescriptorSets`-at-append would become unsafe. The plan asserts the current ring's behavior matches this invariant; an integration test in Task 3 Step 8 verifies the watermark accounting end-to-end.

**Audit gate (codex round 4 finding 1):** before any code lands, walk `crates/yserver/src/kms/v2/descriptor_pool_ring.rs` (200 lines) and confirm:
- No `Vec` / `HashMap` of "free descriptor sets" or "reusable set handles" — the only allocation path is `vkAllocateDescriptorSets` against the active pool.
- `release_up_to` is the ONLY transition from `InFlight` → `Free`, and it always calls `vkResetDescriptorPool` (no shortcut that returns sets to a freelist).
- `ensure_active_with_capacity` only promotes `Free` → `Active`, never `InFlight` → `Active` (which would reuse pending sets).

If any of those assumptions break, B.2's safety analysis is invalidated; the audit must surface the gap before Task 3 lands.

**Cross-frame retirement contract (codex round 4 finding 2):** moving `SubmittedOp.generation` from close-time-increment to open-time-captured means the retire-side paths see generations that are CAPTURED-then-SUBMITTED in time order, not STRICTLY-MONOTONIC-AT-SUBMIT order. Two frames with generations N and N+1 may submit in order N→N+1 (typical) or in order N+1→N (if frame N is open + frame N+1 opens after a close-reopen). Audit:
- `poll_retired` (`engine.rs:746-755`) walks `pending_frames` and pops from the front when `front.ticket.poll_signaled()` returns true. The walk is correct as long as tickets signal in submit-order; same-queue submission ordering on a single graphics queue guarantees that (Vulkan spec § 3.2.4).
- `release_up_to(retired_watermark)` walks pools and releases any whose `high_water_generation <= retired_watermark`. Generations being non-monotonic at submit-time means the watermark itself is non-monotonic across calls — which the existing code already tolerates (each call is a fresh walk, no inter-call state).
- `drain_all` walks both queues until empty. Order-independent.

Task 3 Step 8's integration test must cover the out-of-order case: open frame A (generation 11), open frame B (generation 12 — via close-reopen path or via paint after A closes), submit B before A retires, confirm `release_up_to(generation_b_retired)` does NOT release pool with high_water_generation=11 if frame A is still in flight.

Wait — actually with cap=1 and the FrameBuilder's single-open semantics, two frames cannot be "in flight" overlapping at the same time. Each close drives a `flush_submit_group` synchronously; the next frame can't open until the SubmitGroup has flushed. So generations ARE monotonic at submit-time under B.2's constraints. **The integration test still adds value as a regression guard against future B.4 multi-output where compose-in-frame may introduce overlap.**

- [ ] **Step 8: Integration test — frame's acquire uses frame_generation, ring's high-water matches**

```rust
#[test]
fn acquire_descriptor_uses_frame_generation_when_open() {
    let mut be = KmsBackendV2::for_tests_with_vk().expect("for_tests_with_vk");
    be.set_frame_builder_enabled_for_tests(true);
    // No render_composite sub-gate needed; we test the helper directly via
    // an introspection method.
    be.engine_acquire_generation_set_for_tests(10);
    // Open a frame
    let ticket = be.platform_open_submit_group_for_tests();
    be.engine_open_frame_for_paint_for_tests(ticket);
    let frame_gen = be.engine_open_frame_generation_for_tests();
    assert_eq!(frame_gen, 11, "open bumps acquire_generation");
    // Acquire two descriptor sets while frame is open
    let _ds1 = be.engine_acquire_descriptor_set_for_frame_or_op_for_tests();
    let _ds2 = be.engine_acquire_descriptor_set_for_frame_or_op_for_tests();
    assert_eq!(
        be.descriptor_pool_ring_high_water_generation_for_tests(),
        frame_gen,
        "both acquires tag the pool with the same frame_generation",
    );
    // Acquire one without an open frame
    be.engine_close_open_frame_for_timeout_for_tests();
    let _ds3 = be.engine_acquire_descriptor_set_for_frame_or_op_for_tests();
    assert_eq!(
        be.descriptor_pool_ring_high_water_generation_for_tests(),
        12,
        "post-close acquire bumps acquire_generation to 12",
    );
}
```

Test helpers added to backend.rs under `#[cfg(test)]`:
- `engine_acquire_generation_set_for_tests(u64)` — direct field setter.
- `platform_open_submit_group_for_tests() -> FenceTicket`.
- `engine_open_frame_for_paint_for_tests(FenceTicket)`.
- `engine_open_frame_generation_for_tests() -> u64`.
- `engine_acquire_descriptor_set_for_frame_or_op_for_tests() -> vk::DescriptorSet`.
- `descriptor_pool_ring_high_water_generation_for_tests() -> u64`.
- `engine_close_open_frame_for_timeout_for_tests()`.

- [ ] **Step 9: Commit**

```bash
git add -u
git commit -m "feat(v2): Mechanism 2 watermark — frame_generation + acquire_for_frame_or_op (B.2)

Bundle the OpenFrame::frame_generation field, the open-time bump of
acquire_generation, the close-path consumer of open.frame_generation
on SubmittedOp, AND the acquire_descriptor_set_for_frame_or_op helper
into ONE commit. Avoids the intermediate state where some descriptor
acquires use the old per-op-bump scheme and some use the frame
watermark. B.1's composite_glyphs path (static text-pipeline descriptor;
no per-op acquire) is unaffected. Legacy render_composite under
sub-gate=OFF still bumps per-op via the fallback branch."
```

---

### Task 4: Layout overlay flips to source-of-truth — read accessor

**Files:**
- Modify: `crates/yserver/src/kms/v2/frame_builder.rs` — extend `FrameLayoutTable` with a fall-through accessor.
- Modify: `crates/yserver/src/kms/v2/engine.rs` — add `RenderEngineInner::frame_layout_for_drawable` helper that returns the overlay-or-storage layout.

- [ ] **Step 1: Reuse existing `FrameLayoutTable::current_layout_for_drawable(id, fallback)`**

(USER-codex R6 finding 2.) The existing accessor at `frame_builder.rs:679` already returns `vk::ImageLayout` with an inline storage fallback. Signature:

```rust
pub(crate) fn current_layout_for_drawable(
    &self,
    id: DrawableId,
    storage_fallback: vk::ImageLayout,
) -> vk::ImageLayout
```

Reuse this directly. Do NOT add a new `_or_none` variant; the existing fallback shape covers B.2's needs.

- [ ] **Step 2: Add `RenderEngineInner::current_layout_for_drawable` engine-level wrapper**

```rust
// crates/yserver/src/kms/v2/engine.rs (near other engine helpers):
impl RenderEngineInner {
    pub(crate) fn current_layout_for_drawable(
        &self,
        store: &DrawableStore,
        id: DrawableId,
    ) -> vk::ImageLayout {
        let storage_fallback = store
            .get(id)
            .map(|d| d.storage.current_layout)
            .unwrap_or(vk::ImageLayout::UNDEFINED);
        if let Some(open) = self.frame_builder.open.as_ref() {
            open.layouts.current_layout_for_drawable(id, storage_fallback)
        } else {
            storage_fallback
        }
    }
}
```

The engine-side wrapper threads the storage fallback through. Open-frame callers use this single accessor; non-open-frame paths fall through to storage directly.

- [ ] **Step 3: Write failing test**

```rust
#[test]
fn current_layout_for_drawable_reads_overlay_when_first_touched() {
    let id = DrawableId::for_tests(1);
    let store = DrawableStore::with_test_drawable(
        id,
        /* storage current_layout = */ vk::ImageLayout::UNDEFINED,
    );
    let inner = RenderEngineInner::for_tests();
    // Pre-condition: no frame open → reads storage.
    assert_eq!(
        inner.current_layout_for_drawable(&store, id),
        vk::ImageLayout::UNDEFINED,
    );
    let mut inner = inner;
    let ticket = FenceTicket::for_tests_stub();
    inner.frame_builder.open_for_paint(ticket, 1);
    {
        let open = inner.frame_builder.open.as_mut().unwrap();
        open.layouts
            .first_touch_drawable(id, vk::ImageLayout::UNDEFINED);
        open.layouts
            .set_drawable_in_frame(id, vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);
    }
    assert_eq!(
        inner.current_layout_for_drawable(&store, id),
        vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
    );
}
```

If `DrawableStore::with_test_drawable` and `RenderEngineInner::for_tests` don't exist in the exact form, add minimal helpers under `#[cfg(test)]`.

- [ ] **Step 4: Run + verify**

```bash
cargo test -p yserver --lib current_layout_for_drawable_reads_overlay
```

- [ ] **Step 5: Update `commit_close_success` to actually commit layouts** (USER-codex finding 1 — LOAD-BEARING)

The current `commit_close_success` (engine.rs:7037) does `let _ = layouts;` and ignores the layout overlay. That was correct for B.1 because the recorder mutated `storage.current_layout` in place during emit. Under B.2's "overlay as source of truth + storage NOT mutated during recording" model, the success path MUST write the overlay's `current_in_frame_layout` back to storage. Without this, every B.2 frame leaves storage stale (storage = pre-frame layout) while the GPU actually transitioned the image; the next legacy op (or post-B.4 op) reads storage as the barrier old-layout and emits a wrong barrier → corruption or device loss.

```rust
// crates/yserver/src/kms/v2/engine.rs:commit_close_success — REPLACE the
// `let _ = layouts;` line with:

// Commit each touched drawable's in-frame layout back to storage.
// The recorded ops' barriers transitioned the GPU image to this layout;
// storage was deliberately not mutated during recording so failed frames
// can drop the overlay without rolling back. On success, storage MUST
// catch up — otherwise subsequent ops emit barriers from stale layouts.
for (id, entry) in layouts.drawables {
    if let Some(d) = store.get_mut(id) {
        d.storage.current_layout = entry.current_in_frame_layout;
    }
}
// Atlas: same shape. Today's recorder mutates V2GlyphAtlas::current_layout
// in place during composite_glyphs (B.1 path), so the atlas overlay is
// a no-op write in B.1. Under B.2 with the recorder consulting the
// overlay-resolved layout (per `record_render_composite_open_with_old_layout`),
// the atlas overlay carries the in-frame layout and MUST be committed.
if let Some(entry) = layouts.atlas {
    if let Some(atlas) = inner.glyph_atlas.as_mut() {
        atlas.set_current_layout(entry.current_in_frame_layout);
    }
}
```

The signature changes: add `store: &mut DrawableStore` parameter to `commit_close_success`. Update the caller in `close_open_frame` (engine.rs:1096-1102) to pass `store` through.

- [ ] **Step 6: Test commit-on-success writes overlay back to storage**

```rust
#[test]
fn commit_close_success_writes_overlay_into_storage() {
    let mut be = KmsBackendV2::for_tests_with_vk().expect("for_tests_with_vk");
    be.set_frame_builder_enabled_for_tests(true);
    be.set_frame_builder_render_composite_enabled_for_tests(true);
    let dst = be.allocate_test_pixmap_bgra(64, 64);
    let pre_layout = be.drawable_current_layout_for_tests(dst);
    assert_eq!(pre_layout, vk::ImageLayout::UNDEFINED);
    let _ = be.render_composite_for_tests(/* solid into dst */);
    // Frame still open; storage NOT mutated by recording.
    assert_eq!(be.drawable_current_layout_for_tests(dst), vk::ImageLayout::UNDEFINED,
        "storage unchanged during recording");
    be.close_open_frame_for_timeout_for_tests();
    // After close-success, storage caught up to the overlay's in-frame value.
    assert_eq!(be.drawable_current_layout_for_tests(dst), vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        "commit_close_success wrote overlay → storage");
}
```

- [ ] **Step 7: Commit (single commit covering Steps 1-6)**

```bash
git add -u
git commit -m "feat(v2/engine): overlay-then-storage accessor + commit overlay → storage on success (B.2)

Open-frame paint ops need to see the in-frame layout (set by prior
ops in the same frame), not the pre-frame storage layout. The new
helper consults the overlay first and falls back to storage.current_layout
when the drawable isn't touched in-frame.

Critically: commit_close_success now writes the overlay's
current_in_frame_layout back to Drawable::storage.current_layout
(and atlas.current_layout). Without this, B.2 leaves storage stale
on every successful frame — next op's barrier emits from a wrong
old_layout, corrupting/device-losing on the next render."
```

---

### Task 5: Sub-gate env knob `YSERVER_FRAME_BUILDER_RENDER_COMPOSITE`

**Files:**
- Modify: `crates/yserver/src/kms/v2/engine.rs` — add `frame_builder_render_composite_enabled()` runtime check (env-var + test override flag).
- Modify: `crates/yserver/src/kms/v2/backend.rs` — `set_frame_builder_render_composite_enabled_for_tests(bool)` wrapper.

- [ ] **Step 1: Mirror the existing `frame_builder_enabled` machinery**

```bash
grep -n 'frame_builder_enabled\|FRAME_BUILDER_ENABLED' crates/yserver/src/kms/v2/engine.rs | head -10
```

Identify the existing pattern. Likely a `static AtomicBool` initialized from env-var. Mirror it.

- [ ] **Step 2: Implement the sub-gate**

```rust
// crates/yserver/src/kms/v2/engine.rs:
static FRAME_BUILDER_RENDER_COMPOSITE: std::sync::OnceLock<std::sync::atomic::AtomicBool> =
    std::sync::OnceLock::new();

fn frame_builder_render_composite_enabled() -> bool {
    let cell = FRAME_BUILDER_RENDER_COMPOSITE.get_or_init(|| {
        let on = match std::env::var("YSERVER_FRAME_BUILDER_RENDER_COMPOSITE")
            .ok()
            .as_deref()
        {
            Some("on" | "1" | "true" | "yes") => true,
            Some("off" | "0" | "false" | "no") => false,
            _ => false,  // default OFF for B.2 implementation phase
        };
        std::sync::atomic::AtomicBool::new(on)
    });
    cell.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
pub(crate) fn set_frame_builder_render_composite_enabled_for_tests(on: bool) {
    let cell = FRAME_BUILDER_RENDER_COMPOSITE
        .get_or_init(|| std::sync::atomic::AtomicBool::new(false));
    cell.store(on, std::sync::atomic::Ordering::Relaxed);
}
```

- [ ] **Step 3: Add backend test wrapper**

```rust
// crates/yserver/src/kms/v2/backend.rs (next to set_frame_builder_enabled_for_tests):
#[cfg(test)]
pub(crate) fn set_frame_builder_render_composite_enabled_for_tests(&self, on: bool) {
    super::engine::set_frame_builder_render_composite_enabled_for_tests(on);
}
```

- [ ] **Step 4: Verify the sub-gate is OFF by default**

```rust
#[test]
fn frame_builder_render_composite_defaults_off() {
    // OnceLock semantics — call once with no env var influence.
    // (If process-level state poses an issue, use a separate cell
    //  with explicit reset.)
    let on = super::frame_builder_render_composite_enabled();
    assert!(!on, "default OFF expected");
}
```

- [ ] **Step 5: Run + verify + commit**

```bash
cargo test -p yserver --lib frame_builder_render_composite_defaults_off
```

```bash
git add -u
git commit -m "feat(v2/engine): YSERVER_FRAME_BUILDER_RENDER_COMPOSITE sub-gate (B.2)

Separate env knob from B.1's main YSERVER_FRAME_BUILDER. Default OFF
during the B.2 implementation window so the gate-flip is a single
clean commit at Task 20. Mirrors the B.1 sub-gate machinery."
```

---

### Task 6: `RecordedRenderComposite` payload + `RecordedOp::RenderComposite` variant

**Files:**
- Modify: `crates/yserver/src/kms/v2/frame_builder.rs` — add the payload struct + enum variant.
- Test: `crates/yserver/src/kms/v2/frame_builder.rs` — payload size sanity test.

- [ ] **Step 1: Define the payload**

```rust
// crates/yserver/src/kms/v2/frame_builder.rs (near RecordedCompositeGlyphs):

/// Mirror of the inputs to `vk::ops::render::record_render_composite`,
/// resolved into pinnable handles at append time. The op replay reads
/// these fields + the frame's pin vectors via index, NOT by looking
/// the resource up by id at emit-time.
#[derive(Debug)]
pub(crate) struct RecordedRenderComposite {
    pub(crate) op: u8,
    pub(crate) dst_id: DrawableId,
    pub(crate) dst_image: vk::Image,
    pub(crate) dst_view: vk::ImageView,
    pub(crate) dst_extent: vk::Extent2D,
    pub(crate) dst_format: vk::Format,
    pub(crate) dst_has_alpha: bool,
    pub(crate) dst_old_layout: vk::ImageLayout,
    /// Pre-resolved sample view (Drawable / Solid / Gradient src).
    /// View handle's owning resource is kept alive by the frame's
    /// touched_drawables ticket-touch (Drawable) or the engine's
    /// long-lived ownership (Solid/Gradient).
    pub(crate) src_view: vk::ImageView,
    pub(crate) mask_view: vk::ImageView,
    pub(crate) src_alias_view: Option<vk::ImageView>,
    pub(crate) dst_readback_view: Option<vk::ImageView>,
    /// USER-codex R11.F1+F2 — pre-built CompositeAttrs replay-ready.
    /// Eliminates the field-level mismatch between the recorded payload
    /// and what `record_render_composite_draws` expects. Constructed at
    /// op-append time by resolving src_repeat to the bare shader repeat
    /// constant (packing happens inside `record_render_composite_draws`),
    /// src_force_opaque via the legacy pict-format-aware helper, and the
    /// composed `src_xform` (`picture_xform ∘ user_transform`) — same
    /// logic as `_legacy`'s pre-call site.
    pub(crate) attrs: crate::kms::vk::ops::render::CompositeAttrs,
    /// Per-op solid clear inputs (Solid src/mask) — `record_solid_color_clear`
    /// fires at emit-time against the engine's `solid_src_image` /
    /// `solid_mask_image` before the composite draws. None for non-Solid.
    pub(crate) src_clear_color: Option<[f32; 4]>,
    pub(crate) mask_clear_color: Option<[f32; 4]>,
    /// Pipeline cache key inputs (not packed into CompositeAttrs because
    /// the pipeline lookup happens at emit-time via
    /// `RenderPipelineCache::get`).
    pub(crate) mask_component_alpha: bool,
    pub(crate) needs_dst_readback: bool,
    pub(crate) rects: Box<[crate::kms::vk::ops::render::CompositeRect]>,
    pub(crate) clip_rects: Option<Box<[Rectangle16]>>,
    pub(crate) descriptor_set: vk::DescriptorSet,
}
```

`src_class` / `src_swizzle` / `mask_class` / `mask_swizzle` and the `_is_synthetic_1x1` / `_picture_xform` / `_old_layout` fields from the previous draft are gone — they were intermediates of the resolve step that get rolled into `CompositeAttrs` at append time (USER-codex R11.F3 — the `crate::kms::vk::SamplerConfig` / `SwizzleClass` path didn't exist anyway; those types live in `kms::v2::engine` and are descriptor-write inputs, fully consumed by `allocate_descriptor_for_views_into_ring` at Task 11).

`src_old_layout` / `mask_old_layout` defense-in-depth fields likewise removed: `record_render_composite_open_with_old_layout` only emits the dst barrier; src/mask are assumed `SHADER_READ_ONLY_OPTIMAL` (the standard post-write layout). B.3+ ports that transition drawables intermediately will revisit; for B.2, no field is needed.

(View handles are stable for the life of the frame: Drawable views via cache + ticket-touch, Solid/Gradient/scratch views via engine ownership or `submitted.back` pin per Phase 9A.)

**Compile note:** current `CompositeAttrs` (`crates/yserver/src/kms/vk/ops/render.rs:110`) does not derive `Debug`. Because `RecordedRenderComposite` derives `Debug` and stores `attrs: CompositeAttrs`, Task 6 must also add `#[derive(Debug, Clone, Copy)]` to `CompositeAttrs` (its fields already support those derives), or remove/manualize the payload's `Debug` impl. Prefer deriving on `CompositeAttrs` so frame-builder test failures can print the recorded payload.

- [ ] **Step 2: Add the enum variant**

```rust
#[derive(Debug)]
pub(crate) enum RecordedOp {
    CompositeGlyphs(RecordedCompositeGlyphs),
    GlyphUpload(RecordedGlyphUpload),
    RenderComposite(RecordedRenderComposite),  // NEW (B.2)
    #[allow(dead_code, reason = "...")]
    LayoutTransition(RecordedLayoutTransition),
}
```

- [ ] **Step 3: Add a size-budget test**

```rust
#[test]
fn recorded_render_composite_within_512b() {
    let size = std::mem::size_of::<RecordedRenderComposite>();
    assert!(size <= 512, "RecordedRenderComposite is {size} bytes; spec budget 512");
}
```

512 is roomy — the spec § "Op variant sizing" says 256 is the watch threshold; render_composite's payload is genuinely larger (more views, more options). If size > 512, Box internal fields (`clip_rects` already Boxed, consider Boxing `rects` if needed).

- [ ] **Step 4: Run + commit**

```bash
cargo test -p yserver --lib recorded_render_composite_within_512b
```

```bash
git add -u
git commit -m "feat(v2/frame_builder): RecordedRenderComposite payload + enum variant (B.2)

Pre-resolved params for a render_composite op: dst metadata, src/mask
views (pre-cached from drawable_view_cache or scratch), descriptor
set (acquired at append-time), rects, optional dst_readback/alias
views. No emit logic yet; close-walk in Task 9+ wires it."
```

---

### Task 7: (retired — folded into Task 3)

The `acquire_descriptor_set_for_frame_or_op` helper + its invariant + its test land in **Task 3** (single atomic commit per codex round 3 finding 7). Task numbering keeps 7 as a slot for documentation continuity.

---

### Task 8: `render_composite_via_frame_builder` skeleton (no-Vk-call body)

**Files:**
- Modify: `crates/yserver/src/kms/v2/engine.rs` — add the function, gated path, dispatch.

- [ ] **Step 1: Add the function signature + dispatch**

```rust
// crates/yserver/src/kms/v2/engine.rs (next to render_composite):
impl RenderEngine {
    pub(crate) fn render_composite(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        op: u8,
        src: ResolvedSource,
        mask: ResolvedSource,
        dst_id: DrawableId,
        rects: &[CompositeRect],
        clip_rects: Option<&[Rectangle16]>,
        src_repeat: Repeat,
        mask_repeat: Repeat,
        src_transform: Option<PictTransform>,
        mask_transform: Option<PictTransform>,
        mask_component_alpha: bool,
        src_pict_format: u32,
        mask_pict_format: u32,
        dst_pict_format: u32,
    ) -> Result<CompositeStats, RenderError> {
        // Phase B Invariant M2: keep the close while OTHER non-ported
        // paint ops still exist; render_composite removes this from
        // its own entry at Task 16.
        if !frame_builder_render_composite_enabled() {
            return self.render_composite_legacy(
                store, platform, op, src, mask, dst_id, rects,
                clip_rects, src_repeat, mask_repeat, src_transform,
                mask_transform, mask_component_alpha, src_pict_format,
                mask_pict_format, dst_pict_format,
            );
        }
        self.render_composite_via_frame_builder(
            store, platform, op, src, mask, dst_id, rects,
            clip_rects, src_repeat, mask_repeat, src_transform,
            mask_transform, mask_component_alpha, src_pict_format,
            mask_pict_format, dst_pict_format,
        )
    }

    fn render_composite_legacy(
        // ... same signature, body = today's render_composite verbatim
        //     INCLUDING the M2 `self.close_open_frame_for_non_ported_op(store, platform)?;`
        //     call at the top. The legacy body still runs unconditionally
        //     under sub-gate=OFF, so the M2 close stays correct for it.
        //     The new dispatch wrapper does NOT call M2 close — the
        //     sub-gate decision happens first; under sub-gate=ON the
        //     via_frame_builder body IS the frame builder (no close needed).
        //     This per USER-codex finding 5: M2 close moves into _legacy
        //     only; the dispatch wrapper has no close.
    ) -> Result<CompositeStats, RenderError> {
        // Move the existing body of render_composite (engine.rs:5150-5740)
        // here unchanged, including the existing
        //   self.close_open_frame_for_non_ported_op(store, platform)?;
        // at the top of the legacy body (engine.rs:5186).
    }

    fn render_composite_via_frame_builder(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        op: u8,
        src: ResolvedSource,
        mask: ResolvedSource,
        dst_id: DrawableId,
        rects: &[CompositeRect],
        clip_rects: Option<&[Rectangle16]>,
        src_repeat: Repeat,
        mask_repeat: Repeat,
        src_transform: Option<PictTransform>,
        mask_transform: Option<PictTransform>,
        mask_component_alpha: bool,
        src_pict_format: u32,
        mask_pict_format: u32,
        dst_pict_format: u32,
    ) -> Result<CompositeStats, RenderError> {
        // STUB — Task 9+ fills in.
        let _ = (
            store, platform, op, src, mask, dst_id, rects,
            clip_rects, src_repeat, mask_repeat, src_transform,
            mask_transform, mask_component_alpha, src_pict_format,
            mask_pict_format, dst_pict_format,
        );
        Ok(CompositeStats::default())
    }
}
```

- [ ] **Step 2: Build, ensure tests pass with default-OFF sub-gate**

```bash
cargo build -p yserver
cargo test -p yserver --lib
```

Expected: clean. All existing render_composite tests still run the `_legacy` body via dispatch.

- [ ] **Step 3: Commit**

```bash
git add -u
git commit -m "refactor(v2/engine): extract render_composite into _legacy + _via_frame_builder dispatch (B.2)

Mirror the composite_glyphs structure from B.1. The _via_frame_builder
body is a stub returning an empty CompositeStats; default-OFF sub-gate
keeps every existing test path on _legacy. Subsequent tasks (9-13)
fill in the via_frame_builder body."
```

---

### Task 9: `render_composite_via_frame_builder` — prelude → scratch peek → open + touch

**Files:**
- Modify: `crates/yserver/src/kms/v2/engine.rs` — fill in the first half of `_via_frame_builder`.
- Test: `crates/yserver/tests/v2_acceptance.rs` — integration test stub that flips the sub-gate.

**Per USER-codex R9 finding 2 + R10 finding 1:** the close-on-grow rule (Pitfall 4) must fire BEFORE the current op opens the frame, AND the grow itself must happen BEFORE the new frame opens — otherwise the retired Box from the grow would attach to the NEW frame's pin set (helper case (a)) instead of the just-closed frame's fence. If the new frame later aborts, rollback would release Vk handles still in flight from the just-closed frame's CB.

Restructure into TWO phases:

- **Phase 9A: scratch peek → close-if-needed → grow + adopt → THEN open frame.** Resolve dst metadata. Compute `needs_dst_readback` and `self_alias_used`. Peek each needed scratch's fit. If any growth would fire AND a frame is open with prior ops, close the frame. **Then immediately call `ensure_returning_old` + `adopt_retired_resource_for_gpu_retirement` for each needed scratch BEFORE opening a new frame.** Because no open frame exists at this point, the helper's case (a) is skipped and the retired Box rides the just-closed frame's `SubmittedOp` (helper case (b) — submitted.back).
- **Phase 9B: open frame + touch dst/src/mask.** Scratch slots are now sized correctly; no further grow will fire in Task 10's view query.

- [ ] **Step 1: Implement the restructured prelude**

```rust
fn render_composite_via_frame_builder(
    &mut self,
    store: &mut DrawableStore,
    platform: &mut PlatformBackend,
    op: u8,
    src: ResolvedSource,
    mask: ResolvedSource,
    dst_id: DrawableId,
    rects: &[CompositeRect],
    clip_rects: Option<&[Rectangle16]>,
    src_repeat: Repeat,
    mask_repeat: Repeat,
    src_transform: Option<PictTransform>,
    mask_transform: Option<PictTransform>,
    mask_component_alpha: bool,
    src_pict_format: u32,
    mask_pict_format: u32,
    dst_pict_format: u32,
) -> Result<CompositeStats, RenderError> {
    let mut stats = CompositeStats::default();
    if rects.is_empty() {
        return Ok(stats);
    }

    // (0) Flush pre-existing cow/render batches.
    self.flush_cow_batch(store, platform)?;
    self.flush_render_batch(store, platform)?;

    // (1) Lazy-init RENDER assets.
    self.ensure_render_assets(platform)?;

    // (2) PHASE 9A — scratch peek + close-on-grow. No state mutation yet.
    //     Resolve dst metadata.
    let (dst_image, dst_view, dst_extent, dst_format, dst_depth) = {
        let inner = self.inner.as_ref().ok_or(RenderError::NoVk)?;
        if platform.renderer_failed { return Err(RenderError::RendererFailed); }
        let d = store.get(dst_id).ok_or(RenderError::UnknownDrawable(dst_id))?;
        (d.storage.image, d.storage.image_view, d.storage.extent,
         d.storage.format, d.depth)
    };
    if dst_extent.width == 0 || dst_extent.height == 0 { return Ok(stats); }
    if !matches!(dst_format, vk::Format::B8G8R8A8_UNORM | vk::Format::R8_UNORM) {
        return Ok(stats);
    }
    let dst_has_alpha = dst_has_alpha_for_pict_format(dst_format, dst_depth, dst_pict_format);
    let Some(std_op) = StdPictOp::from_u8(op) else { return Ok(stats); };
    let needs_dst_readback = std_op.needs_dst_readback();
    let self_alias_used = matches!(src, ResolvedSource::Drawable(id) if id == dst_id)
        || matches!(mask, ResolvedSource::Drawable(id) if id == dst_id);

    // (2a) Peek growth. The scratch kinds that may need to grow for this op:
    //      - dst_readback if needs_dst_readback
    //      - src_alias_readback if self_alias_used
    // Both grow to (dst_format, dst_extent.width, dst_extent.height).
    let need_grow_dst_rb = needs_dst_readback && {
        let inner = self.inner.as_ref().expect("inner");
        inner.dst_readback.as_ref()
            .map(|rb| !rb.fits(dst_format, dst_extent.width, dst_extent.height))
            .unwrap_or(true)
    };
    let need_grow_alias = self_alias_used && {
        let inner = self.inner.as_ref().expect("inner");
        inner.src_alias_readback.as_ref()
            .map(|rb| !rb.fits(dst_format, dst_extent.width, dst_extent.height))
            .unwrap_or(true)
    };

    // (2b) If growth would fire AND a frame is open with prior ops,
    //      close before touching anything for the current op.
    if (need_grow_dst_rb || need_grow_alias) && {
        let inner = self.inner.as_ref().expect("inner");
        inner.frame_builder.open.as_ref().is_some_and(|o| !o.ops.is_empty())
    } {
        self.close_open_frame(store, platform,
            super::frame_builder::CloseReason::ScratchGrow)?;
    }

    // (2c) CRITICAL: grow + adopt BEFORE opening the new frame
    //      (USER-codex R10.F1). If we grew AFTER opening, the helper's
    //      case (a) would attach the retired Box to the NEW frame's pin
    //      set — a new-frame abort would then release Vk handles while
    //      the just-closed CB is still sampling them. With no open frame,
    //      the helper falls through to case (b) and rides submitted.back's
    //      fence (which is the just-closed frame's SubmittedOp).
    if need_grow_dst_rb {
        let retired = {
            let inner = self.inner.as_mut().expect("inner");
            inner.dst_readback.as_mut().expect("ensured")
                .ensure_returning_old(dst_format, dst_extent.width, dst_extent.height)
                .map_err(|e| {
                    log::warn!("dst_readback ensure: {e:?}");
                    RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
                })?
        };
        let inner = self.inner.as_mut().expect("inner");
        inner.adopt_retired_resource_for_gpu_retirement(retired);
    }
    if need_grow_alias {
        let retired = {
            let inner = self.inner.as_mut().expect("inner");
            inner.src_alias_readback.as_mut().expect("ensured")
                .ensure_returning_old(dst_format, dst_extent.width, dst_extent.height)
                .map_err(|e| {
                    log::warn!("src_alias_readback ensure: {e:?}");
                    RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
                })?
        };
        let inner = self.inner.as_mut().expect("inner");
        inner.adopt_retired_resource_for_gpu_retirement(retired);
    }

    // (3) PHASE 9B — open + touch. Scratch slots are now sized
    //     correctly; Task 10's view queries call ensure_returning_old
    //     again but it returns None because the slot already fits
    //     (or because Task 10 just queries the view directly without
    //     a redundant ensure — see Task 10 below).
    let inner = self.inner.as_mut().expect("inner");
    if !inner.frame_builder.is_open() {
        let _ = inner;
        let ticket = platform.submit_group_ticket_or_open()?;
        let inner = self.inner.as_mut().expect("inner");
        inner.acquire_generation = inner.acquire_generation.saturating_add(1);
        let frame_gen = inner.acquire_generation;
        inner.frame_builder.open_for_paint(ticket, frame_gen);
    }
    let inner = self.inner.as_mut().expect("inner");

    // (4) Ticket-touch dst + snapshot prior ticket + first-touch layout.
    let frame_ticket = inner.frame_builder.open.as_ref().expect("open").ticket.clone();
    let prior_dst_ticket = store.get(dst_id).and_then(|d| d.last_render_ticket.clone());
    let dst_pre_frame_layout = store.get(dst_id).map(|d| d.storage.current_layout)
        .unwrap_or(vk::ImageLayout::UNDEFINED);
    {
        let open = inner.frame_builder.open.as_mut().expect("open");
        open.touched.first_touch(dst_id, prior_dst_ticket);
        open.layouts.first_touch_drawable(dst_id, dst_pre_frame_layout);
    }
    store.touch_render_fence(dst_id, frame_ticket.clone());

    // STUB — Tasks 10-13 fill in src/mask resolution, scratch pinning,
    // descriptor acquisition, op record, emit.
    Ok(stats)
}
```

- [ ] **Step 2: Write integration test that exercises the open-then-empty close path**

```rust
// crates/yserver/tests/v2_acceptance.rs:
#[test]
fn v2_frame_builder_render_composite_via_fb_opens_frame() {
    let mut be = KmsBackendV2::for_tests_with_vk().expect("for_tests_with_vk");
    be.set_frame_builder_enabled_for_tests(true);
    be.set_frame_builder_render_composite_enabled_for_tests(true);
    let dst = be.allocate_test_pixmap_bgra(64, 64);
    // Empty rects — function returns early but the open isn't reached.
    let _ = be.render_composite_for_tests(
        /* op = */ 3, ResolvedSource::Solid([0.0, 0.0, 0.0, 1.0]),
        ResolvedSource::None, dst, &[], None,
        Repeat::Pad, Repeat::Pad, None, None, false, 0, 0, 0,
    );
    assert!(
        !be.frame_builder_is_open_for_tests(),
        "empty render_composite should NOT open a frame",
    );
}
```

- [ ] **Step 3: Run + verify + commit**

```bash
cargo test -p yserver --test v2_acceptance v2_frame_builder_render_composite_via_fb_opens_frame
```

```bash
git add -u
git commit -m "feat(v2/engine): render_composite_via_frame_builder — open-frame prelude (B.2)

Mirrors composite_glyphs_via_frame_builder steps 0-4: pre-batch
flush, dst format gating, frame open (bumping acquire_generation
into frame_generation), ticket-touch of dst + first-touch layout
overlay snapshot. STUB body returns Ok(empty stats); subsequent
tasks fill in src/mask resolution, scratch pinning, descriptor
acquisition, op record, emit."
```

---

### Task 10: src + mask resolution mirrored from `_legacy`, with pinning

**Files:**
- Modify: `crates/yserver/src/kms/v2/engine.rs:render_composite_via_frame_builder`.

- [ ] **Step 1: Lift the src/mask resolution from `_legacy` (engine.rs:5278-5430)**

Copy the resolution logic verbatim — Drawable / Solid / Gradient / None branches for both src and mask, including the self-alias scratch routing. The shape stays identical to `_legacy`; only thing different is what we DO with the results:

- For each Drawable src/mask: `store.touch_render_fence(id, frame_ticket.clone())` AND `open.touched.first_touch(id, prior)` AND `open.layouts.first_touch_drawable(id, current_layout)`.
- Solid_src / solid_mask / white_mask are 1×1, don't grow, don't need pinning (per Pitfall 4b). Resolve their views directly.
- dst_readback + src_alias_readback: **growth was already done in Phase 9A.** Here we just query the views. `ensure_returning_old` is NOT called; the engine slot is guaranteed to fit `dst_format × dst_extent` because Phase 9A peeked and grew if needed. If `ensure_returning_old` were called and returned `Some(retired)`, that would indicate a bug in Phase 9A's peek (USER-codex R11.F4). Debug-assert against this:

```rust
// (5) Resolve solid scratch views directly — no growth, no pin needed.
let solid_src_view = inner.solid_src_image.as_ref().expect("ensured").image_view();
let solid_mask_view = inner.solid_mask_image.as_ref().expect("ensured").image_view();
let white_mask_view = inner.white_mask_image.as_ref().expect("ensured").image_view();

// (5b) Self-alias readback (src == dst). Phase 9A already grew if needed;
//      just query the view here. view() takes &mut self because it may
//      lazily build the no-alpha variant on first dst_has_alpha=false call
//      against this scratch instance.
let src_alias_view = if self_alias_used {
    debug_assert!(
        inner.src_alias_readback.as_ref().is_some_and(
            |rb| rb.fits(dst_format, dst_extent.width, dst_extent.height)
        ),
        "Phase 9A failed to grow src_alias_readback to required size",
    );
    inner.src_alias_readback.as_mut().expect("ensured")
        .view(dst_format, dst_has_alpha)
        .map_err(RenderError::Vk)?
} else {
    None
};

// (6) dst_readback when needs_dst_readback — Phase 9A already grew.
let dst_readback_view = if needs_dst_readback {
    debug_assert!(
        inner.dst_readback.as_ref().is_some_and(
            |rb| rb.fits(dst_format, dst_extent.width, dst_extent.height)
        ),
        "Phase 9A failed to grow dst_readback to required size",
    );
    inner.dst_readback.as_mut().expect("ensured")
        .view(dst_format, dst_has_alpha)
        .map_err(RenderError::Vk)?
} else {
    None
};
```

Borrowck note: `view()` takes `&mut self` (`dst_readback.rs:156`), so `as_mut()` is required. No `adopt_retired_resource_for_gpu_retirement` calls in Task 10 — those happen in Phase 9A only.

The recorded op's `dst_readback_view: Option<vk::ImageView>` field holds the **post-grow view** which is stable for the rest of the frame: Phase 9A's grow-before-open + Task 9's debug-assert guarantees no later op triggers a mid-frame grow.

NO Arc wrapping. The `&mut DstReadback` access during `record_copy_from` at emit-time (Task 12) targets the engine's CURRENT scratch slot, which IS the same instance this op recorded against (Phase 9A's close-then-grow-before-open guarantee).

- [ ] **Step 2: Resolve src + mask views**

Lift verbatim from `_legacy` (engine.rs:5336-5436). Same outcome: `(src_view, src_extent, src_clear_color, src_is_synthetic_1x1, src_picture_xform)` and the mask analogs as LOCAL bindings — these feed Task 11 Step 3's `CompositeAttrs` build at op-append (not stored on the recorded payload).

For each `ResolvedSource::Drawable(id)` other than dst, also touch:

```rust
let prior = store.get(id).and_then(|d| d.last_render_ticket.clone());
let pre_layout = store.get(id).map(|d| d.storage.current_layout)
    .unwrap_or(vk::ImageLayout::UNDEFINED);
{
    let open = inner.frame_builder.open.as_mut().expect("open");
    open.touched.first_touch(id, prior);
    open.layouts.first_touch_drawable(id, pre_layout);
}
store.touch_render_fence(id, frame_ticket.clone());
```

**Gradient lifetime note** (codex R3 finding 9). `ResolvedSource::Gradient(xid)` resolves through `inner.picture_paint.get(&xid)` and returns a view into an `EngineInner`-owned `PicturePaintState::Gradient(GradientLut)`. The LUT is owned by the engine for the picture's lifetime; it is NOT independently retireable, NOT mutated after build, and NOT exposed via Arc / refcounted ownership. **No ticket-touch or pin is needed for gradient sources** — the view handle remains valid for the entire frame open window because (a) the engine owns the LUT, (b) no concurrent path can free a Picture while a render_composite is in flight (Picture deletion routes through the engine's `picture_paint_remove` which can only run between paint ops). If B.3+ introduces growable gradient LUTs or retireable gradients, this invariant must be revisited; the spec calls it out in § "Frame-wide resource pinning" as something the plan SHOULD make explicit.

**Solid src/mask lifetime note.** `ResolvedSource::Solid(color)` resolves to the engine's `solid_src_image` / `solid_mask_image` (1×1 BGRA8 scratch). These never grow and never need pinning (per Pitfall 4b). Read their views directly; the engine's Drop destroys them at shutdown after all frames have closed.

- [ ] **Step 3: Build + run tests**

```bash
cargo build -p yserver
cargo test -p yserver --lib
```

All existing tests stay green (sub-gate OFF). The new code is compiled but unreached.

- [ ] **Step 4: Commit**

```bash
git add -u
git commit -m "feat(v2/engine): render_composite_via_frame_builder — src/mask resolution + scratch pinning (B.2)

Lifts the src/mask resolution from render_composite_legacy verbatim,
adding (a) ticket-touch + first-touch overlay snapshot for Drawable
src/mask, (b) per-op peek-grow + close-reopen rule for dst_readback /
src_alias_readback when a frame is open with prior ops, (c)
adopt_retired_resource_for_gpu_retirement routes returned
Box<dyn BatchResource> to the correct fence owner: open frame,
newest SubmittedOp, or explicit immediate release only when no work
is in flight.

Solid src/mask/white scratch (1×1, fixed size) needs no growth path
and no pin. Gradient LUTs are engine-owned and CPU-immutable —
no ticket-touch needed."
```

---

### Task 11: `render_composite_via_frame_builder` — descriptor acquisition + `RecordedRenderComposite` append

**Files:**
- Modify: `crates/yserver/src/kms/v2/engine.rs:render_composite_via_frame_builder`.

- [ ] **Step 1: Acquire + write descriptor via existing helper (Mechanism 2 generation)**

(USER-codex R6 finding 3.) The existing `RenderPipeline::allocate_descriptor_for_views_into_ring` (`crates/yserver/src/kms/vk/render_pipeline.rs:482`) acquires AND writes the descriptor in one call. B.2 reuses it, passing the open frame's generation:

```rust
// (7) Acquire + write descriptor for (src_view, mask_view, dst_view).
//     The existing helper:
//       allocate_descriptor_for_views_into_ring(ring, generation, src, mask, dst)
//         -> Result<vk::DescriptorSet, vk::Result>
//     internally calls ring.acquire_set(layout, generation) + write_views_into_descriptor_set.
//     B.2 passes open.frame_generation as the generation arg.
let descriptor_set = {
    // RenderPipelineCache::get takes &mut self and returns
    // Result<vk::Pipeline, RenderPipelineError>. See render_pipeline.rs:416.
    let pipeline_handle = inner.render_pipelines.as_mut().expect("ensured")
        .get(std_op, dst_format, dst_has_alpha, mask_component_alpha)
        .map_err(|e| {
            log::warn!("render_pipelines.get: {e:?}");
            RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
        })?;
    // Note: `get` returns the raw vk::Pipeline. We then need a RenderPipeline
    // reference (the bigger wrapper around pipeline + layout + sampler +
    // descriptor_set_layout) to call `allocate_descriptor_for_views_into_ring`.
    // Audit at write time: the current API exposes
    // `RenderPipelineCache` for the pipeline cache AND a top-level
    // RenderPipeline object owned by EngineInner separately. Whichever
    // owns `allocate_descriptor_for_views_into_ring` is what we borrow.
    let generation = inner.frame_builder.open.as_ref().expect("open").frame_generation;
    let src_for_descriptor = src_alias_view.unwrap_or(src_view);
    let mask_for_descriptor = mask_view;
    let dst_for_descriptor = dst_readback_view.unwrap_or(dst_view);
    inner.render_pipelines.as_ref().expect("ensured")
        .allocate_descriptor_for_views_into_ring(
            &mut inner.descriptor_pool_ring,
            generation,
            src_for_descriptor,
            mask_for_descriptor,
            dst_for_descriptor,
        ).map_err(RenderError::Vk)?
    // Use `pipeline_handle` later when recording draws (Task 12).
};
```

Borrowck note: `as_mut()` for the `.get()` call and `as_ref()` for the `allocate_descriptor_for_views_into_ring` call both borrow the same field; sequence them in distinct `let` bindings so the borrows don't overlap. `&mut inner.descriptor_pool_ring` is a disjoint borrow on a sibling field — fine to take while `inner.render_pipelines` is also borrowed (Rust borrowck splits across fields).

**No new `acquire_descriptor_set_for_frame_or_op` helper is needed** — Task 3 still adds it for use cases (e.g. B.3 ports) that don't go through `RenderPipeline`, but Task 11's render_composite path uses the existing pipeline helper directly. Update Task 3's commit message to note this: the helper is added for future-port use, not for render_composite.

- [ ] **Step 2: Resolve dst/src/mask old-layouts via the overlay accessor BEFORE appending**

Per Pitfall 5+6 and codex R3 findings 4 + 5:

```rust
// `current_layout_for_drawable` consults open frame's overlay first,
// falls back to storage. Pre-resolves the dst barrier old-layout for
// `record_render_composite_open_with_old_layout`. src/mask drawables
// are assumed SHADER_READ_ONLY_OPTIMAL — see Pitfall 5+6 + R11.F1 note
// below. No src_old_layout / mask_old_layout fields on the payload.
let dst_old_layout = inner.current_layout_for_drawable(store, dst_id);
```

- [ ] **Step 3: Build a replay-ready `CompositeAttrs` from the resolved inputs**

USER-codex R11.F1+F2/R12.F2: the recorded payload carries a fully-built `CompositeAttrs` matching the existing `record_render_composite_draws` API (`crates/yserver/src/kms/vk/ops/render.rs:110-134`). Do NOT reconstruct this from pseudo-code. Lift the exact legacy construction at `crates/yserver/src/kms/v2/engine.rs:5517-5571` into a helper and call that helper from both `_legacy` and `render_composite_via_frame_builder`.

```rust
fn build_render_composite_attrs(
    store: &DrawableStore,
    src: &ResolvedSource,
    mask: &ResolvedSource,
    src_pict_format: u32,
    mask_pict_format: u32,
    src_extent: vk::Extent2D,
    mask_extent: vk::Extent2D,
    src_repeat: Repeat,
    mask_repeat: Repeat,
    src_is_synthetic_1x1: bool,
    mask_is_synthetic_1x1: bool,
    src_picture_xform: Option<vk_render::AffineXform>,
    mask_picture_xform: Option<vk_render::AffineXform>,
    src_transform: Option<&PictTransform>,
    mask_transform: Option<&PictTransform>,
) -> vk_render::CompositeAttrs {
    // Synthetic 1x1 scratches use PAD so one texel covers the whole rect.
    // Otherwise pass the bare shader repeat constant. Do NOT call
    // pack_repeat_mode here: record_render_composite_draws packs repeat +
    // force_opaque into push constants at emit time.
    let effective_src_repeat = if src_is_synthetic_1x1 {
        crate::kms::vk::render_pipeline::REPEAT_PAD
    } else {
        crate::kms::backend::repeat_to_shader_const(src_repeat)
    };
    let effective_mask_repeat = if mask_is_synthetic_1x1 {
        crate::kms::vk::render_pipeline::REPEAT_PAD
    } else {
        crate::kms::backend::repeat_to_shader_const(mask_repeat)
    };

    let user_src_xform =
        crate::kms::backend::pixman_transform_to_affine(src_transform, src_extent);
    let user_mask_xform =
        crate::kms::backend::pixman_transform_to_affine(mask_transform, mask_extent);
    let combined_src_xform = match src_picture_xform {
        Some(intrinsic) => crate::kms::backend::compose_affines(intrinsic, user_src_xform),
        None => user_src_xform,
    };
    let combined_mask_xform = match mask_picture_xform {
        Some(intrinsic) => crate::kms::backend::compose_affines(intrinsic, user_mask_xform),
        None => user_mask_xform,
    };

    let src_force_opaque = resolve_force_opaque_pict_format(store, src, src_pict_format);
    let mask_force_opaque = resolve_force_opaque_pict_format(store, mask, mask_pict_format);

    vk_render::CompositeAttrs {
        src_extent,
        mask_extent,
        src_repeat: effective_src_repeat,
        mask_repeat: effective_mask_repeat,
        src_force_opaque,
        mask_force_opaque,
        src_xform: combined_src_xform,
        mask_xform: combined_mask_xform,
    }
}

let attrs = build_render_composite_attrs(/* same resolved inputs legacy uses */);
```

The intermediate primitives (`src_is_synthetic_1x1`, `src_picture_xform`, `src_class`, `src_swizzle`, …) are NOT stored on the payload — they're already consumed by (a) the descriptor writes via `allocate_descriptor_for_views_into_ring` in Step 1, and (b) the `attrs` build above.

- [ ] **Step 4: Append the `RecordedOp::RenderComposite`**

```rust
// (9) Append the recorded op.
let recorded = RecordedRenderComposite {
    op,
    dst_id, dst_image, dst_view, dst_extent, dst_format, dst_has_alpha,
    dst_old_layout,
    src_view, mask_view, src_alias_view, dst_readback_view,
    attrs,
    src_clear_color, mask_clear_color,
    mask_component_alpha,
    needs_dst_readback,
    rects: rects.to_vec().into_boxed_slice(),
    clip_rects: clip_rects.map(|r| r.to_vec().into_boxed_slice()),
    descriptor_set,
};
{
    let open = inner.frame_builder.open.as_mut().expect("open");
    // OVERLAY UPDATE — ONCE per op, to the POST-op layout.
    //
    // record_render_composite_close transitions dst back to
    // SHADER_READ_ONLY_OPTIMAL. The next op-in-frame's append must read
    // that value when resolving its own dst_old_layout. We do NOT
    // write COLOR_ATTACHMENT_OPTIMAL here — that's an intermediate
    // state no observer can see, since the recorded op transitions
    // through it AND back within the same CB.
    //
    // For src/mask drawables: they're sampled read-only; their layout
    // after the op is unchanged from what we used as the recorder's
    // barrier old-layout. Don't write them.
    //
    // Atomicity (codex round 4 finding 3): the op push + overlay write
    // are bundled into push_op_and_set_layouts so no reentrant code
    // path can observe ops.len()=N+1 with overlay still at N's value.
    open.push_op_and_set_layouts(
        RecordedOp::RenderComposite(recorded),
        &[(dst_id, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)],
    );
}

stats.recorded_draws = u32::try_from(rects.len()).unwrap_or(u32::MAX);
store.mark_damage_for_rects(dst_id, rects);

Ok(stats)
```

Codex R3 finding 4 fix is the single `set_drawable_in_frame(dst_id, SHADER_READ_ONLY_OPTIMAL)` write — no intermediate COLOR_ATTACHMENT_OPTIMAL write, no second write at end. Layout state is "what storage would see if the recorded op had executed."

No `src_old_layout` / `mask_old_layout` fields on the payload — `record_render_composite_open_with_old_layout` only emits the dst barrier, and src/mask are assumed `SHADER_READ_ONLY_OPTIMAL` (the post-`record_render_composite_close` layout from any prior op). B.3+ ports that transition drawables intermediately (cow_copy_area, put_image) will revisit; for B.2 this assumption matches `_legacy` and the existing recorder contract.

- [ ] **Step 5: Unit test — second op-in-frame sees SHADER_READ_ONLY_OPTIMAL**

```rust
#[test]
fn render_composite_via_fb_second_op_dst_old_layout_is_shader_read_only() {
    let mut be = KmsBackendV2::for_tests_with_vk().expect("for_tests_with_vk");
    be.set_frame_builder_enabled_for_tests(true);
    be.set_frame_builder_render_composite_enabled_for_tests(true);
    let dst = be.allocate_test_pixmap_bgra(64, 64);
    let _ = be.render_composite_for_tests(/* solid src into dst */);
    let _ = be.render_composite_for_tests(/* solid src into SAME dst */);
    let ops = be.frame_builder_peek_ops_for_tests();
    let RecordedOp::RenderComposite(op1) = &ops[0] else { panic!() };
    let RecordedOp::RenderComposite(op2) = &ops[1] else { panic!() };
    // op 1 reads pre-frame layout (UNDEFINED for fresh pixmap or whatever
    // storage held); op 2 reads SHADER_READ_ONLY_OPTIMAL because op 1's
    // recorded close-transition will leave dst there.
    assert_eq!(op2.dst_old_layout, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        "second op-in-frame reads the post-op layout the recorder will produce");
    // Specifically: NOT COLOR_ATTACHMENT_OPTIMAL — that's an intermediate
    // state never observable across ops.
    assert_ne!(op2.dst_old_layout, vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);
}
```

- [ ] **Step 6: Run + commit**

```bash
cargo test -p yserver --test v2_acceptance render_composite_via_fb_second_op_dst_old_layout_is_shader_read_only
```

```bash
git add -u
git commit -m "feat(v2/engine): render_composite_via_frame_builder — descriptor + RecordedOp append (B.2)

Acquire descriptor set via Mechanism 2 (frame_generation watermark).
Build the RecordedRenderComposite payload from overlay-resolved
old-layouts for dst/src/mask + resolved views + descriptor + rects.
Overlay update at append-end is ONE write per op, to the post-op
SHADER_READ_ONLY_OPTIMAL layout that the recorded op's close
transition will leave dst at."
```

---

### Task 12: `emit_recorded_render_composite_into_cb` — close-time replay

**Files:**
- Modify: `crates/yserver/src/kms/v2/engine.rs:emit_recorded_op_into_cb` — extend with the RenderComposite arm.
- Create: `crates/yserver/src/kms/v2/engine.rs:emit_recorded_render_composite_into_cb` private helper.

- [ ] **Step 1: Add the arm**

```rust
// In emit_recorded_op_into_cb, extend the match:
match op {
    RecordedOp::CompositeGlyphs(p) => emit_recorded_composite_glyphs_into_cb(inner, store, cb, pins, p),
    RecordedOp::GlyphUpload(p) => emit_recorded_glyph_upload_into_cb(inner, store, cb, pins, p),
    RecordedOp::RenderComposite(p) => emit_recorded_render_composite_into_cb(inner, store, cb, pins, p),
    RecordedOp::LayoutTransition(_) => Ok(()),  // unused in B.1/B.2
}
```

- [ ] **Step 2: Audit the existing `record_render_composite` barrier contract**

(Codex round 3 finding 5.) `crates/yserver/src/kms/vk/ops/render.rs:144-172` defines `record_render_composite` as `open + draws + close`:
- `record_render_composite_open` (line 187-212): emits a `to_color` pipeline barrier `dst.current_layout() → COLOR_ATTACHMENT_OPTIMAL`, then `cmd_begin_rendering` + viewport + pipeline.
- `record_render_composite_close` (line 358-383): emits a `to_read` barrier `COLOR_ATTACHMENT_OPTIMAL → SHADER_READ_ONLY_OPTIMAL`, then calls `dst.set_current_layout(SHADER_READ_ONLY_OPTIMAL)`.
- **src/mask barriers: NOT emitted by record_render_composite.** Today's `_legacy` callers expect src/mask drawables to be ALREADY in `SHADER_READ_ONLY_OPTIMAL` (their natural read layout post-write). Under B.2 deferred recording, this remains true so long as the prior op-in-frame's close-transition left them in SHADER_READ_ONLY_OPTIMAL, AND no path opens any drawable in another layout. The plan inherits that assumption.

**Decision:** B.2 introduces a new overload `record_render_composite_open_with_old_layout(vk, cb, dst, pipeline, old_layout: vk::ImageLayout) -> Result<(), vk::Result>` that takes `old_layout` explicitly instead of reading `dst.current_layout()`. `record_render_composite_close` stays unchanged (it always emits the same to_read transition). `record_render_composite_draws` stays unchanged.

- [ ] **Step 3: Add the overload in `crates/yserver/src/kms/vk/ops/render.rs`**

```rust
/// Same as `record_render_composite_open` but takes `old_layout`
/// explicitly. Used by the frame builder so the overlay's
/// `current_in_frame_layout` (resolved at op-append time) drives
/// the barrier instead of `dst.current_layout()` (which reflects
/// `storage.current_layout` and is stale during deferred recording).
pub fn record_render_composite_open_with_old_layout<T: CompositeTarget + ?Sized>(
    vk: &VkContext,
    cb: vk::CommandBuffer,
    dst: &T,                              // shared ref — we don't mutate
    pipeline: vk::Pipeline,
    old_layout: vk::ImageLayout,
) -> Result<(), vk::Result> {
    // Body verbatim from record_render_composite_open EXCEPT:
    //   let old_layout = dst.current_layout();  // ← remove this line
    // and use the parameter instead. Skip dst.storage.current_layout
    // mutation if any (today's open emits the barrier but does not
    // mutate storage — confirm at write time).
    ...
}
```

(If today's `record_render_composite_open` mutates `dst.set_current_layout` after the barrier, the overload must NOT mutate it — under B.2 deferred recording, storage is only mutated on `commit_close_success`. Audit at implementation time.)

- [ ] **Step 4: Implement `emit_recorded_render_composite_into_cb`**

```rust
fn emit_recorded_render_composite_into_cb(
    inner: &mut RenderEngineInner,
    store: &mut DrawableStore,
    cb: vk::CommandBuffer,
    _pins: &FramePinSet,
    p: &RecordedRenderComposite,
) -> Result<(), RenderError> {
    // (1) Synthetic 1×1 source/mask clears, if any. record_solid_color_clear
    //     writes the texel via cmd_clear_color_image; happens BEFORE
    //     record_render_composite_open begins rendering, so no barrier
    //     interleaving with the to_color transition.
    if let Some(c) = p.src_clear_color {
        record_solid_color_clear(/* &inner.vk, cb, solid_src_image_view, c */)?;
    }
    if let Some(c) = p.mask_clear_color {
        record_solid_color_clear(/* &inner.vk, cb, solid_mask_image_view, c */)?;
    }

    // (2) Self-alias copy (Stage 3c.3) — dst → src_alias_readback scratch.
    //     Pre-rendering, so it's done before the to_color transition.
    //     Today this is `record_copy_from(... SHADER_READ_ONLY_OPTIMAL)`
    //     in the legacy path; preserve verbatim.
    if let Some(alias_view) = p.src_alias_view {
        // record_copy_from(...) — see _legacy for the exact shape.
    }

    // (3) Pipeline-and-barrier open. Pass the PRE-RESOLVED dst_old_layout
    //     from the recorded op — this is what makes deferred recording
    //     correct: subsequent ops in the same frame saw their own
    //     dst_old_layout as SHADER_READ_ONLY_OPTIMAL (per Task 11 overlay
    //     write), so each op's barrier transitions from there to
    //     COLOR_ATTACHMENT_OPTIMAL cleanly.
    let pipeline = inner.render_pipelines.as_mut().expect("ensured")
        .get(StdPictOp::from_u8(p.op).expect("validated at append"),
             p.dst_format, p.dst_has_alpha, p.mask_component_alpha)
        .map_err(|e| {
            log::warn!("emit: render_pipelines.get: {e:?}");
            RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
        })?;
    // Build a no-storage CompositeTarget impl over the recorded payload's
    // dst_view + dst_extent. The open overload does not read current_layout
    // because we pass old_layout explicitly; close still takes &mut T and
    // calls set_current_layout, which is a no-op on this adapter.
    let mut target = RecordedCompositeTarget {
        image: p.dst_image,
        view: p.dst_view,
        extent: p.dst_extent,
    };
    crate::kms::vk::ops::render::record_render_composite_open_with_old_layout(
        &inner.vk, cb, &target, pipeline, p.dst_old_layout,
    ).map_err(RenderError::Vk)?;

    // (4) Per-rect draws. Same as _legacy.
    // attrs already built at append-time (Task 11 Step 3).
    let pipeline_layout = inner.render_pipelines.as_ref().expect("ensured")
        .pipeline_layout();
    crate::kms::vk::ops::render::record_render_composite_draws(
        &inner.vk, cb, pipeline_layout, p.descriptor_set,
        p.dst_extent, &p.attrs, &p.rects,
        p.clip_rects.as_deref().unwrap_or(&[/* full-extent fallback */]),
    );

    // (5) Close — back to SHADER_READ_ONLY_OPTIMAL. Stays as the existing
    //     record_render_composite_close, which takes &mut T and calls
    //     set_current_layout(SHADER_READ_ONLY_OPTIMAL). The adapter's
    //     setter is intentionally a no-op; storage layout is committed from
    //     the frame overlay on close success.
    crate::kms::vk::ops::render::record_render_composite_close(&inner.vk, cb, &mut target);

    Ok(())
}

// Helper type — a no-storage CompositeTarget that just exposes the
// pre-recorded image/view/extent. Lives in engine.rs (private).
struct RecordedCompositeTarget {
    image: vk::Image,
    view: vk::ImageView,
    extent: vk::Extent2D,
}

impl CompositeTarget for RecordedCompositeTarget {
    fn vk_image(&self) -> vk::Image { self.image }
    fn vk_image_view(&self) -> vk::ImageView { self.view }
    fn extent(&self) -> vk::Extent2D { self.extent }
    fn current_layout(&self) -> vk::ImageLayout {
        // Unused by record_render_composite_open_with_old_layout (we
        // pass old_layout explicitly). Used by record_render_composite_close
        // only if a future refactor asks for it; the to_read barrier is
        // COLOR_ATTACHMENT_OPTIMAL (always; it was set by the prior
        // open). Return that as a constant.
        vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL
    }
    fn set_current_layout(&mut self, _layout: vk::ImageLayout) {
        // NO-OP for deferred recording. The real layout commit happens
        // in commit_close_success which writes the overlay's
        // current_in_frame_layout back to storage. Today's
        // record_render_composite_close calls `dst.set_current_layout(
        // SHADER_READ_ONLY_OPTIMAL)` (render.rs:383); under B.2 that
        // mutation is meaningless on the adapter and storage is
        // unchanged. Codex R5 audit catch.
    }
}
```

**Audit checklist for Task 12 implementer** (codex R5 ground-truth):

- `record_render_composite_close` (today) calls `dst.set_current_layout(SHADER_READ_ONLY_OPTIMAL)` — confirmed at `crates/yserver/src/kms/vk/ops/render.rs:383`. `RecordedCompositeTarget::set_current_layout` is a no-op per above.
- `record_render_composite_open` (today) reads `dst.current_layout()` for the to_color barrier — confirmed at `crates/yserver/src/kms/vk/ops/render.rs:195`. It does NOT call `dst.set_current_layout`; the explicit-layout overload should preserve that no-mutation property.
- `DescriptorPoolRing::release_up_to` (today) only transitions `InFlight → Free` via `vkResetDescriptorPool` — confirmed (descriptor_pool_ring.rs:107-137).
- `ensure_active_with_capacity` (today) only promotes `Free → Active`; rotates exhausted `Active → InFlight`; never `InFlight → Active` — confirmed (descriptor_pool_ring.rs:158-176).
- `poll_retired` (today) calls `release_up_to(op.generation)` per retired SubmittedOp; walks both `submitted` AND `pending_frames` — confirmed (engine.rs:730-755). The frame's descriptor pool retirement rides on the SubmittedOp that the frame parks in `submitted` (via `close_open_frame`'s `pending_group_ops.push(SubmittedOp { generation: frame_generation, ... })` followed by flush, which drains pending_group_ops → submitted).

This avoids both pitfalls:
- No double-barrier (no manual barrier emission; `record_render_composite_open_with_old_layout` emits exactly one).
- Storage is NOT mutated during recording (the RecordedCompositeTarget is a thin pre-resolved adapter; `current_layout()` is constant and doesn't lie about storage state).

- [ ] **Step 5: Integration test — two render_composite calls produce ONE submit**

```rust
#[test]
fn v2_frame_builder_render_composite_collapses_two_in_one_frame() {
    let mut be = KmsBackendV2::for_tests_with_vk().expect("for_tests_with_vk");
    be.set_frame_builder_enabled_for_tests(true);
    be.set_frame_builder_render_composite_enabled_for_tests(true);
    let dst = be.allocate_test_pixmap_bgra(128, 128);
    let pre = be.platform_queue_submit2_count_for_tests();
    let _ = be.render_composite_for_tests(/* solid src into dst, 1 rect */);
    let _ = be.render_composite_for_tests(/* solid src into SAME dst, 1 rect */);
    // No close trigger yet — frame stays open across both calls.
    be.close_open_frame_for_timeout_for_tests();  // helper that forces a close
    let post = be.platform_queue_submit2_count_for_tests();
    assert_eq!(post - pre, 1, "two render_composite in one frame → ONE vkQueueSubmit2");
}
```

(`close_open_frame_for_timeout_for_tests` is a small helper that calls `engine.close_open_frame_if_timed_out` with `timeout_ms=0` or similar to force a close.)

- [ ] **Step 6: Run + commit**

```bash
cargo test -p yserver --test v2_acceptance v2_frame_builder_render_composite_collapses_two_in_one_frame
```

```bash
git add -u
git commit -m "feat(v2/engine): emit_recorded_render_composite_into_cb (B.2 close-time replay)

Close-walk's emit pass replays each RecordedOp::RenderComposite into
the frame CB: dst barrier (from overlay's dst_old_layout), synthetic
clears, self-alias copy, record_render_composite, dst back-transition.
Integration test confirms two render_composite in one frame collapse
into one vkQueueSubmit2."
```

---

### Task 13: M2 wiring — verify render_fill_rectangles delegation + audit remaining non-ported sites

**Per USER-codex finding 5:** Task 8 already placed the M2 close inside `_legacy` only (not the dispatch wrapper), so the via-frame-builder path collapses two render_composite calls into one frame without the M2 close interfering. Task 13 now only needs to (a) audit that `render_fill_rectangles`'s wrapper-level M2 close is also removed (it delegates to render_composite, which is no longer M2-gated), and (b) verify the remaining 8 non-ported entry points STILL flush.

**Files:**
- Modify: `crates/yserver/src/kms/v2/engine.rs:render_fill_rectangles` — remove the M2 close from the wrapper if present at engine.rs:5754 (`close_open_frame_for_non_ported_op` at the top).

- [ ] **Step 1: Audit current render_fill_rectangles wrapper**

```bash
grep -n -A2 'fn render_fill_rectangles' crates/yserver/src/kms/v2/engine.rs | head -15
```

If the wrapper today does `self.close_open_frame_for_non_ported_op(store, platform)?;` at entry, REMOVE it. The wrapper just delegates to `render_composite` (which under Task 8's split now handles M2 internally in the legacy branch). Two `render_fill_rectangles` calls under sub-gate=ON must stay in one frame; the wrapper-level M2 close would break that.

- [ ] **Step 2: Verify all 8 remaining non-ported entry points STILL flush**

```bash
grep -n 'close_open_frame_for_non_ported_op' crates/yserver/src/kms/v2/engine.rs
```

Expected sites: `fill_rect`, `fill_rect_batch`, `logic_fill`, `copy_area`, `cow_copy_area`, `put_image`, `image_text`, `render_traps_or_tris` — plus the call inside `render_composite_legacy` (Task 8's move). Total: 9 sites. Confirm none accidentally removed.

- [ ] **Step 3: Build + test**

```bash
cargo build -p yserver
cargo test -p yserver --lib
cargo test -p yserver --test v2_acceptance
```

All green. Task 12's "two render_composite collapse to one submit" test now passes (the wrapper has no M2 close to interfere).

- [ ] **Step 4: Commit**

```bash
git add -u
git commit -m "feat(v2/engine): drop M2 close from render_fill_rectangles wrapper (B.2)

render_fill_rectangles delegates to render_composite. Under sub-gate=ON,
the render_composite dispatch routes to via_frame_builder which does
NOT do an M2 close (correctly — it IS the frame builder). Under
sub-gate=OFF, render_composite_legacy still does the M2 close at its
top (per Task 8). The wrapper at render_fill_rectangles must NOT do
its own M2 close, otherwise two render_fill_rectangles in one frame
would close the frame between them, defeating the collapse.

Remaining M2 sites (8 non-ported entry points + legacy render_composite)
unchanged. The remaining 8 entry points (fill_rect/_batch/logic_fill,
copy_area, cow_copy_area, put_image, image_text, render_traps_or_tris)
keep their M2 close until B.3 ports them."
```

---

### Task 14: Telemetry — `frame_builder_renders_per_frame_*`

**Files:**
- Modify: `crates/yserver/src/kms/v2/telemetry.rs` — add render-per-frame counters plus the `scratch_grow` close-reason bucket.
- Modify: `crates/yserver/src/kms/v2/engine.rs:close_open_frame` — count RenderComposite ops in the frame and feed telemetry.

- [ ] **Step 1: Locate the existing glyph_uploads_per_frame telemetry**

```bash
grep -n 'glyph_uploads_per_frame' crates/yserver/src/kms/v2/telemetry.rs
```

Mirror its `Total + _max_in_window + (avg from total/closes)` shape.

- [ ] **Step 2: Add `renders_in_frame` to `FrameCloseEvent`**

(USER-codex finding 4 — telemetry is event-based, not direct `inner.telemetry` writes.) The engine queues a `FrameCloseEvent` per close; the backend's `drain_frame_builder_telemetry` (`backend.rs:1618`) pops the queue and updates per-second telemetry. To add `renders_per_frame`, extend the event:

```rust
// crates/yserver/src/kms/v2/frame_builder.rs — extend FrameCloseEvent:
#[derive(Debug)]
pub(crate) struct FrameCloseEvent {
    pub(crate) reason: CloseReason,
    pub(crate) ops_in_frame: usize,
    pub(crate) glyph_uploads_in_frame: u32,
    pub(crate) renders_in_frame: u32,   // NEW (B.2)
    pub(crate) pin_count: usize,
    pub(crate) aborted: bool,
}
```

- [ ] **Step 3: Populate `renders_in_frame` in `close_open_frame`**

```rust
// crates/yserver/src/kms/v2/engine.rs:close_open_frame — when constructing
// each FrameCloseEvent (success + error paths), include the count:
let renders_in_frame: u32 = open_frame.ops.iter()
    .filter(|op| matches!(op, RecordedOp::RenderComposite(_)))
    .count()
    .try_into()
    .unwrap_or(u32::MAX);

inner.pending_frame_close_events.push(super::frame_builder::FrameCloseEvent {
    reason,
    ops_in_frame: open_frame.ops.len(),
    glyph_uploads_in_frame: open_frame.glyph_uploads_in_frame,
    renders_in_frame,   // NEW
    pin_count: open_frame.pins.len(),
    aborted: false, // or true for error branches
});
```

There are 4 push sites in `close_open_frame` (one per error path + the success path); all need the new field. Failure paths can also count from `open_frame.ops` before drop.

- [ ] **Step 4: Add the counters to `V2Telemetry` / `Bucket`**

```rust
// crates/yserver/src/kms/v2/telemetry.rs (mirror the glyph_uploads_per_frame_* fields):
pub(crate) frame_builder_close_reason_scratch_grow: u64,
pub(crate) frame_builder_renders_per_frame_total: u64,
pub(crate) frame_builder_renders_per_frame_max_in_window: u32,
```

Add these to both the per-second bucket and lifetime structs if the current telemetry shape keeps them separate. `frame_builder_close_reason_scratch_grow` must sit next to the existing `scene_compose`, `non_ported`, `legacy_sc`, `present_completion`, `sync_wait`, `timeout`, `shutdown`, and `pin_ceiling` counters so the close-reason totals stay exhaustive after adding `CloseReason::ScratchGrow`.

- [ ] **Step 5: Update `record_frame_builder_close` to accept renders and count ScratchGrow**

```rust
// crates/yserver/src/kms/v2/telemetry.rs:
pub(crate) fn record_frame_builder_close(
    &mut self,
    reason: super::frame_builder::CloseReason,
    ops_in_frame: usize,
    glyph_uploads_in_frame: u32,
    renders_in_frame: u32,
) {
    // ... existing ops/glyph accounting ...
    let renders = u64::from(renders_in_frame);
    self.bucket.frame_builder_renders_per_frame_total =
        self.bucket.frame_builder_renders_per_frame_total.saturating_add(renders);
    self.lifetime.frame_builder_renders_per_frame_total =
        self.lifetime.frame_builder_renders_per_frame_total.saturating_add(renders);
    self.bucket.frame_builder_renders_per_frame_max_in_window =
        self.bucket.frame_builder_renders_per_frame_max_in_window.max(renders_in_frame);
    self.lifetime.frame_builder_renders_per_frame_max_in_window =
        self.lifetime.frame_builder_renders_per_frame_max_in_window.max(renders_in_frame);

    let (b, l) = match reason {
        // ... existing variants ...
        R::ScratchGrow => (
            &mut self.bucket.frame_builder_close_reason_scratch_grow,
            &mut self.lifetime.frame_builder_close_reason_scratch_grow,
        ),
    };
    *b += 1;
    *l += 1;
}
```

- [ ] **Step 6: Update `drain_frame_builder_telemetry` in `backend.rs` to pass the event field**

```rust
// crates/yserver/src/kms/v2/backend.rs (find drain_frame_builder_telemetry,
// around backend.rs:1618):
for event in events {
    if event.aborted {
        self.telemetry.record_frame_builder_abort();
    } else {
        self.telemetry.record_frame_builder_close(
            event.reason,
            event.ops_in_frame,
            event.glyph_uploads_in_frame,
            event.renders_in_frame,
        );
    }
}
```

- [ ] **Step 7: Update the `v2_telemetry:` log line**

In `telemetry.rs`'s `maybe_emit`, include both:

- `renders/frame_avg=X.Y max=Z` alongside the existing `glyph_uploads/frame_*`.
- `scratch_grow={}` inside `close_reasons[...]`, so the log remains exhaustive:

```text
close_reasons[scene_compose=... non_ported=... legacy_sc=...
present_completion=... sync_wait=... timeout=... shutdown=...
pin_ceiling=... scratch_grow=...]
```

- [ ] **Step 8: Unit test**

```rust
#[test]
fn v2_frame_builder_renders_per_frame_telemetry_records_max() {
    let mut be = KmsBackendV2::for_tests_with_vk().expect("for_tests_with_vk");
    be.set_frame_builder_enabled_for_tests(true);
    be.set_frame_builder_render_composite_enabled_for_tests(true);
    let dst = be.allocate_test_pixmap_bgra(64, 64);
    let _ = be.render_composite_for_tests(/* ... */);
    let _ = be.render_composite_for_tests(/* ... */);
    let _ = be.render_composite_for_tests(/* ... */);
    be.close_open_frame_for_timeout_for_tests();
    let snap = be.telemetry_lifetime_snapshot_for_tests();
    assert!(snap.frame_builder_renders_per_frame_max_in_window >= 3);
}
```

- [ ] **Step 9: Close-reason smoke test for `ScratchGrow`**

Force the telemetry recorder to count a `ScratchGrow` close so the enum, the bucket, and the log line stay in lockstep going forward:

```rust
#[test]
fn v2_telemetry_record_frame_builder_close_counts_scratch_grow() {
    let mut t = V2Telemetry::default();
    t.record_frame_builder_close(
        crate::kms::v2::frame_builder::CloseReason::ScratchGrow,
        /* ops_in_frame = */ 0,
        /* glyph_uploads_in_frame = */ 0,
        /* renders_in_frame = */ 0,
    );
    assert_eq!(t.lifetime.frame_builder_close_reason_scratch_grow, 1);
    assert_eq!(t.bucket.frame_builder_close_reason_scratch_grow, 1);
}
```

If `V2Telemetry::default()` isn't directly constructible, mirror the test-helper shape used by the existing close-reason counter tests (see B.1's telemetry tests).

- [ ] **Step 9: Commit**

```bash
git add -u
git commit -m "feat(v2/telemetry): render-composite frame telemetry + scratch-grow close reason (B.2)

Mirror of glyph_uploads_per_frame telemetry but for RenderComposite
ops. Surfaces in the v2_telemetry: log line as
renders/frame_avg=X.Y max=Z. Also extends close_reasons[...] with
scratch_grow so CloseReason::ScratchGrow is counted instead of silently
disappearing from the close-reason breakdown.

Drained by drain_frame_builder_telemetry at every close-driving site
(no new sites needed; existing wiring covers render_composite via the
maybe_composite tick + the wrapper in backend.rs)."
```

---

### Task 15: `KmsBackendV2::render_composite` + `render_fill_rectangles` drain telemetry

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs` — at the wrappers around `engine.render_composite` and `engine.render_fill_rectangles`, call `drain_frame_builder_telemetry()` after the engine call.

- [ ] **Step 1: Locate the wrappers**

```bash
grep -n 'fn render_composite\|fn render_fill_rectangles' crates/yserver/src/kms/v2/backend.rs
```

- [ ] **Step 2: Add the drain call**

```rust
fn render_composite(/* ... */) -> Result<CompositeStats, RenderError> {
    let stats = self.engine.render_composite(/* ... */)?;
    self.drain_frame_builder_telemetry();
    Ok(stats)
}

fn render_fill_rectangles(/* ... */) -> Result<CompositeStats, RenderError> {
    let stats = self.engine.render_fill_rectangles(/* ... */)?;
    self.drain_frame_builder_telemetry();
    Ok(stats)
}
```

- [ ] **Step 3: Commit**

```bash
git add -u
git commit -m "feat(v2/backend): drain_frame_builder_telemetry after render_composite + render_fill (B.2)

Matches the B.1 pattern for composite_glyphs. The drain pushes the
in-flight close-event window into per-second lifetime counters so the
telemetry emit picks them up without stale lag."
```

---

### Task 16: Integration test — mixed sequence collapses

**Files:**
- Modify: `crates/yserver/tests/v2_acceptance.rs` — `v2_frame_builder_mixed_render_and_glyphs_one_submit`.

- [ ] **Step 1: Write the test**

```rust
#[test]
fn v2_frame_builder_mixed_render_and_glyphs_one_submit() {
    let mut be = KmsBackendV2::for_tests_with_vk().expect("for_tests_with_vk");
    be.set_frame_builder_enabled_for_tests(true);
    be.set_frame_builder_render_composite_enabled_for_tests(true);
    let dst = be.allocate_test_pixmap_bgra(256, 256);
    let pre = be.platform_queue_submit2_count_for_tests();

    // First: 3 render_composite calls into dst.
    for _ in 0..3 {
        let _ = be.render_composite_for_tests(/* solid src into dst */);
    }
    // Then: composite_glyphs into the SAME dst.
    let glyphs = be.synth_4_glyphs();
    let _ = be.composite_glyphs_for_tests(dst, [0.0; 4], &glyphs, None);
    // Two more render_composites.
    for _ in 0..2 {
        let _ = be.render_composite_for_tests(/* ... */);
    }

    be.close_open_frame_for_timeout_for_tests();
    let post = be.platform_queue_submit2_count_for_tests();
    assert_eq!(post - pre, 1,
        "mixed render_composite + composite_glyphs in one frame → ONE vkQueueSubmit2");
}
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p yserver --test v2_acceptance v2_frame_builder_mixed_render_and_glyphs_one_submit
```

```bash
git add -u
git commit -m "test(v2/acceptance): mixed render_composite + composite_glyphs collapse (B.2)

Verifies the load-bearing B.2 property: a realistic MATE-drag-like
sequence (RENDER paints interleaved with text) issues exactly ONE
vkQueueSubmit2 per frame instead of N submits per op."
```

---

### Task 17: Integration test — render_fill_rectangles routes through frame builder

**Files:**
- Modify: `crates/yserver/tests/v2_acceptance.rs`.

- [ ] **Step 1: Write the test**

```rust
#[test]
fn v2_frame_builder_render_fill_rectangles_via_frame_builder() {
    let mut be = KmsBackendV2::for_tests_with_vk().expect("for_tests_with_vk");
    be.set_frame_builder_enabled_for_tests(true);
    be.set_frame_builder_render_composite_enabled_for_tests(true);
    let dst = be.allocate_test_pixmap_bgra(64, 64);
    let pre = be.platform_queue_submit2_count_for_tests();

    let _ = be.render_fill_rectangles_for_tests(/* PictOp::Over, [1,0,0,1], dst, 3 rects */);
    let _ = be.render_fill_rectangles_for_tests(/* PictOp::Over, [0,1,0,1], dst, 2 rects */);

    be.close_open_frame_for_timeout_for_tests();
    let post = be.platform_queue_submit2_count_for_tests();
    assert_eq!(post - pre, 1,
        "two render_fill_rectangles in one frame collapse via render_composite delegate");
}
```

(`render_fill_rectangles_for_tests` exists on `KmsBackendV2` per the B.1 test-helper inventory; if not, add it under `#[cfg(test)]` as a thin shim around `engine.render_fill_rectangles`.)

- [ ] **Step 2: Run + commit**

```bash
cargo test -p yserver --test v2_acceptance v2_frame_builder_render_fill_rectangles_via_frame_builder
```

```bash
git add -u
git commit -m "test(v2/acceptance): render_fill_rectangles routes via frame builder (B.2)

render_fill_rectangles delegates to render_composite with ResolvedSource::Solid;
B.2's port automatically captures it. Test asserts the collapse."
```

---

### Task 18: Renderer-failed integration test — submit failure rolls back overlays

**Files:**
- Modify: `crates/yserver/tests/v2_acceptance.rs`.

- [ ] **Step 1: Write the test**

```rust
#[test]
fn v2_frame_builder_render_composite_renderer_failed_on_submit_failure() {
    let mut be = KmsBackendV2::for_tests_with_vk().expect("for_tests_with_vk");
    be.set_frame_builder_enabled_for_tests(true);
    be.set_frame_builder_render_composite_enabled_for_tests(true);
    let dst = be.allocate_test_pixmap_bgra(64, 64);
    let pre_layout = be.drawable_current_layout_for_tests(dst);

    be.inject_next_submit_failure_for_tests();  // forces next vkQueueSubmit2 to fail
    let _ = be.render_composite_for_tests(/* solid into dst */);
    be.close_open_frame_for_timeout_for_tests();

    assert!(be.renderer_failed_for_tests(), "submit failure → renderer_failed = true");
    assert_eq!(be.drawable_current_layout_for_tests(dst), pre_layout,
        "rollback restores pre-frame layout");
}
```

(`inject_next_submit_failure_for_tests` exists in the test platform; if not, add via the SubmitGroup's test hook.)

- [ ] **Step 2: Run + commit**

```bash
cargo test -p yserver --test v2_acceptance v2_frame_builder_render_composite_renderer_failed_on_submit_failure
```

```bash
git add -u
git commit -m "test(v2/acceptance): render_composite renderer_failed rollback (B.2)

Injected submit failure must (a) set renderer_failed, (b) restore the
drawable's pre-frame current_layout (overlay rollback), (c) drop the
pin set without leaking scratch Arcs. Asserts the structural rollback
discipline that B.1 already proved out for composite_glyphs."
```

---

### Task 19: `cargo +nightly fmt` + `cargo clippy --workspace --all-targets` (B.2 surface)

**Codex R3 finding 10 + AGENTS.md "it's fine not to use clippy pedantic in this repo but DO use regular clippy".**

**Files:** all touched files.

- [ ] **Step 1: Run nightly fmt**

```bash
cargo +nightly fmt --all
```

- [ ] **Step 2: Run plain clippy** (NOT pedantic)

```bash
cargo clippy --workspace --all-targets 2>&1 | head -200
```

Acceptable suppressions:
- `#[allow(clippy::too_many_arguments)]` on `render_composite_via_frame_builder` (inherits from `_legacy`; engine.rs has the same `#[allow]` on the legacy function).

Fix every regular clippy warning that the B.2 surface introduced. Pre-existing pedantic warnings outside B.2 stay alone.

- [ ] **Step 3: Run full test suite**

```bash
cargo test --workspace
```

All green.

- [ ] **Step 4: Commit**

```bash
git add -u
git commit -m "chore(v2/frame_builder): cargo fmt + clippy clean — Phase B.2 surface"
```

---

### Task 20: Flip `YSERVER_FRAME_BUILDER_RENDER_COMPOSITE` default ON — the bee smoothness fix

**Files:**
- Modify: `crates/yserver/src/kms/v2/engine.rs:frame_builder_render_composite_enabled` — change default branch from `false` to `true`.

- [ ] **Step 1: Flip the default**

```rust
fn frame_builder_render_composite_enabled() -> bool {
    let cell = FRAME_BUILDER_RENDER_COMPOSITE.get_or_init(|| {
        let on = match std::env::var("YSERVER_FRAME_BUILDER_RENDER_COMPOSITE")
            .ok()
            .as_deref()
        {
            Some("on" | "1" | "true" | "yes") => true,
            Some("off" | "0" | "false" | "no") => false,
            _ => true,  // ← was `false` — flipping to ON
        };
        std::sync::atomic::AtomicBool::new(on)
    });
    cell.load(std::sync::atomic::Ordering::Relaxed)
}
```

- [ ] **Step 2: Update the test that asserted default OFF**

The `frame_builder_render_composite_defaults_off` test added in Task 5 must invert OR be removed — the gate is now default ON.

```rust
#[test]
fn frame_builder_render_composite_defaults_on() {
    let on = super::frame_builder_render_composite_enabled();
    assert!(on, "default ON expected after Task 20");
}
```

- [ ] **Step 3: Run all tests one more time**

```bash
cargo test --workspace
```

Green.

- [ ] **Step 4: Commit — SINGLE-LINE FLIP, BISECT-CLEAN**

```bash
git add -u
git commit -m "feat(v2/engine): flip YSERVER_FRAME_BUILDER_RENDER_COMPOSITE default ON (B.2)

This is the commit that activates the bee smoothness fix: render_composite
and render_fill_rectangles now route through the FrameBuilder by default.
Three back-to-back render_composite calls in one frame collapse into a
single vkQueueSubmit2. Combined with B.1's composite_glyphs port,
B.2 absorbs ~75 % of bee MATE-drag submits.

Kill-switch: YSERVER_FRAME_BUILDER_RENDER_COMPOSITE=off (or 0/false/no)."
```

---

### Task 21: Status doc update + bee hardware-smoke gate placeholder

**Files:**
- Modify: `docs/status.md` — add Phase B.2 entry alongside the B.1 entry.

- [ ] **Step 1: Append the status entry**

```markdown
- **Phase B sub-phase B.2 — IMPLEMENTED 2026-05-2X.** Plan
  `docs/superpowers/plans/2026-05-24-frame-builder-phase-b2.md`
  landed in 20 commits on `feature/frame-builder-submit-rate`
  (`<sha1>` … `<sha20>`). Spec
  `docs/superpowers/specs/2026-05-24-frame-builder-phase-b-design.md`.

  **Structural changes:**
    - Mechanism 3 (retired-scratch pinning via existing BatchResource
      trait): `EngineInner` scratch slots stay as `Option<DstReadback>` /
      `Option<MaskScratch>` / `Option<SolidColorImage>` (no Arc-wrap;
      the existing `&mut`-mutating record APIs stay unchanged). The
      existing `ensure_returning_old` already returns
      `Option<Box<dyn BatchResource>>` on growth; B.2 routes the Box
      into the open frame's `retired_resources: Vec<Box<dyn BatchResource>>`
      pin slot instead of dropping it on the floor. The Box's Drop
      destroys the Vk handles after the frame ticket signals.
      Closes the existing retired-scratch leak documented at
      `engine.rs:529-535`.
    - Mechanism 2 (descriptor pool ring watermark): `OpenFrame::frame_generation`
      captured at `open_for_paint` from a bumped `acquire_generation`;
      all per-op descriptor acquisitions during the frame use that
      generation; `release_up_to(frame_generation)` at retire releases
      only the frame's pools.
    - Layout overlay flips to source-of-truth: open-frame paint ops
      read `current_in_frame_for_drawable` via the new
      `RenderEngineInner::current_layout_for_drawable` accessor.
      Second `render_composite` op-in-frame sees the prior op's
      post-transition layout, not stale `storage.current_layout`.
    - `RecordedOp::RenderComposite` + `RecordedRenderComposite`
      payload — all resolved view handles, descriptor set, rects,
      clip.
    - `render_composite_via_frame_builder` + `render_composite_legacy`
      dispatch behind `YSERVER_FRAME_BUILDER_RENDER_COMPOSITE`
      sub-gate. Default ON after Task 20.
    - `emit_recorded_render_composite_into_cb` close-time replay.
    - M2 `close_open_frame_for_non_ported_op` removed from
      `render_composite` and `render_fill_rectangles` entry points
      (they ARE the frame builder now). M2 still wraps the remaining
      8 non-ported entry points until B.3.
    - Telemetry: `frame_builder_renders_per_frame_total` /
      `_max_in_window`, drained at every close-driving backend site.

  **Acceptance:**
    - **Implementation gates** (already validated): `cargo build` clean;
      `cargo test --workspace` green; `cargo +nightly fmt` + plain
      `cargo clippy --workspace --all-targets` clean on the B.2
      surface (AGENTS.md: no pedantic by default).
    - **Hardware gates** (user-driven, pending):
      - **bee MATE-load smoothness** — boot MATE with default
        `YSERVER_FRAME_BUILDER_RENDER_COMPOSITE=on`, drag for 30 s,
        expect `queue_submit2/s` peak < 1000/s on bee (post-Phase-A
        baseline was 2300/s; spec target by end of B.4 is 200-400/s,
        B.2 alone should hit 600-1000/s). User-side smoothness
        observation: 1-second initial-drag hitch should ease (warm
        cache loop now amortises across the frame).
      - **bee MATE-load survival** — same telemetry harness;
        `frame_builder_aborts/s = 0` throughout (no
        new failure mode under load).
      - **yoga / iMac / fuji regression check** — same drag, no new
        `ERROR_DEVICE_LOST`, no fault chains. Expected to IMPROVE
        on these platforms vs B.1 (cap=1 reverts to per-op submit;
        B.2 collapses RENDER → one submit per frame).
      - **silence dual-output regression check** — same drag,
        confirm both outputs paint correctly under render_composite
        → frame builder.

  **Open follow-ups:**
    - Q1 (op variant sizing) — measured at B.2 close; if
      `RecordedRenderComposite` size > 512 B, Task X (Box the
      `rects` field).
    - Q3 (gate retirement) — env knob stays as kill-switch through
      B.5.
    - DescriptorPoolRing Mechanism 2 watermark wire-through —
      validated via the existing `release_up_to(op.generation)` call
      site (engine.rs:744 in `poll_retired`); B.2 doesn't add new
      retire sites.
```

- [ ] **Step 2: Commit**

```bash
git add docs/status.md docs/superpowers/plans/2026-05-24-frame-builder-phase-b2.md
git commit -m "docs(status): Phase B.2 implementation + acceptance gate documented"
```

---

## Self-review checklist (run after Task 21; before opening the PR)

- [ ] Spec § "Op representation": RecordedRenderComposite payload covers all `_legacy` params (Task 6).
- [ ] Spec § "Drawable lifetime": ticket-touch + first-touch overlay on dst + src/mask Drawable refs (Tasks 9, 10).
- [ ] Spec § "Glyph upload": unchanged from B.1; no atlas reads in render_composite path.
- [ ] Spec § "Transactional layout state": overlay is source-of-truth during open frame (Task 4 helper + Task 11 cross-op-in-frame test).
- [ ] Spec § "Multi-output topology": out of scope for B.2 (compose joins frame at B.4).
- [ ] Spec § "Per-output partial-failure handling": deferred to B.4.
- [ ] Spec § "Frame close triggers": no new triggers in B.2; the 8 inherited from B.1 still apply.
- [ ] Spec § "Frame-wide resource pinning": Mechanism 1 (staging, B.1) ✓; Mechanism 2 (descriptor watermark, Task 3) ✓; Mechanism 3 (retired-scratch BatchResource pinning, Tasks 1-2) ✓.
- [ ] Spec § "CB recording at close": Pass 2 record + emit_recorded_render_composite_into_cb covered (Task 12).
- [ ] Spec § "Migration boundaries": M1 unchanged; M2 narrowed to 8 entry points (Task 13); M3 unchanged.
- [ ] Spec § "Error handling and rollback": every error path drops the local OpenFrame; pre-flush errors free the CB; post-flush errors rely on abort_flush; layout/ticket/atlas rollback via existing helpers (no change).
- [ ] Spec § "Telemetry": renders_per_frame_* added (Task 14).
- [ ] Spec § "Acceptance tests" integration: composite_glyphs_one_submit (B.1) + render_composite_collapses_two (Task 12) + mixed_render_and_glyphs (Task 16) + render_fill_rectangles_via_frame_builder (Task 17) + renderer_failed (Task 18). All gated via local `set_frame_builder_render_composite_enabled_for_tests(true)`.
- [ ] Spec § "Acceptance tests" hardware: bee MATE-drag smoothness is the headline criterion for Task 20's commit; Task 21 documents the gate.
- [ ] Spec § "Open questions": Q1 size budget tested in Task 6; Q3 sub-gate naming kept distinct; Q5 unchanged from B.1.

## Test-helper inventory (additions on top of B.1)

| Helper | First task | Purpose |
|---|---|---|
| `set_frame_builder_render_composite_enabled_for_tests(b)` | 5 | Toggle the B.2 sub-gate locally. |
| `render_composite_for_tests(args)` | 9 | Thin shim around `self.engine.render_composite(...)`. |
| `render_fill_rectangles_for_tests(args)` | 17 | Thin shim around `self.engine.render_fill_rectangles(...)`. |
| `close_open_frame_for_timeout_for_tests()` | 12 | Forces close via timeout path. Uses `engine.close_open_frame(... CloseReason::Timeout)`. |
| `frame_builder_peek_ops_for_tests()` | 11 | Read-only slice of `open.ops` for assertions. |
| `inject_next_submit_failure_for_tests()` | 18 | Marks the next `vkQueueSubmit2` to fail (`ERROR_OUT_OF_DEVICE_MEMORY`). Reuses any existing SubmitGroup hook. |
| `DstReadback::for_tests()` | 2 | `#[cfg(test)]` stub constructor (null handles). Mirrors `FenceTicket::for_tests_stub`. |
| `SolidColorImage::for_tests()` | 2 | Same shape stub. |
| `MaskScratch::for_tests()` | 2 | Same shape stub. |
| `DescriptorPoolRing::high_water_generation_for_tests()` | 7 | Read active pool's high_water_generation. |
| `RenderEngineInner::for_tests_with_pool_ring()` | 7 | No-Vk constructor + a real DescriptorPoolRing. |

## Codex review history

**Round 13 (2026-05-25, Claude fresh-eyes review after codex plan patch):** 2 BLOCKING + 4 MEDIUM cleanup findings, all addressed:

- **C-R13.F1 BLOCKING/compile (`CloseReason::ScratchGrow` referenced but never added)** — Phase 9A and Pitfall 4 referenced a new variant, but no task extended `CloseReason` or the existing exhaustive close-reason test. **Resolved:** Task 1 Step 1 now adds `CloseReason::ScratchGrow` and renames/updates the exhaustive test from eight B.1 variants to nine B.2 variants before any Phase 9A code references it.
- **C-R13.F2 BLOCKING/telemetry (`scratch_grow` close reason uncounted)** — Task 14 added render-per-frame counters but did not extend close-reason buckets/logging for the new variant. **Resolved:** Task 14 now adds `frame_builder_close_reason_scratch_grow`, extends `record_frame_builder_close`, passes `renders_in_frame` through the event drain, and includes `scratch_grow={}` in `close_reasons[...]`.
- **C-R13.F3 MEDIUM (duplicate step numbers in Tasks 11/12)** — Task 11 and Task 12 had repeated Step 3/4 labels after earlier edits. **Resolved:** renumbered Task 11 to Steps 4/5/6 and Task 12 to Steps 3/4/5/6.
- **C-R13.F4 MEDIUM (stale helper name)** — Task 1 Files still said `inner.adopt_retired_resource(boxed)`. **Resolved:** changed to `inner.adopt_retired_resource_for_gpu_retirement(retired)`.
- **C-R13.F5 MEDIUM (stale helper case enumeration)** — Task 1 Step 6 still said "case (c) or (d)" after the helper was simplified to cases (a)/(b)/(c). **Resolved:** now names case (b) for newest `SubmittedOp` and case (c) for immediate release.
- **C-R13.F6 MEDIUM (Pitfall 4 example contradicted Phase 9A)** — the example implied a newly-opened empty frame branch after grow, while Phase 9A grows before opening. **Resolved:** tightened the comment to the Phase 9A invariant: no frame open, or an open frame with no prior ops.

**Round 12 (2026-05-25, user-driven codex re-pass #7):** 3 BLOCKING + 1 MEDIUM findings, all addressed:

- **U-R12.F1 BLOCKING/compile (`RecordedRenderComposite: Debug` requires `CompositeAttrs: Debug`)** — payload derives `Debug` and stores `attrs: CompositeAttrs`, but current `CompositeAttrs` had no derive. **Resolved:** Task 6 now explicitly requires `#[derive(Debug, Clone, Copy)]` on `CompositeAttrs` (or a manual/removal fallback, with derive preferred).
- **U-R12.F2 BLOCKING/correctness (`CompositeAttrs` construction pseudo-code drifted from legacy)** — the draft packed repeats at append time, used nonexistent helper signatures, and referenced `AffineXform::identity()`. `record_render_composite_draws` already calls `pack_repeat_mode`, so attrs must carry bare shader repeat constants. **Resolved:** Task 11 Step 3 now says to lift `engine.rs:5517-5571` into a shared helper, preserving `repeat_to_shader_const`, `resolve_force_opaque_pict_format(store, &src, pict_format)`, `pixman_transform_to_affine(transform.as_ref(), extent)`, and `compose_affines`.
- **U-R12.F3 BLOCKING/compile (`record_render_composite_close` takes `&mut T`)** — Task 12 passed `&target` and claimed close takes `&T`, but current close calls `set_current_layout`. **Resolved:** sample now uses `let mut target`, passes `&mut target`, and documents `RecordedCompositeTarget::set_current_layout` as a no-op because storage layout is committed from the overlay.
- **U-R12.F4 MEDIUM (stale retirement routing wording)** — architecture + Task 10 commit text still suggested every retired scratch Box goes to `FramePinSet::retired_resources`. **Resolved:** text now describes the actual helper precedence: open frame, newest `SubmittedOp`, immediate explicit release only when no work is in flight.

**Round 11 (2026-05-25, user-driven codex re-pass #6):** 3 BLOCKING + 1 MEDIUM + 1 MINOR findings, all addressed:

- **U-R11.F1+F2 BLOCKING (RecordedRenderComposite payload doesn't match CompositeAttrs)** — current `CompositeAttrs` (`render.rs:110-134`) requires `src_repeat: i32`, `mask_repeat: i32`, `src_force_opaque: bool`, `mask_force_opaque: bool`, `src_xform: AffineXform`, `mask_xform: AffineXform` + the extents. My plan stored intermediate primitives (`src_class`, `src_swizzle`, `src_is_synthetic_1x1`, `src_picture_xform`, …) that don't compose into the real struct, and Task 12's `CompositeAttrs { ... }` construction used field names that don't exist. **Resolved:** payload now carries a **pre-built `attrs: CompositeAttrs`** field; Task 11 Step 3 builds it at append-time by lifting `_legacy`'s exact `CompositeAttrs` construction into a shared helper. Task 12's emit consumes `p.attrs` directly. Intermediate fields removed from the payload.
- **U-R11.F3 BLOCKING/compile (SamplerConfig/SwizzleClass wrong path AND not used at emit)** — plan referenced `crate::kms::vk::SamplerConfig` / `SwizzleClass`; actual types live in `kms::v2::engine`. Also unused at emit time (descriptor is resolved at append). **Resolved:** fields dropped from the payload entirely. SamplerConfig / SwizzleClass are intermediates consumed by `allocate_descriptor_for_views_into_ring` at append; no need to record them.
- **U-R11.F4 MEDIUM (Task 10 still assumed mid-frame grow possible)** — Phase 9A correctly grows BEFORE opening the new frame, but Task 10 still called `ensure_returning_old` again and routed any returned Box back through `adopt_retired_resource_for_gpu_retirement` per the old fence model. **Resolved:** Task 10 now `debug_assert!`s that the engine slot fits and just queries the view — no `ensure_returning_old` call, no adopt call. Any failure of the debug_assert indicates a Phase 9A bug.
- **U-R11.F5 MINOR (Task 1 commit message stale)** — said "no frame open → drops immediately". **Resolved:** commit message rewritten to describe the three-case routing + the explicit-release contract; also references `SubmittedOp.retired_resources` extension + `fits()` API addition.

**Round 10 (2026-05-25, user-driven codex re-pass #5):** 3 BLOCKING + 1 MEDIUM + 1 MINOR findings, all addressed:

- **U-R10.F1 BLOCKING (retired scratch attached to wrong fence after close-on-grow)** — Task 9 closed before touching, but then opened a NEW frame BEFORE Task 10 grew the scratch. Helper case (a) (open frame) won the precedence and attached the retired Box to the NEW frame's pin set. A new-frame abort would then release Vk handles still sampled by the just-closed CB. **Resolved:** Phase 9A now does the grow+adopt BEFORE opening any new frame (moved into Phase 9A step 2c). With no open frame, the helper falls through to case (b) and rides `submitted.back`'s fence — the just-closed frame's SubmittedOp.
- **U-R10.F2 BLOCKING (pending_frames precedence too early)** — helper routed to `pending_frames.back_mut()` before `submitted.back_mut()`. But a pending frame may be older than later submitted per-op work; using it would release before newer in-flight work retires. **Resolved:** dropped the pending_frames case entirely. Helper precedence is now (a) open frame, (b) `submitted.back` (the newest fence owner — covers post-close-on-grow AND legacy fall-through because every closed frame appends a SubmittedOp), (c) immediate release if both empty.
- **U-R10.F3 BLOCKING/compile (SubmittedOp.scratch shape mismatch)** — my plan assumed `SubmittedOp.scratch` was `Option<Box<dyn BatchResource>>`; actual code has `scratch: Option<ScratchImage>` (a concrete RAII type at `engine.rs:218,248`). **Resolved:** Task 1 Step 3 now adds a parallel field `retired_resources: Vec<Box<dyn BatchResource>>` on `SubmittedOp` (leaves `scratch: Option<ScratchImage>` untouched), with `append_retired_scratch` + `drain_retired_scratch` methods. All `SubmittedOp { ... }` initializer sites must add `retired_resources: Vec::new()`.
- **U-R10.F4 MEDIUM (render pipeline API)** — my snippet used `render_pipelines.lookup(...)`; actual API is `RenderPipelineCache::get(&mut self, ...) -> Result<vk::Pipeline, RenderPipelineError>` at `render_pipeline.rs:416`. **Resolved:** Task 11 + Task 12 use `as_mut().get(...).map_err(...)` and map `RenderPipelineError` → `RenderError::Vk(ERROR_INITIALIZATION_FAILED)`.
- **U-R10.F5 MINOR (stale Drop text in invariants)** — invariant section still said "The Box's `Drop` impl destroys the Vk handles when the frame retires." Wrong — `BatchResource::release(self: Box<Self>, &VkContext)` is the explicit teardown path; Drop doesn't free Vk handles. **Resolved:** invariant text rewritten to point at `release(&inner.vk)` at retirement time.

**Round 9 (2026-05-25, user-driven codex re-pass #4):** 3 BLOCKING + 2 MEDIUM findings, all addressed:

- **U-R9.F1 BLOCKING (retired scratch can't release immediately after close_open_frame)** — `close_open_frame` submits but doesn't wait for retirement. The just-closed CB may still reference the about-to-be-retired scratch. My "no frame open → release immediately" branch in Task 1's helper was wrong. **Resolved:** helper renamed to `adopt_retired_resource_for_gpu_retirement` with four cascading owners — (a) open frame, (b) latest `pending_frames` record (post-close-on-grow path), (c) latest `submitted` SubmittedOp (legacy fall-through), (d) immediate release ONLY if both queues are empty. The post-close-on-grow Box now correctly rides the just-closed frame's ticket.
- **U-R9.F2 BLOCKING (Task 9's touches stranded by Task 10's close)** — original sequence opened the frame + ticket-touched dst BEFORE Task 10's scratch growth check fired the close, orphaning those touches and leaving Task 11 expecting an open frame. **Resolved:** Task 9 restructured into Phase 9A (resolve dst metadata → peek scratch growth → close if needed; NO state mutation yet) + Phase 9B (open frame + ticket-touch + ensure scratches). Task 10 inherits the open frame; growth doesn't fire mid-frame.
- **U-R9.F3 BLOCKING/compile (Task 1 helper still dropped without release)** — the helper's no-frame branch still said "drop here." **Resolved:** the no-frame case is now case (d) of the cascade — `boxed.release(&self.vk)` only when both queues are empty. Cases (b)+(c) handle the in-flight retirement.
- **U-R9.F4 MEDIUM (`fits()` doesn't exist)** — Task 10 used `rb.fits(format, w, h)` but the API doesn't exist on DstReadback. **Resolved:** Task 1 Step 1 (new) adds `fits(format, w, h) -> bool` on `DstReadback` and `fits(w, h) -> bool` on `MaskScratch`. Body is a one-line predicate against the stored extent.
- **U-R9.F5 MEDIUM (stale contradictory text)** — file-structure still said "Drop impls destroy retired resources" and mentioned "swap the Arc cleanly". **Resolved:** rewritten to describe explicit `release(&vk)` and the `fits()` predicate.

**Round 8 (2026-05-25, user-driven codex re-pass #3):** 2 BLOCKING + 3 MEDIUM findings, all addressed:

- **U-R8.F1 BLOCKING (BatchResource doesn't release on Drop)** — `BatchResource::release(self: Box<Self>, &VkContext)` is EXPLICIT (`paint_batch.rs:147`). Dropping `Box<dyn BatchResource>` leaks Vk handles. My plan said "Drop impls destroy Vk handles" — false. **Resolved:** Pitfall 4 + Task 1 helper + the poll_retired walk all now explicitly call `boxed.release(&inner.vk)`. The `adopt_retired_resource_for_gpu_retirement` helper releases immediately when no frame is open; the frame-retirement walk drains `retired_resources` and releases each. Close-failure path + `drain_all` + `RenderEngine::shutdown` covered.
- **U-R8.F2 BLOCKING (mid-frame scratch growth → view/copy-target mismatch)** — `record_copy_from(&mut DstReadback)` always writes commands into the engine's CURRENT scratch slot. If op N records `dst_readback_view` against scratch D0, and op N+1 grows the slot to D1 (D0 → pin set), op N's emit-time `record_copy_from` writes into D1 while op N's descriptor samples D0. Bug. **Resolved:** new rule — mid-frame scratch growth is FORBIDDEN. Task 10 now peeks `needs_grow` BEFORE calling `ensure_returning_old`; if growth would fire AND the frame has prior ops, force a close-reopen first (`CloseReason::ScratchGrow`, new variant). After close, ensure can grow freely without violating op N's recorded handle invariant.
- **U-R8.F3 MEDIUM (`view` is `&mut self`)** — `DstReadback::view(format, dst_has_alpha)` at `dst_readback.rs:156` takes `&mut self` (it lazily builds the no-alpha variant). My snippets used `as_ref()` — won't compile. **Resolved:** Task 10 view query uses `as_mut()`; borrowck note updated.
- **U-R8.F4 MEDIUM (solid clear rationale false)** — Pitfall 4b said "solid clears happen only once during ensure_render_assets". Legacy actually clears per op (`engine.rs:5622-5629`). Task 12 already plans per-op clears, so direction is fine, but rationale was wrong. **Resolved:** Pitfall 4b rewritten to explain that `record_solid_color_clear` runs per-op at emit-time, takes `&mut SolidColorImage`, and that the SolidColorImage struct itself never gets replaced (so the engine's `image_view()` handle stays stable across the frame; no pin needed).
- **U-R8.F5 MEDIUM (stale Arc text in load-bearing sections)** — top-of-plan Architecture and Task 10 commit message still said "Arc-wrap", "Arc::clone", "Arc::ptr_eq", "ensure_*_arc". **Resolved:** rewritten to describe the BatchResource + close-reopen model.

**Round 7 (2026-05-24, user-driven codex re-pass #2):** 1 BLOCKING + 2 MEDIUM + 2 MINOR findings, all addressed:

- **U-R7.F1 BLOCKING (Arc-wrap incompatible with mutable scratch APIs)** — `DstReadback::record_copy_from` (`dst_readback.rs:197`), `SolidColorImage` clear (`render_pipeline.rs:706`), and `MaskScratch` resize all take `&mut self`. Arc-wrapping precludes the existing recording APIs. **Resolved:** Pitfall 4 rewritten to use the existing `BatchResource` trait pattern (already in tree at `paint_batch.rs:146`; `dst_readback.rs:57` and `mask_scratch.rs:65` implement it). `ensure_returning_old` already returns `Option<Box<dyn BatchResource>>`; B.2 just routes the returned Box into the open frame's `retired_resources: Vec<Box<dyn BatchResource>>` pin slot instead of dropping it. Engine scratch slots stay as plain `Option<DstReadback>` etc. — no Arc, no interior mutability. Task 1 + Task 2 + Task 10 rewritten end-to-end.
- **U-R7.F2 MEDIUM (Task 4 referenced nonexistent accessor)** — plan said `current_in_frame_for_drawable_or_none` returning `Option`; existing code has `current_layout_for_drawable(id, storage_fallback) -> vk::ImageLayout` at `frame_builder.rs:679`. **Resolved:** Task 4 rewritten to reuse the existing accessor + thread `storage_fallback` through the engine-level wrapper.
- **U-R7.F3 MEDIUM (descriptor plan doesn't match pipeline API)** — `render_pipelines.descriptor_set_layout()` doesn't exist; existing helper is `RenderPipeline::allocate_descriptor_for_views_into_ring(ring, generation, src, mask, dst)` at `render_pipeline.rs:482`, which acquires + writes in one call. **Resolved:** Task 11 Step 1 rewritten to use the existing helper with `open.frame_generation` as the generation arg. The `acquire_descriptor_set_for_frame_or_op` helper Task 3 added stays for future-port use (B.3+ ops that don't go through `RenderPipeline`).
- **U-R7.F4 MINOR (stale file path)** — plan referenced `crates/yserver/src/kms/vk/solid_color.rs`; actual location is `crates/yserver/src/kms/vk/render_pipeline.rs:576`. **Resolved:** file-structure note updated.
- **U-R7.F5 MINOR (test snippets ignored fallible `for_tests_with_vk`)** — snippets had `let mut be = KmsBackendV2::for_tests_with_vk();` (missing `?` or `.expect()`). **Resolved:** replace-all added `.expect("for_tests_with_vk")` to all 9 sites.

**Round 6 (2026-05-24, user-driven codex re-pass):** the user invoked codex directly with full repo access; found 5 issues (2 LOAD-BEARING + 3 MEDIUM) my agent-driven invocations missed:

- **U-R6.F1 LOAD-BEARING: `commit_close_success` doesn't commit layouts (`engine.rs:7037` has `let _ = layouts;`).** Under B.2's "storage NOT mutated during recording", every successful frame leaves storage = pre-frame value; next legacy op emits a barrier from the wrong old_layout → corruption / device loss. **Fixed:** Task 4 Step 5 adds the commit path (walk `layouts.drawables`, write `current_in_frame_layout` into `storage.current_layout`; same for atlas). Step 6 adds a regression test.
- **U-R6.F2 LOAD-BEARING: Task 10 still had obsolete `Arc::make_mut` / `strong_count` code blocks.** Pitfall 4 rejected these but Task 10's body never got the corresponding rewrite. **Fixed:** Task 10 Steps now use the unified `ensure_*_arc` helper from Task 1; no Arc::make_mut, no strong_count, no `DstReadback::new_growing_from`.
- **U-R6.F3 MEDIUM: wrong module paths.** `super::dst_readback::DstReadback` should be `crate::kms::vk::dst_readback::DstReadback`; `crate::kms::vk::solid_color::SolidColorImage` should be `crate::kms::vk::render_pipeline::SolidColorImage`. **Fixed:** replace-all done across Task 2 + Task 10.
- **U-R6.F4 MEDIUM: Task 14 telemetry used nonexistent `inner.telemetry`.** Telemetry is event-based: engine pushes `FrameCloseEvent`, backend drains via `drain_frame_builder_telemetry`. **Fixed:** Task 14 rewritten — adds `renders_in_frame: u32` to `FrameCloseEvent`, populates in `close_open_frame` (4 push sites), accumulates in `drain_frame_builder_telemetry`, updates the `v2_telemetry:` log line.
- **U-R6.F5 MEDIUM: M2 close sequencing (Task 13 vs Task 12 expectation).** Task 12 expects two render_composite calls to stay in one frame, but the original Task 8 placed the M2 close in the dispatch WRAPPER (which would fire for both legacy + via_frame_builder). **Fixed:** Task 8 now places the M2 close inside `render_composite_legacy` ONLY; the dispatch wrapper does no close. Task 13 narrowed to "drop wrapper-level M2 from render_fill_rectangles + audit remaining sites."
- **Cleanup:** removed duplicate Task 1 commit step (was two commit blocks with conflicting messages); `KmsBackendV2::test_with_vk()` → `KmsBackendV2::for_tests_with_vk()` everywhere.

**Round 5 (2026-05-24, code-ground-truth):** codex read 4 specific code ranges (descriptor_pool_ring.rs state machine + release_up_to + ensure_active_with_capacity; render.rs record_render_composite + _open + _close; engine.rs poll_retired) and confirmed:

- DescriptorPoolRing has NO freelist reuse path. `release_up_to` is the only `InFlight → Free` transition (always via `vkResetDescriptorPool`). `ensure_active_with_capacity` only promotes `Free → Active`, never `InFlight → Active`. R4.F1 invariant matches reality.
- `record_render_composite_close` (render.rs:95) calls `dst.set_current_layout(SHADER_READ_ONLY_OPTIMAL)` — a mutation B.2 must elide via `RecordedCompositeTarget::set_current_layout` no-op. **Added to Task 12 as an audit catch.**
- `poll_retired` calls `release_up_to(op.generation)` per retired `SubmittedOp` and walks both `submitted` + `pending_frames` queues. Frame-builder descriptor pool retirement rides on the frame's SubmittedOp.

**Round 4 (2026-05-24, post-R3-fixes):** 3 load-bearing + 1 nice-to-have findings, all addressed:

- **R4.F1 (descriptor freshness must be ENFORCED in the ring, not assumed)** → resolved. Task 3 / Pitfall 5 of close-path now requires an audit gate before code lands: walk `descriptor_pool_ring.rs` to confirm no freelist reuse, no `InFlight → Active` shortcut, only `release_up_to` performs the reset. Task 3 Step 8 integration test verifies end-to-end.
- **R4.F2 (open-generation retirement contract — verify out-of-order polling)** → resolved. Mechanism 2 invariant section now documents that under B.2's cap=1 + single-open semantics generations ARE submit-time monotonic (frames can't overlap). Test added as a regression guard for future B.4 multi-output.
- **R4.F3 (forbid nested open/close during append)** → resolved. `OpenFrame::push_op_and_set_layouts` helper bundles `ops.push` + `layouts.set_drawable_in_frame` atomically; Task 11 step 3 calls the helper instead of inline writes. Debug-assert in the helper documents that the pair is the only atomic mutation path.
- **R4.F4 nice-to-have (pin retention bound)** → resolved. Pitfall 4 now documents the `G + 1` bound (G = grow events per scratch kind per frame) and notes that typical workloads have `G ≤ 1`; pathological cases are caught by the pin ceiling. Future coalesced-growth optimization deferred to B.3+.

**Round 3 (2026-05-24, post-rewrite of round-3 plan):** 8 load-bearing + 2 nice-to-have findings, all addressed:

- **F1 (Mechanism 3 dual-path inconsistent)** → resolved. Pitfall 4 rewritten to a single `ensure_*_arc` helper per scratch kind (always allocate fresh on grow, pin new, conditionally pin old). Task 1 + Task 10 use the unified helper.
- **F2 (`Arc::strong_count > 1` wrong test)** → resolved. Plan uses `Arc::ptr_eq` against the `FramePinSet` for the "already pinned?" question.
- **F3 (descriptor update at append safety)** → resolved. Task 3 codifies the invariant: `acquire_set` returns brand-new (host-allocated) sets, not recycled in-flight ones. Update-at-append is safe.
- **F4 (overlay must reflect post-op layout, not intermediate)** → resolved. Pitfall 6 added. Task 11 overlay update is now ONE write per op, to `SHADER_READ_ONLY_OPTIMAL` (the post-back-transition layout).
- **F5 (src/mask barriers underspecified)** → resolved. `RecordedRenderComposite` now carries `src_old_layout` / `mask_old_layout`. Task 12 audited: `record_render_composite_open/close` only handle dst; src/mask drawables are assumed `SHADER_READ_ONLY_OPTIMAL` by convention (today's `_legacy` matches). Defense-in-depth fields go on the payload for future ports.
- **F6 (self-alias pin-the-right-Arc)** → resolved via the unified ensure-then-pin pattern in Pitfall 4: op N+1 always pins the engine's CURRENT post-ensure Arc; the OLD Arc is conditionally pinned only if a prior op already pinned it. View handles in `RecordedRenderComposite` payloads are pre-resolved against the Arc the op was recorded against; that Arc is in the pin set.
- **F7 (Task 3 not behavior-preserving)** → resolved. Task 7 folded into Task 3; the descriptor-watermark + frame_generation + close-path consumer land in one commit. B.1's composite_glyphs path is unaffected (text pipeline uses a static descriptor set, no per-op `acquire_set`).
- **F8 (Task 1 Arc-wrap affects legacy via Arc::make_mut)** → resolved. Plan no longer uses `Arc::make_mut`. Task 1 swaps the slot type only; mutation routes through `ensure_*_arc` (always-fresh-on-grow) which is behavior-equivalent for `_legacy` callers (no frame open ⇒ old Arc drops immediately).
- **F9 nice-to-have (gradient lifetime)** → resolved. Task 10 inline note: gradient LUTs are engine-owned, no ticket-touch needed.
- **F10 nice-to-have (clippy pedantic violates AGENTS.md)** → resolved. Task 19 uses plain clippy.

## Remaining open questions

These ride along to implementation; surface answers only emerge after the code lands:

1. **`record_render_composite_open_with_old_layout` audit at implementation time.** The new overload (Task 12 Step 3) must NOT mutate `dst.storage.current_layout` even if today's `record_render_composite_open` does. Confirm at write time; if storage mutation is present today, refactor it out into the caller's commit step.
2. **Empty rects vs no-op frame.** Task 9 step 1 returns `Ok(empty stats)` on empty rects WITHOUT opening a frame. If a frame is already open (from a prior `composite_glyphs`), the empty `render_composite` is a no-op and does NOT close the frame. Confirmed alignment with `_legacy`.
3. **render_traps_or_tris staying on legacy.** Spec § "Sub-phases" puts traps/tris in B.3. M2 still flushes for it. Confirmed.

## Out of scope (intentional — these belong to future sub-phases)

- **Porting `render_traps_or_tris`, `cow_copy_area`, `copy_area`, `put_image`, `image_text`, `fill_rect`, `fill_rect_batch`, `logic_fill`** — all stay on legacy paths with M2 close wrapping. B.3 ports them.
- **Folding compose into the frame** — B.4.
- **Multi-output rendering instances** — entailed by compose-in-frame; B.4.
- **Removing SubmitGroup** — B.5. M1 cap=1 stays in place.
- **Removing the `_legacy` body of render_composite** — kept as the off-branch for kill-switch + bisect. Retired in B.5 alongside SubmitGroup.
- **bee RADV/Mesa bug characterisation** — out of yserver's repo, per the spec.
