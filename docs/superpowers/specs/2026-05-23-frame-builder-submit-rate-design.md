# Frame-builder submit-rate reduction — design idea

**Status:** draft / not started. Captured 2026-05-23 from a Task 6.1
post-mortem discussion, sharpened by codex's review the same day.
Needs full spec work + agreement on scope before it becomes a plan.

**Framing:** not "rewrite v2" — **"replace v2's submit scheduler."**
The data model stays. The discipline that controls *when* GPU work
is submitted changes.

**Load-bearing design goal:** *one scanout opportunity should
consume one submitted frame worth of accumulated X11 work*, not
dozens of independent submits that happen before the next pageflip
can display anything.

**Why this exists.** Task 6.1 (deferred PRESENT completion) closed
the per-PRESENT CPU fence-wait stall but did not change yserver's
underlying *per-X11-operation* submit model. Telemetry on bee MATE
post-Task-6.1: `queue_submit2/s` peak **3304** ≈ 55 submits/frame.
Xorg + glamor on the same machine sustains the same workload at
~1–5 submits/frame (one GL flush + one pageflip per frame); wlroots
sustains it at ~1–3 (one scene render pass + one pageflip). The
**bee drag-lag is now bounded by per-`queue_submit2` `ioctl →
libvulkan_radeon → amdgpu` round-trip cost**, not by anything Task
6.1 touched. Catching up to Xorg/wlroots on submit rate requires
changing yserver's render scheduling discipline.

## The architectural difference

**Xorg + glamor (since 2014).** Each XRender / XCopyArea / XPutImage
operation records GL commands into glamor's lazy command buffer. No
GPU submission happens until `glFlush`/`glFinish`/SwapBuffers. Per
frame: glamor amortises hundreds-to-thousands of X11 paint ops into
one GL flush → one `queue_submit2` → one pageflip.

**wlroots.** Compositor walks the scene graph once per frame, issues
GL draw calls into one render pass, ends with one submit. DMA-buf
passthrough + direct scanout where possible (zero compositing for
fullscreen apps). Per frame: 1–3 submits total.

**yserver v2 (today).** Each X11 paint primitive → its own
`engine.copy_area` / `engine.render_composite` / `engine.fill_rect`
→ records its own CB → submits with its own fence. `cow_batch` +
`render_batch` aggregate **within an op class** (5–8 ops/batch on
the bee capture, dependent on intervening non-batched ops). Scene
compose is a separate CB with its own submit. KMS pageflip with
IN_FENCE_FD chains them in order, but the kernel still sees N
submits per frame.

## Proposed shape

A `FrameBuilder` (working name) state machine on `KmsBackendV2`:

- `FrameState::Closed` — between frames; first paint op transitions
  to Open and opens a single long-running CB.
- `FrameState::Open { frame_cb, frame_ticket, layouts, … }` —
  every paint op appends to `frame_cb`. Layout transitions on
  drawable images are deferred until the next op that needs a
  different layout, batched into one `vkCmdPipelineBarrier`.
- Frame-end trigger: vblank deadline (KMS pageflip retire) or a
  watchdog timer (e.g. "if frame open >16 ms, close it").
- Scene compose appends to the same `frame_cb` instead of its own
  CB + submit. A barrier between client paints and compose; end
  the CB; submit once with IN_FENCE_FD into KMS.
- One `queue_submit2` + one pageflip per frame.

PRESENT completion (Task 6.1's machinery) maps onto this naturally:
attach the completion semaphore to the per-frame submit; drain when
its `SYNC_FD` signals. The current per-batch semaphore design
becomes per-frame.

## Phasing (each phase shippable independently)

The highest-risk surface is **image layout / lifetime correctness
across mixed ops**. The phasing below is measurement-first +
narrow-surface-first, deliberately *not* "merge batches blindly."

1. **Measure submit sources precisely.** Add trace categories for
   every submit source during bee drag. We need to know whether
   the worst offenders are COW copies, RENDER batches, fills,
   `GetImage` / `CopyPlane` readbacks, glyph uploads, scene
   compose, or something else. The existing
   `yserver-mate.submit.tsv` has `kind` but not consistent
   granularity across all paths; close those gaps first. ~200–500
   lines, mostly telemetry plumbing. Output is a quantitative
   ranking of the per-frame submit budget; everything after this
   targets the top offenders by ranking, not by guess.
2. **Introduce a frame submit scheduler shell** without moving any
   ops yet. `FrameBuilder` / `FrameScheduler` owns: the open CB,
   the frame ticket, touched drawables, staging resources, pending
   PRESENT completions, and the abort path. Existing ops continue
   to submit individually — the scheduler is dormant. ~500–1000
   lines, almost all new infrastructure, no behaviour change. The
   point is to prove the lifecycle + error handling before any
   real op is moved into it.
3. **Move COW copy + scene compose into one frame submit.** This
   is the bee drag hot path — the smallest move that should
   produce a large measurable win. Append COW copies and final
   compose into one submit per scanout opportunity. ~1000–1500
   lines, narrow surface (`engine.cow_copy_area` +
   `scene.tick` + `backend::maybe_composite`). Expected: bee drag
   submits/frame from ~55 to ~20–30, with most of the lag relief.
4. **Fold in RENDER batches.** Once COW+compose works, add
   `render_composite` batch append into the same frame CB. Removes
   the current artificial split between cow_batch and render_batch
   driving intermediate flushes. ~500–1000 lines. Expected: bee
   drag submits/frame to ~10–15.
5. **Fold in fills / PutImage / glyph uploads where safe.** These
   need more staging-buffer and descriptor lifetime care. Do them
   after the frame model is stable. ~500–1000 lines, distributed
   across paint ops. Expected: bee drag submits/frame ~5–10,
   approaching Xorg/glamor parity.
6. **Keep readback as explicit flush points.** `GetImage`,
   `CopyPlane` CPU readback, and any protocol path that requires
   CPU-visible pixels close/submit/wait the frame when needed.
   They are exceptions, not the normal path. ~200 lines of
   documentation + an `escape_hatch_flush()` API on
   `FrameScheduler`.

Total: ~3000–5000 net lines across `engine.rs`, `backend.rs`,
`scene.rs`, `store.rs`, tests. 2–3 weeks focused work. Each phase
is independently shippable + measurable, so we know phase-by-phase
whether the strategy is delivering its predicted submit-rate cut.

## What this touches (rough surface)

- **`crates/yserver/src/kms/v2/engine.rs`** — all engine ops switch
  from `begin_op_cb → record → end_and_submit_op` to append-only.
- **`crates/yserver/src/kms/v2/backend.rs`** — `FrameState` machine;
  every paint dispatch checks "is a frame open?".
- **`crates/yserver/src/kms/v2/scene.rs`** — `tick` appends to the
  open frame CB instead of submitting its own.
- **`crates/yserver/src/kms/v2/store.rs`** — layout tracking moves
  from per-op transitions to per-frame planning.
- **`crates/yserver/src/kms/v2/platform.rs`** — `FenceTicket` /
  `FencePool` lifecycle changes (one ticket per frame instead of
  per submit). Touches descriptor pool ring strategy.
- **`crates/yserver-core/src/core_loop/process_request.rs`** —
  PRESENT::Pixmap timing: enqueue continues to fire at request time,
  but the underlying submit is deferred until frame close.
- **Tests** — every engine test that asserts per-op submit
  semantics breaks. Acceptance tests likely need updating. Telemetry
  counter semantics change shape (`paint_submits/s` becomes
  ~`frames/s ≈ 60`, not 2000+).

## Open questions

- Frame boundary detection: KMS pageflip retire is the natural
  trigger, but yserver may want to close a frame *before* the next
  vblank if the CB is getting too large. What's the right
  watermark? Memory? Op count? Time?
- Error handling: if an engine op fails mid-frame, the partial CB
  needs a clean abort path. Currently per-op failures isolate
  cleanly. Frame-builder needs `frame_abort()` that retires the
  partial CB without breaking ordering invariants for subsequent
  frames.
- Layout transition planning: what's the data structure? A
  pending-transition map keyed by `DrawableId`, applied lazily
  before the next op that disagrees? Or one big pre-frame pass?
- Descriptor pool ring: one pool per frame instead of one per op
  changes the pool depth math. Stage 5 Task 4 sizing may need a
  redo.
- Telemetry: counters need redesigning around per-frame stats. The
  existing `paint_submits/s` shape becomes meaningless.

## Dependencies / unlock conditions

- Task 6.1 (deferred PRESENT completion) landed — PRESENT timing
  decoupled from per-op submit.
- Task 3 (cow_batch aggregation) and Task 4 (DescriptorPoolRing)
  provide the foundations; this builds on both.
- Should be its own Stage 5 sub-task (e.g. "Task 7 — frame-builder
  submit-rate reduction") with its own spec round + plan round
  before implementation.

## Out of scope (intentional)

- Direct scanout for fullscreen GL clients (wlroots-style) — would
  be a separate Stage 5 task once frame-builder lands.
- Plane composition (cursor + scene on different planes) — already
  partially landed; orthogonal.
- Refactoring v1 — v1 is in maintenance mode; this is v2-only.
- Blind merging of `cow_batch` + `render_batch` before the frame
  scheduler exists. That was an earlier draft of this spec;
  codex's review correctly noted the highest-risk surface is image
  layout / lifetime correctness across mixed ops, which the
  scheduler shell should prove first. The structural merge falls
  out naturally once Phases 2–4 land.

## References

- bee 2026-05-22 perf-branch capture: `docs/status.md` § "Bee
  hardware capture 2026-05-22" — establishes the per-`queue_submit2`
  kernel-round-trip baseline (~470 µs per submit, ~35 submits/frame,
  ~2119 submits/s).
- bee 2026-05-23 post-Task-6.1 capture: `docs/status.md` § "2026-05-23
  bee hardware close — Task 6.1 functionally fixed" —
  `queue_submit2/s` peak 3304, drag still laggy.
- Glamor design history: `gl-renderer.c` in xserver. The 2014 EXA →
  glamor migration is the closest analogue to what this proposes.
