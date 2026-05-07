# Phase 4.1 — Vulkan compositor on KMS (design)

Status: design, awaiting implementation plan
Author: brainstormed 2026-05-07
Branch target: `accel` (recreated from master after `kms-xts-tooling` merges)

## 1. Goals and non-goals

### Goal

Replace the pixman-based CPU compositor in the KMS backend
(`crates/yserver/src/kms/`) with a Vulkan compositor built on a
per-window-texture scene-graph model. The `Backend` trait surface
stays unchanged, so `yserver-core` and the ynest backend are
untouched.

### In scope

- Vulkan instance / device / queue / allocator setup; GBM-backed
  scanout images; atomic KMS pageflip with explicit fences
  (DRM syncobj ↔ `VK_KHR_external_semaphore_fd`).
- Per-window GPU image owned by each X window; per-pixmap GPU image
  owned by each X pixmap.
- Vulkan implementations of every core X drawing op currently served
  by pixman: `PolyFillRectangle`, `CopyArea`, `CopyPlane`, `PolyLine`,
  `PolySegment`, `PolyPoint`, `PolyArc`, `PolyFillArc`, `PutImage`,
  `GetImage`, `ImageText{8,16}`, `PolyText{8,16}`, `ClearArea`,
  tiles, stipples, GC clipping, plane masking.
- Vulkan implementations of the RENDER ops currently served by
  pixman: `Composite`, `CompositeGlyphs`, `FillRectangles`,
  `Trapezoids`, `Triangles`, `SetPictureFilter`,
  `SetPictureClipRectangles`, `SetPictureTransform`.
- Glyph atlas — FreeType still rasterises CPU-side; results upload
  to a shared GPU texture atlas.
- MIT-SHM v1.2 — `PutImage`, `GetImage`, `CreatePixmap`, `CreateSegment`,
  the legacy `Attach` minor 1. The wire-level extension already lives
  in `yserver-core` (Phase 3.5); this phase replaces the
  pixman-backed staging copies with Vulkan upload/readback paths.
  Zero-copy dma-buf import for SHM segments is *not* in scope —
  that's a Phase 4.2 concern via DRI3.
- Cursor compositing — the existing software cursor design
  ([`docs/plans/2026-05-04-software-cursor.md`](../../plans/2026-05-04-software-cursor.md))
  becomes a Vulkan quad in the composite pass.

### Out of scope (Phase 4.1)

- DRI3, Present, SYNC fences, GLX, EGL/Vulkan WSI — all Phase 4.2 / 4.3.
- ynest backend changes — none.
- Multi-GPU / hotplug-change-of-GPU — keep current single-GPU
  assumption.
- Direct-scanout / overlay-plane fast path — folded into Phase 4.2
  alongside Present.
- VRR / tearing-control — not relevant until per-window flip
  semantics arrive in Phase 4.2.

### Explicit non-goal

Parity is measured by `xts5` + `rendercheck` against the post-merge
baseline, not by output-pixel-bit-equality with the pixman backend.
Two correct pipelines may produce subtly different anti-aliased
edges; the test suites are the arbiter.

## 2. Architecture

### Module layout

```
crates/yserver/src/kms/
├── mod.rs
├── backend.rs        # KmsBackend: Backend trait impl, dispatches into vk/
├── event.rs          # input events, unchanged
├── xkb.rs            # keymap, unchanged
├── fonts.rs          # FreeType rasterisation only (CPU); upload via vk/glyph.rs
└── vk/               # all Vulkan code lives here
    ├── mod.rs
    ├── instance.rs   # VkInstance, debug callback, extension selection
    ├── device.rs     # physical device pick, logical device, queue family, allocator
    ├── memory.rs     # gpu-allocator wrapper, image/buffer creation helpers
    ├── scanout.rs    # GBM bo allocation, dma-buf import as VkImage, KMS pageflip
    ├── target.rs     # WindowImage, PixmapImage — per-target GPU resources
    ├── glyph.rs      # shared glyph atlas, FT bitmap → atlas upload
    ├── pipelines.rs  # graphics pipelines (solid fill, image blit, RENDER blend, glyph)
    ├── ops/          # one file per X drawing-op family
    │   ├── poly.rs       # PolyLine, PolySegment, PolyPoint, PolyArc, PolyRectangle
    │   ├── fill.rs       # PolyFillRectangle, PolyFillArc, FillPoly
    │   ├── copy.rs       # CopyArea, CopyPlane
    │   ├── image.rs      # PutImage, GetImage
    │   ├── text.rs       # ImageText/PolyText (uses glyph.rs)
    │   ├── render_op.rs  # Composite, CompositeGlyphs, FillRectangles,
    │   │                 # Trapezoids, Triangles
    │   └── tile.rs       # tiles, stipples, plane mask helpers
    ├── batch.rs      # per-target command-buffer batching, flush triggers
    └── compositor.rs # frame composite pass: walk window tree, composite into scanout
```

`backend.rs` keeps its `HashMap<u32, _>` resource maps; the value
types switch from `WindowState`/`PixmapState` (pixman-backed) to
`DrawableImage` (Vulkan, see "Per X resource" subsection) over
the lifetime of sub-phase 4.1.4.

### Stack choice (fixed)

- `ash` for raw Vulkan bindings — matches the codebase's hand-rolled,
  no-framework style. `vulkano` and `wgpu` rejected: `vulkano` is
  high-level-safe (fights with our explicit-fence requirements);
  `wgpu` is a heavy abstraction layer for a 2D X compositor.
- `gpu-allocator` for VMA-style memory management (pure Rust,
  well-maintained).
- `gbm` crate for GBM bo allocation. Already an indirect dep via
  the DRM crates; promoted to a direct dep here.
- `drm` crate (already in use).
- No new dep on `smithay` or any compositor framework.

### Object lifetimes

**Long-lived** (created at `KmsBackend::new`, dropped at shutdown):

- `VkInstance`, `VkPhysicalDevice`, `VkDevice`.
- One graphics+transfer queue (single queue family).
- Descriptor pools, pipeline cache, all graphics pipelines.
- Glyph atlas (`VkImage` + offscreen layout state).
- Command pools (one per frame-in-flight).

**Per scanout / per CRTC:**

- 3 GBM bos rotating as scanout buffers, each imported as a
  `VkImage` via `VK_EXT_external_memory_dma_buf` +
  `VK_EXT_image_drm_format_modifier`. Triple-buffered so frame N's
  GPU work doesn't block frame N-1's KMS scan-out.
- **Semaphore objects** (`VkSemaphore`): per CRTC, one
  `VK_KHR_external_semaphore_fd`-capable binary semaphore per bo
  for the in-flight render → KMS handoff (3 total in the steady
  state). Created once at backend init via `vkCreateSemaphore` +
  `VkExportSemaphoreCreateInfo`, destroyed at shutdown. **The
  object is long-lived; only the exported fd churns.** Standard
  binary-semaphore-with-export pattern: each `vkQueueSubmit2`
  uses the bo's `VkSemaphore` as `signalSemaphore`; immediately
  after, `vkGetSemaphoreFdKHR` exports the binary payload as a
  fresh fd (transferring payload ownership to the fd) — the
  `VkSemaphore` returns to no-payload state and is reusable on
  the next submit. On retry/preempt we close the (still-payloaded
  or already-signalled) fd; the kernel destroys the payload; the
  underlying `VkSemaphore` is again reusable. No leak per retry.
- **Per-buffer release fence.** Each of the 3 bos owns its own
  release fence (a DRM syncobj exported as a
  `VK_KHR_external_semaphore_fd`). State machine per bo:
  - `Free` — not in flight; GPU may write into it. No fence
    handles attached.
  - `Recording` — composite CB being recorded for this bo. No
    submit yet; no fences live.
  - `Submitted` — `vkQueueSubmit2` issued, signalSemaphore
    handed to KMS as `IN_FENCE_FD`. The signalSemaphore is
    exported as a one-shot fd via
    `vkGetSemaphoreFdKHR(VK_SEMAPHORE_TYPE_BINARY)`. The fd is
    held by the bo until the atomic commit either accepts (kernel
    consumes & closes the fd) or rejects (we still own it). KMS
    has not yet been told to flip.
  - `Pending` — `drmModeAtomicCommit` returned 0 (accepted).
    `IN_FENCE_FD` ownership has transferred to the kernel; we
    null the bo's reference. `OUT_FENCE_PTR` was passed in and
    KMS allocated a new fd which we now own — store as the bo's
    release fence.
  - `OnScreen` — pageflip-complete uevent for this bo arrived.
    Bo is scanning out. The release fence is now signal-pending
    (KMS will signal it when the *next* flip retires this bo).
  - `Retiring` — a later flip's pageflip-complete arrived; this
    bo is no longer on screen. Release fence is signalled. When
    any GPU users still reading the bo (e.g. as a damage-diff
    source) complete, bo returns to `Free`. Fence fd is closed.

  Transitions and their fence-handle ownership rules:

| from → to | trigger | fence handles |
|---|---|---|
| `Free → Recording` | acquire for next frame | none |
| `Recording → Submitted` | `vkQueueSubmit2` | export `IN_FENCE_FD` from signalSemaphore (we own it) |
| `Submitted → Pending` | atomic accepted (rc=0) | kernel consumes `IN_FENCE_FD`; we receive `OUT_FENCE_PTR` (we own it now) |
| `Submitted → Recording` | atomic rejected `-EBUSY` (retry) | discard CB to pool; close the still-payloaded/signalled `IN_FENCE_FD` (kernel destroys the payload — bo's `VkSemaphore` returns to no-payload state, reusable). Allocate a fresh CB; re-record the same composite work; re-submit using the **same long-lived bo `VkSemaphore`** as `signalSemaphore`; export a fresh fd; retry the atomic. Cost: one extra CB recording (~200 ops). No object leaks per retry — only fd handles cycle. |
| `Submitted → Free` | modeset preempts (e.g. CRTC reconfigure mid-flight) | wait host-side on the GPU CB completion fence; close `IN_FENCE_FD`; CB returns to pool |
| `Pending → OnScreen` | first pageflip-complete | none |
| `OnScreen → Retiring` | next flip's pageflip-complete | release fence signal-pending |
| `Retiring → Free` | all GPU readers done | close release fence fd |
| any → `Free` (modeset reset) | KMS resource gone (hotunplug, mode change) | wait/cancel as appropriate; close all fence fds attached to bo |

  The acquire path picks any `Free` bo for the next frame's render
  target. With 3 bos there's almost always one available; if not,
  block on the oldest bo's release fence host-side.

- **Modeset / hot-config events** (resize, mode change, hotplug):
  - Bos in `Recording` are dropped (CB recording aborted; CB
    returned to pool).
  - Bos in `Submitted` need a host-side wait on the in-flight
    GPU work, then `IN_FENCE_FD` close, then drop.
  - Bos in `Pending` / `OnScreen` / `Retiring` need their release
    fences waited-on then closed, before the bos themselves are
    freed.
  - Then: free all bos, allocate fresh ones with the new
    dimensions/modifier, reset state machine.

- **Dropped commits** (atomic returns `-EBUSY`): see the
  `Submitted → Recording` row in the table above. We discard the
  old CB + close the fence fd, allocate a fresh CB, re-record,
  re-submit reusing the bo's long-lived `VkSemaphore`, and retry
  the atomic with a freshly-exported `IN_FENCE_FD`. Pays one
  extra CB record per retry — small and predictable. No
  `VkSemaphore` leaks; the object is created at init and reused
  forever.

**Per X resource — `DrawableImage` abstraction:**

Every X drawable backed by GPU memory in this backend is held as
a `DrawableImage`, a single type with two backing variants:

```rust
struct DrawableImage {
    vk_image: VkImage,
    extent: vk::Extent2D,
    format: vk::Format,
    backing: ImageBacking,
    // … sampler, descriptor-set caches, damage region …
}

enum ImageBacking {
    /// Memory allocated and owned by KmsBackend via gpu-allocator.
    /// All Phase 4.1 windows and pixmaps use this variant.
    ServerOwned {
        allocation: gpu_allocator::vulkan::Allocation,
    },
    /// Memory imported from a client-provided dma-buf fd.
    /// Phase 4.2 (DRI3) introduces this variant; not constructed
    /// by anything in 4.1, but the variant is present from day one
    /// so the compositor and drawing-op call sites are written
    /// against `DrawableImage` rather than a concrete server-owned
    /// type. The compositor reads this variant identically to
    /// `ServerOwned`; the difference lives in the lifecycle and
    /// sync rules:
    ///   - drop closes the imported fd; allocation is not ours
    ///     to free.
    ///   - acquire/release fences from the client (Phase 4.2
    ///     adds these via DRI3 `BufferReleaseFence` / Present
    ///     `pixmap-from-buffers`) live alongside the image and
    ///     gate per-frame access.
    Imported {
        dma_buf_fd: std::os::fd::OwnedFd,
        // … modifier, plane offsets, acquire/release sync state …
    },
}
```

This is the load-bearing abstraction for the Phase 4.2 hand-off.
The compositor and every drawing op accept `&DrawableImage` and
never branch on `ImageBacking` — the **read path** is uniform.
What differs across variants is **construction, drop, format
source-of-truth, and sync**:

- `ServerOwned` images derive their format from the X
  `class`/`depth` (server picks).
- `Imported` images take their format and DRM modifier from the
  client — the dma-buf was allocated by the client's GBM/EGL/
  Vulkan stack with parameters we did not choose. Phase 4.2's
  DRI3 protocol carries the format on the wire.

To make this explicit, `DrawableImage` exposes three constructors
rather than a single one keyed on the variant:

```rust
impl DrawableImage {
    /// Build a window-class image. format is fixed to
    /// VK_FORMAT_B8G8R8A8_UNORM (alpha for compositing).
    fn new_server_owned_window(
        ctx: &VkCtx, w: u32, h: u32,
    ) -> Result<Self, …>;

    /// Build a pixmap-class image. format is derived from `depth`
    /// per the depth → format table below.
    fn new_server_owned_pixmap(
        ctx: &VkCtx, w: u32, h: u32, depth: u8,
    ) -> Result<Self, …>;

    /// Phase 4.2: build from a client-supplied dma-buf fd. format
    /// and modifier are caller-supplied; no depth → format
    /// derivation. Acquire/release fences (also caller-supplied)
    /// gate per-frame access.
    fn from_dmabuf(
        ctx: &VkCtx,
        dma_buf_fd: OwnedFd,
        format: vk::Format,
        modifier: u64,
        w: u32, h: u32,
        // … sync handles, plane offsets …
    ) -> Result<Self, …>;
}
```

The constructors live in 4.1's `vk/target.rs`. Only the first two
are *called* in 4.1; `from_dmabuf` is present (compiles, has a
unit test) so 4.2 doesn't need to retrofit a new constructor onto
a sealed type.

**Window-class drawables.** Each X window with `class != InputOnly`
→ owns one `DrawableImage` (built via `new_server_owned_window`)
for its **entire lifetime**, from `CreateWindow` to
`DestroyWindow`. The image persists across unmapping; X clients
are allowed to draw into unmapped windows and toolkits/WMs rely
on offscreen contents being preserved (e.g. wmaker iconified app
contents, GTK background-thread redraws). Format always
`VK_FORMAT_B8G8R8A8_UNORM` regardless of the window's X depth —
we carry alpha to make compositing trivial; depth-24 windows
expose `0xFF` alpha.

**Pixmap-class drawables.** Each X pixmap → owns one
`DrawableImage` (built via `new_server_owned_pixmap`) for its
lifetime. Format per declared X depth (depth 32 → `B8G8R8A8`,
depth 24 → `B8G8R8A8` with alpha ignored on read-out, depth 8 →
`R8`, depth 1 → `R8` with shader bit-extract). Created on
`CreatePixmap`, freed on `FreePixmap`.

**Resize / `bit_gravity`.** `ConfigureWindow` that changes
width/height allocates a new `DrawableImage` and copies preserved
content from the old image per the window's `bit_gravity`
attribute. See "bit_gravity rect math" subsection below for the
exact formulas. The old `DrawableImage` goes onto the recycle
pool.

**Map / unmap.** No image lifecycle changes — image persists.
Unmapped windows still receive drawing ops; their content is
invisible until the window is remapped.

**Free list / recycle pool**, power-of-two-bucketed by area, to
avoid alloc churn on rapid resize. Pool is `ServerOwned` only;
imported images are dropped immediately on release.

### `bit_gravity` rect math

Given `(W_old, H_old) → (W_new, H_new)`, define the preserved
extent:

```
W_pres = min(W_old, W_new)
H_pres = min(H_old, H_new)
```

Each gravity selects an `(ax, ay)` anchor pair where each axis is
0 (left/top), ½ (center) or 1 (right/bottom):

| gravity | (ax, ay) |
|---|---|
| NorthWest (default) | (0, 0) |
| North | (½, 0) |
| NorthEast | (1, 0) |
| West | (0, ½) |
| Center | (½, ½) |
| East | (1, ½) |
| SouthWest | (0, 1) |
| South | (½, 1) |
| SouthEast | (1, 1) |

The blit rect is then (with **integer truncation** for the ½
anchor):

```
src = ( (ax_num * (W_old - W_pres)) / ax_den,
        (ay_num * (H_old - H_pres)) / ay_den,
        W_pres, H_pres )
dst = ( (ax_num * (W_new - W_pres)) / ax_den,
        (ay_num * (H_new - H_pres)) / ay_den,
        W_pres, H_pres )
```

Where `(ax_num, ax_den)` is `(0, 1)` for left, `(1, 2)` for
center, `(1, 1)` for right (and same for y). The rounding rule
matches the X server convention — center anchor with an odd-pixel
delta truncates to the smaller half (one extra pixel on the
right/bottom).

For shrinks the `src` offsets are non-zero (selecting the
preserved chunk); for grows the `dst` offsets are non-zero
(positioning the preserved chunk in the larger image). For the
identity-axis case (e.g. width unchanged but height grew), one
axis evaluates to (0, 0, W_new, H_old) which is what we want.

Mixed shrink/grow (one axis up, one axis down) falls out of the
formula — `min` handles each axis independently. Asymmetric
resize where the preserved region falls partly outside the new
image is impossible by construction: `W_pres ≤ W_new`,
`H_pres ≤ H_new`, and dst offsets are non-negative.

#### Static gravity

`StaticGravity` keeps window contents fixed relative to the
**root**, not the parent — clients use it to keep absolute screen
positions stable across reparenting and resize. For our
compositor, this matters when the `ConfigureWindow` request
*also* shifts the window's parent-relative origin: the content
must move by `-(Δx_screen, Δy_screen)` inside the window's local
frame to compensate.

Concretely, given the window's old root-relative origin
`(X_old, Y_old)` and new `(X_new, Y_new)`:

```
shift = (X_new - X_old, Y_new - Y_old)   // root-frame delta
W_keep = max(0, min(W_old, W_new) - |shift.x|)
H_keep = max(0, min(H_old, H_new) - |shift.y|)
src = (max(0, shift.x), max(0, shift.y), W_keep, H_keep)
dst = (max(0, -shift.x), max(0, -shift.y), W_keep, H_keep)
```

The `max(0, ...)` on `W_keep` / `H_keep` matters: if the window
moves farther than the overlap area while also resizing, the
preserved extent goes negative *before* the clamp and the blit
would be invalid. With the clamp, a zero-extent preserved region
falls through naturally — we skip the blit, fill the entire new
image with background, and let yserver-core generate `Expose`
covering the full new extent. Equivalent to a per-axis fallback
to `Forget` semantics in the degenerate case.

If the window only resized (no positional shift), `shift = (0, 0)`
and the formula reduces to `src = dst = (0, 0, W_pres, H_pres)` —
the simple case. If the window shifted but didn't resize, content
is offset within the new image to compensate. If both, the two
effects compose.

#### Forget gravity

`Forget` skips the blit entirely. The new `DrawableImage`'s fill
behaviour depends on the window's `background` attribute:

| `background` | new image fill |
|---|---|
| `Pixel(p)` | `vkCmdFillBuffer`-equivalent solid fill with `p` |
| `Pixmap(pid)` | tiled blit of pixmap `pid` over the entire new extent |
| `ParentRelative` | tiled blit of the *parent's* background, offset so the parent's origin lands at the parent-relative origin of this window. If the parent is also `ParentRelative`, recurse up to the first concrete background (root window's background is the recursion floor — guaranteed concrete by core). |
| `None` | no fill — image contents are undefined; X spec permits this. yserver-core still generates `Expose` events covering the full new extent so the client can repaint. |

Per X spec, `Forget` semantically discards the previously-drawn
contents; the backend signals "nothing to preserve" to
yserver-core so core generates `Expose` for the entire new
extent (not just the newly-allocated regions).

#### Background fill regions (non-Forget gravities)

After the preserve blit lands, the not-covered area of the new
image = `(0, 0, W_new, H_new) ∖ dst`. Computed as up to four
rectangles bordering the dst rect (any may be empty). Filled
with the window's background attribute using the same rule as
`Forget` above (Pixel / Pixmap / ParentRelative / None). Damage
union covers exactly these rectangles; `Expose` events from
yserver-core land for the same region.

### RENDER attribute matrix

Phase 3.5 already plumbed every attribute below through the
protocol layer. Phase 4.1 must ship a Vulkan implementation for
each — not a "transforms, filters, clips" handwave. Mapping:

| Picture attribute | X11 values | Vulkan implementation |
|---|---|---|
| `repeat` | None / Normal / Pad / Reflect | `VK_SAMPLER_ADDRESS_MODE_*` on the source sampler. `Pad` = `CLAMP_TO_EDGE`. None = sample-zero outside source rect (border colour). |
| `alpha_map` + `alpha_x_origin` / `alpha_y_origin` | optional second pixmap providing per-pixel alpha | second sampled image bound to a second descriptor-set slot; shader multiplies sampled colour by `alpha_map`'s alpha at the offset coords. |
| `clip_mask` | None / Pixmap (depth 1) / Region | None: shader path A. Region: scissor rectangles in the render pass. Pixmap: shader path B sampling the depth-1 mask as an `R8` (bit-extracted) and discarding fragments where mask=0. |
| `subwindow_mode` | ClipByChildren / IncludeInferiors | composite traversal includes/excludes sub-windows when sampling the source. Drives whether we sample the parent window's image or walk child window images too. |
| `poly_edge` | Smooth / Sharp | `Smooth` = analytic AA in the trapezoid/triangle vertex+fragment shaders. `Sharp` = hard edges, no MSAA. We render to a non-MSAA target; AA is shader-side. |
| `poly_mode` | Precise / Imprecise | always Precise — fixed-point coordinates carried through unchanged into vertex shader UV. |
| `dither` | enum, mostly ignored by modern X | ignored — we render at 32 bpp throughout. |
| `component_alpha` | bool | when true, source's RGB channels each act as a per-channel alpha mask (sub-pixel font rendering path). Separate composite pipeline that takes a 3-component mask and applies per-channel `over` blending. |
| `graphics_exposures` | bool | not applicable to Vulkan rendering itself; affects only whether `GraphicsExpose`/`NoExpose` events fire, which `yserver-core` handles. |

**PictOp values.** RENDER defines three op families:

- **Standard** (0–12): Clear, Src, Dst, Over, OverReverse, In,
  InReverse, Out, OutReverse, Atop, AtopReverse, Xor, Add. All 13
  map to Vulkan **fixed-function blend state** —
  `VkPipelineColorBlendStateCreateInfo` with the appropriate
  src/dst factors. One pre-baked pipeline per op.
- **Disjoint** (16–27): DisjointClear, DisjointSrc, DisjointDst,
  DisjointOver, DisjointOverReverse, DisjointIn, DisjointInReverse,
  DisjointOut, DisjointOutReverse, DisjointAtop, DisjointAtopReverse,
  DisjointXor.
- **Conjoint** (32–43): same 12-element shape as Disjoint.

Disjoint and Conjoint operators have semantics that **cannot be
expressed in Vulkan fixed-function blend**. They are defined by
control functions `F_a(Sa, Da)` and `F_b(Sa, Da)` per the X
Render Extension Spec ([renderproto §7.1, "Disjoint Operators"
and §7.2, "Conjoint Operators"][renderproto-ops]). The general
form is:

```
result_alpha = Sa * F_a + Da * F_b
result_color = (Sc * Sa * F_a + Dc * Da * F_b) / max(ε, result_alpha)
```

For Disjoint operators, F_a/F_b take values from
`{0, 1, min(1, (1-Da)/Sa), min(1, (1-Sa)/Da), max(0, 1-(1-Da)/Sa),
max(0, 1-(1-Sa)/Da)}` per a 12-row table (one F_a/F_b pair per
operator: Clear/Src/Dst/Over/OverReverse/In/InReverse/Out/
OutReverse/Atop/AtopReverse/Xor). Conjoint operators use a
parallel 12-row table with `min` ↔ `max` swaps. The Render
spec linked above is the canonical source; we encode the table as
a `const` shader constant array indexed by op specialization
constant.

[renderproto-ops]: https://cgit.freedesktop.org/xorg/proto/renderproto/tree/renderproto.txt

Pixman handles all 24 ops via per-pixel software combine; the
Vulkan port does the equivalent with **shader-side
read-modify-write** of the destination. The fragment shader reads
its own destination texel as an input-attachment, runs the
control-function math, and writes the result.

Vulkan mechanism: `VK_KHR_dynamic_rendering_local_read` (the
modern path — Mesa Venus + RADV + ANV + lvp all support it). The
ShaderRMW pipeline is set up with:

- `VkRenderingAttachmentInfoKHR` listing the destination as both
  a colour attachment (write) and an input attachment (read).
- `VkRenderingAttachmentLocationInfoKHR` to map the colour-write
  slot to the same physical attachment.
- `vkCmdSetRenderingInputAttachmentIndicesKHR` to map the input
  read to the same attachment.
- Pipeline created with the matching layout
  (`VK_IMAGE_LAYOUT_RENDERING_LOCAL_READ_KHR` for the input
  attachment slot).

Older `VK_EXT_attachment_feedback_loop_layout` path is the
fallback if `dynamic_rendering_local_read` is unavailable. Both
are present in Mesa ≥ 24.x; we don't anticipate needing a
software-emulation fallback on our target drivers.

This is a **separate render-pass shape** from the standard
fixed-function blend pass; the compositor batches
Disjoint/Conjoint draws into their own pass.

Phase 4.1 must implement all 37 operators, since rendercheck 1.6
exercises Disjoint/Conjoint in the triangles suite (the upstream
`3d7add9 triangles: Fix tests for conjoint and disjoint ops`
fix is precisely the reason we require ≥1.6) and pixman currently
serves them.

**Filters** (`SetPictureFilter`):

| filter | Vulkan |
|---|---|
| `Nearest` | `VK_FILTER_NEAREST` on the sampler |
| `Bilinear` | `VK_FILTER_LINEAR` |
| `Convolution` (kernel attached) | shader-side convolution; up to 7×7 kernel uploaded as a UBO |
| `Best` / `Fast` | aliased to `Bilinear` and `Nearest` respectively |

**Transforms** (`SetPictureTransform`): a 3×3 fixed-point projective
matrix per source picture, applied in the vertex shader as a
homogeneous transform of the destination quad's UV → source
sample coordinate. Identity by default; transformed sources need a
non-trivial vertex shader so the rasteriser interpolates UV
correctly.

`ChangePicture` updates carry exactly one of these attributes at a
time and rebind/recompile the per-Picture descriptor set lazily.

### RENDER pipeline-key model

Picture attributes form a cross-product. Naïve enumeration is
intractable (>10⁴ combinations); rendercheck doesn't test the
entire space, but it does test enough cross-products that
"implement attributes individually" passes smoke and fails the
suite. We use a **pipeline-key tuple** that uniquely identifies
each compiled Vulkan pipeline, and explicitly bound the supported
key space.

```rust
struct RenderPipelineKey {
    op:               PictOp,         // 0..=12 | 16..=27 | 32..=43
    op_family:        OpFamily,       // FixedBlend | ShaderRMW (derived from op)
    src_repeat:       Repeat,         // None | Normal | Pad | Reflect
    has_mask:         bool,
    mask_repeat:      Repeat,         // ignored when !has_mask
    has_alpha_map:    bool,           // src has CPAlphaMap set
    clip:             ClipKind,       // None | Region | Pixmap
    src_filter:       Filter,         // Nearest | Bilinear | Convolution
    mask_filter:      Filter,         // ignored when !has_mask
    src_transform:    bool,           // identity vs non-identity
    mask_transform:   bool,
    component_alpha:  bool,
    subwindow_mode:   SubwindowMode,  // ClipByChildren | IncludeInferiors
}

enum OpFamily {
    /// Standard operators 0..=12: pipeline uses fixed-function
    /// VkPipelineColorBlendStateCreateInfo + a colour-write
    /// fragment shader.
    FixedBlend,
    /// Disjoint 16..=27 / Conjoint 32..=43: pipeline uses a
    /// feedback-loop attachment; fragment shader reads dst,
    /// computes operator, writes back. No fixed-function blend.
    ShaderRMW,
}
```

Each key compiles to one Vulkan graphics pipeline. The shader
selects code paths via specialization constants derived from
the key (no per-frame branching on attribute state). `op_family`
is derived from `op` and stored in the key so that pipeline
lookups can be sharded by family.

**Key-space cap (Phase 4.1 ships).** The space is bounded as
follows; combinations *outside* these bounds either don't occur
in rendercheck/xts/real-WM workloads or fall through to a fallback
path:

| dimension | values supported | values deferred |
|---|---|---|
| `op` | standard 0..=12, Disjoint 16..=27, Conjoint 32..=43 (37 total) | none — pixman serves all today, must keep parity |
| `src_repeat` / `mask_repeat` | None / Normal / Pad / Reflect | none |
| `has_mask` | both | — |
| `has_alpha_map` | both | — |
| `clip` | None / Region / Pixmap (depth-1) | clip-pixmap with non-trivial src/mask transform: very rare; falls through to a slow shader path that resolves the clip CPU-side. |
| `src_filter` | Nearest / Bilinear / Convolution | Convolution + non-identity src_transform: 0 hits in rendercheck, real WMs don't issue this combination; deferred. |
| `mask_filter` | Nearest / Bilinear | Convolution mask: 0 hits; deferred. |
| `src_transform` / `mask_transform` | identity / non-identity | — |
| `component_alpha` | both | component_alpha + alpha_map: per RENDER spec the two combine, but rendercheck doesn't test it and no real client we care about uses it; deferred to a known-issues fallback. |
| `subwindow_mode` | both | — |

**`component_alpha` semantics.** When `component_alpha=true`, the
mask's RGB channels each act as an *independent per-channel
alpha*. The composite operation is applied separately for R, G,
B, with each channel's mask value scaling the corresponding
channel of the source. The standard derivation (per the RENDER
spec for sub-pixel font rendering):

```
For Over with component_alpha:
  let m_avg = (mask.r + mask.g + mask.b) / 3
  dst.r = src.r * mask.r + dst.r * (1 - src.a * mask.r)
  dst.g = src.g * mask.g + dst.g * (1 - src.a * mask.g)
  dst.b = src.b * mask.b + dst.b * (1 - src.a * mask.b)
  dst.a = src.a * m_avg   + dst.a * (1 - src.a * m_avg)
```

The alpha output channel uses the average of the three mask
channels (`m_avg`) as the effective mask alpha — this is the
pixman convention and matches what rendercheck's
`cacomposite` test expects.

For other ops (`In`, `Atop`, etc.), the per-channel mask
substitution rule is the same: replace the single `mask.a`
multiplier with the corresponding channel's `mask.{r,g,b}` for
the colour outputs, and use `m_avg` for the alpha output.
Operators `Clear`, `Src`, `Dst`, `Add`, `Xor` interact with
component_alpha via the same per-channel substitution. The
ShaderRMW family operators (Disjoint/Conjoint) compose with
component_alpha by applying the same per-channel substitution
to the F_a/F_b control function inputs. This is implementable as
a single fragment shader specialised on `(op, component_alpha)`,
with the per-channel branch being a specialization constant —
produces 37 × 2 = 74 component-alpha-aware pipeline variants in
total, parameterised over the rest of the key space.

**Pipeline cache.** Because the key space is large but populated
sparsely in any given run, pipelines are compiled lazily on first
use (and warmed at backend init for the most-common keys: `Over`
+ identity transforms + nearest filter + None repeat, ×{has_mask}
×{component_alpha}).  `VkPipelineCache` is persisted to
`~/.cache/yserver/pipeline-cache.bin` so subsequent runs skip
compile.

### Data flow (drawing pipeline)

1. Client request enters `KmsBackend::poly_fill_rectangle(...)` (or
   any other op).
2. Backend looks up the target's `DrawableImage`.
3. Op records GPU commands into the target's *batch command buffer*,
   lazily begun on first op since last flush.
4. Damage region accumulated alongside the recorded commands —
   updated immediately so DAMAGE notifications and compositor scissor
   union see consistent state.
5. Batch is flushed (`vkEndCommandBuffer` + `vkQueueSubmit2`) on:
   - frame boundary (compositor about to read this target),
   - `GetImage` on this target (host wait + read-back),
   - target destruction,
   - 64 KiB or 1024 ops, whichever first (cap to bound CB memory).

### Frame composite pass

Triggered when at least one of: dirty target, window
mapped/unmapped/configured/restacked/reparented, shape/clip/transform
change, cursor moved. Idle desktop = zero GPU work.

Each frame:
1. Compute `frame_damage = ⋃ window_damage[w] translated to screen space, clipped by w's screen-space rect, ∩ w's SHAPE clip if any`.
2. **Per-window culling** before issuing any draw: skip windows
   whose screen-space rect (after stacking + shape clip) does not
   intersect `frame_damage`. Cheap AABB test; cuts the worst case
   from `O(visible windows)` draws to `O(windows touching damage)`.
3. **Occlusion culling** for back-to-front draw: when a window
   above is fully opaque (no alpha, no SHAPE holes) and covers a
   lower window's intersection with `frame_damage`, skip the lower
   draw. Same cheap rect arithmetic; prevents wasted overdraw.
4. Single composite render pass into the scanout image, with
   **scissor rectangles set to `frame_damage`**.
5. For each surviving window in stacking order (back to front),
   draw a quad sampling the window's `VkImage`. Pipeline state per
   window covers PictOp blend (Over for normal windows, Src for the
   bottom window or for the redirect-output path), SHAPE-mask
   sampling if the window has a non-trivial shape, transform.
6. Software cursor draws last as a single quad in the same pass.
7. Single `vkQueueSubmit2`:
   - waitSemaphore: target scanout bo's release fence (signalled
     when KMS finishes scanning it out — see "Per scanout / per CRTC"
     above).
   - signalSemaphore: external fence exported via
     `VK_KHR_external_semaphore_fd`, fed to `drmModeAtomicCommit`
     as `IN_FENCE_FD`.
8. Atomic commit returns `OUT_FENCE_PTR`; the bo's release fence
   adopts that handle.

**What lives where in the clipping pipeline.** Drawing-time clips
(GC `clip_mask`, `SetClipRectangles`, picture `clip_mask`) are
applied during the per-target draw passes — they shape what
actually lands in the window's `VkImage`. Composite-time clips
(stacking obscuration, SHAPE bounding/clip regions on the
destination window, COMPOSITE redirect overrides) are applied in
the composite pass. The two clipping stages are independent: a
fully-opaque window with an oddly-shaped SHAPE region still has
its drawing-op rasterisation reach the full backing image; SHAPE
just gates what reaches the scanout.

The scissor on `frame_damage` is necessary but not sufficient — a
one-pixel damage rect against 500 visible windows still means 500
quad submissions if you don't cull, and most GPUs are limited by
draw-call count, not pixel fill. Steps 2 + 3 above cut both.

### Cross-target draws

`CopyArea(src=window_A, dst=window_B)` reads from A's image and
writes to B's. The pre-frame command buffer needs a barrier between
A's "write" stage and B's "read" stage if A had pending writes
earlier in the same frame. We build a per-frame DAG of target
dependencies, topologically sort, and emit a single barrier at each
seam. Almost always one barrier or zero in practice.

### Same-target overlap

`CopyArea(src=A, dst=A, src_rect, dst_rect)` with
`src_rect ∩ dst_rect != ∅` (overlapping scroll, terminal
line-shift) is a hot path. Vulkan blit/copy hazards are stricter
than pixman's aliasing rules — `vkCmdCopyImage` requires
non-overlapping source and destination regions; `vkCmdBlitImage`
has the same restriction. Three cases:

1. **No overlap** between src_rect and dst_rect → direct
   `vkCmdCopyImage` (or `vkCmdBlitImage` if scaling needed).
2. **Disjoint plane** copies (CopyPlane between depth-1 mask and
   destination drawable) → no aliasing concern; case 1.
3. **Overlapping** src/dst rects on the same image → blit through
   a per-target staging `VkImage` from a recycled pool: copy
   src_rect → staging, then staging → dst_rect. One extra blit
   pays for correctness. Detected at request time by an axis-aligned
   rect-intersection check; the staging path is the exception, not
   the default.

(A shader-scatter alternative — single dispatch reads source
texels, writes dest with offset — is faster on paper but requires
a compute pipeline and a barrier dance for image layout. Default
is the staging-image path; revisit if profiling shows
`CopyArea`-on-self is hot enough to matter.)

### MIT-SHM upload / readback

MIT-SHM v1.2 already lives in `yserver-core` (the FD-passing wire
plumbing landed in Phase 3.5 via `unix_fd::FdReader`). Phase 4.1
replaces the pixman staging copies with Vulkan equivalents.

- **`PutImage`** (`Attach`-mode): client's shm segment is
  `mmap(MAP_SHARED)`'d into `KmsBackend`. PutImage handler:
  1. Allocate a host-visible Vulkan staging buffer sized to the
     image (or recycle from a pool).
  2. `memcpy` from the client's shm region into the staging
     buffer (CPU-side; both pointers are mapped).
  3. `vkCmdCopyBufferToImage` into the target's `VkImage`.
  4. Damage region updated to the put rect.

  No zero-copy: SHM segments are `MAP_SHARED` host pages, not
  dma-buf imports — Vulkan can't bind them as device memory.
  Phase 4.2's DRI3 path is where zero-copy lives.

- **`GetImage`**: standard `GetImage` flush-and-readback path
  (see "Synchronous reads" below), then `memcpy` from the
  host-mapped readback buffer into the client's shm segment.

- **`CreatePixmap`** (server-allocated SHM, MIT-SHM minor 5):
  pixmap gets a normal `DrawableImage` (built via
  `new_server_owned_pixmap`) plus an attached host shm segment;
  client gets the segment fd. Drawing into the pixmap goes
  through Vulkan as usual; SHM-side reads are served by flushing
  + readback into the segment on demand.

- **`CreateSegment`** (MIT-SHM minor 7, server-allocated `memfd`):
  same as `CreatePixmap` modulo the fd source. Already implemented
  in core; backend just needs to track the segment-to-VkImage
  binding.

### Synchronous reads (`GetImage`)

X clients calling `GetImage` need pixel data immediately. We:

1. Flush that target's pending batch CB.
2. `vkQueueSubmit2` with a fence; wait host-side.
3. Read pixels from a host-mappable staging buffer the CB copied
   into.
4. Return reply.

Cost: one X client stalled on a GPU round-trip.  No frame is forced;
the compositor runs on its own schedule.

### Key invariants

- yserver-core's window tree is the single source of truth. Backend
  never holds a parallel tree; reads core's structures at composite
  time.
- All GPU ops on a given target are serialised on the single graphics
  queue (no inter-target sync needed within a frame except where
  cross-target draws require it, see above).
- Compositor is the only writer of the scanout image; per-target
  work is the only writer of per-target images. No cross-image
  sync hazards by construction.

### Things this design explicitly does not do

- No async-compute / multi-queue. Single graphics queue. Simpler
  and sufficient for a 2D compositor.
- No render-graph framework. Hand-written CB recording with explicit
  barriers.
- No bindless. Per-pipeline descriptor sets. Fine for a 2D compositor;
  revisit if/when Phase 4.2's DRI3 pixmaps push texture counts up.

## 3. Test strategy

Three loops with decreasing tightness:

1. **Inner loop (per-edit, ~30 s).** `cargo build --bin yserver`
   then `just yserver` or
   `just rendercheck-yserver 30 fill,bug7366`. Catches build breaks
   and gross drawing-op regressions.
2. **Mid loop (per-sub-phase landing on `accel`, ~5 min).** Full
   `rendercheck-yserver` + `xts-yserver scenario=Xproto` +
   `xts-yserver scenario=ShapeExt`.
3. **Gate loop (pre-merge, ~80 min).** Full xts5 across all 17
   scenarios, full rendercheck, WM matrix smoke (e16, Window Maker,
   fvwm3-boots-but-broken). Run twice for stability; compare against
   the post-`kms-xts-tooling` baseline.

New unit tests (no GPU required):
- Pure-logic helpers: command-buffer batch flush triggers, scissor
  union math, damage region coalescing, syncobj-fd ↔ Vulkan semaphore
  translation, glyph-atlas allocator, `bit_gravity` resize-blit-rect
  computation, same-target-overlap rect intersection detector,
  per-window cull AABB, occlusion subtraction.
- Run on host as part of `cargo test`.

GPU integration tests on lavapipe (CPU Vulkan, runnable in
`cargo test`, no vng):
- **Scanout fence cycle.** Allocate 3 GBM bos (or lavapipe
  equivalents), submit 6 frames in sequence, assert each bo's
  release fence is signalled before the bo is reused. Catches
  per-buffer fence reuse bugs.
- **Explicit-fence submit/readback round-trip.** Submit a CB that
  writes a known pattern, signal an external fence, host wait,
  read back via mapped staging buffer, assert pattern matches.
  Catches the explicit-sync plumbing.
- **Same-target overlap CopyArea.** Draw a checkerboard into a
  pixmap, `CopyArea` it onto itself with a half-overlapping rect,
  assert the overlap region is the source pattern shifted (not
  the destination's previous contents). Catches missing
  staging-image hazard handling.
- **MIT-SHM PutImage/GetImage round-trip.** Allocate a SHM segment
  client-side, `PutImage` a known buffer into a pixmap, `GetImage`
  back, assert byte-equal. Catches Vulkan staging-buffer copy
  bugs and depth/format mismatches.
- **`bit_gravity` resize preservation.** `CreateWindow` 100×100,
  draw distinguishable patterns at each corner, `ConfigureWindow`
  → 200×200 with each of the 9 gravity modes in turn, sample
  pixels in the preserved region, assert the source quadrant
  landed in the right place per gravity. Catches resize blit
  rect math.
- **Single-window draw.** Direct ports of the Phase 4.1.1
  smoke-tests once `vk/` is in place — render a triangle, run
  the composite pass, read back scanout, assert non-empty.

Visual smoke remains user-driven: vng + WM + xterm in a QEMU
window, eyeball verification. Same workflow as today. The
lavapipe integration tests above are necessary because
`rendercheck`, `xts5`, and visual smoke do not exercise
fence-reuse, overlapping copies, MIT-SHM, or resize-preserve in
ways that surface Vulkan-specific failure modes.

## 4. Branch strategy and parity bar

### Branch model

Phase 4.1 is a multi-week, breaks-everything-on-KMS-while-in-flight
job. It lives on `accel`, a long-lived branch off `master`.

- `accel` is recreated from master immediately after
  `kms-xts-tooling` merges. The pre-existing empty `accel` is
  discarded.
- Once recreated, **all KMS-touching work pauses on master** until
  `accel` lands. Master may still take ynest fixes, protocol/core
  changes, doc updates. KMS-only fixes that come up during 4.1 land
  on `accel`.
- `accel` periodically merges from master (or rebases, depending on
  noise) to pick up non-KMS work.
- Single squash-merge `accel → master` at the end. The dev-history
  commits on `accel` are not preserved in master.

### Parity bar — three gates, all must pass

1. **xts5 (KMS):** match-or-beat the post-`kms-xts-tooling` master
   baseline to within ±5 PASS, across the same 17 scenarios. The
   baseline number is captured by running `just xts-yserver` on the
   tip of the merged `kms-xts-tooling` branch.
2. **rendercheck (KMS):** match-or-beat the post-`kms-xts-tooling`
   master baseline. Per-test breakdown documented in the merge PR;
   any regression on a specific test (e.g. `cacomposite` slips by 1)
   needs an explanation note.
3. **WM matrix smoke (KMS):** e16 + Window Maker boot to a usable
   desktop with xterm visible and rendered cleanly. fvwm3
   boot-but-broken-menu remains broken (it's known-issues
   territory and not load-bearing on the Vulkan port).

If gates 1+2 pass but 3 doesn't, branch lands and the WM regression
goes into `known-issues.md`. If gate 3 passes but 1 or 2 doesn't,
branch doesn't land — fix or revert.

## 5. Sub-phases / sequencing within Phase 4.1

Big-bang means we don't land partial-Vulkan onto master mid-port.
Internally on `accel`, work is split into reviewable chunks:

### 4.1.0 — Branch + baseline

Merge `kms-xts-tooling`. Recreate `accel`. Run
`just xts-yserver` and `just rendercheck-yserver` on the tip;
record numbers in `docs/test-status.md` (or equivalent) as the
parity-bar starting line. ~1 session.

### 4.1.1 — Vulkan plumbing, idle

Add deps (`ash`, `gpu-allocator`, `gbm`, syncobj support). New
`crates/yserver/src/kms/vk/` subtree with instance/device/memory
init. `KmsBackend::new` brings up a Vulkan device, prints device
info on startup, tears down cleanly on shutdown. yserver still
renders via pixman. Verify on Venus and lavapipe in vng. ~1 session.

### 4.1.2 — Vulkan-fed scanout

Allocate GBM bos as `VkImage`s. Atomic KMS pageflip moves to the
explicit-fence path (`IN_FENCE_FD`/`OUT_FENCE_PTR` ↔
`VK_KHR_external_semaphore_fd`). Vulkan does a single blit-pass that
copies the pixman shadow buffer into the active GBM image. Pixman
still renders everything; Vulkan owns the path from "pixels" to
"what KMS sees." Verify rendercheck/xts unchanged. ~1-2 sessions.

### 4.1.3 — Scene-graph compositor (parallel to pixman)

Each window gets a `VkImage` mirror alongside its pixman image.
Drawing ops still go to pixman; results uploaded to the VkImage on
damage. Composite pass walks the window tree and reads per-window
VkImages instead of the unified shadow. Architectural checkpoint —
once green, all remaining sub-phases are mechanical drawing-op
ports with no architecture changes. Verify rendercheck/xts
unchanged. ~1-2 sessions.

### 4.1.4 — Drawing op port, family by family

Each landing port-and-delete-the-pixman-path of one family. Order
chosen to minimise blast radius — simplest/most-tested first:

1. `PolyFillRectangle` + `ClearArea` (solid fill).
2. `CopyArea` + `CopyPlane` (cross-target + same-target overlap
   path).
3. `PutImage` + `GetImage`, *both regular and MIT-SHM* (the SHM
   path's staging-buffer + segment-mmap plumbing lands in this
   sub-phase).
4. `PolyLine` / `PolySegment` / `PolyPoint` / `PolyArc` /
   `PolyFillArc`.
5. Glyph atlas + `ImageText{8,16}` / `PolyText{8,16}`.
6. RENDER `Composite` + `FillRectangles` (centre of mass; the full
   attribute matrix from Section 2 lands here; schedule 2-3
   sessions).
7. RENDER `Trapezoids` / `Triangles` / `CompositeGlyphs`.
8. Tile / stipple / plane-mask helpers.
9. `bit_gravity` resize preservation — once the per-window
   `VkImage` lifetime extends across map/unmap (no longer
   destroy-on-unmap), `ConfigureWindow` resize must blit the
   preserved corner per gravity. Lands as a separate slice so
   the gravity matrix can be tested in isolation.

After each family lands, run rendercheck mini-suite as smoke; full
xts on the branch tip after every 2-3 families.

### 4.1.5 — Pixman removal

Drop `pixman.workspace = true` from `crates/yserver/Cargo.toml`.
Remove `PixmanImage`, `composite32`, `composite_trapezoids`, all
the pixman helpers in `kms/render.rs`. Final full xts + rendercheck
pass against parity bar. If green, ready to merge. ~1 session.

### 4.1.6 — Merge

Squash-merge `accel → master`. Phase 4.1 done; Phase 4.2
(DRI3 + Present) starts on a fresh branch.

### Risk: 4.1.4.6 (RENDER `Composite`)

Biggest pixman call site. Full attribute matrix from §2: PictOp
combinations × repeat × alpha_map × clip_mask (region or pixmap)
× transforms × filters × component_alpha × subwindow_mode. What
rendercheck overwhelmingly tests. Schedule as 2-3 sessions. If it
slips, sub-phases 4.1.4.7-9 still proceed; they can use pixman
temporarily because pixman removal is sub-phase 4.1.5.

## 6. Open questions

These are decisions deferred to implementation (not blocking the
plan):

- Whether to fold per-target flushes and the composite pass into a
  single primary CB (with secondary CBs per target) or two separate
  primary CBs in the same submit. Default: two primary CBs in the
  same submit, simpler. Revisit if profiling shows excess submission
  overhead.
- Whether to use `VK_EXT_image_drm_format_modifier` strictly, or
  fall back to linear-tiled `VkImage` with explicit memory copies on
  drivers that don't expose modifiers cleanly. Default: modifier
  path; lavapipe will exercise the linear fallback if it's needed.
- Whether the glyph atlas grows on demand (allocate larger `VkImage`,
  re-upload all live glyphs) or evicts (LRU). Default: grow on
  demand up to a 4096×4096 cap, then evict.
- How to handle X's `XYPixmap` `PutImage` format on Vulkan — three
  options: (a) reject as `BadImplementation`, (b) translate to
  `ZPixmap` host-side, (c) shader-side bit-plane reassembly. Default:
  (b), translate; it's what every modern X server does.

## 7. Reference

- Brainstorm transcript: this session, 2026-05-07.
- Vulkan dev-loop reference:
  [`reference_vng_vulkan_venus.md`](../../../.claude/projects/-home-jos-Projects-yserver/memory/reference_vng_vulkan_venus.md)
  (memory file — Venus + lavapipe vng harness verified working).
- Software cursor design (folds into compositor):
  [`docs/plans/2026-05-04-software-cursor.md`](../../plans/2026-05-04-software-cursor.md).
- High-level Phase 4 framing:
  [`docs/high-level-design.md`](../../high-level-design.md), §"Phase 4:
  accelerated clients".
- Per-phase status: [`docs/status.md`](../../status.md), §"Phase 4 —
  Accelerated clients".
