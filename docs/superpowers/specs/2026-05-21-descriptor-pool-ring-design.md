# DescriptorPoolRing — Stage 5 Task 4 (layer 1)

**Date:** 2026-05-21
**Stage:** 5 (perf), Task 4 ("make compose cheap")
**Scope:** Replace per-call descriptor pool creation with a long-lived,
reusable pool ring owned by the v2 engine.

## Motivation

Perf capture (`yserver-mate.perf.data`, 2026-05-21, yoga / Snapdragon X1
/ Turnip) under a mate-session with marco compositing + wezterm running
btop + caja drag pinned the dominant CPU consumer inside yserver:

```
run_core → process_request → handle_render_request → render_composite
  → allocate_descriptor_for_views_into → allocate_set → grow
    → create_descriptor_pool                        ← vkCreateDescriptorPool
      → drmIoctl → ioctl → __arm64_sys_ioctl
        → msm_ioctl_vm_bind                         ← Turnip GEM allocation
          → vm_bind_job_pin_objects → msm_gem_get_pages_locked
            → get_pages → shmem_alloc_and_add_folio ← shmem-backed pages
```

That call chain accounted for ~36% of yserver's own CPU time during the
moderate-lag steady-state portion of the capture. The shape lines up with
the bucket counters from the same run: `paint_submits/s` at 700-4700,
`composite_submits/s` at 60, `storage_allocations/s` and
`image_view_creates/s` at 60-1162 — i.e. yserver was issuing thousands
of paint command buffers per second, each ending up with a fresh
descriptor pool allocation.

Root cause is mechanical. `try_vk_render_composite` and
`try_vk_render_traps_or_tris` in `crates/yserver/src/kms/v2/engine.rs`
each instantiate a fresh `BatchDescriptorArena` per call:

```rust
// crates/yserver/src/kms/v2/engine.rs:2867
let mut arena = BatchDescriptorArena::new(Arc::clone(&inner.vk));
let descriptor_set = inner
    .render_pipelines
    .as_ref()
    .expect("ensured")
    .allocate_descriptor_for_views_into(&mut arena, src_view, mask_view, dst_view)?;
```

The arena's first `allocate_set` triggers `grow()` which calls
`vkCreateDescriptorPool`. The arena is then attached to the
`SubmittedOp` and its pool is `vkDestroyDescriptorPool`'d when the op
retires (`engine.rs:506`). With one RENDER op per arena, every RENDER
call pays for one full pool create/destroy round-trip, and on Turnip
that round-trip allocates fresh shmem pages via `msm_ioctl_vm_bind`.

## Goal

Move the descriptor pool lifecycle off the per-call hot path. After
warm-up, steady-state `vkCreateDescriptorPool` calls per second should
be ≤ a small constant (single-digit), not scaling with paint submit
rate.

## Non-goals

- **View-tuple descriptor set caching.** Reusing the descriptor *set*
  itself (not just its backing pool) when the same `(src_view, mask_view,
  dst_view)` repeats across calls. Tracked as a possible layer 2 in a
  separate spec, gated on whether layer 1 leaves residual cost.
- **Pool right-sizing / per-format pools.** The fixed pool sizing
  (256 sets, 1024 samplers, 256 UB, 64 SB) inherited from
  `BatchDescriptorArena` is preserved for now. Right-sizing is an
  independent tuning question.
- **Glyph atlas / text pipeline.** `TextPipeline` already preallocates
  a single per-pipeline descriptor set; it isn't on the create-pool hot
  path and isn't touched.
- **Pipeline / descriptor set layout changes.** Bindings stay as today:
  three `COMBINED_IMAGE_SAMPLER`s (src=0, mask=1, dst=2) with the shared
  linear sampler.

## Architecture

One `DescriptorPoolRing` lives on the v2 engine's inner state
(alongside `render_pipelines`, `glyph_atlas`, etc.). It owns a
dynamically-sized collection of descriptor pools cycling through
three normal-operation states plus one degenerate state:

- **Free** — pool has been reset and is ready to hand out sets.
- **Active** — currently the target of `acquire_set` calls; not yet
  full.
- **In-flight** — full or rotated out; sets currently referenced by
  un-retired `SubmittedOp` command buffers.
- **Poisoned** — `vkResetDescriptorPool` failed; slot is dead until
  engine teardown. Hard-error policy (see §"Error handling"); never
  observed on healthy hardware.

At any moment there is at most one Active pool. New pools are created
on demand when Free is empty and Active fills up; pools are never
destroyed during normal operation (only at engine teardown).

`BatchDescriptorArena` (`crates/yserver/src/kms/scheduler/
batch_descriptor_arena.rs`) **stays in tree** — v1's `KmsBackend`
calls `batch.descriptor_arena_mut()` at `backend.rs:5233` and
`backend.rs:6273` and is out of Stage 5's scope. The v2 engine
detaches from `BatchDescriptorArena` entirely:

- The two engine call sites in `try_vk_render_composite` and
  `try_vk_render_traps_or_tris` (`engine.rs:2867`, `engine.rs:3413`)
  switch to `inner.descriptor_pool_ring.acquire_set(layout,
  generation)` — no more local-arena instantiation.
- The `descriptor_arena: Option<BatchDescriptorArena>` field on v2's
  `SubmittedOp` (`engine.rs:128`) is removed, along with the two
  release blocks in `release_retired_ops` (`engine.rs:506`) and
  `drain_all` (`engine.rs:532`).
- The v1 paint_batch path (`PaintBatch::descriptor_arena_mut`,
  `paint_batch.rs:246`) is untouched.

## Components

### `DescriptorPoolRing` (new file: `crates/yserver/src/kms/v2/descriptor_pool_ring.rs`)

```rust
pub struct DescriptorPoolRing {
    vk: Arc<VkContext>,
    /// Active + In-flight + Free pools in a single Vec. Each entry
    /// carries its own state + generation tag.
    pools: Vec<PoolSlot>,
    /// Index into `pools` of the current Active pool. None if no
    /// pool is currently active (initial state or after every pool
    /// is In-flight or Free with capacity 0).
    active: Option<usize>,
}

struct PoolSlot {
    pool: vk::DescriptorPool,
    state: PoolState,
    /// Highest acquire-generation tag for any set issued from this
    /// pool since its last reset. Used to decide when In-flight can
    /// transition back to Free.
    high_water_generation: u64,
    /// Approximate sets remaining (same heuristic + OUT_OF_POOL_MEMORY
    /// retry as today's BatchDescriptorArena).
    sets_remaining: u32,
}

enum PoolState { Free, Active, InFlight, Poisoned }
```

API:

```rust
impl DescriptorPoolRing {
    pub fn new(vk: Arc<VkContext>) -> Self;

    /// Acquire one descriptor set with `layout`, tagging the issuing
    /// pool with `generation`. The caller (engine) passes a
    /// monotonically-increasing generation per op submission.
    pub fn acquire_set(
        &mut self,
        layout: vk::DescriptorSetLayout,
        generation: u64,
    ) -> Result<vk::DescriptorSet, vk::Result>;

    /// Caller signals "all submissions up to and including generation
    /// `retired_watermark` have retired." Pools whose
    /// `high_water_generation <= retired_watermark` and are in state
    /// InFlight move to Free (via `vkResetDescriptorPool`, NOT
    /// destroy). Active pool is untouched.
    ///
    /// Returns the number of pools reclaimed (for telemetry).
    pub fn release_up_to(&mut self, retired_watermark: u64) -> usize;

    /// Test helper: number of pools currently allocated.
    pub fn pool_count(&self) -> usize;

    /// Test helper: pool-state breakdown
    /// (Free, Active, InFlight, Poisoned).
    pub fn state_counts(&self) -> (usize, usize, usize, usize);
}
```

Internals on `acquire_set`:

1. If `active` is `None` or its `PoolSlot.sets_remaining == 0`, rotate
   the Active pool (if any) to In-flight, then pick a Free pool. If
   none, create a new one (the only `vkCreateDescriptorPool` call
   site).
2. Allocate one set via `vkAllocateDescriptorSets` from the now-Active
   pool. On `OUT_OF_POOL_MEMORY` / `FRAGMENTED_POOL`, set
   `sets_remaining = 0` on the Active slot, rotate, retry once
   (mirrors today's behaviour).
3. Update Active slot's `high_water_generation = max(current, generation)`
   and decrement `sets_remaining`.

Internals on `release_up_to(w)`:

```rust
for slot in &mut self.pools {
    if slot.state == PoolState::InFlight && slot.high_water_generation <= w {
        match unsafe {
            self.vk.device.reset_descriptor_pool(
                slot.pool,
                vk::DescriptorPoolResetFlags::empty(),
            )
        } {
            Ok(()) => {
                slot.state = PoolState::Free;
                slot.sets_remaining = SETS_PER_POOL;
                slot.high_water_generation = 0;
            }
            Err(e) => {
                log::error!(
                    "DescriptorPoolRing: vkResetDescriptorPool failed on \
                     pool {:?}: {e:?} — poisoning slot",
                    slot.pool,
                );
                slot.state = PoolState::Poisoned;
                // Don't bump telemetry's reset counter on failure.
            }
        }
    }
}
```

**Poison is ring-wide, not slot-local.** Once any slot transitions
to `Poisoned`, every subsequent `acquire_set` call returns
`vk::Result::ERROR_UNKNOWN` unconditionally — even if other Free /
Active slots could still satisfy the request. The rationale is that
a reset failure on healthy hardware is a strong signal that the
descriptor-pool subsystem is in an undefined state; continuing to
hand out sets from sibling pools risks compounding the bug. The
caller in `try_vk_render_composite` propagates `ERROR_UNKNOWN` as
`RenderError::Vk(...)` and the op is dropped, same shape as
`vkCreateDescriptorPool` failure.

Implementation note: `acquire_set` checks for any
`PoolState::Poisoned` slot at the top of the function (a single
linear scan of `pools` since the ring is small in steady state) and
short-circuits with `Err(ERROR_UNKNOWN)` before considering Active
or Free. A separate `poisoned: bool` field on the ring can cache
this check if the linear scan ever becomes hot, but is not required
for correctness.

`Drop`: destroy every pool (mirrors today's batch retirement path,
just at engine teardown instead of per-op).

### Engine integration (changes to `crates/yserver/src/kms/v2/engine.rs`)

1. Add `descriptor_pool_ring: DescriptorPoolRing` to `EngineInner`
   (the inner struct backing `RenderEngine`). Initialized in `new()`
   alongside `render_pipelines`.

2. Remove `descriptor_arena: Option<BatchDescriptorArena>` from
   `SubmittedOp`. Remove the two `op.descriptor_arena.take() ...
   release(&inner.vk)` blocks in `release_retired_ops` and
   `drain_all`.

3. Add a monotonic `acquire_generation: u64` counter on `EngineInner`.
   Bumps on each paint-op submission (one bump per RENDER call,
   regardless of how many sets the op uses internally).

4. The two call sites in `try_vk_render_composite` and
   `try_vk_render_traps_or_tris` change from:

   ```rust
   let mut arena = BatchDescriptorArena::new(Arc::clone(&inner.vk));
   let descriptor_set = inner.render_pipelines.as_ref().expect("ensured")
       .allocate_descriptor_for_views_into(&mut arena, src, mask, dst)?;
   // ... record CB ...
   submitted_op.descriptor_arena = Some(arena);
   ```

   to:

   ```rust
   let descriptor_set = inner.render_pipelines.as_ref().expect("ensured")
       .allocate_descriptor_for_views_into_ring(
           &mut inner.descriptor_pool_ring,
           inner.acquire_generation,
           src, mask, dst,
       )?;
   // ... record CB, no per-op arena attachment ...
   ```

5. Add a new method `RenderPipelineCache::
   allocate_descriptor_for_views_into_ring(&mut DescriptorPoolRing,
   generation: u64, src_view, mask_view, dst_view) -> Result<vk::
   DescriptorSet, vk::Result>`. The existing
   `allocate_descriptor_for_views_into(&mut BatchDescriptorArena, ...)`
   stays as-is — v1 still calls it indirectly via `paint_batch.
   descriptor_arena_mut()`. The descriptor write logic (bindings
   0/1/2 for src/mask/dst with the shared sampler) is shared between
   the two methods via a small private helper that takes a
   pre-allocated `vk::DescriptorSet` and the three views.

6. In `release_retired_ops` (`engine.rs:493`), after popping a
   retired op from the front of `submitted`, call
   `inner.descriptor_pool_ring.release_up_to(retired_op.generation)`.
   Since the queue is FIFO and ops retire in submit order, this is
   monotonically increasing across iterations of the loop.

   Equivalent integration in `drain_all` (`engine.rs:516`).

### `BatchDescriptorArena` and v1

`BatchDescriptorArena` stays in tree. v1's two call sites
(`kms/backend.rs:5233`, `:6273`) go through `paint_batch.
descriptor_arena_mut()` and that path is untouched by this work.

After the v2 detach, `grep -rn BatchDescriptorArena --include="*.rs"`
should still find the type's definition + the v1 callers + the
`paint_batch.rs` field — but no longer find the two v2 `engine.rs`
sites. That's the post-fix expected state.

If we later port v1 onto the ring (or delete v1 entirely per the
post-Stage-4 deletion gates), `BatchDescriptorArena` retires
naturally. That's not in this spec's scope.

## Data flow

```
X11 RENDER::Composite request
  → handle_render_request (process_request.rs)
    → engine.render_composite (v2 backend)
      → try_vk_render_composite (engine.rs:~2700)
        ↓ inner.acquire_generation += 1; gen = inner.acquire_generation;
        ↓ descriptor_pool_ring.acquire_set(layout, gen)
        │     ├─ Active full? → rotate Active → InFlight (tag with gen),
        │     │                  pick Free or create new Active
        │     └─ vkAllocateDescriptorSets from Active pool
        │        update Active.high_water_generation = gen
        │
        ↓ vkUpdateDescriptorSets (writes src/mask/dst views)
        ↓ record CB, submit
        ↓ SubmittedOp { generation: gen, ticket, cb, ... } pushed to inner.submitted
        ...
        (some time later, GPU finishes the CB)
        ...
  → engine tick / release_retired_ops
    ↓ pop front op, ticket signaled?
    ↓ free CB, drop staging
    ↓ descriptor_pool_ring.release_up_to(op.generation)
        └─ InFlight slots with high_water_generation <= op.generation
           → vkResetDescriptorPool → Free
```

Steady-state invariant once the working set fits: one Active pool
provides 256 sets, which under the worst observed `paint_submits/s` ≈
4700 fills in ~55ms. That pool rotates to InFlight, another (or new)
Free pool becomes Active. The In-flight pool retires within a frame or
two (GPU work is ~16ms per frame, queue depth limited) and is reset
back to Free. The total pool population stabilises at 2-4 pools
covering the in-flight window. `vkCreateDescriptorPool` calls per
second: zero after warm-up.

## Lifetime invariants

I1. **No pool reset while any un-retired CB references a set from it.**
    Enforced by the `high_water_generation` watermark: a pool moves
    from In-flight back to Free only when the engine has explicitly
    signalled that all generations ≤ that pool's watermark have
    retired.

I2. **Submission order matches generation order.** Generations are
    handed out monotonically inside the engine. The `submitted` queue
    is FIFO and ops retire in submission order. Therefore the
    `release_up_to(op.generation)` call in `release_retired_ops` is
    safe — every set with `set.generation <= op.generation` is either
    in a pool currently being released, or already in a previously
    released pool.

I3. **Pools are never destroyed during normal operation.** Only at
    `Drop` (engine teardown). This is the architectural intent: trade
    a small fixed Vk-handle footprint for elimination of the hot-path
    `vkCreateDescriptorPool`/`vkDestroyDescriptorPool` calls.

I4. **`vkResetDescriptorPool` is the only state-changing Vk call on
    the steady-state path beyond `vkAllocateDescriptorSets` and
    `vkUpdateDescriptorSets`.** It returns all sets allocated from the
    pool to the free state; the pool's underlying memory is not
    released, so on Turnip there's no shmem page churn.

## Error handling

`vkCreateDescriptorPool` failure: propagate as `vk::Result` to the
caller; `try_vk_render_composite` already converts to its
`RenderError::Vk(...)` variant. The op is dropped; behaviour matches
today's grow-failure path.

`vkAllocateDescriptorSets` returning `OUT_OF_POOL_MEMORY` or
`FRAGMENTED_POOL`: same retry-once-after-rotate logic as today's
`BatchDescriptorArena::allocate_set` (`batch_descriptor_arena.rs:84`).
Both error variants must be tested individually since `FRAGMENTED_POOL`
exercises a different driver path (post-reset fragmentation rather
than initial capacity exhaustion).

`vkResetDescriptorPool` failure: **hard error**. The pool ring is the
mechanism that bounds pool growth and removes the hot-path churn; if
reset fails on a live device the ring becomes append-only and yserver
regresses past the pre-fix baseline (creates new pools without
destroying the old ones). The slot is marked Poisoned, an `error!`
line is emitted naming the pool handle + `vk::Result`, and the next
`acquire_set` after the poison is observed returns
`vk::Result::ERROR_UNKNOWN` so the calling engine path drops the op
the same way it would for a `vkCreateDescriptorPool` failure. In
practice `vkResetDescriptorPool` should never fail with a valid pool
handle, but treating it as bounded-fatal keeps the residency
guarantee honest.

## Telemetry

Two new bucket counters in `crates/yserver/src/kms/v2/telemetry.rs`:

```rust
pub struct Bucket {
    // ... existing fields ...
    /// vkCreateDescriptorPool calls in this second. Gate metric for
    /// Task 4 layer 1: should reach a near-zero floor after warm-up.
    pub descriptor_pool_creates: u64,
    /// vkResetDescriptorPool calls in this second. Tells us how busy
    /// the recycle path is — a healthy value is "tracks
    /// paint_submits/s / SETS_PER_POOL".
    pub descriptor_pool_resets: u64,
}
```

`record_descriptor_pool_create()` and `record_descriptor_pool_reset()`
methods follow the existing pattern. Sites: the
`vkCreateDescriptorPool` call inside `DescriptorPoolRing::acquire_set`
(the "create a new pool when no Free slot is available" branch of
the rotate-on-exhaustion path), and the `vkResetDescriptorPool` call
inside `release_up_to` (only on the `Ok` arm of the reset match).

Emission: appended to the existing `v2_telemetry:` log line in
`maybe_emit`.

The existing `descriptor_allocations/s` (which counts
`vkAllocateDescriptorSets`) continues to count set allocations as
today; it should be unchanged in absolute value after this work
(allocations still happen, just from reused pools).

## Testing

### Unit tests (`crates/yserver/src/kms/v2/descriptor_pool_ring.rs`)

These need a `VkContext`; reuse the existing v2 test fixture pattern
(see `dst_readback.rs` tests for the shape).

- `acquire_grows_when_no_free_pool`: empty ring, call `acquire_set`,
  assert `pool_count == 1` and one set issued.
- `acquire_fills_active_then_rotates`: call `acquire_set` 257 times
  with the same generation; assert `pool_count == 2`, two pools
  In-flight + Active (or one of each).
- `release_moves_inflight_to_free`: fill one pool, acquire enough to
  rotate it to In-flight (bump generation, acquire one more from a
  new pool), call `release_up_to(rotated_pool's high_water)`. Assert
  state_counts shows one Free + one Active.
- `release_below_watermark_is_noop`: pool tagged with generation 5;
  call `release_up_to(4)`. Pool stays In-flight.
- `interleaved_generations_partial_release`: pool A used by
  generation N (fill it so it rotates to InFlight), pool B used by
  generation N+1 (still Active). Call `release_up_to(N)`. Assert
  pool A moves to Free, pool B stays Active. Crucial for the
  `high_water_generation` invariant — the case the previous tests
  do not exercise. Then issue another acquire at generation N+2,
  fill, retire, `release_up_to(N+1)`, assert B is now Free; etc.
  Walks at least one full cycle of cross-pool retirement.
- `out_of_pool_memory_retry`: drive a `vkAllocateDescriptorSets`
  error path returning `ERROR_OUT_OF_POOL_MEMORY` (may need an
  artificial pool size tweak; can be a doc-hidden test helper) and
  assert acquire succeeds after retry.
- `fragmented_pool_retry`: same shape as `out_of_pool_memory_retry`
  but driving `ERROR_FRAGMENTED_POOL` specifically — covers the
  post-reset fragmentation path, which is a distinct driver code
  path that `out_of_pool_memory_retry` does not exercise.
- `reset_failure_poisons_slot_and_drops_acquire`: drive a
  `vkResetDescriptorPool` error (test helper that swaps the device
  function pointer, or an `assume_reset_fails` flag on the ring),
  call `release_up_to(...)`, assert the slot transitions to
  Poisoned and the next `acquire_set` returns
  `ERROR_UNKNOWN` (the hard-error policy from §"Error handling").
- `drop_destroys_all_pools`: instantiate, acquire, drop the ring;
  assert via Vk validation layer that no leaked handles remain.
- `pool_create_count_zero_after_warmup`: run 5000 acquire/release
  cycles in a tight loop with bounded in-flight depth (e.g. release
  every iteration), assert `pool_count` stabilises at ≤ 2 AND
  `descriptor_pool_resets` lifetime counter is in the expected
  range (proves the recycle path actually ran — bounding creates
  alone is insufficient since a never-resetting implementation can
  also exhibit a bounded create count by simply leaking all pools
  as InFlight forever).

### Integration tests (`crates/yserver/tests/v2_acceptance.rs`)

Two parallel tests, one per v2 call site (`try_vk_render_composite`
and `try_vk_render_traps_or_tris`), so both engine paths that
acquire from the ring get direct coverage. Body shape is identical
between the two; the difference is the X11 request that drives the
engine into the corresponding path.

- `v2_render_composite_pool_creates_bounded_after_warmup`: drive N
  RENDER::Composite ops (N significantly larger than
  `SETS_PER_POOL`, e.g. N=2000) through the public Backend trait
  surface with bounded `engine.pending_count()` — call
  `engine.release_retired_ops()` between batches to simulate frame
  retirement. Three assertions, all required:
  1. `lifetime.descriptor_pool_creates <= ceil(N / SETS_PER_POOL)
     + small_warmup_slack` (e.g. +4) — bounds growth. Today this
     equals N.
  2. `lifetime.descriptor_pool_resets >= N / SETS_PER_POOL - slack`
     — proves the recycle path actually ran. Without this, a
     never-resetting implementation passes (1) by leaking all pools
     as InFlight indefinitely.
  3. `engine.descriptor_pool_ring().pool_count() <= small_bound`
     (e.g. ≤ 4) at the end of the run — proves steady-state
     residency stays small.

  All three together demonstrate the intended behaviour:
  pools are created at warm-up only (1), get recycled (2), and the
  working-set residency stays bounded (3). Any single assertion
  alone admits a degenerate-but-passing implementation.

- `v2_render_traps_pool_creates_bounded_after_warmup`: identical
  three-assertion shape as the Composite test above, but driving
  RENDER::Trapezoids (or Triangles, whichever is easier to
  construct through the Backend trait) so the
  `try_vk_render_traps_or_tris` call site
  (`engine.rs:3413`) is exercised directly rather than only via the
  shared helper. Both sites share the same ring acquire path so a
  passing Composite test almost certainly implies a passing traps
  test, but landing both makes the regression surface explicit.

### Regression coverage

The pre-existing
`v2_render_composite_*` acceptance tests must still pass — descriptor
set allocation behaviour from the caller's point of view is
unchanged. No new test invalidates them.

## Risks

R1. **Pool memory residency growth.** Today pools are destroyed at op
    retirement; under the ring they live until engine teardown.
    Mitigation: the SETS_PER_POOL × pool-size budget is small (an
    estimate: ~256 sets × ~64 bytes/set descriptor data + pool
    metadata is well under 1 MiB per pool); a working set of 2-4
    pools is negligible against the rest of v2's storage footprint.
    Pool count is exposed via `pool_count()` for the regression
    test's upper-bound assertion.

R2. **Wrong generation watermark causes use-after-reset.** Mitigated
    by the FIFO submission order (I2) and the explicit `submit ->
    generation` mapping carried on each `SubmittedOp`. The watermark
    is only ever advanced when a `SubmittedOp` with that exact
    generation is popped from `submitted` in `release_retired_ops`.

R3. **`vkResetDescriptorPool` semantics on Turnip.** The Vk spec
    guarantees that reset returns sets to the free state without
    releasing the pool's underlying memory. Turnip should honour
    this. If a Turnip-specific bug surfaces, fall back to destroying
    + re-creating the pool at reset time (loses some of the win;
    keeps the ring abstraction intact).

R4. **Pool fragmentation.** Theoretically possible if `acquire_set`
    pattern fragments the descriptor pool faster than
    `OUT_OF_POOL_MEMORY` retry can rotate. Inherited from today's
    behaviour and the SETS_PER_POOL / SAMPLERS_PER_POOL sizing was
    chosen for that ratio. Out of scope; flag if observed in
    practice.

## Capture recipe (post-fix verification)

After landing, re-run the same workload:

```
just yserver-mate-hw-telemetry          # for v2_telemetry bucket line
just yserver-mate-hw-perf               # for perf flamegraph confirmation
```

Expected `v2_telemetry` deltas vs the 2026-05-21 baseline:
- `descriptor_pool_creates/s`: ≤ 5 in steady state (was implicit ~4700).
- `descriptor_pool_resets/s`: tracks paint_submits/s / SETS_PER_POOL,
  roughly tens per second under the captured workload.
- `descriptor_allocations/s`: unchanged from today (still ~180/s).
- `paint_submits/s`: unchanged (this work doesn't reduce submit count).

Expected perf-flamegraph deltas:
- `create_descriptor_pool` → `msm_ioctl_vm_bind` should drop from
  ~1.63% of total CPU to ≤ 0.1%.
- `handle_render_request` total should drop proportionally (was ~3.32%).
- Other hot spots (whatever they are) should now be visible above the
  noise floor for the next Task 5 cut.

## Out-of-scope follow-ups

- **Layer 2 — descriptor set caching by view tuple.** If the post-fix
  capture still shows `descriptor_allocations/s` correlating with
  paint_submits/s and the per-call overhead is still material, the
  next cut keys descriptor sets on `(src_view, mask_view, dst_view)`
  and avoids the `vkAllocateDescriptorSets` +
  `vkUpdateDescriptorSets` pair on cache hits. Separate spec.

- **Per-format / per-pipeline pool stratification.** If
  `OUT_OF_POOL_MEMORY` retries become hot, split pools per descriptor
  count profile.

- **Image-view caching.** The current `image_view_creates/s ==
  storage_allocations/s` (1:1) tells us each storage gets a fresh view.
  That's a separate Task 4 / Task 5 sub-task ("Avoid per-frame
  image-view creation; stable storage should have stable views" —
  Stage 5 plan §Task 4).
