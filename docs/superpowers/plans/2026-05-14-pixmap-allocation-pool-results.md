# Pixmap-allocation pool — burst-absorbing `VkImage` recycling — results

Date: 2026-05-14
Plan: `docs/superpowers/plans/2026-05-14-pixmap-allocation-pool.md`
Branch: `graphics-followups`
Predecessor: `docs/superpowers/plans/2026-05-14-rendering-rearchitecture-phase5-results.md`

## Scope landed

The post-3F-2 perf data + cross-vendor reproducer (catastrophic lag on bee/RDNA2 + Arch and fuji/Intel + Arch under adapta-nokto + mate-cc) pinned the remaining lag at **amdgpu / i915 kernel-allocator serialization under burst `vkCreateImage` + `vkAllocateMemory` traffic**. Phase 4 (close-time wait) and Phase 5 (readback + scratch grow) had already retired the paint-side waits; the post-Phase-5 hot path on the reproducer is `CreatePixmap` / `FreePixmap` of small widget pixmaps firing into the kernel allocator at hundreds of allocs per 100ms. This phase recycles the VkImage / VkImageView / VkDeviceMemory triples in a backend-owned `PixmapPool` keyed by `(width, height, format)`, so a fresh `CreatePixmap` of a recently-freed key hits the pool instead of round-tripping the kernel. The synchronous `flush_if_needed(ProtocolBarrier)` that previously gated drawable destruction in `free_pixmap` is gone from the common path — the mirror is adopted as a `PooledPixmapReturn` `BatchResource` into the currently-open paint batch via Phase 5 T2's `defer_resource_release`, and the pool-return-or-destroy decision happens at batch-retire time (after the open batch's fence signals — non-blocking on the input loop). Two effects compound: fewer synchronous flushes lets the input loop service input events smoothly; fewer kernel ioctls lets amdgpu / i915 lock contention drop.

- **T1 (`850bb9c`)**: `PixmapPool` infrastructure — new file `crates/yserver/src/kms/vk/pixmap_pool.rs` with `PixmapPool`, `PixmapPoolKey`, `PooledPixmapImage`, `PooledPixmapReturn` (BatchResource impl), `PixmapPoolStats`. `MAX_POOLED_DIM = 128` and `PIXMAP_POOL_BUCKET_CAP = 32` private constants. `try_take` + `try_return` + `stats` + `drain` API. `Arc<Mutex<HashMap<…>>>`-shaped per-bucket storage (codex round 1 P0: `Arc<RefCell<…>>` fails `BatchResource: Send`; `Mutex` is the right choice on the single-threaded core loop — one uncontended atomic CAS per op). `DrawableImage::new_from_pool` constructor on `target.rs`. Unit tests for `eligible` + `try_return` cap behaviour. No callers wired yet.
- **T2 (`9443a2e`)**: `free_pixmap` wired to defer-release through `PooledPixmapReturn`. `KmsBackend.pixmap_pool: Option<Arc<PixmapPool>>` field added; initialized at backend construction when VkContext is up. The synchronous `flush_if_needed(ProtocolBarrier)` is gone from the main path. Every mirror with a live `VkImage` routes through `PooledPixmapReturn::release` (eligibility + bucket-cap decided at retire time per codex round 3 P0 — uniform defer-release for all Vulkan-up server-owned mirrors). Two fallback paths retain the old synchronous-flush behaviour: missing prereqs (`pixmap_pool` / `vk` / `ops_command_pool` not yet up — pre-init only) and `ImageBacking::Imported` (DRI3-imported dma-buf clients; `into_pool_entry` panics for that variant — caught by the T2 reviewer agent and folded in pre-commit). Picture-rescue path stays unchanged.
- **T3 (`8b3f243`)**: `allocate_pixmap_mirror` consults `self.pixmap_pool.as_ref().and_then(|p| p.try_take(key))` before falling through to `new_server_owned_pixmap`. Pool hit returns a `DrawableImage` via `new_from_pool` with the previous tenant's terminal `current_layout` preserved (the X11 spec says CreatePixmap contents are undefined; the new tenant's first barrier transitions from whatever-layout-was-saved normally). `format_for_depth` helper factored — depth-1/8 → `R8_UNORM`, depth-24/32 → `B8G8R8A8_UNORM`. Pool miss falls through to the existing fresh-alloc path; on pool hit the kernel `vkCreateImage` + `vkAllocateMemory` is skipped entirely.
- **T4 (`2966407`)**: shutdown drain — `pixmap_pool.drain()` called after `scheduler.drain_submitted_paint_batches()`. Phase 4 T5's drain retires every in-flight batch, which walks each batch's `retire_resources` (including any in-flight `PooledPixmapReturn`s — each one returns its entry to the pool or destroys). Once scheduler-drain returns, every `PooledPixmapReturn` strong-ref on `Arc<PixmapPool>` is dropped and the pool holds the survivors. `drain()` is a `&self` method (no `try_unwrap` needed); a defensive `Arc::strong_count > 1` check logs a warning if a BatchResource leaked past scheduler drain. After drain, `Drop` on `PixmapPool` is a no-op (`queue_wait_idle` only fires if the buckets are non-empty, which is the partial-init failure path).
- **T5 (`a7c2384`)**: synthetic burst test (`crates/yserver/tests/pixmap_pool_burst.rs`) + stats accessor (`KmsBackend::pixmap_pool_stats() -> Option<PixmapPoolStats>`) + test-only retire helper (`force_retire_in_flight_for_test`, `cfg(test)`-gated on the impl block — Pattern A pub fns so integration tests can reach them). Test asserts the absorption shape: create 100 `(32, 32, depth=24)` pixmaps, free them all (defer-releases into the open paint batch), force-retire (closes + submits + waits the batch, walks `PooledPixmapReturn::release` on each, returns up to `PIXMAP_POOL_BUCKET_CAP = 32` entries to the bucket, destroys the remaining 68), re-create 100, assert `total_hits == 32` and `total_misses == 68 + 32 = 100`. Codex round-1 P1 caught that `cfg(test)` on the impl block doesn't reach the integration-test crate; moved to Pattern A pub fns (visible to the integration crate by name but gated to test usage by their docs). Codex round-1 P1 also caught that `force_retire_in_flight_for_test` must `close_and_submit_async` the open batch first — otherwise the open batch's `PooledPixmapReturn`s never run their `release`.

## Preflight checks

End of T5 (HEAD = `a7c2384`, plus this T6 docs commit):

- `cargo +nightly fmt --check` — clean (no diff, exit 0).
- `cargo clippy -p yserver` — 5 pre-existing `doc_lazy_continuation` warnings (`backend.rs:71`, `backend.rs:72`, `backend.rs:73`, `backend.rs:74`, `vk/pipeline.rs:104`). No new warnings.
- `cargo test --workspace`:
  - `yserver` lib: **151 passed, 0 failed, 3 ignored**.
  - `yserver` binary (`ynest`): 9 passed.
  - `yserver-core`: **284 passed**.
  - `yserver-protocol`: **208 passed**.
  - `fixture_smoke`: 2 passed, 1 ignored.
  - `pixmap_pool_burst`: **1 passed** (the new burst test).
  - Other test binaries: green (`alpha_invariant` 17 ignored, `dri3_fd_leak` 1 ignored, doc-tests 1 ignored). Same shape as Phase 5.

## Cutover greps

Captured semantically. Line numbers are informational and will drift; the load-bearing claim is the SITE list.

```
$ rg -n 'PixmapPool' crates/yserver/src/kms/
crates/yserver/src/kms/backend.rs:792:    pub(crate) pixmap_pool: Option<Arc<crate::kms::vk::pixmap_pool::PixmapPool>>,
crates/yserver/src/kms/backend.rs:1536:        let pixmap_pool = Arc::new(crate::kms::vk::pixmap_pool::PixmapPool::new(Arc::clone(
crates/yserver/src/kms/backend.rs:1558:    pub fn pixmap_pool_stats(&self) -> Option<crate::kms::vk::pixmap_pool::PixmapPoolStats> {
crates/yserver/src/kms/backend.rs:2283:            Arc::new(crate::kms::vk::pixmap_pool::PixmapPool::new(Arc::clone(
crates/yserver/src/kms/backend.rs:6933:    /// pixmap-pool T3: try the `PixmapPool` first; on hit we
crates/yserver/src/kms/backend.rs:6952:        let key = crate::kms::vk::pixmap_pool::PixmapPoolKey { … };
crates/yserver/src/kms/backend.rs:8454:        // pool's buckets hold entries to destroy. PixmapPool::Drop is
crates/yserver/src/kms/backend.rs:8460:                    "shutdown: PixmapPool strong_count={strong} > 1 at drain time; …"
crates/yserver/src/kms/backend.rs:9826:        let key = crate::kms::vk::pixmap_pool::PixmapPoolKey { … };
crates/yserver/src/kms/vk/pixmap_pool.rs: (struct decls, unit tests; full file)
```

Field decl at `backend.rs:792`, dual init sites at `1536` (main backend ctor) + `2283` (alternate ctor for the hostless dev path). `pixmap_pool_stats` accessor at `1558` (T5). Try-take at `6952` inside `allocate_pixmap_mirror` (T3). Shutdown drain at `8454..8460` (T4). Try-return-construction at `9826` inside `free_pixmap` (T2). Plus the full implementation in `vk/pixmap_pool.rs`.

```
$ rg -n 'flush_if_needed.*ProtocolBarrier' crates/yserver/src/kms/backend.rs
1835:        if let Err(e) = self.flush_if_needed(BatchFlushReason::ProtocolBarrier) {
9255:            .flush_if_needed(crate::kms::scheduler::paint_batch::BatchFlushReason::ProtocolBarrier)
11291:            .flush_if_needed(crate::kms::scheduler::paint_batch::BatchFlushReason::ProtocolBarrier)
11724:            .flush_if_needed(crate::kms::scheduler::paint_batch::BatchFlushReason::ProtocolBarrier)
11799:            .flush_if_needed(crate::kms::scheduler::paint_batch::BatchFlushReason::ProtocolBarrier)
```

Five `flush_if_needed(ProtocolBarrier)` call sites tree-wide — NONE in `free_pixmap`'s body when `pixmap_pool` is present and the mirror is not `ImageBacking::Imported`. Two physical occurrences (`9784` and `9806` — not on the rg above because they're spelled across two lines) live inside `free_pixmap`'s fallback paths intentionally:
- `9784`: prereqs missing (`pixmap_pool` / `vk` / `ops_command_pool` not yet up) — pre-init / partial-init only.
- `9806`: `ImageBacking::Imported` (DRI3-imported dma-buf) — pooling client-imported memory makes no sense; T2-review-caught route to flush+drop.

The five sites in the rg above are all unrelated to pixmap free: pre-paint protocol barriers in other handlers. The plan's "synchronous flush gone from `free_pixmap`'s common path" claim holds.

```
$ rg -n 'pixmap_pool_stats|force_retire_in_flight_for_test' crates/yserver/
crates/yserver/tests/pixmap_pool_burst.rs:13: //! …`force_retire_in_flight_for_test`. After this returns, every…
crates/yserver/tests/pixmap_pool_burst.rs:74:        .pixmap_pool_stats()
crates/yserver/tests/pixmap_pool_burst.rs:94:        .force_retire_in_flight_for_test()
crates/yserver/tests/pixmap_pool_burst.rs:95:        .expect("force_retire_in_flight_for_test");
crates/yserver/tests/pixmap_pool_burst.rs:97:    let s2 = backend.pixmap_pool_stats().expect("pixmap_pool present");
crates/yserver/tests/pixmap_pool_burst.rs:118:    let s3 = backend.pixmap_pool_stats().expect("pixmap_pool present");
crates/yserver/src/kms/backend.rs:1558:    pub fn pixmap_pool_stats(&self) -> Option<crate::kms::vk::pixmap_pool::PixmapPoolStats> {
crates/yserver/src/kms/backend.rs:1587:    pub fn force_retire_in_flight_for_test(
```

Both accessors are public `KmsBackend` methods (Pattern A: pub fn visible to the integration test crate). Test calls them at three sites — pre-burst snapshot, force-retire (closes and drains the open batch so `PooledPixmapReturn::release` actually fires), post-first-burst snapshot, post-second-burst snapshot.

## Done conditions

Per the plan's 8 Phase-level Done conditions in section "## Phase-level Done conditions":

1. ✅ `cargo +nightly fmt --check`, `cargo clippy -p yserver`, `cargo test --workspace` all green. Same 5 pre-existing `doc_lazy_continuation` warnings as Phase 5; no new lints. Test counts above.
2. ✅ `crates/yserver/src/kms/vk/pixmap_pool.rs` exists. T1 commit `850bb9c`.
3. ✅ `flush_if_needed(BatchFlushReason::ProtocolBarrier)` is NOT in `free_pixmap`'s common path. Two physical occurrences remain inside fallback paths (no-prereqs at `9784`, imported at `9806`) — those are intentional per the plan + T2 review (DRI3-imported variant doesn't go through the pool; pre-init partial-state shouldn't trigger post-init).
4. ✅ `allocate_pixmap_mirror` consults `self.pixmap_pool.as_ref().and_then(|p| p.try_take(...))` before `new_server_owned_pixmap`. Verified at `backend.rs:6933..6952` (T3 commit `8b3f243`).
5. ✅ `PixmapPool::drain` is called in the shutdown sequence after `scheduler.drain_submitted_paint_batches`. Verified at `backend.rs:8454..8460` (T4 commit `2966407`). The Phase 4 T5 scheduler-drain runs first, retiring every in-flight `PooledPixmapReturn`; then the pool drain destroys the survivors.
6. ✅ Synthetic burst test passes. `pixmap_pool_burst::pool_absorbs_burst_of_size_pixmaps`: 1 passed (T5 commit `a7c2384`). Asserts `total_hits == 32` and `total_misses == 100` for the 100-pixmap second burst after a 100-pixmap first burst.
7. ✅ `docs/status.md` reflects pool done; results doc exists (this commit).
8. ⏳ **TBD — pending user's hardware smoke**. See "Hardware smoke results" below.

## Hardware smoke results

Hardware smoke is user-owned (separate TTY on bare metal). The user runs `just yserver-mate-hw-release`, applies adapta-nokto with mate-cc visible, and fills in the subsections below. `just rendercheck-yserver` for the regression check.

**Phase expectation**: adapta-nokto + mate-cc workloads should be substantially less laggy on both bee (RDNA2 + Arch) and fuji (Intel + Arch). Pre-pool: catastrophic on both (per `project_amd_lag_investigation.md` memory + cross-vendor reproducer; mate-cc launcher fires hundreds of 16×16 / 32×32 widget pixmaps per <100ms burst). Post-pool: should be smooth or near-smooth. If `bee` improves: confirms the kernel-allocator-burst hypothesis and obviates the AMD-specific investigation phase. If `bee` is still slow: AMD-specific investigation (amdgpu ftrace + ioctl-rate measurement) re-emerges as the next move.

### Host

TBD.

### General smoke

TBD. Was MATE under default theme clean (no-regression check)? Theme switching responsive? rendercheck regression vs Phase 5 baseline?

### adapta-nokto + mate-cc on bee

TBD. **The load-bearing test.** Pre-pool: catastrophic. Post-pool expectation: smooth or near-smooth. If improved: kernel-allocator burst hypothesis confirmed; AMD investigation is NOT the next priority. If unchanged: capture amdgpu ftrace per `project_amd_lag_investigation.md` and reconcile.

### adapta-nokto + mate-cc on fuji

TBD. Cross-vendor confirmation. Intel kernel allocator was also catastrophic pre-pool; post-pool should be smooth or near-smooth. This data point is load-bearing for the "pool fixes the vendor-agnostic burst" claim.

### rendercheck regression

TBD. `just rendercheck-yserver`. Expected: no regressions vs Phase 5 baseline. The pool path is invisible to rendercheck (which never frees pixmaps under burst conditions); this is a no-regression check, not a delta hunt.

### Anomalies

TBD. Particular watch: any pool-related shutdown warning (`PixmapPool strong_count > 1`), any leaked-image log at process exit, any rendering corruption that points to a pool entry being reused before its previous tenant's GPU work completed.

## Plan bugs caught (folded back into plan / fixed in-tree)

This phase ran 6 rounds of codex plan review before dispatch + per-task code review (T1–T3). Findings:

### Round 1 (plan review)

- **P0 (`Arc<RefCell<PixmapPool>>` doesn't satisfy `BatchResource: Send`).** Original plan used `Arc<RefCell<…>>` for the BatchResource. Codex caught that `BatchResource: Send + std::fmt::Debug` is the existing trait bound (`paint_batch.rs:146`), and `Arc<RefCell<…>>` is NOT `Send` (RefCell isn't Sync). Plan rewritten to use `Arc<PixmapPool>` where `PixmapPool` internally uses `std::sync::Mutex<HashMap<…>>` for its buckets. Single-threaded core loop invariant means the Mutex is never contended; the lock cost is one atomic CAS per pool op.
- **P1 (`into_pool_entry` `mem::forget` leaks vk Arc).** Original plan had `into_pool_entry` use `mem::forget` to suppress `DrawableImage::Drop`. Codex caught that this also leaks the `Arc<VkContext>` strong reference embedded in `DrawableImage::vk`. Plan rewritten to swap `vk_image` / `vk_image_view` / `vk_memory` for null handles before dropping the `DrawableImage`, so the Arc decrement runs but the Vulkan handles are preserved.
- **P1 (T5 test wiring — `cfg(test)` on impl not visible to integration tests).** Original plan put the test accessors behind `#[cfg(test)]` on the impl block. Codex caught that integration test crates compile under their own cfg context — the impl-level cfg gate doesn't make the methods visible to `tests/pixmap_pool_burst.rs`. Plan rewritten to use Pattern A: plain `pub fn` with doc-comments scoping them to test usage. Codex also caught that `force_retire_in_flight_for_test` must `close_and_submit_async` the open paint batch first — otherwise the `PooledPixmapReturn`s inside it never run their `release` and the test asserts on an empty pool.
- **P2 (`free_pixmap` consumed mirror before vk_arc / pool_handle checks).** Original plan's pseudocode called `mirror.into_pool_entry()` before checking that `vk_arc` and `pool_handle` are present. Codex caught that this leaves a partially-consumed mirror on the failure path. Plan rewritten to reorder: prereq checks first, then `into_pool_entry()`.

### Round 2 (plan review after Round 1 folds)

- Round-1 nits (wording cleanups). T2 acceptance signal: ready to dispatch T1; T2 plan acceptable post-Round 1 fixes.

### Round 3 (plan review after T1 work-in-progress)

- **P0 (oversize direct-drop after flush removal = UAF risk).** Original plan had oversize / ineligible mirrors take a "direct Drop" path after the synchronous flush was removed. Codex caught that `DrawableImage::Drop` is non-waiting — direct-dropping a mirror after the flush is removed is a UAF / driver-crash risk for any in-flight VkImage. Plan rewritten so EVERY mirror with a live `VkImage` (on Vulkan-up backends) routes through defer-release uniformly via `PooledPixmapReturn::release`. Eligibility and bucket-cap rejection are handled INSIDE `release` via `try_return`'s `Err` path: ineligible (oversize) and full-bucket entries are destroyed by the BatchResource at batch-retire time — by which point the open batch's fence has signalled and the GPU is done with the image. This is the load-bearing UAF avoidance.

### Round 4 (plan review)

- Stale oversize wording in the "Key invariants" section still referenced the old "skip pool, direct-Drop" semantics from Round 2's plan. Rewritten in `c6500cd`.

### Round 5 (plan review)

- Two more stale "skip pool, direct-Drop" refs in the pre-task notes + the T2 commit-message preamble. Fixed in `c9f18bf` (the dispatch-ready cleanup).

### Round 6 (plan review)

- Ready to dispatch.

### T1 review

- Clean. No code-quality findings; the codex reviewer signed off.

### T2 review

- **P1 false alarm (matches! doesn't move on rest-bind pattern).** The reviewer flagged a potential ownership issue in the rescue path's `matches!` macro. Investigated and confirmed false alarm — `matches!` doesn't move on rest-bind, the value is still usable after the check.
- **Real bug caught: DRI3-imported pixmaps reach `free_pixmap` and `into_pool_entry` would panic.** Separate from the P1 false alarm, the T2 reviewer (running side-by-side with the implementor) spotted that `ImageBacking::Imported` (DRI3 dma-buf client) variants would reach `into_pool_entry`, which panics for that variant (pooling client-imported memory makes no sense — the client owns the dma-buf, not us). Folded the `imported` branch into T2's flush+drop fallback path. The variant routes through the same synchronous-flush shape as missing-prereqs.

### T3 review

- Clean. No findings.

### T4 / T5

- Skipped review (T4 was trivial drain plumbing — 4 lines + 1 defensive strong-count log; T5 was test plumbing isolated from the rest of the codebase).

## Commit summary

| Task | Commit | Subject |
|---|---|---|
| Plan | `c6500cd` | docs(plans): pixmap-allocation pool draft plan (codex round 3, iterating) |
| Plan: round 5/6 polish | `c9f18bf` | docs(plans): pixmap-pool plan ready to dispatch — clean up stale oversize wording |
| T1 | `850bb9c` | refactor(kms): add PixmapPool infrastructure (pixmap-pool T1) |
| T2 | `9443a2e` | refactor(kms): wire free_pixmap → defer-release into PixmapPool (pixmap-pool T2) |
| T3 | `8b3f243` | refactor(kms): wire allocate_pixmap_mirror → try-take from PixmapPool (pixmap-pool T3) |
| T4 | `2966407` | refactor(kms): drain PixmapPool on backend shutdown (pixmap-pool T4) |
| T5 | `a7c2384` | test(kms): synthetic pixmap-pool burst test (pixmap-pool T5) |
| T6 (results doc) | this commit | docs(plans): pixmap-allocation pool validation results |

5 implementation commits (T1–T5); 2 plan commits; 1 results-doc commit = 8 total in the pixmap-pool series.

## Known deferred items

- **`PixmapPool::Drop`'s defensive `queue_wait_idle`.** Shutdown-only; on the partial-init / panic path the buckets are non-empty when Drop runs and `queue_wait_idle` ensures GPU is done before destroying the VkImages. Could be narrowed to a per-batch fence wait when the scheduler-drain isn't called first — but the cost is zero on the steady-state path (`drain()` runs first, buckets are empty, Drop is a no-op). Same shape as `target.rs::initialize_clear`'s `queue_wait_idle` (Phase 5 deferral).
- **`target.rs::initialize_clear` `queue_wait_idle`.** Phase 5 deferral, unchanged. Fresh allocations still take this wait on first use; pool reuse skips `initialize_clear` entirely. A future micro-pass can fence-narrow it.
- **`GlyphAtlas::intern` per-glyph submit pattern.** Phase 5 deferral, unchanged. Text-heavy workloads still take per-glyph one-shot uploads via `run_one_shot_op` (which has a per-op fence now, but it's still one fence per glyph). Atlas-batched uploads via `record_paint_op` is the Phase 6+ shape.
- **Window mirror pooling.** Window VkImages are pool-eligible in shape (`(extent, format)`-keyed) but alloc/free rate is dominated by pixmaps. Profile first if windows become a bottleneck.
- **DRI3-imported pixmaps still take synchronous flush path on free.** Rare in practice (only when a DRI3 client allocates and frees a pixmap within the same composite tick). Acceptable per the round-3 T2-review fold — pooling client-imported memory makes no sense.

## What's next

**Phase 6 — batch-owned refcounted handles + holders wiring.** Promotes to top priority. Subsumes `RetiredCopyImage` / `RetiredDstReadbackImage` / `RetiredMaskImage` (Phase 5) and `PooledPixmapReturn` (this phase) into a uniform refcounted-handle model. The structural fix for the record-time CPU layout tracking caveat; the right place to wire `OutputFrame::holders + PaintBatch::holders` if/when cross-batch / cross-queue dependencies appear. Codex's long-term recommendation from 3B salvage finally gets implemented.

**AMD-specific investigation: deprioritized pending hardware-smoke result on bee.** Per `project_amd_lag_investigation.md` memory, AMD investigation (amdgpu ftrace + ioctl-rate measurement) was the next-priority phase pre-pool. If T6 hardware smoke confirms the pool closes the bee adapta-nokto + mate-cc lag, the AMD investigation is no longer the next-priority — the lag's root cause was the kernel allocator burst (vendor-agnostic), not amdgpu-specific behaviour. If `bee` is still slow post-pool, fall back to amdgpu ftrace + ioctl-rate measurement per the memory; the pool didn't move the needle and there's something amdgpu-specific (a recent regression or RDNA2-particular behaviour) below the kernel allocator.
