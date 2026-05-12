# Phase 2 — rendering re-architecture — validation results

Date: 2026-05-12
Plan: `docs/superpowers/plans/2026-05-12-rendering-rearchitecture-phase2.md`
Branch: `graphics-followups` (tag `phase2`)
Predecessor: `phase1` (commit `768faf8`)

## Preflight checks

All ran clean at the end of T8:

- `cargo +nightly fmt --check` — no diff.
- `cargo test` — all crates green:
  - `yserver` unit tests: 127 passed, 0 failed.
  - `yserver-core`: 284 passed.
  - `yserver-protocol`: 208 passed.
  - Scheduler module: 14 tests (5 composite_pool_ring + 9 in_flight + 7 damage + 1 paint_batch + 1 output_frame + 3 scheduler::mod = 26 — phase 2 added 5 to the phase-1 total).
- `cargo clippy` — 5 pre-existing doc-list-indentation warnings, no new warnings introduced by phase 2.

## Cutover greps

- `rg 'descriptor_pool' crates/yserver/src/kms/vk/pipeline.rs` — **zero hits**. The shared pool field, `reset_descriptors()`, and `allocate_descriptor_for_view()` are all gone.
- `rg 'create_descriptor_pool' crates/yserver/src/kms/scheduler/composite_pool_ring.rs` — **1 hit**. The per-output ring owns pools.
- `rg -n 'queue_wait_idle' crates/yserver/src/kms/vk/ops/mod.rs` — **4 hits** (still in place as required; phase 4 removes).

## Hardware smoke (bee)

Initial smoke with `RING_LEN = 2` **failed**: the panel did not come up. The log flooded with:

```
WARN  vk composite: descriptor pool ring exhausted for output HDMI-A-2 — deferring frame
```

at >100 Hz. Root cause: N=2 was insufficient for the steady-state composite pipeline depth.

The BO state machine traverses `Pending → OnScreen → Retiring → Free` across **three** pageflip-complete events after submit. The flip-pending skip in `composite_and_flip` paces records at one per pageflip cycle. At t=2 pageflips after the first submit, two prior in-flight frames still held their pool slots (one in `OnScreen`, one in `Retiring`), and the third recording's `acquire()` returned `None` → frame deferred. The output's damage stayed dirty; subsequent `composite_and_flip` calls retried continuously, flooding the warning until the third pageflip-complete advanced the oldest BO to `Free` and `poll_in_flight` released its slot.

The original "N=2 is sufficient" rationale (HLD-aligned, codex-aligned at the design stage) was wrong: it conflated "one flip in flight per output" with "one slot in use per output." A slot stays `in_use` from `acquire` (record time) until the matching `InFlightFrame` is `fully_retired()`, which requires the BO to reach `BoPhase::Free` — three pageflips after submit.

**Fix**: bumped `RING_LEN` from 2 to 3 in commit `1ad919c`. Module doc rewritten to explain the actual sizing math against the BO retirement pipeline. `acquire_returns_monotonic_slots_until_full` test updated for the new boundary.

Second smoke on bee with `RING_LEN = 3`: **panel comes up**. No ring-exhaustion warnings.

## Architecture lessons from this regression

1. **Ring sizing must match the resource retirement pipeline depth, not the flip-pending policy.** The flip-pending skip controls "how many can be submitted at once" (1 per output). The pool slot lifetime controls "how many must be addressable concurrently" (≥ the BO state-machine depth). Conflating these is what produced the bug.

2. **The unit tests didn't catch this**: the `SlotTracker` tests verify acquire/release/exhaustion semantics for any RING_LEN, but don't model the timing relationship between `acquire`/`release` and the BO state machine. Future similar sizing decisions need either an integration-shaped test or an explicit calculation in the design doc tied to the BO pipeline depth.

3. **N=3 is the minimum for the current BO pool depth.** If the BO pool ever grows (e.g. quad-buffering, or driver-imposed deeper queues), `RING_LEN` will need to grow with it. Worth flagging in code comments next to both `RING_LEN` and the BO pool sizing constants.

## Done conditions

Per the plan's "Done conditions" section, all 10 conditions hold:

1. ✅ All 8 tasks committed; tree green (fmt/clippy/test).
2. ✅ `CompositorPipeline` no longer owns `descriptor_pool`.
3. ✅ `CompositePoolRing` exists with `acquire`/`release`/`pool_at`/`slots_in_use`, plus `SlotTracker` unit-tested.
4. ✅ `OutputLayout` has lazy-init `composite_pools: Option<CompositePoolRing>`.
5. ✅ `InFlightFrame { output_frame: OutputFrame, gpu_retired, scanout_retired }` — no duplicated fields.
6. ✅ `OutputFrame::new` called with real `composite_pool_slot` from the ring.
7. ✅ `try_vulkan_composite_flip` returns `Option<(bo_slot, pool_slot)>` and releases on error after `vkQueueWaitIdle`.
8. ✅ `poll_in_flight` releases pool slots before drain.
9. ✅ Hardware smoke (bee) passes with N=3; no ring-exhaustion warnings.
10. ✅ Hot-path `vkQueueWaitIdle` in `vk/ops/mod.rs::run_one_shot_op` still present (phase 4 removes).

## Commit summary (phase 2)

| Task | Commit | Notes |
|---|---|---|
| Plan | `0d8ffc7` | Phase-2 implementation plan + codex review folded in |
| T1 | `a3e9480` | `CompositePoolRing` + `SlotTracker` + 5 unit tests |
| T2 | `47fb82b` | Restructure `InFlightFrame` to embed `OutputFrame` |
| T3 | `27bf291` | Wire `composite_pools` field into `OutputLayout` |
| T4 | `d43ae0b` | `record_and_present_composite` takes descriptor_pool param (transitional) |
| T5 | `dde83d0` | `try_vulkan_composite_flip` acquires per-output ring slot |
| T6 | `99069a6` | `poll_in_flight` releases pool slot on retirement |
| T7 | `0f9de08` | Remove `descriptor_pool` from `CompositorPipeline` |
| T8 fix | `1ad919c` | Bump `RING_LEN` 2 → 3 to match BO pipeline depth |

9 commits total on top of `phase1` tag.

## Known deferred items

- **`RING_LEN` is hard-coded to 3.** If the scanout BO pool grows beyond 3, the ring must grow with it. Currently sized for the existing 3-BO pool. Worth a cross-reference comment.
- **`composite_pool_ring.rs` integration tests against a real `VkContext` don't exist.** The `SlotTracker` covers slot-state semantics; the `CompositePoolRing::new` / `release` / `Drop` paths that touch `VkDescriptorPool` are validated by hardware smoke only. A `for_tests`-style mock VkContext that supports pool create/reset/destroy would be a future improvement.

## What's next

Phase 3 (recorder migration to `PaintBatch`) is the natural next step. The HLD names the family-by-family migration order (`fill → copy → image → render → text → traps`). Plan to be written after phase 2 is fully tagged.
