# Phase 4 — sync rework: retire close-time vkQueueWaitIdle — results

Date: 2026-05-13
Plan: `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase4.md`
Branch: `graphics-followups`
Predecessor: `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3f-2-results.md`

## Scope landed

Phase 4 retired the close-time `vkQueueWaitIdle` from `PaintBatch::submit_and_wait` and built the async-retirement machinery around the new per-batch fence so best-effort flushes no longer block the X-protocol input loop on GPU drain. The strict-flush path keeps its synchronous wait contract (callers that need data-back semantics — readback handlers, drawable destruction, protocol-barriered ops — get the same observable behaviour as pre-4), but waits on a private fence instead of draining the whole graphics queue. Best-effort flushes (`MaxOpsReached`, `FlipReadiness`) record + submit and return immediately; retirement happens on the next composite tick (`poll_in_flight` → `poll_retired_paint_batches`). A backpressure cap of 4 in-flight batches bounds queue depth, and a shutdown drain pairs with `vkDeviceWaitIdle` to walk the queue cleanly so we exit with no leaked-Submitted warnings.

The Phase 4 target documented in `docs/status.md` is closed: `queue_wait_idle` is gone from `paint_batch.rs`. Phase 4's GPU handoff to composite stays correct because each paint recorder ends its CB with an explicit barrier to `SHADER_READ_ONLY_OPTIMAL` and composite is submitted later on the same graphics queue (same-queue submission order + per-CB ending barriers = the proper Vulkan memory dependency; no semaphore handoff needed).

- **T1 (`2135a16`)**: `PaintBatch::submit_and_wait` swapped `vkQueueWaitIdle` for `create_fence` + `queue_submit2(.., fence)` + `wait_for_fences(&[fence], true, u64::MAX)`. Added `fence: Option<vk::Fence>` field, plumbed through 4 distinct failure paths (1a fence-create fail, 1b submit fail, 2 wait fail, 3 success) with the path-2 leak contract documented in detail. Strict-flush callers observe the same blocking semantics; the wait is just narrower (this submission only, not the whole queue).
- **T1 fix-up (`b86bfbb`)**: tightened T1 docs after the code-quality reviewer flagged misleading "mechanically identical" wording for path 1a, a `release_holder` landmine (a future T2 holders wiring would silently destroy a still-in-flight fence), and `poison()` fence-blindness in the defensive helper. All three folded into landmine comments at the relevant sites so the next phase doesn't trip over them.
- **T2 (`642d544`)**: async-retirement building blocks added — `PaintBatch::submit_async` (submit-without-wait, returns to caller in Submitted state with the fence in flight), `try_retire_if_signaled` (non-blocking `get_fence_status` probe; retires the batch if the fence is signalled, returns `Pending` otherwise), `wait_for_completion` (blocking `wait_for_fences` for backpressure + drain). Building blocks only; no caller change yet.
- **T3 (`6fe4a71`)**: building blocks wired into `RenderScheduler`. New field `submitted_paint_batches: VecDeque<PaintBatch>`, method `close_and_submit_async` (uses `submit_async` + pushes onto the queue), `poll_retired_paint_batches` (called from `poll_in_flight` at the top of composite tick — walks the front of the queue, retires every signalled batch). `flush_if_needed` now branches by reason: strict reasons (`ProtocolBarrier`, `Readback`, `DrawableDestruction`, `EndOfFrame`) go through `close_and_submit` (synchronous); best-effort reasons (`MaxOpsReached`, `FlipReadiness`) go through `close_and_submit_async`.
- **T4 (`49ff484`)**: backpressure cap — `const MAX_IN_FLIGHT_PAINT_BATCHES: usize = 4`. When `close_and_submit_async` would push onto a full queue, it `pop_front`s the oldest and `wait_for_completion`s it before queuing the new batch. Prevents unbounded fence accumulation under burst-paint workloads.
- **T5 (`f68d8c2`)**: shutdown drain — `RenderScheduler::drain_submitted_paint_batches` walks the queue retiring every batch (each `wait_for_completion` is a no-op if the fence is signalled, blocking otherwise). Called from the backend shutdown path **after** `vkDeviceWaitIdle()` so when the device-wide wait returns, every in-flight paint batch is by definition signalled and the drain is a cheap walk. Eliminates leaked-Submitted warnings at process exit.

## Preflight checks

End of Phase 4 (HEAD = `f68d8c2`, plus the upcoming T6 docs commit):

- `cargo +nightly fmt --check` — clean (no diff, exit 0).
- `cargo clippy -p yserver` — 5 pre-existing `doc_lazy_continuation` warnings (`backend.rs:33`, `backend.rs:73`, `backend.rs:74`, `vk/pipeline.rs:104`, and one sibling site). No new warnings.
- `cargo test --workspace`:
  - `yserver` lib: **138 passed, 0 failed, 3 ignored**.
  - `yserver` binary integration (`ynest`): 9 passed.
  - `yserver-core`: **284 passed**.
  - `yserver-protocol`: **208 passed**.
  - `fixture_smoke`: 2 passed, 1 ignored.
  - Other test binaries: green (1 with 17 ignored, 1 with 1 ignored, 3 with 0 passed — same shape as 3F-2).

## Cutover greps

Captured semantically. Line numbers are informational and will drift; the load-bearing claim is the SITE list.

```
$ rg -n 'queue_wait_idle' crates/yserver/src/kms/scheduler/paint_batch.rs
(no hits)
```

ZERO. The Phase 4 target is gone from `paint_batch.rs`. Other `queue_wait_idle` call sites in the tree (Drop impls for scratch resources, `run_one_shot_op` body, the `ensure_size` grow paths) stay — they're Phase 5 / Phase 6 scope, not the paint hot path.

```
$ rg -n 'wait_for_fences' crates/yserver/src/kms/scheduler/paint_batch.rs
356:    /// narrows the wait from `queue_wait_idle` (all queue) to
357:    /// `wait_for_fences` on this batch's own fence — composite-
380:    /// 2.  **Wait fails** (`queue_submit2` Ok, `wait_for_fences`
418:        // Then wait_for_fences on it instead of the broad
463:        match unsafe { self.vk.device.wait_for_fences(&fences, true, u64::MAX) } {
482:                    "PaintBatch::submit_and_wait: wait_for_fences failed ({e:?}); \
508:    /// `wait_for_fences` themselves can do so without going back
609:    /// branch: on `wait_for_fences` error, the batch is left in
620:        match unsafe { self.vk.device.wait_for_fences(&fences, true, u64::MAX) } {
630:                    "PaintBatch::wait_for_completion: wait_for_fences failed \
```

Two real call sites — `463` in `submit_and_wait`, `620` in `wait_for_completion`. The other hits are docs / log strings. Matches the plan's "exactly two places" Done condition.

```
$ rg -n 'get_fence_status' crates/yserver/src/kms/scheduler/paint_batch.rs
582:        let status = unsafe { self.vk.device.get_fence_status(fence) };
594:                    "PaintBatch::try_retire_if_signaled: get_fence_status failed \
```

One real call site at `582` in `try_retire_if_signaled`. The second hit is a log string. Matches the plan's "exactly one place" Done condition.

```
$ rg -n 'submitted_paint_batches' crates/yserver/src/kms/scheduler/
crates/yserver/src/kms/scheduler/mod.rs:46:    pub submitted_paint_batches: std::collections::VecDeque<PaintBatch>,
crates/yserver/src/kms/scheduler/mod.rs:101:    /// paint batch and moves it to `submitted_paint_batches` for
crates/yserver/src/kms/scheduler/mod.rs:126:        while self.submitted_paint_batches.len() >= MAX_IN_FLIGHT_PAINT_BATCHES {
crates/yserver/src/kms/scheduler/mod.rs:127:            let Some(mut oldest) = self.submitted_paint_batches.pop_front() else {
crates/yserver/src/kms/scheduler/mod.rs:141:                    self.submitted_paint_batches.push_back(batch);
crates/yserver/src/kms/scheduler/mod.rs:167:        while let Some(batch) = self.submitted_paint_batches.front_mut() {
crates/yserver/src/kms/scheduler/mod.rs:169:                self.submitted_paint_batches.pop_front();
crates/yserver/src/kms/scheduler/mod.rs:197:    pub fn drain_submitted_paint_batches(&mut self) -> Result<(), BatchError> {
crates/yserver/src/kms/scheduler/mod.rs:198:        while let Some(mut batch) = self.submitted_paint_batches.pop_front() {
crates/yserver/src/kms/scheduler/mod.rs:208:        self.submitted_paint_batches.len()
```

Field decl at `46`, backpressure check at `126..127`, push in `close_and_submit_async` at `141`, pop in `poll_retired_paint_batches` at `167..169`, drain in `drain_submitted_paint_batches` at `197..198`, len in `pending_paint_batches` at `208`. Matches the plan's expected layout.

```
$ rg -n 'close_and_submit_async|close_and_submit\(' crates/yserver/src/kms/backend.rs
1595:        // Best-effort reasons use the async path: close_and_submit_async
1599:            self.scheduler.close_and_submit(dirty_outputs)
1602:                .close_and_submit_async(dirty_outputs)
```

Both call sites at `1599` (strict) + `1602` (async). They live inside `flush_if_needed`, branching by `BatchFlushReason::is_strict()`. Matches Done condition 9.

```
$ rg -n 'poll_retired_paint_batches' crates/yserver/src/kms/
crates/yserver/src/kms/backend.rs:6909:        if let Err(e) = self.scheduler.poll_retired_paint_batches() {
crates/yserver/src/kms/scheduler/mod.rs:42:    /// `poll_retired_paint_batches` when each batch's fence
crates/yserver/src/kms/scheduler/mod.rs:165:    pub fn poll_retired_paint_batches(&mut self) -> Result<usize, BatchError> {
```

Definition at `mod.rs:165` + one call site at `backend.rs:6909`. The backend call lives inside `poll_in_flight`, at the top before the existing output-frame polling. Matches Done condition 10.

```
$ rg -n 'drain_submitted_paint_batches' crates/yserver/src/kms/
crates/yserver/src/kms/backend.rs:8287:        if let Err(e) = self.scheduler.drain_submitted_paint_batches() {
crates/yserver/src/kms/backend.rs:8289:                "shutdown: drain_submitted_paint_batches failed ({e:?}); \
crates/yserver/src/kms/scheduler/mod.rs:197:    pub fn drain_submitted_paint_batches(&mut self) -> Result<(), BatchError> {
```

Definition at `mod.rs:197` + one call site at `backend.rs:8287`, which sits in the shutdown path **after** the device-wide `vkDeviceWaitIdle()` per the plan. Matches Done condition 12.

```
$ rg -n 'MAX_IN_FLIGHT_PAINT_BATCHES' crates/yserver/src/kms/
crates/yserver/src/kms/scheduler/mod.rs:34:const MAX_IN_FLIGHT_PAINT_BATCHES: usize = 4;
crates/yserver/src/kms/scheduler/mod.rs:126:        while self.submitted_paint_batches.len() >= MAX_IN_FLIGHT_PAINT_BATCHES {
```

Constant defined at `mod.rs:34`, used at `mod.rs:126` for the backpressure check inside `close_and_submit_async`. Private to the scheduler module — tuning is local. Matches Done condition 11.

## Done conditions

Per the plan's 13 Done conditions in section "## Done conditions":

1. ✅ `cargo +nightly fmt --check` clean (exit 0).
2. ✅ `cargo clippy -p yserver` produces 5 pre-existing `doc_lazy_continuation` warnings; no new warnings.
3. ✅ `cargo test --workspace` green; `yserver` lib **138 passed**.
4. ✅ `queue_wait_idle` does NOT appear in `paint_batch.rs` (grep returns ZERO in that file). Other call sites (Drop impls in `mask_scratch` / `dst_readback` / `copy_scratch` / `solid_color_image` / `gradient`, scratch grow paths, `run_one_shot_op` body) remain — they are Phase 5 / Phase 6 scope, not the per-frame paint hot path.
5. ✅ `PaintBatch` has a `fence: Option<vk::Fence>` field. Allocated in `submit_and_wait` (T1, `2135a16`) before the `queue_submit2` call (paint_batch.rs:435) and in `submit_async` (T2, `642d544`); destroyed in `retire_now`; leaked on path-2 wait failure per the documented contract.
6. ✅ `wait_for_fences` is called in exactly two real places in `paint_batch.rs`: `submit_and_wait` at `463` and `wait_for_completion` at `620`. Matches the cutover grep above.
7. ✅ `get_fence_status` is called in exactly one place in `paint_batch.rs`: `try_retire_if_signaled` at `582`. Matches the cutover grep above.
8. ✅ `RenderScheduler` has `submitted_paint_batches: VecDeque<PaintBatch>` (`mod.rs:46`). `close_and_submit_async` (`mod.rs:~110`), `poll_retired_paint_batches` (`mod.rs:165`), `pending_paint_batches` (`mod.rs:~207`) all exist. Verified via the `submitted_paint_batches` grep across scheduler files.
9. ✅ `flush_if_needed` branches by strict vs best-effort and calls `close_and_submit` (strict, `backend.rs:1599`) or `close_and_submit_async` (best-effort, `backend.rs:1602`).
10. ✅ `poll_in_flight` calls `poll_retired_paint_batches` at its top, before the existing output-frame polling. Verified via grep — single call site at `backend.rs:6909`.
11. ✅ Backpressure: `close_and_submit_async` blocks on the oldest fence (`wait_for_completion`) when `submitted_paint_batches.len() >= MAX_IN_FLIGHT_PAINT_BATCHES` (constant = 4 at `mod.rs:34`, check at `mod.rs:126`).
12. ✅ `drain_submitted_paint_batches` exists on `RenderScheduler` (`mod.rs:197`) and is called from the shutdown path at `backend.rs:8287` **after** `vkDeviceWaitIdle()`. After shutdown returns, `submitted_paint_batches.is_empty()` (every batch retired by the drain).
13. ⏳ **TBD — pending the user's hardware smoke**. See "Hardware smoke results" below.

## Hardware smoke results

Hardware smoke is user-owned (separate TTY on bare metal). The user runs `just yserver-mate-hw-release` or `just yserver-xfce-hw`, plus `just rendercheck-yserver`, and fills in the subsections below.

**Phase 4 expectation**: non-adapta-nokto workloads should feel noticeably better than pre-4 because the core loop no longer blocks on `queue_wait_idle` per composite — best-effort flushes return immediately and retire asynchronously on the composite tick. The adapta-nokto + mate-cc on `bee` reproducer (`docs/known-issues.md` lines 409+) is **not** expected to materially close per the post-3F-2 perf profile (`submit_and_wait` was 0.09% children — the bottleneck is amdgpu ioctl rate, not the close-time wait). If unchanged: confirms AMD investigation phase (per `project_amd_lag_investigation.md` memory) is still needed. If better: surprise — and the close-time wait was hiding something the profile missed.

### Host

TBD.

### General smoke

TBD. Was MATE under default theme clean? Theme switching responsive? rendercheck regression check vs 3F-2 baseline?

### Subjective input fluidity vs pre-4

TBD. Phase 4 expectation: non-adapta-nokto workloads should feel noticeably better because the core loop no longer blocks on `queue_wait_idle` per composite. Best-effort flushes (`MaxOpsReached` mid-paint, `FlipReadiness` pre-flip) return immediately and retire on the composite tick poll. Where this should be most visible: scroll responsiveness in xterm / wezterm under typical workloads, drag latency in fvwm3, cursor motion under heavy paint.

### adapta-nokto + mate-cc on bee

TBD. Per the post-3F-2 profile (`submit_and_wait` 0.09% children — `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3f-2-results.md` cross-ref via `docs/known-issues.md` lines 409+), Phase 4 should NOT materially close the bee/RDNA2 lag. The bottleneck on this reproducer is amdgpu ioctl rate, not the close-time wait. If unchanged: confirms AMD investigation phase is still needed. If better: surprise — capture a fresh perf trace and reconcile against the post-3F-2 profile.

### silence + adapta-nokto if tested Friday

TBD. The Polaris10 + Arch-recent-kernel test point per `project_amd_lag_investigation.md`. This data point is load-bearing for the next-phase decision: if `silence` reproduces the same catastrophic lag as `bee` under adapta-nokto, the AMD investigation phase becomes the next move (broad recent-amdgpu regression hypothesis). If `silence` is fine, the lag is `bee`/RDNA2-specific and Phase 5 (atlas + readback rewrite) is the next move.

### fuji regression check

TBD. Intel was fine pre-Phase 4; should stay fine. Phase 4's strict-flush callers observe the same blocking semantics as pre-4 (just on a narrower wait), so this is a no-regression check, not a delta hunt.

### Anomalies

TBD.

## Plan bugs caught (folded back into plan / fixed in-tree)

### 1. Codex review P1: stale "Phase 4 wires holders" framing (folded into plan at `ea051ad` + `4b1182d`)

The biggest plan-bug catch of the phase. The original Phase 4 plan claimed `OutputFrame::holders + PaintBatch::holders` were load-bearing for Phase 4 — that the composite path's GPU-side visibility would depend on a refcounted-holder handoff between the paint batch and the composite frame. Codex's review caught that this framing was inconsistent with the rest of the plan: T1–T5 never wired holders, the plan's correctness argument actually relied on same-queue submission order + per-CB ending barriers, and the holders field stays at zero across Phase 4. Two commits folded this back: `ea051ad` rewrote the correctness section to ground on same-queue submission order + per-CB barriers (the actual mechanic), and `4b1182d` removed the contradictory holders framing from the intro. **Phase 4 does NOT wire holders.** Visibility is guaranteed by same-queue submission order + per-CB ending barriers, not by holders. Phase 6 (batch-owned refcounted handles) is the right place to wire holders if/when cross-batch / cross-queue dependencies appear.

### 2. Codex review P2: shutdown drain missing (became T5, `f68d8c2`)

The original plan ended at T4 (backpressure). Codex flagged that with the async queue introduced, the shutdown path needed to drain `submitted_paint_batches` explicitly — otherwise process exit could leak Submitted batches (warnings) or, more subtly, race with `VkContext::Drop`. T5 was added as a discrete task: `drain_submitted_paint_batches` called from the backend shutdown path **after** `vkDeviceWaitIdle()` so the drain is a cheap walk over already-signalled fences.

### 3. Codex review recipe: 4-path failure taxonomy (folded into T1 plan + landed in `2135a16` doc)

Codex recommended documenting the four distinct failure paths through `submit_and_wait` in detail — specifically calling out path 2 (wait fails after successful submit) as a leak-not-recover state, not a "try again" state. Folded into the T1 plan and into the T1 commit's doc-comment block on `submit_and_wait` (paint_batch.rs:368..401). The path-2 leak contract is now explicit: CB / fence / arenas / resources all leaked until device destruction, batch stays in `Submitted` forever, caller MUST treat the KMS renderer as failed.

### 4. T1 code-quality review: three follow-ups folded into `b86bfbb`

During T1 implementation, a code-quality reviewer flagged three issues:

- **Misleading "mechanically identical" doc wording for path 1a.** Path 1a (fence-create fail) is NOT mechanically identical to path 1b (submit fail) — 1a has no fence to destroy, 1b has a fence that needs explicit destruction before the early-return. Reworded to call out the difference explicitly.
- **`release_holder` landmine.** A future T2 / Phase 6 wiring of holders would call `release_holder` from `retire_now`, which would silently destroy a still-in-flight fence if the holder is dropped after submit but before fence-wait. Added a landmine comment at the holder-release site so the next implementer doesn't trip over it.
- **`poison()` fence-blindness.** The defensive `poison()` helper does not destroy the fence — for path 1a this is correct (no fence yet), for path 1b it would matter if `poison()` were called from a different site. Documented the precondition.

All three folded into the T1 fix-up `b86bfbb` as doc-comment edits + landmine comments — no behaviour change.

## Commit summary (phase 4)

| Task | Commit | Subject |
|---|---|---|
| Plan | `548116d` | docs(plans): phase 4 implementation plan — retire close-time queue_wait_idle |
| Plan: fold codex review | `ea051ad` | docs(plans): fold codex review feedback into phase 4 |
| Plan: remove stale holders framing | `4b1182d` | docs(plans): remove stale "Phase 4 wires holders" framing from intro |
| T1 | `2135a16` | refactor(kms): replace queue_wait_idle with per-batch VkFence in submit_and_wait |
| T1 fix-up | `b86bfbb` | refactor(kms): tighten T1 doc + add landmine comments |
| T2 | `642d544` | refactor(kms): add submit_async + try_retire_if_signaled + wait_for_completion |
| T3 | `6fe4a71` | refactor(kms): wire async paint-batch retirement into scheduler + flush_if_needed |
| T4 | `49ff484` | refactor(kms): bound submitted_paint_batches queue (backpressure) |
| T5 | `f68d8c2` | refactor(kms): drain submitted_paint_batches on shutdown |
| T6 (results doc) | this commit | docs(plans): phase-4 validation results |

9 commits from plan to T5; 10 with this results doc.

## Known deferred items

- **Phase 5 — per-glyph + readback-handler wait-idle retirement.** `GlyphAtlas::intern`'s per-glyph one-shot upload + `vkQueueWaitIdle` is the remaining big-ticket sync cost on text-heavy / theme-switch workloads — the second half of the `docs/known-issues.md` adapta-nokto + mate-cc root cause (the first half was the traps-side close-time wait, retired piecewise across 3F-2 + Phase 4). The readback handler triplet (`hw_cursor_refresh`, `read_mirror_pixels`, `try_vk_get_image_pixels`) is the natural targeted-VkFence rewrite per the rendering rework HLD. Phase 4 narrowed the close-time wait; Phase 5 closes the remaining per-op wait-idles on paths outside `PaintBatch`.
- **Phase 6 — batch-owned refcounted handles + batched fence destruction + holders model wiring.** Phase 4 created one `VkFence` per close and destroys it per close. For burst-paint workloads under backpressure this is one extra create+destroy pair per submit; Phase 6 should pool fences (or move to timeline semaphores) and batch their destruction. Also the right place to wire `OutputFrame::holders + PaintBatch::holders` if cross-batch / cross-queue dependencies appear — the structural fix for the record-time CPU layout tracking caveat, subsumes the destruction-barrier pattern at 5 drawable-free sites and the `needs_grow` pre-flush gates at 3D / 3F-1 / 3F-2.
- **AMD-investigation phase — resource pooling + ftrace.** Per `project_amd_lag_investigation.md` memory, the post-3F-2 perf data showed `submit_and_wait` at 0.09% children on the bee/RDNA2 adapta-nokto + mate-cc reproducer — Phase 4 is not expected to materially close that lag. The amdgpu ioctl rate (BO create / submit / wait) is the suspected bottleneck. AMD investigation is not closed by Phase 4; it stays on the next-phase decision tree.

## What's next

Per `docs/status.md`, the next-phase decision after Phase 4 depends on the Friday `silence` + Nvidia test results (per `project_amd_lag_investigation.md` memory). Two provisional branches:

- **If `silence` reproduces the catastrophic adapta-nokto lag** (broad recent-amdgpu regression hypothesis): the AMD-investigation phase becomes the next move. Resource pooling + ftrace + ioctl-rate measurement. Phase 5's atlas portion may be re-scoped depending on whether the lag-cause hypothesis holds — if the dominant cost is amdgpu ioctl rate, retiring the per-glyph wait-idle alone won't fix `bee`.
- **If `silence` is fine** (lag is bee/RDNA2-specific, or a kernel/driver bisection target): Phase 5 narrowed to readback + glyph atlas is the next move. The readback handler triplet + `GlyphAtlas::intern` are the remaining per-op wait-idles on the paint side; Phase 5 closes them with targeted VkFences using the same machinery Phase 4 built for `PaintBatch`.

Phase 6 (batch-owned refcounted handles + holders wiring) stays the structural follow-up after Phase 5 either way — it's not a critical-path next step but it's the cleanup that lets the `needs_grow` pre-flushes go away and gives cross-batch dependencies a proper home.
