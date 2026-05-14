# Phase 5 — readback fence + scratch grow defer-release

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Retire the `vkQueueWaitIdle` calls listed in this plan's task table — the readback paths' close-time wait (T1) and the five scratch-grow paths (T3–T6). Two `queue_wait_idle` sites outside `Drop` are deliberately deferred to follow-up phases: `GlyphAtlas::intern`'s per-glyph wait (profile shows not steady-state hot) and `vk/target.rs::initialize_clear` (one-shot during `DrawableImage` creation; same fence-narrowing pattern, separate scope). After Phase 5, every catalogue row classified **sync** or **temporary** that this phase touches is retired; the only remaining non-`Drop` non-modeset `queue_wait_idle`s are the two explicit deferrals above.

Two distinct moves:

1. **Readback narrowing** — `run_one_shot_op`'s close-time `vkQueueWaitIdle(graphics_queue)` becomes `vkWaitForFences([fence], true, UINT64_MAX)` on a per-op `VkFence`. Same blocking semantics for the caller (readback handlers still get their data-back guarantee), narrower wait scope (no longer serializes on unrelated composite submissions). Behaviour-equivalent to Phase 4 T1's `submit_and_wait` change, applied to the per-op one-shot CB path.

2. **Scratch grow defer-release** — `CopyScratch::ensure_size`, `DstReadback::ensure`, `MaskScratch::ensure_image_size`, `OpsStaging::ensure`, `GlyphAtlas::grow_staging` all destroy the old resource after a synchronous `queue_wait_idle`. The three on `record_paint_batch_op` paths (`CopyScratch`, `DstReadback`, `MaskScratch`) currently force a `BatchFlushReason::ProtocolBarrier` pre-flush at the call site to make that destroy safe — Phase 5 replaces that with `RenderScheduler::defer_resource_release`: the old resource is adopted into the currently-open `PaintBatch` (or released synchronously if no in-flight batch exists). The two others (`OpsStaging`, `GlyphAtlas`) don't need defer-release because their callers wait per-op (readback fence in T1; per-glyph wait still in `GlyphAtlas::intern`, retired in a future phase) — for them the `queue_wait_idle` is genuinely redundant and gets deleted with a doc-comment explaining why.

**Architecture:** Three structural additions on top of the Phase 4 baseline:

1. **Per-op `VkFence` inside `run_one_shot_op`.** Lazy-create at submit; destroy after the wait. On the device-lost path-2 the fence is leaked alongside the CB — same handle-abandonment rule Phase 4 introduced for `PaintBatch::submit_and_wait`.

2. **`RenderScheduler::defer_resource_release` helper.** Adopts a `Box<dyn BatchResource>` into the currently-open paint batch (opening one if needed). Releases synchronously if there is no in-flight or open batch (which means no recorded CB could possibly reference the resource).

3. **`ensure_*_returning_old` variants on each scratch type.** Allocate the new resource first (so a failure leaves the scratch untouched); take the old fields into a type-specific `BatchResource` impl (`RetiredCopyScratchImage`, `RetiredDstReadbackImage`, `RetiredMaskScratchImage`); swap in the new fields; return the boxed old as the caller's defer-release problem.

**Tech Stack:** Rust, ash (Vulkan), existing Phase 3A/4 infrastructure (`PaintBatch`, `BatchResource`, `RenderScheduler`, `BatchFlushReason`).

---

## Prerequisite — confirm post-Phase-4 baseline

Before T1, verify the tree state:

```bash
cd /home/jos/Projects/yserver
git log --oneline graphics-followups | head -20
rg -n 'queue_wait_idle' crates/yserver/src/kms/scheduler/paint_batch.rs
rg -n 'run_one_shot_op\(' crates/yserver/src/kms/ | wc -l
rg -n 'needs_grow\|needs_image_grow' crates/yserver/src/kms/backend.rs
```

Expected:
- Branch tip at minimum `f68d8c2` (T5 shutdown drain). Status doc shows Phase 4 done; status entry `2135a16 + 642d544 + 6fe4a71 + 49ff484 + f68d8c2` present in `docs/status.md`.
- `queue_wait_idle` in `paint_batch.rs`: **zero hits** (Phase 4 retired it).
- `run_one_shot_op(` returns 5–7 callers across `kms/backend.rs` (`hw_cursor_refresh`, `read_mirror_pixels`, `try_vk_get_image_pixels`, `dump_scanout_one`, `run_legacy_paint_op` body) + `kms/vk/copy_scratch.rs::quick_smoke` (test scaffolding).
- `needs_grow` / `needs_image_grow` hits: three pre-flush gate sites in `backend.rs` (the 3D CopyScratch site, the 3F-1 DstReadback site, the 3F-2 MaskScratch+DstReadback site).
- `cargo test --workspace`, `cargo clippy -p yserver`, `cargo +nightly fmt --check` all green.

If any of the above don't hold, STOP.

## Phase context

Read `docs/superpowers/specs/2026-05-12-rendering-rearchitecture-hld.md`, `docs/superpowers/specs/2026-05-12-waitidle-catalogue.md` (Phase 5 target rows), and `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase4-results.md` for the predecessor state. The catalogue classifies the rows Phase 5 touches:

- `crates/yserver/src/kms/vk/ops/mod.rs:100` (`run_one_shot_op`) — classification **sync**, removal phase 4 → carried into 5 because Phase 4 narrowed it for `PaintBatch` only.
- `crates/yserver/src/kms/vk/ops/mod.rs:168` (`OpsStaging::ensure`) — classification **temporary**.
- `crates/yserver/src/kms/vk/glyph.rs:444` (`GlyphAtlas::grow_staging`) — classification **temporary**.
- `crates/yserver/src/kms/vk/copy_scratch.rs:86` (`CopyScratch::ensure_size`) — classification **temporary**.
- `crates/yserver/src/kms/vk/dst_readback.rs:128` (`DstReadback::ensure`) — classification **temporary**.
- `crates/yserver/src/kms/vk/mask_scratch.rs:111` (`MaskScratch::ensure_image_size`) — classification **temporary**.

After Phase 5, every catalogue row classified **sync** or **temporary** is retired. The remaining `queue_wait_idle` calls are exclusively the **teardown** rows (14 `Drop` impls + the modeset `drain_all_pending`); those stay forever by design.

### Phase 5 is NOT a hot-path performance phase

The post-3F-2 perf snapshots showed `submit_and_wait` at 0.09% children on `bee`/RDNA2 under adapta-nokto + mate-cc — Phase 4 already cleared the close-time paint wait. The remaining waits Phase 5 retires are:

- **Readback fence narrowing**: the readback callers (`hw_cursor_refresh`, `read_mirror_pixels`, `try_vk_get_image_pixels`, `dump_scanout_one`) are not in the steady-state paint loop. `try_vk_get_image_pixels` fires on every Composite Manual readback; narrowing its wait from full-queue to per-op fence avoids serializing it on concurrent paint-side submissions. Phase 5 is the structural symmetry to Phase 4: paint side already fence-narrowed; readback side now matches.
- **Scratch grow defer-release**: today the pre-flush gates fire on the rare cycles where a scratch resize is needed (after window-resize, after a Trapezoids workload jumps to a larger bbox, etc.). On those cycles the gate forces a synchronous flush of the open batch. Phase 5 removes the synchronous flush — the resize now defers cleanup through the same retire-queue Phase 4 built. Steady-state cycles (no grow) are unchanged.

The user-visible win is **predictability under resize bursts**, not steady-state throughput. The adapta-nokto + mate-cc reproducer on `bee` is **not** expected to materially close per Phase 5 alone — its bottleneck remains downstream of paint-side waits (amdgpu ioctl rate). Phase 5 is justified as the symmetric completion of Phase 4's wait-narrowing program.

### Out of scope (deferred)

- **`GlyphAtlas::intern`'s per-glyph one-shot submit-and-wait** — `intern` calls `run_one_shot_op` per glyph (so after T1 the wait is via `wait_for_fences`, not a direct `queue_wait_idle` — but the per-glyph one-shot pattern itself remains, which is what profiling identifies as a target for batching into `PaintBatch`). Catalogue glossed this as a "site inside a non-`Drop` body." Profile shows it's not steady-state hot; deferred to a follow-up phase. Phase 5 leaves `GlyphAtlas::intern` untouched.
- **`GlyphAtlas::intern` + `GradientPicture` creation `renderer_failed` latching** — both call `run_one_shot_op` directly (atlas at `vk/glyph.rs:~376`, gradient at `vk/gradient.rs:~522`) and today swallow the Err with a `log::warn!`. Phase 5 T1 widens the per-op wait's failure contract (Err = abandoned handles, renderer-fatal) but does NOT propagate that latching through atlas/gradient — the propagation paths go through `GlyphAtlas::intern`'s `Result` into the `try_vk_text_run` / `try_vk_render_composite_glyphs` callers and through `GradientPicture::new`'s Result into RENDER's `try_vk_render_composite` — both require structural changes outside T1's scope. **Pre-existing gap, not regressed by Phase 5**; recorded in T7's "Known deferred items" and the `docs/known-issues.md` ticklist.
- **`vk/target.rs::initialize_clear`** — one-shot `vkQueueWaitIdle` after a clear-and-transition CB during `DrawableImage::initialize`. Classified `sync` removal phase 4 in the catalogue but was not retired then. Same fence-narrowing pattern as `run_one_shot_op`; could fold into a future Phase 5 micro-pass or wait for Phase 6's refcounted handles. Phase 5 leaves it untouched (no caller change needed; the wait is on a one-shot during construction, not in the steady-state paint loop).
- **Phase 6 — batch-owned refcounted handles**. The `ensure_*_returning_old` pattern Phase 5 introduces is a stop-gap; Phase 6's `BatchResource`-on-handle model subsumes it. Phase 5 deliberately picks the pattern that Phase 6 can replace with minimal churn (defer-release stays; just the per-site `Retired*` impl gets folded into the generic refcounted-handle release).
- **AMD-specific investigation** (separate stream; see `project_amd_lag_investigation.md`).

### Strict vs best-effort flush reasons (unchanged from Phase 4)

`BatchFlushReason` semantics are unchanged. The pre-flush gates Phase 5 removes are all `ProtocolBarrier` (strict) — they exist *because* the resize would otherwise race the open batch's CB. After Phase 5 the resize is deferred, so the gate is no longer needed; no flush of any kind happens at the resize-callsite. Strict callers (`Readback`, `ExternalSync`, `ProtocolBarrier`) keep their synchronous wait contract through `close_and_submit` — Phase 5 changes none of `flush_if_needed`'s strict-vs-best-effort branching.

### Key invariants Phase 5 inherits

1. **Drop-order**: `KmsBackend.scheduler` before `KmsBackend.ops_command_pool`. Don't reorder. (Phase 3A.)
2. **`renderer_failed` gate**: every paint entry still goes through `paint_resources()`. (Phase 3.)
3. **Path-2 (device-lost) semantics** of `submit_and_wait` / `wait_for_completion` / `try_retire_if_signaled`: preserved verbatim. `run_one_shot_op`'s new fence add ONE more handle to the abandonment list on path-2 — the per-op fence is leaked alongside the CB. Documented at `vk/ops/mod.rs` with the same 4-path failure taxonomy Phase 4 used for `submit_and_wait`.
4. **CPU-side layout tracking at record time** (3F-2 #8): unchanged. The defer-release pattern moves *resource freeing* not *layout state*; the scratch's `current_layout` field is reset to `UNDEFINED` synchronously inside `ensure_*_returning_old` (same as today), because the NEW image starts in `UNDEFINED` regardless of when the OLD image actually frees.
5. **Single-threaded core loop**: still single-threaded. Defer-release adoption happens on the same thread that records and submits.
6. **`PaintBatch::adopt` semantics**: a resource adopted into a batch is released at `retire_now()` (when the fence has been waited / observed signaled) OR at `Drop` if the batch never submits (poisoned + dropped). Phase 5's defer-release uses both paths correctly — the catch is that if the open batch is *poisoned* between `defer_resource_release` and the next `close_and_submit`, the adopted resource releases synchronously via `Drop` (which is correct: poison means no CB submitted, so no GPU reference exists).

## File structure

| File | Role | Touched in |
|---|---|---|
| `crates/yserver/src/kms/vk/ops/mod.rs` | `run_one_shot_op` body: create fence, submit with it, `wait_for_fences`, destroy fence. Add 4-path failure taxonomy in doc-comment. `OpsStaging::ensure`: remove redundant `queue_wait_idle`; doc-comment why. | T1 + T6 |
| `crates/yserver/src/kms/scheduler/paint_batch.rs` | No changes. The `BatchResource` trait + `adopt` already exist. | — |
| `crates/yserver/src/kms/scheduler/mod.rs` | Add `RenderScheduler::defer_resource_release(vk_arc, pool, resource)`. Add a unit test that verifies synchronous release with no in-flight batch + adoption with an open batch. | T2 |
| `crates/yserver/src/kms/vk/copy_scratch.rs` | Add `RetiredCopyScratchImage` (BatchResource impl). Add `ensure_size_returning_old(...)`. Keep `ensure_size` as a deprecated no-callers shim OR delete (audit shows only one external caller; delete is the goal). | T3 |
| `crates/yserver/src/kms/vk/dst_readback.rs` | Add `RetiredDstReadbackImage`. Add `ensure_returning_old(...)`. Same path as T3. | T4 |
| `crates/yserver/src/kms/vk/mask_scratch.rs` | Add `RetiredMaskScratchImage`. Add `ensure_image_size_returning_old(...)`. Same path as T3. | T5 |
| `crates/yserver/src/kms/vk/glyph.rs` | `GlyphAtlas::grow_staging`: remove redundant `queue_wait_idle`; doc-comment why. | T6 |
| `crates/yserver/src/kms/backend.rs` | Three caller sites switch from `ensure_*` + pre-flush gate to `ensure_*_returning_old` + `scheduler.defer_resource_release`. Remove `needs_grow` / `needs_image_grow` pre-flush blocks. | T3 + T4 + T5 |
| `docs/superpowers/plans/2026-05-14-rendering-rearchitecture-phase5-results.md` | Results doc. | T7 |

## Pre-task notes (read before starting)

1. **The per-op fence in `run_one_shot_op` is owned by the one-shot itself.** Same lifecycle as Phase 4's `PaintBatch::fence` but scoped to a single function call: create at top of the helper (after CB alloc, before submit), destroy after `wait_for_fences` returns Ok. No persistence between calls. On path-2 (wait fails) the fence is leaked alongside the CB — abandoned handles, function returns Err.

2. **`vkWaitForFences([fence], wait_all=true, UINT64_MAX)` ≠ `vkQueueWaitIdle(queue)`.** Same thread-blocking semantics, **different** wait scope:
   - `vkQueueWaitIdle`: blocks until **every** submission on the queue completes — including unrelated composite-side submissions.
   - `vkWaitForFences`: blocks until **the specific submission** that signals this fence completes. Other submissions on the queue can be in flight or complete in any order.
   - For `run_one_shot_op`'s callers (single-CB submit per call) the wait-completion *time* is the same (the CB completes when the CB completes). The wait *scope* matters when the same queue is shared with composite, which it is. Today readback can sit blocked while composite work pads onto the queue ahead of it; after Phase 5 it doesn't.

3. **`BatchResource::release(self: Box<Self>, vk: &VkContext)`** is the existing trait shape. Phase 5's `Retired*` impls store the Vulkan handles directly (image, view, memory; or buffer, memory; or for DstReadback, the optional `no_alpha_view` too). No `Arc<VkContext>` inside the impl — the trait method already gets `&VkContext`. On adopt-into-batch the batch holds the box; on `retire_now` it walks `retire_resources` and calls `release(&self.vk)` per box.

4. **`RenderScheduler::defer_resource_release` decision tree:**

   ```text
   if !submitted_paint_batches.is_empty() OR current_paint_batch.is_some():
       // SOME batch (open or in-flight) might own a CB referencing
       // this resource. Adopt into the open batch so its retire_now
       // releases the resource AFTER the latest referencing CB
       // completes. If no batch is open, open one (Idle state, no CB
       // allocated — adopt-only; the batch retires synchronously at
       // the next close_and_submit if it stays empty).
       open_batch(vk, pool);
       current_paint_batch.as_mut().unwrap().adopt(resource);
   else:
       // No batch open, none in flight. No CB anywhere references
       // this resource. Release synchronously.
       resource.release(&vk);
   ```

   **Why adopt into the OPEN batch even when only submitted batches are at risk.** Submitted-batches' CBs reference what was recorded at submit time; the resource we're deferring was *just* allocated and assigned to the scratch, so it cannot be referenced by any submitted batch's CB. But it CAN be referenced by the open batch's CB if the caller recorded ops between `ensure_*_returning_old` and the defer-release call — even if today the call sites don't do that, the API must be safe by construction. Adopting into the open batch makes the lifetime "release when this batch's fence signals", which is correct regardless of caller call-order. The currently-open-batch's fence signals AFTER all submitted-batches' fences (same-queue FIFO submit order), so this is strictly safer than adopting into any of the submitted batches.

   **Subtlety: the OPEN batch may not yet have a CB.** `open_batch` lazily-creates the batch struct but `begin_recording` only fires on first `append`/`record_paint_batch_op`. An adopted-into-idle batch stays Idle. If the caller never appends, `submit_and_wait` (or `close_and_submit_async`) sees Idle state and transitions directly to Retired without submit — `retire_now` releases the adopted resource synchronously. Correct.

5. **`ensure_*_returning_old` allocation-first ordering.** Allocate the new image/buffer BEFORE taking the old fields. If allocation fails, the scratch struct is untouched (no leak, no torn state); callers see `Err` and bail. Today's `ensure_*` already orders this way (allocate-then-replace); Phase 5 just adds the "wrap old in `Box<dyn BatchResource>` instead of immediately destroying" step.

   ```rust
   // Sketch for CopyScratch:
   pub fn ensure_size_returning_old(
       &mut self,
       width: u32,
       height: u32,
   ) -> Result<Option<Box<dyn BatchResource>>, CopyScratchError> {
       if width <= self.extent.width && height <= self.extent.height {
           return Ok(None);
       }
       let new_extent = vk::Extent2D {
           width: self.extent.width.max(width).next_power_of_two().max(256),
           height: self.extent.height.max(height).next_power_of_two().max(256),
       };
       let (image, memory) = allocate_image(&self.vk, new_extent)?;
       // Allocation succeeded. From here on, the function MUST NOT fail.
       let old_image = std::mem::replace(&mut self.image, image);
       let old_memory = std::mem::replace(&mut self.memory, memory);
       self.extent = new_extent;
       self.current_layout = vk::ImageLayout::UNDEFINED;
       Ok(Some(Box::new(RetiredCopyScratchImage {
           image: old_image,
           memory: old_memory,
       })))
   }
   ```

   `RetiredCopyScratchImage`'s `release` calls `destroy_image` + `free_memory`. No view to destroy for `CopyScratch` (it doesn't keep one). `DstReadback` and `MaskScratch` impls additionally destroy `vk::ImageView` (and `no_alpha_view` for the BGRA variant of DstReadback).

6. **`OpsStaging::ensure` and `GlyphAtlas::grow_staging` deletion safety.** Today both unconditionally `queue_wait_idle` before destroying the old buffer. After Phase 5 T1:
   - `OpsStaging::ensure`: only callers are readback-handler paths (`hw_cursor_refresh`, `read_mirror_pixels`, `try_vk_get_image_pixels`, `dump_scanout_one`). All of them use `run_one_shot_op`, which post-T1 does `wait_for_fences` per call. The OLD staging buffer is last-referenced by the IMMEDIATELY-PRIOR readback's CB; by the time the next readback's `ensure` runs, that CB has retired (the wait happened inside `run_one_shot_op`). The old buffer free is safe to do synchronously without ANY wait.
   - `GlyphAtlas::grow_staging`: caller is `GlyphAtlas::intern`, which still has its own per-glyph `queue_wait_idle` (out of scope for Phase 5). At the time `grow_staging` runs, either (a) there has been no prior glyph upload yet (first-grow case: no buffer reference outstanding) OR (b) the last successful glyph upload has already been waited on. In either case the OLD staging buffer's last referencing CB (if any) has retired.

   Both `queue_wait_idle` calls are therefore redundant in steady state. Deleting them is safe. The doc-comment in T6 calls out the precondition (readback / intern callers wait per-op).

7. **`run_legacy_paint_op` is a candidate dead path.** Per the post-3F-2 catalogue, every paint-side recorder migrated to `record_paint_batch_op`. `run_legacy_paint_op` (`backend.rs:~1750`) is the wrapper for non-migrated recorders; if there are zero callers, T1 may detect this with a `cargo build -p yserver` warn-on-dead-code and the result doc can note it as a follow-up (delete in a separate housekeeping commit, NOT in Phase 5). DO NOT delete in T1 — keep the scope focused.

8. **No backpressure change.** Phase 5 introduces no new queue-pushing path that could grow unboundedly. Defer-release adopts into the existing open batch; that batch is bound by the existing `MAX_IN_FLIGHT_PAINT_BATCHES = 4` cap on submission. The cap doesn't need adjustment.

9. **Test coverage.**
   - **`run_one_shot_op` fence path**: unit test in `vk/ops/mod.rs::tests` is not viable without a Vulkan device. Covered by the existing rendercheck (every readback path runs through `run_one_shot_op`). T1's behaviour change is observable via the catalogue grep (no `queue_wait_idle` in the body) and by rendercheck staying green.
   - **`defer_resource_release` decision tree**: unit test in `scheduler/mod.rs::tests`. Use a mock `BatchResource` impl that increments an `AtomicUsize` on `release`; assert it releases synchronously when no batch is open, and is adopted (not yet released) when a batch is open. Mock requires a `&VkContext` argument but the mock impl can ignore it — fabricate a minimal `VkContext` is too costly; instead use a private `#[cfg(test)]` overload of `defer_resource_release` that skips the fallback synchronous release and just records the decision. Decision: test the decision tree via a public method `defer_resource_release_decision(&self) -> DeferDecision { Synchronous, AdoptOpen }` that's the load-bearing branch — pure function over scheduler state, no Vk needed. The actual `defer_resource_release(vk, pool, resource)` calls into it then performs the action.
   - **`ensure_*_returning_old`**: unit-test the no-grow path (returns `Ok(None)`, scratch unchanged) by constructing a `CopyScratch`/`DstReadback`/`MaskScratch` — except these constructors need `Arc<VkContext>`. Not unit-testable in isolation. Covered by `cargo test --workspace` integration (the scratch types are exercised by `fixture_smoke` and by binary integration tests).
   - **State-machine round-trip**: T2's `defer_resource_release` test is the only new unit test. The rest is rendercheck + smoke.

10. **clippy / fmt**: plain `cargo clippy -p yserver`, `cargo +nightly fmt`. The 5 pre-existing `doc_lazy_continuation` warnings stay (unrelated). No new warnings.

11. **Stop-the-world check after every task.** Per Phase 4's protocol: `cargo +nightly fmt --check`, `cargo clippy -p yserver`, `cargo test --workspace` after each commit. If any task fails to pass, STOP and surface to the user — do not pile on the next task.

---

## Task 1: Fence the per-op wait in `run_one_shot_op`

**Goal:** Replace the trailing `vkQueueWaitIdle(graphics_queue)` inside `run_one_shot_op` with a per-op `VkFence` + `vkWaitForFences`. Behavior-equivalent for callers (still blocking; data is back on return), narrower wait scope (no longer serialises on unrelated submissions). Single source-of-truth change benefits every caller automatically.

**Files:**
- Modify: `crates/yserver/src/kms/vk/ops/mod.rs`

### Step 1: Edit `run_one_shot_op` body

- [ ] **Step 1: Replace `queue_wait_idle` with fence-based wait inside `run_one_shot_op`**

Current body (lines ~88–103):

```rust
    let result = (|| -> Result<(), vk::Result> {
        let begin = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        unsafe { vk.device.begin_command_buffer(cb, &begin)? };
        record(vk, cb)?;
        unsafe { vk.device.end_command_buffer(cb)? };

        let cb_info = [vk::CommandBufferSubmitInfo::default().command_buffer(cb)];
        let submit = [vk::SubmitInfo2::default().command_buffer_infos(&cb_info)];
        unsafe {
            vk.device
                .queue_submit2(vk.graphics_queue, &submit, vk::Fence::null())?;
            vk.device.queue_wait_idle(vk.graphics_queue)?;
        }
        Ok(())
    })();
```

Replace with:

```rust
    // 5-T1: per-op fence instead of vkQueueWaitIdle. Same
    // blocking semantics for the caller (data is back on return)
    // but narrower wait scope — only waits for THIS submission,
    // not every prior submission on the graphics queue (which
    // today includes composite-side work).
    //
    // 5-path failure taxonomy (extends Phase 4's submit_and_wait
    // model — `run_one_shot_op` has an extra failure window
    // before fence-create because `record(...)` and
    // `end_command_buffer` can fail):
    //
    //   0a. begin_command_buffer fails: CB allocated but never
    //       recorded. Free CB, return Err.
    //   0b. record(...) callback returns Err: CB partially
    //       recorded but never submitted. Free CB, return Err.
    //   0c. end_command_buffer fails: same — CB never submitted.
    //       Free CB, return Err.
    //   1a. create_fence fails: CB recorded, no fence yet. Free
    //       CB, return Err. No fence to destroy.
    //   1b. queue_submit2 fails: CB never queued. Destroy fence,
    //       free CB, return Err.
    //   2.  wait_for_fences fails: CB IS in flight or device is
    //       lost. ABANDON the CB and the fence — Vulkan handles
    //       are leaked until VkContext::Drop. Same leak-not-UB
    //       contract as Phase 4's submit_and_wait. Returns Err;
    //       caller MUST treat the renderer as fatal.
    //   3.  wait_for_fences Ok: destroy fence, free CB, Ok(()).
    //
    // Implementation: the closure tracks whether the failure was
    // pre-submit (CB free is safe) or post-submit (CB free is
    // UB). The simplest encoding is a flag returned alongside
    // the Result, or — as below — a custom enum the outer free
    // matches on. The flag-out-of-closure approach (used here)
    // keeps the closure body simple at the cost of one extra
    // mutable binding.
    let mut cb_safe_to_free = true;
    let result = (|| -> Result<(), vk::Result> {
        let begin = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        // Paths 0a / 0b / 0c — pre-submit failures. cb_safe_to_free
        // stays true; the outer block frees the CB on the Err
        // returned here.
        unsafe { vk.device.begin_command_buffer(cb, &begin)? };
        record(vk, cb)?;
        unsafe { vk.device.end_command_buffer(cb)? };

        // Path 1a — fence creation failure (pre-submit).
        let fence_info = vk::FenceCreateInfo::default();
        let fence = unsafe { vk.device.create_fence(&fence_info, None) }?;

        // Path 1b — submit failure. Destroy the fence; the CB is
        // still safe to free (cb_safe_to_free stays true).
        let cb_info = [vk::CommandBufferSubmitInfo::default().command_buffer(cb)];
        let submit = [vk::SubmitInfo2::default().command_buffer_infos(&cb_info)];
        if let Err(e) = unsafe { vk.device.queue_submit2(vk.graphics_queue, &submit, fence) } {
            unsafe { vk.device.destroy_fence(fence, None) };
            return Err(e);
        }

        // From this point on, the CB is in flight; on any Err
        // before fence-destroy, freeing the CB is UB.
        let fences = [fence];
        match unsafe { vk.device.wait_for_fences(&fences, true, u64::MAX) } {
            Ok(()) => {
                // Path 3: clean. Destroy fence; CB free happens
                // outside the closure.
                unsafe { vk.device.destroy_fence(fence, None) };
                Ok(())
            }
            Err(e) => {
                // Path 2: device-lost or similar. Leak the CB AND
                // the fence — both handles are abandoned. Caller
                // observes Err and treats the renderer as failed.
                cb_safe_to_free = false;
                log::error!(
                    "run_one_shot_op: wait_for_fences failed ({e:?}); \
                     CB and fence abandoned. KMS renderer is in an \
                     unrecoverable state — caller MUST tear down or disable."
                );
                Err(e)
            }
        }
    })();

    // Free the CB on every path EXCEPT path 2 (post-submit wait
    // failure). Pre-submit failures (paths 0a/0b/0c/1a/1b) leave
    // the CB unsubmitted, so freeing it is safe. Path 3 (clean
    // success) frees it. Path 2 leaves cb_safe_to_free = false
    // and the CB is abandoned.
    if cb_safe_to_free {
        unsafe { vk.device.free_command_buffers(pool, &[cb]) };
    }
    result
}
```

**Note for reviewer:** the doc-comment block above the function (lines ~65–73 today) needs an update to reference the fence pattern and to remove the "wait_idle" framing. Add a short paragraph mirroring Phase 4 T1's doc-update style:

> ```rust
> /// Phase 5 retires the trailing `vkQueueWaitIdle` in favour of a
> /// per-op `VkFence` + `vkWaitForFences`. Caller semantics are
> /// unchanged (still blocking; data is back on return); wait scope
> /// is narrower (this submission only, not the whole queue).
> ///
> /// 5-path failure taxonomy (extends `PaintBatch::submit_and_wait`'s
> /// 4-path model — `run_one_shot_op` has the additional pre-submit
> /// failure window of `record(...)` and `end_command_buffer`):
> ///   0. pre-submit failure (begin/record/end): CB safe to free.
> ///   1a. fence-create failure: CB safe to free.
> ///   1b. submit failure: destroy fence, CB safe to free.
> ///   2.  wait failure (CB in flight): LEAK CB + fence. Renderer
> ///       must be torn down.
> ///   3.  success: destroy fence + free CB; Ok(()).
> /// See inline comments for the exact branching.
> ```

### Step 2: Latch `renderer_failed` at `run_one_shot_op` callers (codex round-2 P1)

- [ ] **Step 2: Update readback / scanout call sites to set `self.renderer_failed = true` on `run_one_shot_op` Err**

Phase 5 T1's new path-2 contract says "wait failure means CB+fence abandoned; renderer is fatal." Phase 4's `flush_if_needed` already latches `self.renderer_failed = true` on `BatchError::Vk` (see `backend.rs:1605–1614`); Phase 5 widens that contract to `run_one_shot_op` callers and must latch the same way.

**In-scope call sites** (all in `kms/backend.rs`):
- `hw_cursor_refresh` (~`backend.rs:2431`)
- `read_mirror_pixels` (~`backend.rs:2781`)
- `try_vk_get_image_pixels` (~`backend.rs:4385`)
- `dump_scanout_one` (~`backend.rs:8027`)
- `run_legacy_paint_op` body (~`backend.rs:1767`) — wrapper for not-yet-migrated paint ops; the wrapper is `&mut self` already, the latch fits.

Pattern (apply per call site, adapted to the local Err shape — `return false` for `try_vk_get_image_pixels`, `return None` for `read_mirror_pixels`, plain `return` for `hw_cursor_refresh`, `Err(io::Error)` for `dump_scanout_one`):

```rust
// BEFORE:
if let Err(e) = run_one_shot_op(&vk_arc, pool_handle, |vk, cb| {
    vk_image::record_get_image(vk, cb, mirror, staging_buffer, &regions)
}) {
    log::warn!("read_mirror_pixels: record_get_image failed: {e:?}");
    return None;
}

// AFTER:
if let Err(e) = run_one_shot_op(&vk_arc, pool_handle, |vk, cb| {
    vk_image::record_get_image(vk, cb, mirror, staging_buffer, &regions)
}) {
    log::error!(
        "read_mirror_pixels: run_one_shot_op returned fatal {e:?}; \
         latching renderer_failed — KMS renderer disabled until restart"
    );
    self.renderer_failed = true;
    return None;
}
```

**Out-of-scope call sites** (NOT touched in Phase 5):
- `GlyphAtlas::intern`'s internal `run_one_shot_op` (`vk/glyph.rs:~376`) — the per-glyph wait is deferred to a follow-up phase; latching needs propagation through `intern`'s `Result` into the `try_vk_text_run` / `try_vk_render_composite_glyphs` callers in `backend.rs`. Folding that here widens T1 beyond its goal; instead, document the gap explicitly in the **Out of scope** section and add it as a Phase-5 followup in the results doc (T7).
- `GradientPicture` creation (`vk/gradient.rs:~522`) — same propagation pattern. Same deferral.
- `white_mask_image` setup in `open_with_commit` (`backend.rs:~2137`) — this `run_one_shot_op` runs during `KmsBackend::open_with_commit` (backend construction), where `&mut self` doesn't yet exist as a coherent target for the latch. Today's failure mode leaves `white_mask_image = None` and the backend constructs successfully; downstream paint paths that need the white mask detect `None` and bail or fall back. **Intentional init-only best-effort exception**: backend construction's failure model is "log + leave partially initialized + let downstream code degrade gracefully," distinct from the steady-state "fatal-on-path-2" contract. Phase 5 preserves this behavior verbatim. If a future phase tightens construction-failure handling (e.g., bubble construction errors and fail-fast), this site should be migrated then.

**Acceptable narrowing**: T1 closes the in-scope readback gap. The atlas + gradient + white-mask-init gaps remain a known issue (pre-existing; not regressed by Phase 5 — all still log-and-degrade today). Documented in T7's "Known deferred items."

### Step 3: Verify gates

- [ ] **Step 3: Run validation commands**

```bash
cargo +nightly fmt
cargo clippy -p yserver
cargo test --workspace
```

Expected:
- `cargo +nightly fmt --check` clean.
- `cargo clippy -p yserver`: 5 pre-existing `doc_lazy_continuation` warnings only.
- `cargo test --workspace`: green (138 in `yserver` lib, etc. — same shape as Phase 4 T5 baseline).

### Step 4: Catalogue grep

- [ ] **Step 4: Verify `queue_wait_idle` is gone from `run_one_shot_op`**

```bash
rg -n 'queue_wait_idle' crates/yserver/src/kms/vk/ops/mod.rs
```

Expected: hits only at `OpsCommandPool::drop` (line ~59) and `OpsStaging::drop` (line ~184) and `OpsStaging::ensure` (line ~168 — retired in T6). The `run_one_shot_op` body hit (current line ~100) MUST be gone.

```bash
rg -n 'wait_for_fences|create_fence' crates/yserver/src/kms/vk/ops/mod.rs
```

Expected: hits inside `run_one_shot_op` for the new fence path.

### Step 5: Commit

- [ ] **Step 5: Commit with message**

```text
refactor(kms): fence run_one_shot_op's per-op wait (5-T1)

Replace vkQueueWaitIdle(queue) inside run_one_shot_op with a per-op
VkFence + vkWaitForFences. Same blocking semantics for callers; wait
scope narrows from "all queue" to "this submission". 5-path failure
taxonomy extends Phase 4's PaintBatch::submit_and_wait model
(pre-submit failure window of record/end_command_buffer adds paths
0a/0b/0c above 1a/1b). Auto-applies to all readback callers
(hw_cursor_refresh, read_mirror_pixels, try_vk_get_image_pixels,
dump_scanout_one) and to run_legacy_paint_op.

Latches self.renderer_failed = true on Err return at the 5
in-scope backend.rs call sites (same pattern as Phase 4's
flush_if_needed). Atlas + gradient call sites still warn-and-continue
per pre-existing behavior — known gap, called out in T7 results
doc as a Phase-5 followup.

Catalogue row vk/ops/mod.rs:100 (sync) retired.
```

### Done conditions for T1

1. `cargo +nightly fmt --check` clean.
2. `cargo clippy -p yserver` produces only the 5 pre-existing `doc_lazy_continuation` warnings.
3. `cargo test --workspace` green.
4. `queue_wait_idle` is NOT inside `run_one_shot_op`'s body.
5. `wait_for_fences` IS inside `run_one_shot_op`'s body.
6. The `cb_safe_to_free` flag exists and gates the outer CB free; only path 2 (post-submit wait failure) sets it false. Pre-submit failures (begin / record / end_command_buffer / create_fence / queue_submit2) free the CB.
7. All 5 in-scope call sites (`hw_cursor_refresh`, `read_mirror_pixels`, `try_vk_get_image_pixels`, `dump_scanout_one`, `run_legacy_paint_op`) latch `self.renderer_failed = true` on `run_one_shot_op` Err return; the `log::warn!` is upgraded to `log::error!` and the message references "latching renderer_failed".
8. Atlas + gradient call sites are NOT touched (preserved pre-existing behavior); the gap is recorded in T7's "Known deferred items."
9. Single new commit, hooked to no other changes.

---

## Task 2: `RenderScheduler::defer_resource_release` helper

**Goal:** Add the scheduler-side helper that adopts a `Box<dyn BatchResource>` into the currently-open paint batch (or releases synchronously if no batch state references could exist). Pure new function; no behavior change at call sites yet.

**Files:**
- Modify: `crates/yserver/src/kms/scheduler/mod.rs`

### Step 1: Define `DeferDecision` enum (test-visible)

- [ ] **Step 1: Add a pure-function decision helper**

Add near the top of `mod.rs`:

```rust
/// Outcome of `RenderScheduler::defer_resource_release_decision`. Pure
/// view over the scheduler state at the call site. Test-only callers
/// use this to verify the decision tree; production code uses
/// `defer_resource_release` which does the action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeferDecision {
    /// No live (non-poisoned) batch could reference the resource.
    /// Caller releases synchronously. This covers:
    ///   - Empty scheduler (no submitted batches, no open batch).
    ///   - Open batch is `Poisoned` AND `submitted_paint_batches` is empty
    ///     (poison drop is a no-op — adopting would leak).
    Synchronous,
    /// At least one live (non-poisoned) batch (open or in-flight) might
    /// hold a CB that references the resource. Adopt into the open
    /// batch (creating one in Idle state if none exists). The open
    /// batch's `state()` is guaranteed non-Poisoned by the time we
    /// adopt: either it's already non-Poisoned, or
    /// `defer_resource_release` discards the Poisoned batch before
    /// opening a fresh Idle one to host the adoption.
    AdoptOpen,
}

impl RenderScheduler {
    /// Pure-function decision over `(has_submitted, current_state)`.
    /// Codex round-2 P2 refactor: exposing the args explicitly lets
    /// `#[cfg(test)]` callers exercise the full decision tree —
    /// including the `Poisoned` branches — without needing to
    /// construct a real `PaintBatch` (which would require an
    /// `Arc<VkContext>` and a `vk::CommandPool`).
    ///
    /// **Poisoned-batch handling**: a `Poisoned` current batch is
    /// NOT a host for adoption — `PaintBatch::Drop` for Poisoned is
    /// a no-op, so an adopted resource would leak. If the only
    /// "live" thing is a Poisoned current batch with no submitted
    /// predecessors, the answer is `Synchronous`. If there ARE
    /// submitted predecessors, the production
    /// `defer_resource_release` discards the Poisoned batch and
    /// opens a fresh Idle one to host the adoption — this pure
    /// helper returns `AdoptOpen` for that pre-discard case.
    #[must_use]
    pub fn defer_resource_release_decision_for(
        has_submitted: bool,
        current_state: Option<BatchState>,
    ) -> DeferDecision {
        let current_is_live = matches!(
            current_state,
            Some(s) if s != BatchState::Poisoned
        );
        if !has_submitted && !current_is_live {
            DeferDecision::Synchronous
        } else {
            DeferDecision::AdoptOpen
        }
    }

    /// Thin wrapper that snapshots `self`'s state and delegates to
    /// the pure helper. Production callers use this; tests use the
    /// pure form directly.
    #[must_use]
    pub fn defer_resource_release_decision(&self) -> DeferDecision {
        Self::defer_resource_release_decision_for(
            !self.submitted_paint_batches.is_empty(),
            self.current_paint_batch.as_ref().map(PaintBatch::state),
        )
    }
}
```

### Step 2: Define `defer_resource_release`

- [ ] **Step 2: Add the production helper**

Below the decision helper:

```rust
impl RenderScheduler {
    /// Defer-release the boxed `BatchResource`: adopt it into the
    /// currently-open paint batch if any batch (open or in flight)
    /// might hold a CB referencing the resource, OR release it
    /// synchronously if nothing in flight could possibly reference
    /// it.
    ///
    /// `vk_arc` and `pool` are required for the adopt branch — when
    /// no batch is open, `defer_resource_release` lazy-opens one
    /// (Idle state, no CB allocated) to host the adoption. If the
    /// caller never appends to that batch, the next
    /// `close_and_submit` transitions Idle → Retired directly and
    /// the adopted resource releases at that moment (no submit, no
    /// fence, no wait).
    ///
    /// **Why adopt into the OPEN batch even when only submitted
    /// batches exist.** Submitted-batches' CBs reference what was
    /// recorded at submit time; the resource being deferred was just
    /// freshly-allocated and assigned to its owning scratch struct,
    /// so it cannot be in any submitted batch's CB. It CAN be in the
    /// open batch's CB (if the caller recorded ops between
    /// `ensure_*_returning_old` and this call), and even if today's
    /// call sites don't do that, the API must be safe by
    /// construction. The currently-open batch's fence signals after
    /// all submitted batches' fences (same-queue FIFO submit order),
    /// so adopting there is strictly safer than adopting into any
    /// of the submitted batches.
    ///
    /// **Subtlety: open batch may be Idle.** That's fine — Idle
    /// batches still have `retire_resources`; their `submit_and_wait`
    /// at Idle short-circuits to `retire_now`, which walks and
    /// releases the adopted resource. The single-threaded core loop
    /// invariant ensures no race: this function runs on the same
    /// thread as the next `close_and_submit`.
    pub fn defer_resource_release(
        &mut self,
        vk: Arc<VkContext>,
        pool: vk::CommandPool,
        resource: Box<dyn paint_batch::BatchResource>,
    ) {
        // Discard a Poisoned current batch before deciding. A
        // Poisoned batch's Drop is a no-op (the leak-on-error
        // contract), so adopting into it would silently leak the
        // resource. If there are submitted predecessors, the
        // resource still needs adopt-into-a-live-batch lifetime —
        // open a fresh Idle batch below. If there are no
        // predecessors either, the synchronous branch runs.
        if let Some(b) = self.current_paint_batch.as_ref()
            && b.state() == BatchState::Poisoned
        {
            self.current_paint_batch = None;
        }
        match self.defer_resource_release_decision() {
            DeferDecision::Synchronous => {
                resource.release(&vk);
            }
            DeferDecision::AdoptOpen => {
                let _ = self.open_batch(vk, pool);
                self.current_paint_batch
                    .as_mut()
                    .expect("open_batch just ran")
                    .adopt(resource);
            }
        }
    }
}
```

### Step 3: Add unit test

- [ ] **Step 3: Decision-tree unit test**

In the existing `#[cfg(test)] mod tests` block:

```rust
    // ============ Pure decision-helper tests (codex round-2 P2). ============
    // The pure form takes (has_submitted, current_state) explicitly
    // so we exercise all 12 combinations without constructing a
    // PaintBatch.

    #[test]
    fn defer_decision_empty_is_synchronous() {
        assert_eq!(
            RenderScheduler::defer_resource_release_decision_for(false, None),
            DeferDecision::Synchronous
        );
    }

    #[test]
    fn defer_decision_submitted_only_is_adopt() {
        assert_eq!(
            RenderScheduler::defer_resource_release_decision_for(true, None),
            DeferDecision::AdoptOpen
        );
    }

    #[test]
    fn defer_decision_current_idle_is_adopt() {
        assert_eq!(
            RenderScheduler::defer_resource_release_decision_for(
                false,
                Some(BatchState::Idle)
            ),
            DeferDecision::AdoptOpen
        );
    }

    #[test]
    fn defer_decision_current_recording_is_adopt() {
        assert_eq!(
            RenderScheduler::defer_resource_release_decision_for(
                false,
                Some(BatchState::Recording)
            ),
            DeferDecision::AdoptOpen
        );
    }

    #[test]
    fn defer_decision_current_closed_is_adopt() {
        assert_eq!(
            RenderScheduler::defer_resource_release_decision_for(
                false,
                Some(BatchState::Closed)
            ),
            DeferDecision::AdoptOpen
        );
    }

    #[test]
    fn defer_decision_current_poisoned_no_submitted_is_synchronous() {
        // The load-bearing P2 case: a Poisoned current batch is
        // NOT a valid adoption host (its Drop is a no-op). With no
        // submitted predecessors, the resource must release
        // synchronously.
        assert_eq!(
            RenderScheduler::defer_resource_release_decision_for(
                false,
                Some(BatchState::Poisoned)
            ),
            DeferDecision::Synchronous
        );
    }

    #[test]
    fn defer_decision_current_poisoned_with_submitted_is_adopt() {
        // Predecessors might reference the resource → adopt. The
        // production fn discards the Poisoned batch and opens a
        // fresh Idle one; this pure helper sees only the
        // (has_submitted=true, Poisoned) snapshot and answers
        // AdoptOpen accordingly.
        assert_eq!(
            RenderScheduler::defer_resource_release_decision_for(
                true,
                Some(BatchState::Poisoned)
            ),
            DeferDecision::AdoptOpen
        );
    }

    // Retired/Submitted as a current_state is a state-machine
    // invariant violation (current_paint_batch is never observed in
    // those states at a defer-release call site — Submitted lives
    // in submitted_paint_batches, Retired is short-lived inside
    // close_and_submit). Test for completeness; should answer
    // AdoptOpen (the conservative direction) but is unreachable.
    #[test]
    fn defer_decision_current_submitted_is_adopt() {
        assert_eq!(
            RenderScheduler::defer_resource_release_decision_for(
                false,
                Some(BatchState::Submitted)
            ),
            DeferDecision::AdoptOpen
        );
    }

    #[test]
    fn defer_decision_current_retired_is_adopt() {
        assert_eq!(
            RenderScheduler::defer_resource_release_decision_for(
                false,
                Some(BatchState::Retired)
            ),
            DeferDecision::AdoptOpen
        );
    }

    // Empty-scheduler convenience test that goes through the
    // self-wrapping form. Verifies the wrapper composes correctly
    // with the pure helper.
    #[test]
    fn defer_decision_is_synchronous_with_empty_scheduler() {
        let s = RenderScheduler::new();
        assert_eq!(s.defer_resource_release_decision(), DeferDecision::Synchronous);
    }

    // AdoptOpen branch's downstream behavior (PaintBatch::adopt +
    // retire_now release) is covered by binary integration tests +
    // hardware smoke — those require a real Vulkan context.
```

### Step 4: Gates + commit

- [ ] **Step 4: Validate and commit**

```bash
cargo +nightly fmt
cargo clippy -p yserver
cargo test --workspace
```

Commit message:

```text
refactor(kms): add RenderScheduler::defer_resource_release (5-T2)

Adopts a BatchResource into the currently-open paint batch (lazy-
opening one if needed) when any batch (open or in flight) might
reference the resource; releases synchronously if nothing is in
flight. Pure addition; no caller wired yet.

Special-cases Poisoned current batch: discards it before deciding,
so a resource adopted into a poison-Drop-no-op batch can't leak.
Reason a Poisoned batch is unsafe to adopt into: PaintBatch::Drop
on Poisoned is a no-op (the leak-on-error contract), so any
adopted resource would never see release().

Companion DeferDecision enum + pure-function helper
`defer_resource_release_decision_for(has_submitted, current_state)`
for full decision-tree unit coverage. 12-case test matrix covers
every (has_submitted, current_state) combination including the
load-bearing Poisoned cases.
```

### Done conditions for T2

1. `cargo +nightly fmt --check` clean.
2. `cargo clippy -p yserver` clean (only pre-existing warns).
3. `cargo test --workspace` green; all 10 new unit tests pass.
4. `defer_resource_release_decision_for(false, None) == Synchronous`.
5. `defer_resource_release_decision_for(false, Some(Poisoned)) == Synchronous` (the load-bearing P2 fix from codex round 2).
6. `defer_resource_release_decision_for(true, Some(Poisoned)) == AdoptOpen` (pre-discard view).
7. Production `defer_resource_release` discards Poisoned current batch before invoking the decision helper.
8. Single new commit.

---

## Task 3: CopyScratch migration — `ensure_size_returning_old` + caller wiring

**Goal:** Replace `CopyScratch::ensure_size` (which `queue_wait_idle`s before destroying the old image) with `ensure_size_returning_old` (which returns the old image as a `Box<dyn BatchResource>` for the caller to defer-release). Migrate the single caller (`try_vk_copy_area` same-overlap arm) to use the new variant and remove the pre-flush gate.

**Files:**
- Modify: `crates/yserver/src/kms/vk/copy_scratch.rs`
- Modify: `crates/yserver/src/kms/backend.rs`

### Step 1: `RetiredCopyScratchImage` BatchResource impl

- [ ] **Step 1: Add the retire-wrapper in `copy_scratch.rs`**

```rust
use crate::kms::scheduler::paint_batch::BatchResource;

#[derive(Debug)]
struct RetiredCopyScratchImage {
    image: vk::Image,
    memory: vk::DeviceMemory,
}

impl BatchResource for RetiredCopyScratchImage {
    fn release(self: Box<Self>, vk: &VkContext) {
        unsafe {
            vk.device.destroy_image(self.image, None);
            vk.device.free_memory(self.memory, None);
        }
    }
}
```

### Step 2: `ensure_size_returning_old`

- [ ] **Step 2: Add the new method on `CopyScratch`**

```rust
    /// 5-T3: like `ensure_size` but returns the old image wrapped
    /// as a `BatchResource` for the caller to defer-release through
    /// the scheduler. Returns `Ok(None)` if no grow was needed (old
    /// image is fine, scratch unchanged).
    ///
    /// Allocation-first ordering: if `allocate_image` fails the
    /// scratch is untouched and the caller sees `Err`.
    pub fn ensure_size_returning_old(
        &mut self,
        width: u32,
        height: u32,
    ) -> Result<Option<Box<dyn BatchResource>>, CopyScratchError> {
        if width <= self.extent.width && height <= self.extent.height {
            return Ok(None);
        }
        let new_extent = vk::Extent2D {
            width: self.extent.width.max(width).next_power_of_two().max(256),
            height: self.extent.height.max(height).next_power_of_two().max(256),
        };
        let (image, memory) = allocate_image(&self.vk, new_extent)?;
        // Allocation succeeded — past here the function MUST NOT fail.
        let old_image = std::mem::replace(&mut self.image, image);
        let old_memory = std::mem::replace(&mut self.memory, memory);
        self.extent = new_extent;
        self.current_layout = vk::ImageLayout::UNDEFINED;
        Ok(Some(Box::new(RetiredCopyScratchImage {
            image: old_image,
            memory: old_memory,
        })))
    }
```

### Step 3: Delete `ensure_size` (or mark dead)

- [ ] **Step 3: Audit `ensure_size` callers**

```bash
rg -n 'copy_scratch.*ensure_size\b' crates/yserver/src/kms/
rg -n '\.ensure_size\b' crates/yserver/src/kms/vk/copy_scratch.rs
```

If `ensure_size` has zero external callers after T3 wires the new variant (Step 4 below), delete it. If `quick_smoke` or another test caller still uses it, keep it as a deprecated test helper with a doc-comment pointer.

### Step 4: Wire `try_vk_copy_area` same-overlap caller

- [ ] **Step 4: Edit `backend.rs` around line ~3319–3368**

Today (paraphrased):

```rust
// 3D: pre-flush gate
let needs_scratch_grow = self
    .copy_scratch
    .as_ref()
    .is_some_and(|s| s.needs_grow(u32::from(width), u32::from(height)));
if needs_scratch_grow {
    use crate::kms::scheduler::paint_batch::BatchFlushReason;
    if let Err(e) = self.flush_if_needed(BatchFlushReason::ProtocolBarrier) {
        log::warn!("vk copy same-overlap: pre-resize flush failed: {e:?}");
        return false;
    }
}

// ... acquire mirror borrow ...

// Step 3: ensure_size
let Some(scratch) = self.copy_scratch.as_mut() else {
    return false;
};
if let Err(e) = scratch.ensure_size(u32::from(width), u32::from(height)) {
    log::warn!("vk copy: scratch resize failed: {e:?}");
    return false;
}
```

Replace with:

```rust
// 5-T3: defer-release replaces pre-flush. No need to drain the
// open batch — the old scratch image is adopted into the
// scheduler's retire flow so it survives any in-flight CB.
//
// CRITICAL borrow-checker note (codex P1): the scratch's &mut
// borrow MUST end BEFORE `self.scheduler.defer_resource_release`
// borrows `&mut self`. Use a tight block so the `as_mut()`
// binding drops at the closing brace. Reborrow `self.copy_scratch`
// later (for the recorder closure) as a fresh borrow — this is
// fine because the earlier &mut already ended.
let retired = {
    let Some(scratch) = self.copy_scratch.as_mut() else {
        return false;
    };
    match scratch.ensure_size_returning_old(
        u32::from(width),
        u32::from(height),
    ) {
        Ok(r) => r,
        Err(e) => {
            log::warn!("vk copy: scratch resize failed: {e:?}");
            return false;
        }
    }
}; // <-- scratch's &mut borrow ends here.
if let Some(old) = retired {
    let Some(vk_arc) = self.vk.as_ref().cloned() else {
        // Without a Vk handle we couldn't have allocated the new
        // image above. Defensive; unreachable in practice.
        return false;
    };
    let Some(pool_handle) = self.ops_command_pool.as_ref().map(|p| p.handle()) else {
        return false;
    };
    self.scheduler
        .defer_resource_release(vk_arc, pool_handle, old);
}

// ... acquire mirror borrow + reborrow copy_scratch for the
// recorder closure ...
```

**Borrow-checker note (folded from codex P1 review)**: the original code re-borrowed `self.copy_scratch.as_mut()` after the pre-flush; with the pre-flush gone, the resize and the defer-release must be carefully scoped so the scratch's `&mut` borrow ends BEFORE the scheduler borrow starts. The block-scoped `let retired = { ... };` above is the load-bearing pattern. **Verify this compiles before the T3 commit** — if rustc rejects it (e.g. NLL inference issues), the implementer should hoist the `Vec<Box<dyn BatchResource>>` capture inline (no `scratch` binding), then defer-release after.

The `needs_grow` check in the existing code becomes redundant — `ensure_size_returning_old` returns `None` on no-grow. The pre-flush block can be deleted in its entirety (lines ~3319–3329).

### Step 5: Gates + commit

- [ ] **Step 5: Validate and commit**

```bash
cargo +nightly fmt
cargo clippy -p yserver
cargo test --workspace
```

Run rendercheck to confirm copy paths still pass:

```bash
just rendercheck-yserver
```

Expected: same pass/fail shape as the pre-T3 baseline. (The user runs rendercheck on hardware; this command runs in CI if the sandbox supports it.)

Commit message:

```text
refactor(kms): defer-release CopyScratch grow (5-T3)

Add CopyScratch::ensure_size_returning_old + RetiredCopyScratchImage
BatchResource impl. Migrate try_vk_copy_area same-overlap caller from
pre-flush + ensure_size to ensure_size_returning_old + defer-release.

Removes one ProtocolBarrier flush from the copy-same hot path on
resize cycles; steady-state cycles (no grow) unaffected.

Catalogue row vk/copy_scratch.rs:86 (temporary) retired.
```

### Done conditions for T3

1. `cargo +nightly fmt --check` clean.
2. `cargo clippy -p yserver` clean.
3. `cargo test --workspace` green.
4. `queue_wait_idle` does NOT appear in `copy_scratch.rs` body of `ensure_size_returning_old` (or `ensure_size` if kept).
5. The `needs_grow` check at the caller site (backend.rs:~3319) is gone.
6. The `flush_if_needed(BatchFlushReason::ProtocolBarrier)` pre-resize call at the same site is gone.
7. `defer_resource_release` is called from the new caller.
8. Single new commit.

---

## Task 4: DstReadback migration — `ensure_returning_old` + caller wiring

**Goal:** Same shape as T3 but for `DstReadback`. Per-format slot management (BGRA + R8) makes the retire wrapper slightly more elaborate; otherwise identical.

**Files:**
- Modify: `crates/yserver/src/kms/vk/dst_readback.rs`
- Modify: `crates/yserver/src/kms/backend.rs`

### Step 1: `RetiredDstReadbackImage` BatchResource impl

- [ ] **Step 1: Add the retire-wrapper**

```rust
use crate::kms::scheduler::paint_batch::BatchResource;

#[derive(Debug)]
struct RetiredDstReadbackImage {
    image: vk::Image,
    view: vk::ImageView,
    no_alpha_view: Option<vk::ImageView>,
    memory: vk::DeviceMemory,
}

impl BatchResource for RetiredDstReadbackImage {
    fn release(self: Box<Self>, vk: &VkContext) {
        unsafe {
            if let Some(v) = self.no_alpha_view {
                vk.device.destroy_image_view(v, None);
            }
            vk.device.destroy_image_view(self.view, None);
            vk.device.destroy_image(self.image, None);
            vk.device.free_memory(self.memory, None);
        }
    }
}
```

### Step 2: `ensure_returning_old`

- [ ] **Step 2: Add the new method on `DstReadback`**

```rust
    /// 5-T4: like `ensure` but returns the old per-format image
    /// wrapped as a `BatchResource` for the caller to defer-release.
    /// Returns `Ok(None)` if no grow was needed.
    pub fn ensure_returning_old(
        &mut self,
        format: vk::Format,
        width: u32,
        height: u32,
    ) -> Result<Option<Box<dyn BatchResource>>, DstReadbackError> {
        let slot = match format {
            vk::Format::B8G8R8A8_UNORM => &mut self.bgra,
            vk::Format::R8_UNORM => &mut self.r8,
            _ => return Err(DstReadbackError::NoMemoryType),
        };
        if let Some(img) = slot.as_ref()
            && img.extent.width >= width
            && img.extent.height >= height
        {
            return Ok(None);
        }
        let new_extent = match slot.as_ref() {
            Some(img) => vk::Extent2D {
                width: img.extent.width.max(width).next_power_of_two().max(64),
                height: img.extent.height.max(height).next_power_of_two().max(64),
            },
            None => vk::Extent2D {
                width: width.next_power_of_two().max(64),
                height: height.next_power_of_two().max(64),
            },
        };
        let new_img = allocate(&self.vk, format, new_extent)?;
        // Allocation succeeded — past here the function MUST NOT fail.
        let retired = slot.take().map(|old| {
            Box::new(RetiredDstReadbackImage {
                image: old.image,
                view: old.view,
                no_alpha_view: old.no_alpha_view,
                memory: old.memory,
            }) as Box<dyn BatchResource>
        });
        *slot = Some(new_img);
        Ok(retired)
    }
```

### Step 3: Delete `ensure` (or keep as deprecated)

- [ ] **Step 3: Audit `DstReadback::ensure` callers**

```bash
rg -n 'dst_readback\.as_mut\(\)\.[^)]*\.ensure\(' crates/yserver/src/kms/
```

If the only caller is the one rewritten in Step 4, delete `ensure`.

### Step 4: Wire `try_vk_render_composite` and `try_vk_render_traps_or_tris` callers

- [ ] **Step 4: Edit `backend.rs` around line ~4995–5020 (traps + composite share the pre-flush block)**

Today the pre-flush block guards BOTH `mask_scratch.needs_image_grow` AND `dst_readback.needs_grow`. After T5 (MaskScratch migration), neither needs a pre-flush. T4's diff handles ONLY the dst_readback half — be careful not to leave a half-deleted block.

**Critical**: T4 lands BEFORE T5. After T4, the pre-flush block still exists but only the mask-grow condition keeps it alive. T5 then removes it entirely. Plan T4's edit so:

1. The `needs_readback_grow` term is removed from the pre-flush block's condition.
2. The dst_readback caller switches to `ensure_returning_old` + `defer_resource_release`.

Replace the `dst_readback.ensure(...)` call (search for `scratch.ensure(dst_format, dst_extent.width, dst_extent.height)`):

```rust
// Apply the same block-scoped borrow pattern as T3 — the
// scratch's &mut borrow MUST end before self.scheduler is
// borrowed. Reborrow the scratch for view extraction afterwards.
let dst_readback_view = if needs_dst_readback {
    let retired = {
        let scratch = self.dst_readback.as_mut().expect("checked above");
        match scratch.ensure_returning_old(
            dst_format,
            dst_extent.width,
            dst_extent.height,
        ) {
            Ok(r) => r,
            Err(e) => {
                log::warn!("vk render_traps: dst readback ensure failed: {e:?}");
                // ... existing error-recovery path (return false /
                // fall back per the pre-T4 code shape)
                return false;
            }
        }
    }; // <-- scratch &mut ends here.
    if let Some(old) = retired {
        let vk_arc = self.vk.as_ref().cloned().expect("vk present");
        let pool_handle = self
            .ops_command_pool
            .as_ref()
            .map(|p| p.handle())
            .expect("pool present");
        self.scheduler
            .defer_resource_release(vk_arc, pool_handle, old);
    }
    // Reborrow self.dst_readback for view extraction (now that
    // the earlier &mut has ended and the scheduler borrow is also
    // done):
    let scratch = self.dst_readback.as_mut().expect("checked above");
    // ... existing view-extraction logic against `scratch` ...
    scratch
        .view(dst_format, dst_has_alpha)
        .ok()
        .flatten()
};
```

And in the pre-flush block:

```rust
// BEFORE:
let needs_mask_grow = self
    .mask_scratch
    .as_ref()
    .is_some_and(|m| m.needs_image_grow(bbox_w, bbox_h));
let needs_readback_grow = needs_dst_readback
    && self
        .dst_readback
        .as_ref()
        .is_some_and(|r| r.needs_grow(dst_format, dst_extent.width, dst_extent.height));
if needs_mask_grow || needs_readback_grow {
    use crate::kms::scheduler::paint_batch::BatchFlushReason;
    if let Err(e) = self.flush_if_needed(BatchFlushReason::ProtocolBarrier) {
        log::warn!("vk render_traps: pre-resize flush failed: {e:?}");
        return false;
    }
}

// AFTER (T4 only — mask half still alive; T5 will remove the rest):
let needs_mask_grow = self
    .mask_scratch
    .as_ref()
    .is_some_and(|m| m.needs_image_grow(bbox_w, bbox_h));
if needs_mask_grow {
    use crate::kms::scheduler::paint_batch::BatchFlushReason;
    if let Err(e) = self.flush_if_needed(BatchFlushReason::ProtocolBarrier) {
        log::warn!("vk render_traps: pre-resize flush failed: {e:?}");
        return false;
    }
}
```

The `render_composite` (3F-1) site uses the same `DstReadback::ensure` pattern at `crates/yserver/src/kms/backend.rs:~6081-6122` (codex confirmed the line range during plan review). Apply the same edit pattern: scope the `DstReadback::ensure_returning_old` call in a block, defer-release after, narrow any `needs_grow` term out of its pre-flush gate. If 3F-1's pre-flush gate is dst_readback-only (no mask term), the gate can be deleted outright here in T4 (T5 only needs to touch the render-traps gate).

### Step 5: Gates + commit

- [ ] **Step 5: Validate and commit**

```bash
cargo +nightly fmt
cargo clippy -p yserver
cargo test --workspace
just rendercheck-yserver  # render paths
```

Commit message:

```text
refactor(kms): defer-release DstReadback grow (5-T4)

Add DstReadback::ensure_returning_old + RetiredDstReadbackImage
BatchResource impl. Migrate render_composite (3F-1) and
render_traps_or_tris (3F-2) callers to ensure_returning_old +
defer-release.

Removes the dst_readback half of the render-traps pre-flush block;
T5 will remove the mask half next.

Catalogue row vk/dst_readback.rs:105 (temporary) retired.
```

### Done conditions for T4

1. `cargo +nightly fmt --check` clean.
2. `cargo clippy -p yserver` clean.
3. `cargo test --workspace` green; rendercheck no regressions.
4. The `needs_readback_grow` term in the render_traps pre-flush block is gone.
5. The dst_readback caller in render_composite (3F-1) uses `ensure_returning_old`.
6. Single new commit.

---

## Task 5: MaskScratch migration — `ensure_image_size_returning_old` + caller wiring

**Goal:** Same shape as T3/T4 but for `MaskScratch`. After T5 the render-traps pre-flush block is entirely gone.

**Files:**
- Modify: `crates/yserver/src/kms/vk/mask_scratch.rs`
- Modify: `crates/yserver/src/kms/backend.rs`

### Step 1: `RetiredMaskScratchImage` BatchResource impl

- [ ] **Step 1: Add the retire-wrapper**

```rust
use crate::kms::scheduler::paint_batch::BatchResource;

#[derive(Debug)]
struct RetiredMaskScratchImage {
    image: vk::Image,
    view: vk::ImageView,
    image_memory: vk::DeviceMemory,
}

impl BatchResource for RetiredMaskScratchImage {
    fn release(self: Box<Self>, vk: &VkContext) {
        unsafe {
            vk.device.destroy_image_view(self.view, None);
            vk.device.destroy_image(self.image, None);
            vk.device.free_memory(self.image_memory, None);
        }
    }
}
```

### Step 2: `ensure_image_size_returning_old`

- [ ] **Step 2: Add the new method on `MaskScratch`**

```rust
    /// 5-T5: like `ensure_image_size` but returns the old image
    /// wrapped as a `BatchResource` for the caller to defer-release
    /// through the scheduler. Returns `Ok(None)` if no grow was
    /// needed.
    pub fn ensure_image_size_returning_old(
        &mut self,
        width: u32,
        height: u32,
    ) -> Result<Option<Box<dyn BatchResource>>, MaskScratchError> {
        if width <= self.extent.width && height <= self.extent.height {
            return Ok(None);
        }
        let new_extent = vk::Extent2D {
            width: self.extent.width.max(width).next_power_of_two().max(256),
            height: self.extent.height.max(height).next_power_of_two().max(256),
        };
        let (image, view, image_memory) = allocate_image(&self.vk, new_extent)?;
        // Allocation succeeded — past here the function MUST NOT fail.
        let old_image = std::mem::replace(&mut self.image, image);
        let old_view = std::mem::replace(&mut self.view, view);
        let old_memory = std::mem::replace(&mut self.image_memory, image_memory);
        self.extent = new_extent;
        self.current_layout = vk::ImageLayout::UNDEFINED;
        Ok(Some(Box::new(RetiredMaskScratchImage {
            image: old_image,
            view: old_view,
            image_memory: old_memory,
        })))
    }
```

### Step 3: Delete `ensure_image_size` (or keep deprecated)

- [ ] **Step 3: Audit callers**

```bash
rg -n 'mask_scratch.*ensure_image_size\b' crates/yserver/src/kms/
```

If only the rewritten render_traps caller remains, delete the old `ensure_image_size`.

### Step 4: Wire `try_vk_render_traps_or_tris` caller

- [ ] **Step 4: Edit `backend.rs` around line ~5013–5021 (the mask ensure_image_size call) AND delete the residual pre-flush block from T4**

Today (post-T4):

```rust
let needs_mask_grow = self
    .mask_scratch
    .as_ref()
    .is_some_and(|m| m.needs_image_grow(bbox_w, bbox_h));
if needs_mask_grow {
    use crate::kms::scheduler::paint_batch::BatchFlushReason;
    if let Err(e) = self.flush_if_needed(BatchFlushReason::ProtocolBarrier) {
        log::warn!("vk render_traps: pre-resize flush failed: {e:?}");
        return false;
    }
}

// 3F-2: ensure_image_size now happens outside the closure ...
if let Err(e) = self
    .mask_scratch
    .as_mut()
    .expect("checked above")
    .ensure_image_size(bbox_w, bbox_h)
{
    log::warn!("vk render_traps: mask ensure_image_size failed: {e:?}");
    return false;
}
```

Replace with:

```rust
// 5-T5: defer-release replaces pre-flush. The old mask image is
// adopted into the scheduler's retire flow so it survives any
// in-flight CB.
//
// Block-scoped borrow per the T3 pattern: scratch's &mut ends
// before scheduler is borrowed; reborrow for view/extent below.
let retired = {
    let scratch = self.mask_scratch.as_mut().expect("checked above");
    match scratch.ensure_image_size_returning_old(bbox_w, bbox_h) {
        Ok(r) => r,
        Err(e) => {
            log::warn!("vk render_traps: mask ensure_image_size failed: {e:?}");
            return false;
        }
    }
}; // <-- scratch &mut ends here.
if let Some(old) = retired {
    let vk_arc = self.vk.as_ref().cloned().expect("vk present");
    let pool_handle = self
        .ops_command_pool
        .as_ref()
        .map(|p| p.handle())
        .expect("pool present");
    self.scheduler
        .defer_resource_release(vk_arc, pool_handle, old);
}
// Reborrow self.mask_scratch (as &ref) for view + extent below.
// This is the existing 3F-2 pattern; preserve verbatim.
```

The `needs_mask_grow` check is gone. The pre-flush block is entirely gone.

### Step 5: Gates + commit

- [ ] **Step 5: Validate and commit**

```bash
cargo +nightly fmt
cargo clippy -p yserver
cargo test --workspace
just rendercheck-yserver  # render paths
```

Commit message:

```text
refactor(kms): defer-release MaskScratch grow (5-T5)

Add MaskScratch::ensure_image_size_returning_old +
RetiredMaskScratchImage BatchResource impl. Migrate render_traps_or_tris
caller to the new variant. The render-traps pre-flush block is now
entirely gone (T4 cleared the dst_readback half).

Catalogue row vk/mask_scratch.rs:110 (temporary) retired.
```

### Done conditions for T5

1. `cargo +nightly fmt --check` clean.
2. `cargo clippy -p yserver` clean.
3. `cargo test --workspace` green; rendercheck no regressions.
4. The `needs_mask_grow` check at the caller site is gone.
5. The `flush_if_needed(BatchFlushReason::ProtocolBarrier)` block in render_traps is entirely gone.
6. Single new commit.

---

## Task 6: Delete redundant `queue_wait_idle`s in `OpsStaging::ensure` and `GlyphAtlas::grow_staging`

**Goal:** Both grow paths' `queue_wait_idle` calls are redundant because their callers wait per-op (readback fence post-T1; per-glyph wait in `GlyphAtlas::intern` — still queue_wait_idle, out of scope for Phase 5 but the property still holds). Delete the calls; document why with reference to the per-op wait.

**Files:**
- Modify: `crates/yserver/src/kms/vk/ops/mod.rs`
- Modify: `crates/yserver/src/kms/vk/glyph.rs`

### Step 1: `OpsStaging::ensure` cleanup

- [ ] **Step 1: Delete the `queue_wait_idle` in `OpsStaging::ensure`**

In `vk/ops/mod.rs`, find the body of `OpsStaging::ensure` (lines ~158–178). Replace:

```rust
        let (buffer, memory, mapped) = allocate_ops_staging(&self.vk, new_size)?;
        unsafe {
            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
            self.vk.device.unmap_memory(self.memory);
            self.vk.device.destroy_buffer(self.buffer, None);
            self.vk.device.free_memory(self.memory, None);
        }
```

With:

```rust
        let (buffer, memory, mapped) = allocate_ops_staging(&self.vk, new_size)?;
        // 5-T6: no queue_wait_idle. All callers of `OpsStaging`
        // (`hw_cursor_refresh`, `read_mirror_pixels`,
        // `try_vk_get_image_pixels`, `dump_scanout_one`) go through
        // `run_one_shot_op` which after 5-T1 waits on a per-op
        // fence before returning. The immediately-prior readback's
        // CB therefore has retired before we get here, and the OLD
        // staging buffer can be freed without any additional wait.
        // If a future caller takes this buffer through a non-waiting
        // path, this comment block becomes the audit point — DO NOT
        // remove without re-auditing.
        unsafe {
            self.vk.device.unmap_memory(self.memory);
            self.vk.device.destroy_buffer(self.buffer, None);
            self.vk.device.free_memory(self.memory, None);
        }
```

### Step 2: `GlyphAtlas::grow_staging` cleanup

- [ ] **Step 2: Delete the `queue_wait_idle` in `GlyphAtlas::grow_staging`**

In `vk/glyph.rs`, find the body of `grow_staging` (lines ~437–454). Replace:

```rust
        let (buffer, memory, mapped) = allocate_staging(&self.vk, new_size)?;
        unsafe {
            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
            self.vk.device.unmap_memory(self.staging_memory);
            self.vk.device.destroy_buffer(self.staging_buffer, None);
            self.vk.device.free_memory(self.staging_memory, None);
        }
```

With:

```rust
        let (buffer, memory, mapped) = allocate_staging(&self.vk, new_size)?;
        // 5-T6: no queue_wait_idle. `grow_staging`'s only caller is
        // `GlyphAtlas::intern`, which submits per-glyph one-shot CBs
        // and waits on each (today via `queue_wait_idle` — still in
        // intern, deliberate Phase 5 deferred; tomorrow via a
        // per-glyph fence in a follow-up phase). At the time
        // `grow_staging` runs, either:
        //   (a) there has been no prior glyph upload yet (first
        //       grow: no CB ever referenced this buffer); OR
        //   (b) the last successful glyph upload has already been
        //       waited on (intern's per-glyph wait drained it).
        // In either case the OLD staging buffer has no live CB
        // reference and is safe to free synchronously. If a future
        // caller takes the atlas staging through a non-waiting
        // path, this comment block becomes the audit point.
        unsafe {
            self.vk.device.unmap_memory(self.staging_memory);
            self.vk.device.destroy_buffer(self.staging_buffer, None);
            self.vk.device.free_memory(self.staging_memory, None);
        }
```

### Step 3: Catalogue grep

- [ ] **Step 3: Verify `queue_wait_idle` is gone from non-`Drop` sites**

```bash
rg -n 'queue_wait_idle' crates/yserver/src/kms/
```

Expected hits **only**:
- `vk/ops/mod.rs:59` — `OpsCommandPool::drop`
- `vk/ops/mod.rs:184` — `OpsStaging::drop`
- `vk/glyph.rs:460` — `GlyphAtlas::drop`
- `vk/copy_scratch.rs:??` — `CopyScratch::drop`
- `vk/dst_readback.rs:??` — `DstReadback::drop`
- `vk/gradient.rs:??` — `GradientPicture::drop`
- `vk/mask_scratch.rs:??` — `MaskScratch::drop`
- `vk/pipeline.rs:??` — `CompositorPipeline::drop`
- `vk/text_pipeline.rs:??` — `TextPipeline::drop`
- `vk/logic_fill_pipeline.rs:??` — `LogicFillPipelineCache::drop`
- `vk/render_pipeline.rs:??` — `RenderPipelineCache::drop`, `SolidColorImage::drop`
- `vk/target.rs:??` — `DrawableImage::initialize_clear` (the `target.rs:735` row from the catalogue, classified `sync` removal phase 4 — actually a deferred follow-up; cross-check whether it was retired)
- `vk/scanout.rs:??` — `ScanoutBoPool::drain_all_pending`
- `vk/device.rs:??` — `VkContext::drop`

If `target.rs::initialize_clear` still has its `queue_wait_idle`, note it in the results doc as a Phase 5 follow-up (it's a one-shot CB on `DrawableImage` creation — same pattern as `run_one_shot_op`, can be fenced the same way in a follow-up). DO NOT widen Phase 5 scope to grab it; results doc notes it instead.

### Step 4: Gates + commit

- [ ] **Step 4: Validate and commit**

```bash
cargo +nightly fmt
cargo clippy -p yserver
cargo test --workspace
```

Commit message:

```text
refactor(kms): delete redundant queue_wait_idle in OpsStaging::ensure
and GlyphAtlas::grow_staging (5-T6)

Both grow paths' waits are redundant — their callers wait per-op
before the grow code runs. Doc-comments mark the audit precondition
for future callers.

Catalogue rows vk/ops/mod.rs:168 + vk/glyph.rs:444 (both temporary)
retired.
```

### Done conditions for T6

1. `cargo +nightly fmt --check` clean.
2. `cargo clippy -p yserver` clean.
3. `cargo test --workspace` green.
4. `queue_wait_idle` does NOT appear in `OpsStaging::ensure` or `GlyphAtlas::grow_staging`.
5. The doc-comments explaining the deletion are present in both bodies.
6. Single new commit.

---

## Task 7: Results doc

**Goal:** Write `docs/superpowers/plans/2026-05-14-rendering-rearchitecture-phase5-results.md` mirroring Phase 4's results-doc format. Include:

- Scope landed (per-task summary with commits)
- Preflight checks (fmt/clippy/test output)
- Cutover greps (catalogue rows retired, before/after)
- Done conditions table (all from T1–T6 + the overall phase-5 list below)
- Hardware smoke results placeholders (user-owned)
- Plan bugs caught (folded back into this plan)
- Commit summary table
- Known deferred items (atlas intern per-glyph wait, Phase 6 refcounted handles, `target.rs::initialize_clear` if still present)
- What's next

**Files:**
- Create: `docs/superpowers/plans/2026-05-14-rendering-rearchitecture-phase5-results.md`
- Update: `docs/status.md` (Phase 5 entry: move from "Remaining" to "Done"; add commits + results-doc pointer)

### Step 1: Write the results doc

- [ ] **Step 1: Mirror Phase 4's results structure**

Source: `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase4-results.md`. Copy section headings; replace content with Phase 5's specifics.

### Step 2: Update status.md

- [ ] **Step 2: Move Phase 5 entry to "Done" in status.md**

Find the "### Remaining — in priority order" Phase 5 entry, copy to the "### Done" section above the "Inter-phase chores landed alongside" block, mark all commit SHAs from T1–T6, and add the results-doc pointer. Adjust the "Remaining" list (Phase 5 row gone; Phase 6 promoted; pixmap-pool stays first).

### Step 3: Commit

- [ ] **Step 3: Commit the results doc + status update**

Commit message:

```text
docs(plans): phase 5 results — readback fence + scratch grow defer-release
```

### Done conditions for T7

1. `docs/superpowers/plans/2026-05-14-rendering-rearchitecture-phase5-results.md` exists.
2. `docs/status.md` has Phase 5 in the "Done" section.
3. Single commit.

---

## Phase-level Done conditions

Aggregating across T1–T7:

1. `queue_wait_idle` calls in non-`Drop` non-modeset bodies are reduced to **at most one** site: `vk/target.rs::initialize_clear` if still present (out of scope — separate follow-up). After T1 + T6, every other direct `queue_wait_idle` outside `Drop` impls and `ScanoutBoPool::drain_all_pending` (modeset teardown) is gone. **Note on `GlyphAtlas::intern`**: post-T1 it no longer has a direct `queue_wait_idle` — its per-glyph wait now lives inside `run_one_shot_op` as `wait_for_fences`. The deferral framing for `intern` is therefore "still uses the per-glyph one-shot submit-and-wait pattern (now fence-narrowed), not yet batched into PaintBatch" — that's separate from the `queue_wait_idle` retirement program and tracked as a Phase-5 followup.
2. `queue_wait_idle` is NOT inside `run_one_shot_op`'s body.
3. `RenderScheduler::defer_resource_release` exists, with both `Synchronous` and `AdoptOpen` branches.
4. Each of `CopyScratch`, `DstReadback`, `MaskScratch` has an `ensure_*_returning_old` method that returns `Option<Box<dyn BatchResource>>`. The corresponding `Retired*Image` BatchResource impls exist.
5. The three pre-flush gates in `backend.rs` (3D CopyScratch site, 3F-1 DstReadback site, 3F-2 MaskScratch+DstReadback site) are entirely gone.
6. `cargo +nightly fmt --check`, `cargo clippy -p yserver`, `cargo test --workspace` all green.
7. `just rendercheck-yserver` regression-free vs the Phase 4 baseline (T7 results doc captures the comparison).
8. `docs/status.md` reflects Phase 5 done; `docs/superpowers/plans/2026-05-14-rendering-rearchitecture-phase5-results.md` exists.

---

## Smoke plan (T7 hardware section)

Hardware smoke is user-owned. Two checks:

### Check 1 — non-regression smoke

```bash
just yserver-mate-hw-release   # or just yserver-xfce-hw
```

- Steady-state MATE session should feel indistinguishable from post-Phase-4 (no new lag, no rendering corruption).
- Window resize bursts should NOT lag more than pre-T3/T4/T5 (the pre-flush gates we removed only fired on resize cycles; their absence should be invisible at steady state and net-positive on resize).
- `dmesg` / journal: no new amdgpu / intel-i915 errors.

### Check 2 — readback wait scope (synthetic)

- xterm `import -window root window.ppm` (uses GetImage). Should be fast.
- SIGUSR1 scanout dump from a running yserver should produce a valid PPM (`dump_scanout_one` path, T1 site).

### Check 3 — rendercheck no regressions

```bash
just rendercheck-yserver
```

Compare pass/fail counts vs the Phase 4 baseline captured in `2026-05-13-rendering-rearchitecture-phase4-results.md`. Identical shape expected.

### Check 4 — adapta-nokto + mate-cc on bee

- **Phase 5 is not expected to materially close the bee/RDNA2 lag.** Capture a fresh perf snapshot anyway for the results doc; reconcile against the post-3F-2 profile. The narrative the result reinforces is "wait-idle is gone from the paint hot path; remaining lag is downstream."

---

## Codex review checkpoints

After each task's commit, run a codex review pass per `commit-commands:commit` / `codex:codex` workflow:

1. **After T1**: focus on the 4-path failure taxonomy. Path 2 (wait fails after submit) must clearly leak the fence + CB; reviewers should flag any double-free, any free-on-Err that doesn't preserve the leak contract.
2. **After T2**: focus on the `DeferDecision::AdoptOpen` lazy-batch-open subtlety. Reviewer should confirm that an Idle batch with no CB but with adopted resources retires correctly (calls `retire_now`, which walks `retire_resources`).
3. **After T3/T4/T5**: focus on the borrow-checker boundary between scratch-grow and the recorder closure. Reviewer should confirm that the defer-release call happens BEFORE the recorder closure (so the scratch's NEW image is in place when the recorder runs).
4. **After T6**: focus on the audit comment — reviewer should confirm the precondition holds today and that the comment makes future-violation detectable.

Fold P0/P1 codex findings as a fix-up commit per task (same pattern as Phase 4 T1's `b86bfbb`). DO NOT amend.

---

## Glossary

- **defer-release**: Adopting an old Vulkan handle into the open paint batch (or releasing it synchronously) so it stays alive until the GPU has stopped referencing it. Replaces the explicit `queue_wait_idle` + `destroy_*` pattern.
- **`BatchResource`**: Existing trait (Phase 3A). Implementors are released at `retire_now()` or at `Drop` of the owning `PaintBatch`.
- **`Retired*Image`**: Phase-5-local BatchResource impls wrapping a scratch's old handles for defer-release.
- **`defer_resource_release`**: Phase 5's new scheduler-side helper. Pure dispatcher over `DeferDecision`.
- **Pre-flush gate**: The Phase 3D/3F-1/3F-2 pattern of `if scratch.needs_grow() { flush_if_needed(ProtocolBarrier) }`. Phase 5 removes all three; defer-release replaces them.
- **Path-2 / device-lost contract**: Phase-4-defined invariant — on wait failure, Vulkan handles are leaked, function returns Err, caller treats the renderer as fatal.
