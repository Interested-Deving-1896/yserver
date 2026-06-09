# GLX_EXT_texture_from_pixmap Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement `GLX_EXT_texture_from_pixmap` so a GLX compositor (muffin/Cinnamon) binds yserver window pixmaps as live, coherent GL textures and composites fresh content — fixing the cinnamon-settings stale-pane repro.

**Architecture:** Four components landed in dependency order. (1) **Exportable-pixmap promotion** — on first export, permanently migrate a server-owned pixmap's Vulkan image onto external-memory/dmabuf-exportable storage (mirrors Xorg glamor). (2) **Bidirectional dma-buf implicit sync** — bridge Vulkan explicit sync ↔ the exported dmabuf's reservation fences via `sync_file` ioctls, both write→read and read→write. (3) **GLX protocol surface** — runtime-gate the extension string, emit bind-to-texture FBConfig attrs in Xorg's exact order, track GLXPixmaps, open the indirect `BindTexImageEXT`/`ReleaseTexImageEXT` path. (4) **DRI3** — confirm single-fd `BufferFromPixmap` is the correct minimal contract; defer multi-plane.

**Tech Stack:** Rust, Vulkan (ash), Linux DRM/dma-buf ioctls, X11/GLX/DRI3 wire protocols. Crates: `yserver` (KMS backend: `src/kms/v2/`, `src/kms/vk/`), `yserver-core` (`src/core_loop/process_request.rs`, `src/server.rs`), `yserver-protocol` (`src/x11/glx.rs`, `src/x11/dri3.rs`).

**Reference impl:** Xorg at `/home/jos/Projects/xserver` (glamor/glamor_egl.c, glx/glxcmds.c, glx/glxdricommon.c, dri3/dri3_request.c).

**Conventions (AGENTS.md / CLAUDE.md):**
- Format with `cargo +nightly fmt`. Lint with regular `cargo clippy` (NOT pedantic in this repo). Fix all warnings before committing.
- Work on a feature branch (`feat/glx-texture-from-pixmap` already exists — use it). Squash-merge when ready, after asking confirmation.
- Spec compliance is the goal; where Xorg deviates from spec, follow Xorg.
- Vulkan/HW-dependent tests are gated `#[ignore]` and run with `--ignored` on a real GPU machine (silence / bee). They are silently red in CI otherwise.
- HW visual gates are the user's to run — do not claim a render/KMS fix works without observed hardware behavior (see `feedback_no_commit_before_smoke`).

---

## Post-implementation findings (2026-06-10)

What HW validation taught us, in order of discovery:

1. **Damage-emission bug was an upstream prerequisite, NOT a TFP issue.** The cinnamon-settings stale-pane repro pre-existed all TFP work. Xorg-vs-yserver xtrace diff showed yserver delivered ~half the `DamageNotify` events for the same workload (176 vs 315 on the same hot drawable region). Root cause: `damage_fanout.rs:587` applied `NonEmpty`'s one-shot-per-Subtract gate uniformly to all levels; muffin uses `BoundingBox` (level=2) which per X11 DAMAGE spec + Xorg `damageext/damageext.c:136-153` must emit on EVERY paint. Fixed on master (`8b93ce6 fix(damage): emit DamageNotify on every paint for Raw/Delta/BoundingBox`). **TFP work alone would not have fixed the stale-pane repro** — the spec's "TFP is the live-texture cure" framing was correct in principle but missed this upstream damage gate.

2. **Modifier-tiled (UBWC) breaks same-GPU dma-buf coherence on Turnip/Adreno.** Phase 0 / handoff plan flipped the original `LINEAR-only MVP` to `modifier-first` because RADV rejects `LINEAR + COLOR_ATTACHMENT + dma-buf` with `VK_ERROR_FORMAT_NOT_SUPPORTED`. That decision was right for RADV but wrong for Turnip: Turnip's modifier-tiled UBWC keeps compression metadata in driver caches that don't reach the dma-buf-backed memory on same-GPU share, so the Mesa GL importer samples a frozen snapshot regardless of `sync_file` fences. Diagnosed via apitrace of muffin during a cinnamon-settings scroll: 86× `glXBindTexImageEXT` + 82× `glXReleaseTexImageEXT` paired re-binds confirm muffin requests fresh content per frame; 5× `BuffersFromPixmap` (server-side) + 0× `glEGLImageTargetTexture2DOES` (client-side) confirm Mesa imports the dma-buf once and reuses the cached import per Bind — so the live-share path is the gap. Forcing LINEAR (via a diagnostic env override) made TFP work end-to-end on yoga.

3. **Correct tiling strategy is LINEAR-preferred, modifier-fallback, cached per `VkContext`.** Landed in `0eb7432 fix(glx-tfp): prefer LINEAR tiling for exported images, fall back to modifier`. `target.rs` defines `TilingStrategy { Linear, Modifier }`; `VkContext::tfp_tiling_strategy: OnceLock<TilingStrategy>` caches the winner after the first successful allocation. Turnip → `Linear` (LINEAR succeeds). RADV → falls through to `Modifier` (LINEAR rejected with `FORMAT_NOT_SUPPORTED`, modifier path succeeds). Probe + cache → no per-allocation retry cost on either driver.

4. **Phase 2 sync_file ioctls (Task 2.0-2.4) are wired and firing but not sufficient on the UBWC path.** `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` (WRITE scope) read→write wait + `DMA_BUF_IOCTL_IMPORT_SYNC_FILE` (WRITE) write→read publish are in `flush_submit_group_with_exports`. They do not cause the regression they fix to surface on the LINEAR path either, so they are not strictly required for LINEAR to work, but they cost ~nothing per submit and remain correct for the GL-may-be-reading-while-we-write hazard. **Open question:** the RADV/modifier path post-damage-fix was NEVER re-validated on bee. If RADV's AMD GFX9 modifiers exhibit an analogous compression-cache issue to Turnip UBWC, the canonical fix is queue-family-FOREIGN release/acquire (release at end-of-write, acquire at start-of-next-write). Stub-attempted in this session as a layout-only barrier prototype and reverted (the prototype used the wrong `oldLayout`, captured from `Storage::current_layout` at flush time rather than the actual post-paint layout — Vulkan validation would catch this on a properly-instrumented run).

5. **muffin uses classical GLX TFP (`glXBindTexImageEXT`), NOT the EGLImage path.** Apitrace confirmed 0× `glEGLImageTargetTexture2DOES`. Mesa-loader_dri3 caches the dma-buf import and reuses it across every Bind; the spec's reference to `glEGLImageTargetTexture2DOES` (component 1 data flow §2) as muffin's TFP receive-side primitive is incorrect for this version of muffin/cogl. The wire protocol is still `glXCreatePixmap` + DRI3 `BuffersFromPixmap` + per-frame `glXBindTexImageEXT`/`glXReleaseTexImageEXT`.

**Status at the end of this session — all plan goals met on yoga, bee validation outstanding:**
- All four components landed: Phase 1 (promotion), Phase 2 (sync), Phase 3 (GLX surface), Phase 4 (DRI3). Plus the upstream damage-emission fix on master.
- HW-verified on yoga (Turnip): cinnamon-settings + nemo scroll redraw live end-to-end. MVP success gate (spec §Goal, Task 5.1) met for this driver.
- **Bee (RADV) validation is the remaining gate before declaring the feature done.** The modifier-fallback path is what RADV will take; that path was last validated pre-damage-fix and pre-LINEAR-preferred-rework. If it's clean: done. If broken: most likely cure is queue-family-FOREIGN release/acquire on the modifier branch only (not in scope for any branch already validated).
- The original spec's "Phase 1 + Phase 2 + Phase 3" components are all correctly required; the new finding is just that they're necessary-but-not-sufficient on Turnip without the LINEAR tiling pick, and necessary-but-may-not-be-sufficient on RADV pending bee retest.

---

## Component map (files this plan touches)

| File | Responsibility | Tasks |
|---|---|---|
| `crates/yserver/src/kms/vk/target.rs` | `ImageBacking`, `DrawableImage`, exportable image alloc | 1.1 |
| `crates/yserver/src/kms/v2/store.rs` | `Storage`, `Drawable`, `DrawableStore`, layout transitions, retirement | 1.2 |
| `crates/yserver/src/kms/v2/engine.rs` | `drawable_view_cache`, view invalidation, in-flight CB retire | 1.2 |
| `crates/yserver/src/kms/v2/backend.rs` | `dri3_export_pixmap`, promotion entry, `copy_area`, sync wiring, `name_window_pixmap` | 1.3, 2.x |
| `crates/yserver/src/kms/vk/dri3.rs` | `export_dmabuf`, dma-buf sync_file ioctls (`EXPORT`/`IMPORT`) | 2.1, 2.2 |
| `crates/yserver/src/kms/vk/sync.rs` | `import_sync_file` / `export_sync_file` (exist) | 2.3 (reuse) |
| `crates/yserver-protocol/src/x11/glx.rs` | GLX ext-string constant, TFP attribute constants, FBConfig encoder | 3.1, 3.2, 3.3 |
| `crates/yserver-core/src/core_loop/process_request.rs` | GLX dispatch, `synthesise_glx_fb_configs`, `drawable_attributes_for`, VendorPrivate, BufferFromPixmap | 3.1, 3.3, 3.4, 3.5, 4.1 |
| `crates/yserver-core/src/server.rs` | `GlxDrawable`, GLX capability flag | 3.1, 3.4 |
| `crates/yserver/tests/glx_tfp_export.rs` (new) | promotion + liveness integration tests | 1.4 |
| `crates/yserver-protocol/src/x11/glx.rs` tests | FBConfig attribute unit tests | 3.3 |

---

# Phase 1 — Exportable-pixmap promotion (the engine)

**Outcome:** any server-owned pixmap can be exported as a live dmabuf; after promotion a `copy_area` into it is visible in a re-export readback. This is the spec's "main blocker."

**Grounding:**
- `dri3_export_pixmap(&mut self, host_xid: u32) -> io::Result<(u32, u16, u16, u16, u8, u8, OwnedFd)>` at `backend.rs:13248`. The gate `drawable.storage.imported_drawable.as_ref().ok_or_else(...)` is at `backend.rs:13261`. It calls `crate::kms::vk::dri3::export_dmabuf(vk, imported)` (`dri3.rs:156`).
- `Storage` (`store.rs:90`) fields: `image`, `memory`, `image_view`, `sample_view`, `extent`, `format`, `depth`, `current_layout`, `is_test_stub`, `imported_drawable: Option<DrawableImage>`. Server-owned storage has `imported_drawable: None`.
- Server-owned images are allocated `OPTIMAL` tiling with NO external-memory chain (`platform.rs:1435`), so `vkGetMemoryFdKHR` on them fails — this is exactly why the `imported_drawable` gate exists.
- The exportable-image Vulkan pattern is demonstrated in `tests/dri3_fd_leak.rs:87-209`: `VkExternalMemoryImageCreateInfo{handle_types: DMA_BUF_EXT}` on image create, `VkExportMemoryAllocateInfo{handle_types: DMA_BUF_EXT}` + `VkMemoryDedicatedAllocateInfo{image}` on alloc, `vkGetImageSubresourceLayout` for stride/size, `VkMemoryGetFdInfoKHR{DMA_BUF_EXT}` → `vkGetMemoryFdKHR`.
- DRM-modifier-aware import already exists in `DrawableImage::from_dmabuf` (`target.rs:299-329`): uses `VkImageDrmFormatModifierExplicitCreateInfoEXT` when `vk.image_drm_format_modifier` is true, else `LINEAR`.
- `drawable_view_cache: HashMap<(DrawableId, SamplerConfig, SwizzleClass), CachedDrawableView>` at `engine.rs:578`; populated by `ensure_drawable_view` (`engine.rs:6530`); invalidated per-DrawableId by `notify_drawable_retired` (`engine.rs:2428`). It keys on `DrawableId` only and never re-checks the `VkImage` handle — a storage swap without invalidation keeps sampling the OLD image.
- In-flight tracking: every paint stamps `Drawable::last_render_ticket` (a `FenceTicket`) via `touch_render_fence`; `poll_retired` (`engine.rs:1096`) frees CBs once `ticket.poll_signaled(vk)` returns true. `DrawableStore::decref` parks not-yet-signaled drawables in `pending_retire` (`store.rs`), drained by `poll_pending_retire`.

---

### Task 1.1: Exportable Vulkan image allocation helper

Add a helper that allocates a fresh external-memory, dmabuf-exportable `VkImage`+`VkDeviceMemory` for a given extent/format, modifier-aware with linear fallback. This is the allocation half only — no copy, no swap yet.

**Files:**
- Modify: `crates/yserver/src/kms/vk/target.rs` (add `ExportableImage` struct + `allocate_exportable` fn near `DrawableImage::from_dmabuf` at `target.rs:270`)
- Modify: `crates/yserver/src/kms/vk/dri3.rs` (reuse `export_dmabuf` logic; no change expected, confirm it accepts the new backing)
- Test: `crates/yserver/tests/glx_tfp_export.rs` (new file)

- [ ] **Step 1: Write the failing test**

Create `crates/yserver/tests/glx_tfp_export.rs`. Model the Vulkan setup harness on `tests/dri3_fd_leak.rs` (reuse its context-construction helper — copy the `setup_vk()`-equivalent or call into the shared test util it uses).

```rust
//! GLX texture-from-pixmap: exportable-image allocation + promotion liveness.
//! Vulkan-gated — run with `cargo test --test glx_tfp_export -- --ignored`.

use std::os::fd::AsRawFd;

mod common; // if dri3_fd_leak.rs already factors a shared helper, reuse it; else inline.

#[test]
#[ignore = "requires a Vulkan device"]
fn allocate_exportable_yields_valid_dmabuf_fd() {
    let vk = common::test_vk_context();
    let img = yserver::kms::vk::target::allocate_exportable(
        &vk,
        /* width */ 64,
        /* height */ 32,
        yserver::kms::vk::target::EXPORT_FORMAT_BGRA8, // ash::vk::Format::B8G8R8A8_UNORM
    )
    .expect("allocate exportable image");

    // Stride/size came from vkGetImageSubresourceLayout, must be sane.
    assert!(img.stride >= 64 * 4, "stride {} too small", img.stride);
    assert!(img.size as usize >= img.stride as usize * 32);

    // Export must succeed (server-owned-but-exportable memory).
    let export = yserver::kms::vk::dri3::export_backing(&vk, &img).expect("export dmabuf");
    assert!(export.fd.as_raw_fd() >= 0);
    assert!(export.stride >= 64 * 4);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test glx_tfp_export allocate_exportable_yields_valid_dmabuf_fd -- --ignored`
Expected: FAIL to compile — `allocate_exportable`, `ExportableImage`, `EXPORT_FORMAT_BGRA8`, `export_backing` not defined.

- [ ] **Step 3: Implement `ExportableImage` + `allocate_exportable`**

In `crates/yserver/src/kms/vk/target.rs`, add (adapt field/type names to the existing `VkContext` accessors — `vk.device`, `vk.external_memory_fd`, `vk.image_drm_format_modifier`, `vk.physical_device` as used by `from_dmabuf`):

```rust
/// BGRA8 is yserver's single-plane backing format for depth-24/32 pixmaps.
pub const EXPORT_FORMAT_BGRA8: vk::Format = vk::Format::B8G8R8A8_UNORM;

/// A freshly-allocated external-memory image that can be exported as a dma-buf.
/// Distinct from `DrawableImage` (which aliases imported memory): this owns
/// server-allocated, export-capable memory.
pub struct ExportableImage {
    pub image: vk::Image,
    pub memory: vk::DeviceMemory,
    pub extent: vk::Extent2D,
    pub format: vk::Format,
    pub stride: u32,
    pub size: u64,
    pub modifier: u64, // DRM_FORMAT_MOD_LINEAR (0) when no explicit modifier path
}

/// Allocate an external-memory, dmabuf-exportable image.
///
/// MVP DECISION (firm): LINEAR tiling, modifier = DRM_FORMAT_MOD_LINEAR only.
/// This keeps the export single-plane with a COLOR-aspect layout query, and Mesa
/// imports a linear single-plane BGRA8 dma-buf without trouble. The
/// `DRM_FORMAT_MODIFIER_EXT` path is a deferred follow-up (see Open items) —
/// when added, it MUST switch the layout query to MEMORY_PLANE_0 aspect and
/// thread the real modifier through `ExportableImage.modifier`. Do NOT add the
/// modifier branch in this task.
/// Mirrors the alloc pattern in `tests/dri3_fd_leak.rs` (export side).
pub fn allocate_exportable(
    vk: &VkContext,
    width: u32,
    height: u32,
    format: vk::Format,
) -> Result<ExportableImage, vk::Result> {
    const DMA_BUF_MOD_LINEAR: u64 = 0; // DRM_FORMAT_MOD_LINEAR
    let extent = vk::Extent3D { width, height, depth: 1 };

    // 1. Image create with external-memory chain, LINEAR tiling.
    let mut ext_mem = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);

    let create = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(format)
        .extent(extent)
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::LINEAR)
        .usage(
            vk::ImageUsageFlags::TRANSFER_SRC
                | vk::ImageUsageFlags::TRANSFER_DST
                | vk::ImageUsageFlags::SAMPLED
                | vk::ImageUsageFlags::COLOR_ATTACHMENT,
        )
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(&mut ext_mem);

    let image = unsafe { vk.device.create_image(&create, None)? };

    // 2. Allocate export-capable, dedicated memory.
    let reqs = unsafe { vk.device.get_image_memory_requirements(image) };
    let mem_type = match find_memory_type(
        vk,
        reqs.memory_type_bits,
        vk::MemoryPropertyFlags::DEVICE_LOCAL,
    ) {
        Some(t) => t,
        None => {
            unsafe { vk.device.destroy_image(image, None) };
            return Err(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY);
        }
    };
    let mut export_alloc = vk::ExportMemoryAllocateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
    let alloc = vk::MemoryAllocateInfo::default()
        .allocation_size(reqs.size)
        .memory_type_index(mem_type)
        .push_next(&mut export_alloc)
        .push_next(&mut dedicated);

    let memory = match unsafe { vk.device.allocate_memory(&alloc, None) } {
        Ok(m) => m,
        Err(e) => {
            unsafe { vk.device.destroy_image(image, None) };
            return Err(e);
        }
    };
    if let Err(e) = unsafe { vk.device.bind_image_memory(image, memory, 0) } {
        unsafe {
            vk.device.free_memory(memory, None);
            vk.device.destroy_image(image, None);
        }
        return Err(e);
    }

    // 3. Query layout for stride/size.
    let subresource = vk::ImageSubresource {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        mip_level: 0,
        array_layer: 0,
    };
    let layout = unsafe { vk.device.get_image_subresource_layout(image, subresource) };

    Ok(ExportableImage {
        image,
        memory,
        extent: vk::Extent2D { width, height },
        format,
        stride: layout.row_pitch as u32,
        size: layout.size,
        modifier: DMA_BUF_MOD_LINEAR,
    })
}
```

> If a `find_memory_type` helper already exists in this module (the import path needs one), reuse it instead of declaring a new one. Grep first: `rg "fn find_memory_type" crates/yserver/src/kms/vk/`.
>
> `vkGetImageSubresourceLayout` with `aspect_mask: COLOR` is correct for the LINEAR tiling chosen above. The `stride`/`size` captured here are the values the export reply will carry (Task 1.2 / 1.3), so they must be correct at allocation time — which they are, since LINEAR + COLOR aspect is the matching query.

- [ ] **Step 4: Add `export_backing` in `dri3.rs`**

`export_dmabuf` (`dri3.rs:156`) currently takes a `&DrawableImage`. Factor the fd-export tail into a function that works on raw `(image, memory)` so both `DrawableImage` and `ExportableImage` can use it. In `crates/yserver/src/kms/vk/dri3.rs`:

```rust
/// Export an already-allocated external-memory image as a dma-buf fd.
/// Shared by `export_dmabuf` (imported images) and the GLX-TFP promotion path
/// (server-owned exportable images).
pub fn export_backing(
    vk: &VkContext,
    img: &super::target::ExportableImage,
) -> io::Result<DmabufExport> {
    let ext = vk
        .external_memory_fd
        .as_ref()
        .ok_or_else(|| io::Error::other("VK_KHR_external_memory_fd unavailable"))?;
    let info = vk::MemoryGetFdInfoKHR::default()
        .memory(img.memory)
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let raw = unsafe { ext.get_memory_fd(&info) }
        .map_err(|e| io::Error::other(format!("vkGetMemoryFdKHR: {e}")))?;
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    Ok(DmabufExport { fd, size: img.size as u32, stride: img.stride })
}
```

Refactor the existing `export_dmabuf` to call `get_memory_fd` the same way (no behavior change) — or leave it and just add `export_backing`. Keep the diff minimal; the test only needs `export_backing`.

- [ ] **Step 5: Run test to verify it passes (on a Vulkan machine)**

Run: `cargo test --test glx_tfp_export allocate_exportable_yields_valid_dmabuf_fd -- --ignored`
Expected: PASS. (Run on silence/bee — see `reference_hw_xts_via_tmux` for HW test running.)

- [ ] **Step 6: fmt, clippy, commit**

```bash
cargo +nightly fmt
cargo clippy
git add crates/yserver/src/kms/vk/target.rs crates/yserver/src/kms/vk/dri3.rs crates/yserver/tests/glx_tfp_export.rs
git commit -m "feat(glx-tfp): exportable Vulkan image allocation + dma-buf export helper"
```

---

### Task 1.2: Pixmap promotion — swap storage onto exportable image

Add `RenderEngine`/`DrawableStore`-level machinery to permanently migrate a pixmap's `Storage` onto a freshly-allocated exportable image: copy current content + carry layout, swap the `Storage`, invalidate the view cache, retire in-flight CBs referencing the old image before freeing it.

**Files:**
- Modify: `crates/yserver/src/kms/v2/store.rs` (add `Storage::adopt_exportable` / a swap method; add a way to detect "already exportable")
- Modify: `crates/yserver/src/kms/v2/engine.rs` (add `promote_drawable_exportable`; reuse `notify_drawable_retired` invalidation logic for a single DrawableId; flush+wait on `last_render_ticket`)
- Test: extend `crates/yserver/tests/glx_tfp_export.rs`

- [ ] **Step 1: Write the failing liveness test**

Append to `crates/yserver/tests/glx_tfp_export.rs`:

```rust
#[test]
#[ignore = "requires a Vulkan device"]
fn promotion_preserves_content_and_is_live() {
    let mut h = common::test_engine_harness(); // engine + store, see helper note below

    // Create a normal (non-exportable) server-owned pixmap, fill it with a known color.
    let pix = h.create_pixmap(64, 32, /*depth*/ 24);
    h.fill_solid(pix, 0xFF_00_00); // red (BGRA convention per yserver)
    h.flush_and_wait();

    // Promote it. Old image must be retired, content preserved.
    let exported = h.promote_and_export(pix).expect("promote + export");
    let pixels = common::read_dmabuf_pixels(&h.vk, &exported, 64, 32);
    assert_eq!(pixels[0], 0xFF_00_00, "promoted image lost original content");

    // Liveness: a copy_area AFTER promotion lands in the exported backing.
    h.fill_solid(pix, 0x00_FF_00); // green
    h.flush_and_wait();
    let pixels2 = common::read_dmabuf_pixels(&h.vk, &exported, 64, 32);
    assert_eq!(pixels2[0], 0x00_FF_00, "post-promotion write not visible in dmabuf — not live");
}
```

> Helper note: `test_engine_harness`, `create_pixmap`, `fill_solid`, `flush_and_wait`, `promote_and_export`, `read_dmabuf_pixels` belong in `tests/common` / `tests/glx_tfp_export.rs`. Build them from the existing engine test scaffolding (search `rg "fn .*harness|RenderEngine::new" crates/yserver/tests crates/yserver/src/kms/v2/engine.rs` for the closest existing pattern; the `--ignored` v2_acceptance suite is the model — see `feedback_v2_acceptance_suite_runs_on_silence`).
>
> **`read_dmabuf_pixels` must NOT assume the dmabuf is CPU-mappable** (codex finding): `allocate_exportable` requests `DEVICE_LOCAL` memory, and on a real dGPU (silence/rx580 — the HW test box) exported DEVICE_LOCAL memory is typically not host-visible even though GL import works. So `read_dmabuf_pixels` re-imports the exported fd as a Vulkan image via the production import path (`DrawableImage::from_dmabuf`, `target.rs:270`), then `vkCmdCopyImageToBuffer` into a `HOST_VISIBLE | HOST_COHERENT` staging buffer and reads BGRA from that (the standard GPU-readback pattern). This validates the dmabuf is the genuine live shared buffer through the same import path Mesa uses. A raw `mmap` of the fd is acceptable only as an optional smoke check guarded by "if mappable", never as the assertion.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test glx_tfp_export promotion_preserves_content_and_is_live -- --ignored`
Expected: FAIL — `promote_and_export` not defined.

- [ ] **Step 3: Add the storage swap on `Storage`**

In `crates/yserver/src/kms/v2/store.rs`, add a method that replaces a server-owned storage's image/memory with an adopted exportable image, carrying the layout. The new image starts `UNDEFINED`; the engine (Step 4) transitions it before this is read as a sample source.

```rust
impl Storage {
    /// True when this storage's memory is already dma-buf-exportable
    /// (imported, or previously promoted). Used to avoid re-promoting.
    pub(crate) fn is_exportable(&self) -> bool {
        self.imported_drawable.is_some() || self.promoted_exportable
    }

    /// Adopt a freshly-allocated exportable image as this drawable's permanent
    /// backing. Caller MUST have copied old→new content and invalidated the
    /// view cache, and MUST destroy the returned old handles only after the
    /// old image's last render fence has signaled.
    ///
    /// `export_stride`/`export_size` are captured from the exportable image's
    /// `vkGetImageSubresourceLayout` at allocation time and stored so the DRI3
    /// export reply (Task 1.3) never has to reconstruct or re-query them.
    pub(crate) fn adopt_exportable(
        &mut self,
        new_image: vk::Image,
        new_memory: vk::DeviceMemory,
        new_sample_view: vk::ImageView,
        new_image_view: vk::ImageView,
        new_layout: vk::ImageLayout,
        export_stride: u32,
        export_size: u64,
    ) -> RetiredImage {
        let retired = RetiredImage {
            image: self.image,
            memory: self.memory,
            image_view: self.image_view,
            sample_view: self.sample_view,
        };
        self.image = new_image;
        self.memory = new_memory;
        self.image_view = new_image_view;
        self.sample_view = new_sample_view;
        self.current_layout = new_layout;
        self.promoted_exportable = true;
        self.export_stride = export_stride;
        self.export_size = export_size;
        retired
    }
}

/// Old Vk handles displaced by promotion — destroyed once the fence guarding
/// them has signaled.
pub(crate) struct RetiredImage {
    pub image: vk::Image,
    pub memory: vk::DeviceMemory,
    pub image_view: vk::ImageView,
    pub sample_view: vk::ImageView,
}
```

Add three fields to `Storage`: `promoted_exportable: bool` (default `false`), `export_stride: u32` (default `0`), `export_size: u64` (default `0`) — defaulted in all three constructors `new_server_owned`/`from_imported_drawable_image`/`from_pooled`. These three fields are populated ONLY on the promotion path (`adopt_exportable`).

**Imported storage does NOT use these fields.** `DrawableImage::from_dmabuf` (`target.rs:270`) does not store reply metadata on the struct, and adding it is unnecessary: `dri3_export_pixmap` keeps routing imported storage through the existing `export_dmabuf` path (`dri3.rs:156`), which queries `vkGetImageSubresourceLayout` itself. So leave `export_stride`/`export_size` at `0` for imported storage and do NOT set `promoted_exportable` for it — the `imported_drawable.is_some()` arm of `is_exportable()` already reports it as exportable, and the Task 1.3 export branch picks `export_dmabuf` for it. (`promoted_exportable` is strictly "this server-owned image was migrated onto exportable memory".)

**Critical:** a promoted image MUST NOT be returned to `PixmapPool` on destroy (pool images are OPTIMAL/non-exportable) — guard `Storage::destroy`'s pool-return branch (`store.rs:300`) with `!self.promoted_exportable`.

- [ ] **Step 4: Add `promote_drawable_exportable` on the engine**

In `crates/yserver/src/kms/v2/engine.rs`, add the orchestration. It: (a) allocates the exportable image (Task 1.1), (b) records a `vkCmdCopyImage` old→new and submits it, (c) waits for that copy to complete, (d) builds the new sample/attachment views, (e) swaps storage, (f) invalidates the view cache for that DrawableId, (g) retires the old image once its fence signals.

```rust
impl RenderEngine {
    /// Permanently migrate a pixmap onto dma-buf-exportable storage.
    /// Idempotent: returns early if already exportable.
    pub(crate) fn promote_drawable_exportable(
        &mut self,
        platform: &mut PlatformBackend,
        store: &mut DrawableStore,
        id: DrawableId,
    ) -> io::Result<()> {
        {
            let d = store.get(id).ok_or_else(|| io::Error::other("promote: unknown drawable"))?;
            if d.storage.is_exportable() {
                return Ok(());
            }
        }
        let (extent, format, depth, old_layout) = {
            let s = &store.get(id).unwrap().storage;
            (s.extent, s.format, s.depth, s.current_layout)
        };

        // (a) allocate exportable target.
        let vk = platform.vk();
        let exp = crate::kms::vk::target::allocate_exportable(vk, extent.width, extent.height, format)
            .map_err(|e| io::Error::other(format!("allocate_exportable: {e}")))?;

        // (b)+(c) copy old content into new image and wait for completion.
        //   - barrier old image (old_layout -> TRANSFER_SRC_OPTIMAL)
        //   - barrier new image (UNDEFINED -> TRANSFER_DST_OPTIMAL)
        //   - vkCmdCopyImage
        //   - barrier new image (TRANSFER_DST_OPTIMAL -> SHADER_READ_ONLY_OPTIMAL)
        //   - submit on a dedicated one-shot CB + fence, wait_for_fences.
        self.copy_image_blocking(
            platform,
            store.get(id).unwrap().storage.image,
            old_layout,
            exp.image,
            extent,
        )?;
        let new_layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;

        // (d) build new views (reuse the same swizzle policy the store uses for
        //     depth-24/32 — `PlatformBackend::build_sample_view`).
        let sample_view = platform.build_sample_view(exp.image, format, depth)
            .map_err(|e| io::Error::other(format!("build_sample_view: {e}")))?;
        let image_view = platform.build_attachment_view(exp.image, format)
            .map_err(|e| io::Error::other(format!("build_attachment_view: {e}")))?;

        // (e) swap storage, carrying the export stride/size queried at allocation.
        let retired = {
            let d = store.get_mut(id).unwrap();
            d.storage.adopt_exportable(
                exp.image, exp.memory, sample_view, image_view, new_layout,
                exp.stride, exp.size,
            )
        };

        // (f) invalidate the view cache for this DrawableId — it keys on
        //     DrawableId and never re-checks the VkImage handle.
        self.invalidate_drawable_views(id);

        // (g) retire old handles once the old image's last render fence signals.
        //     Reuse the fence the drawable last touched; if none, retire now.
        let guard = store.get(id).and_then(|d| d.last_render_ticket.clone());
        self.retire_image_after(retired, guard);
        Ok(())
    }
}
```

Add the three private helpers if they don't exist:
- `copy_image_blocking(&mut self, platform, src, src_layout, dst, extent)` — one-shot CB + fence, `vkWaitForFences`. (Search for an existing blocking-submit helper first; the promotion path is rare so a dedicated fence is fine.)
- `invalidate_drawable_views(&mut self, id)` — factor the body of `notify_drawable_retired` (`engine.rs:2428`) so both call it: `retain` the cache dropping entries where key.0 == id, destroying each view.
- `retire_image_after(&mut self, retired: RetiredImage, guard: Option<FenceTicket>)` — push onto a `Vec<(RetiredImage, Option<FenceTicket>)>` drained by `poll_retired`; if `guard` is None or already signaled, destroy immediately. Destroy order: `destroy_image_view(sample_view)`, `destroy_image_view(image_view)`, `destroy_image(image)`, `free_memory(memory)`.

> `build_attachment_view` may need adding alongside `build_sample_view` (`platform.rs:1327`) — it's the IDENTITY-swizzle view. If the store already constructs `image_view` via a helper, reuse it.

- [ ] **Step 5: Run liveness test**

Run: `cargo test --test glx_tfp_export promotion_preserves_content_and_is_live -- --ignored`
Expected: PASS — content preserved AND post-promotion write visible.

- [ ] **Step 6: Run the existing v2 acceptance suite to catch regressions**

Run: `cargo test --test v2_acceptance -- --ignored` (on silence/RADV per `feedback_v2_acceptance_suite_runs_on_silence`)
Expected: no new failures vs. baseline. The construction-leaves-FB-open caveat applies — compare against the known-green baseline, don't bisect noise.

- [ ] **Step 7: fmt, clippy, commit**

```bash
cargo +nightly fmt
cargo clippy
git add crates/yserver/src/kms/v2/store.rs crates/yserver/src/kms/v2/engine.rs crates/yserver/src/kms/vk/platform.rs crates/yserver/tests/glx_tfp_export.rs
git commit -m "feat(glx-tfp): permanent pixmap promotion onto exportable storage (copy+swap+view-invalidate+retire)"
```

---

### Task 1.3: Extend `dri3_export_pixmap` to promote-if-needed

Drop the `imported_drawable`-only gate. On export, if the pixmap is not already exportable, promote it (Task 1.2), then export the (now exportable) backing via `export_backing` (Task 1.1).

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs` (`dri3_export_pixmap` at `backend.rs:13248`)
- Test: extend `crates/yserver/tests/glx_tfp_export.rs`

- [ ] **Step 1: Write the failing test (regular server-owned pixmap exports)**

```rust
#[test]
#[ignore = "requires a Vulkan device"]
fn dri3_export_promotes_server_owned_pixmap() {
    let mut backend = common::test_kms_backend();
    let host_xid = backend.create_test_pixmap(64, 32, 24);
    // Previously this returned Err (imported_drawable gate). Now it must succeed.
    let (size, w, h, stride, depth, bpp, fd) =
        backend.dri3_export_pixmap(host_xid).expect("export server-owned pixmap");
    assert_eq!((w, h, depth, bpp), (64, 32, 24, 32));
    assert!(stride as u32 >= w as u32 * 4 && size >= stride as u32 * h as u32);
    assert!(fd.as_raw_fd() >= 0);
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --test glx_tfp_export dri3_export_promotes_server_owned_pixmap -- --ignored`
Expected: FAIL — currently returns `Err("...no imported_drawable...")`.

- [ ] **Step 3: Rewrite the export gate**

Replace the gate at `backend.rs:13261`. Current:

```rust
let imported = drawable
    .storage
    .imported_drawable
    .as_ref()
    .ok_or_else(|| io::Error::other("pixmap has no exportable backing"))?;
```

New flow (promote first, then re-borrow):

```rust
// Promote-if-needed so server-owned backings become exportable (glamor model).
let id = self.store.lookup(host_xid)
    .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "export: unknown pixmap"))?;
if !self.store.get(id).map(|d| d.storage.is_exportable()).unwrap_or(false) {
    self.engine.promote_drawable_exportable(&mut self.platform, &mut self.store, id)?;
}

let drawable = self.store.get(id).ok_or_else(|| io::Error::other("export: drawable vanished"))?;
let depth = drawable.depth;
let (width, height) = (drawable.storage.extent.width as u16, drawable.storage.extent.height as u16);
let bpp: u8 = match depth { 24 | 32 => 32, 4 | 8 => 8, _ => 32 };

// Export: imported images go through the DrawableImage path; promoted/server-owned
// images go through export_backing on the storage's image+memory, using the
// stride/size carried in Storage by adopt_exportable (no zero placeholders).
let export = if let Some(imported) = drawable.storage.imported_drawable.as_ref() {
    crate::kms::vk::dri3::export_dmabuf(vk, imported)?
} else {
    let exp = crate::kms::vk::target::ExportableImage {
        image: drawable.storage.image,
        memory: drawable.storage.memory,
        extent: drawable.storage.extent,
        format: drawable.storage.format,
        stride: drawable.storage.export_stride, // carried from allocation-time layout query
        size: drawable.storage.export_size,
        modifier: 0,
    };
    debug_assert!(exp.stride != 0 && exp.size != 0, "promoted storage missing export metadata");
    crate::kms::vk::dri3::export_backing(vk, &exp)?
};
let stride16 = u16::try_from(export.stride).map_err(|_| io::Error::other("stride overflow"))?;
Ok((export.size, width, height, stride16, depth, bpp, export.fd))
```

> Note the borrow ordering: `promote_drawable_exportable` needs `&mut self.engine/platform/store`, so resolve & promote BEFORE the `&drawable` borrow. Adjust to satisfy the borrow checker (split the `self.platform.vk` borrow used at `backend.rs:13252` accordingly — capture the raw vk handle or re-fetch after promotion). `export_backing` consumes `exp.stride`/`exp.size` directly (it does NOT re-query layout), so those fields must be the real values `adopt_exportable` stored — the `debug_assert!` guards that.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --test glx_tfp_export dri3_export_promotes_server_owned_pixmap -- --ignored`
Expected: PASS.

- [ ] **Step 5: Map export failure to BadPixmap NOW (before any advertising)**

> **Moved earlier (was Phase 4):** Phase 3 makes real clients call `BufferFromPixmap`, so Xorg-compatible error mapping must land here — before the advertise gate (Task 3.1), not after. Doing it now also keeps Task 1.3 self-contained: promotion can fail (no external-memory support), and that failure must surface as `BadPixmap`.

In the `BufferFromPixmap` arm (`process_request.rs:7550`), change the `dri3_export_pixmap` `Err` branch from `BAD_ALLOC` to `BAD_PIXMAP` (Xorg `dri3/dri3_request.c:277`). Keep `BadDrawable` for the unresolvable-XID case, and reserve `BadAlloc` strictly for a `send_reply_with_fd` failure (matching Xorg's split). Add the citing comment. Add a focused unit test:

```rust
#[test]
fn buffer_from_pixmap_export_failure_returns_bad_pixmap() {
    // Backend stub whose dri3_export_pixmap always errors.
    let mut state = test_server_state_with_backend(FailingExportBackend);
    let client = test_client(&mut state);
    let x_pixmap = make_test_pixmap(&mut state, client, 16, 16, 24);
    let outcome = handle_dri3_buffer_from_pixmap(&mut state, client, seq(1), x_pixmap);
    assert_eq!(outcome.error_code(), Some(x11::error::BAD_PIXMAP));
}
```

Run: `cargo test -p yserver-core buffer_from_pixmap_export_failure_returns_bad_pixmap`
Expected: was FAIL (`BadAlloc`) → PASS (`BadPixmap`).

- [ ] **Step 6: fmt, clippy, commit**

```bash
cargo +nightly fmt
cargo clippy
git add crates/yserver/src/kms/v2/backend.rs crates/yserver-core/src/core_loop/process_request.rs crates/yserver/tests/glx_tfp_export.rs
git commit -m "feat(glx-tfp): dri3_export_pixmap promotes server-owned pixmaps; BufferFromPixmap failure → BadPixmap"
```

---

# Phase 2 — Bidirectional dma-buf implicit sync

**Outcome:** yserver's Vulkan writes to an exported image participate in the dmabuf's implicit reservation fences (write→read), and yserver waits on all readers before overwriting (read→write). The observed intermittency is gone on HW.

**Grounding:**
- `import_sync_file(vk, fd) -> Result<vk::Semaphore>` exists (`sync.rs:29`, TEMPORARY, SYNC_FD). `export_sync_file(vk, sem) -> Result<OwnedFd>` exists (`sync.rs:86`).
- `wait_present_source_ready` → `wait_dmabuf_read_ready` (`dri3.rs:266`) already does `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` with `DMA_BUF_SYNC_READ` (`0xc008_6202`, `dri3.rs:247`) + `poll`. **This covers read-before-our-copy of an IMPORTED buffer (we are the reader).**
- **MISSING:** `DMA_BUF_SYNC_WRITE` constant; `DMA_BUF_IOCTL_IMPORT_SYNC_FILE` (struct `dma_buf_import_sync_file{u32 flags; i32 fd}`, ioctl `_IOW('b',3,...)` = `0x4008_6203`) and any call site.
- Per spec Open questions: the **standard pattern** (wlroots/mutter) is — *write→read:* after the Vulkan copy submission to the exported image, signal an SYNC_FD semaphore, export it (`vkGetSemaphoreFdKHR`), attach as a WRITE fence via `DMA_BUF_IOCTL_IMPORT_SYNC_FILE`/`DMA_BUF_SYNC_WRITE`. *read→write:* before the next Vulkan write, `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` with `DMA_BUF_SYNC_WRITE` (waits on ALL users — readers and writers), import as Vulkan semaphore, wait on it in the submission.
- copy_area submits via `end_and_submit_op` → `flush_submit_group` → `vkQueueSubmit2(graphics_queue, [CBs], shared_fence)` (`platform.rs:1736`). `PresentCompletionSignal` (`platform.rs:275`) shows the exportable-semaphore-signaled-by-submit pattern to copy.

> **Validate-first directive (spec):** prototype against the cinnamon repro EARLY. If RADV's external memory turns out to already be implicitly synced for same-GPU sharing, the IMPORT/EXPORT_SYNC_FILE work collapses to a no-op — but DO NOT assume that. Task 2.0 is the data-gathering gate.

---

### Task 2.0: Characterize the sync gap (data before code)

**Files:** none (investigation). Output: a note appended to the spec's Open-questions and a go/no-go on 2.1–2.3.

- [ ] **Step 1:** On silence, run the cinnamon-settings pane-switch repro under yserver with the Phase-1 promotion landed (TFP not yet advertised — muffin still on fallback). Confirm the repro still reproduces (baseline). Capture `COGL_DEBUG=winsys` + xtrace.
- [ ] **Step 2:** Make a **non-committable, env-gated** TFP-advertise experiment so muffin issues `glXCreatePixmap`+`BufferFromPixmap`: gate it behind a `YSERVER_TFP_EXPERIMENT=1` env check (hardcode the ext string + bind-to-texture attrs, skip resource tracking) so it can NEVER take effect in a normal run. Do NOT commit it to the feature branch — keep it as a local `git stash`/scratch patch. Observe whether content is live-but-racy (tearing/stale flashes) vs. never-updates. Racy ⇒ sync gap real ⇒ proceed with 2.1–2.3. Always-stale ⇒ revisit Phase 1 liveness.
- [ ] **Step 3:** **Revert the experiment patch** (`git checkout` / drop the stash) — verify `git status` is clean and the env gate is gone before any further commits. Record findings in the plan/spec. Decide: full bidirectional sync (2.1–2.3) vs. (if RADV already implicit-syncs) a documented no-op with just the read→write guard for safety.

> This task gates the rest of Phase 2. The experiment is throwaway by construction — env-gated AND uncommitted — so it cannot bypass the capability/resource tracking the real Phase 3 depends on. If the data says RADV implicit-syncs same-GPU sharing, 2.1/2.2 may reduce to the read→write direction only.

---

### Task 2.1: dma-buf EXPORT_SYNC_FILE at WRITE scope (read→write guard)

Add the `DMA_BUF_SYNC_WRITE` constant and a `wait_dmabuf_write_ready` that snapshots ALL current users (readers + writers) of an exported dmabuf and waits, so yserver does not overwrite a buffer muffin is still sampling.

**Files:**
- Modify: `crates/yserver/src/kms/vk/dri3.rs` (near `wait_dmabuf_read_ready` at `dri3.rs:266`)
- Test: `crates/yserver/tests/glx_tfp_export.rs` (ioctl-level smoke; full ordering verified on HW)

- [ ] **Step 1: Write the failing test**

```rust
#[test]
#[ignore = "requires a Vulkan device"]
fn export_sync_file_write_scope_is_idle_on_fresh_buffer() {
    let vk = common::test_vk_context();
    let img = yserver::kms::vk::target::allocate_exportable(&vk, 16, 16, yserver::kms::vk::target::EXPORT_FORMAT_BGRA8).unwrap();
    let export = yserver::kms::vk::dri3::export_backing(&vk, &img).unwrap();
    // No GPU work touched it via the dmabuf path → write-scope wait is Idle.
    let r = yserver::kms::vk::dri3::wait_dmabuf_write_ready(export.fd.as_fd(), 0);
    assert!(matches!(r,
        yserver::kms::vk::dri3::DmabufWait::Idle
        | yserver::kms::vk::dri3::DmabufWait::Unsupported));
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --test glx_tfp_export export_sync_file_write_scope_is_idle_on_fresh_buffer -- --ignored`
Expected: FAIL — `wait_dmabuf_write_ready` / `DmabufWait` not defined.

- [ ] **Step 3: Implement**

In `crates/yserver/src/kms/vk/dri3.rs`, add alongside the existing read-scope code:

```rust
const DMA_BUF_SYNC_WRITE: u32 = 1 << 1;

/// Result of a dma-buf fence wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmabufWait {
    Ready,
    TimedOut,
    Idle,        // no fence in the reservation object
    Unsupported, // ioctl not supported on this dmabuf
}

/// Wait until ALL current users (readers AND writers) of the dmabuf are done.
/// WRITE scope is required before yserver overwrites a buffer a GL consumer may
/// still be sampling. `timeout_ms = 0` polls without blocking.
pub fn wait_dmabuf_write_ready(fd: BorrowedFd<'_>, timeout_ms: i32) -> DmabufWait {
    sync_file_export_and_poll(fd, DMA_BUF_SYNC_WRITE, timeout_ms)
}
```

Factor the existing `wait_dmabuf_read_ready` body (`dri3.rs:266`) into a shared `sync_file_export_and_poll(fd, flags, timeout_ms) -> DmabufWait` (the `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` + `poll` logic), parameterized by the `flags` (READ vs WRITE). Have `wait_dmabuf_read_ready` call it with `DMA_BUF_SYNC_READ` and return its existing enum (or migrate it to `DmabufWait`). Keep the 50 ms call-site timeout for the read path unchanged.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --test glx_tfp_export export_sync_file_write_scope_is_idle_on_fresh_buffer -- --ignored`
Expected: PASS.

- [ ] **Step 5: fmt, clippy, commit**

```bash
cargo +nightly fmt && cargo clippy
git add crates/yserver/src/kms/vk/dri3.rs crates/yserver/tests/glx_tfp_export.rs
git commit -m "feat(glx-tfp): dma-buf EXPORT_SYNC_FILE at WRITE scope (read-before-overwrite guard)"
```

---

### Task 2.2: dma-buf IMPORT_SYNC_FILE (write→read fence publication)

Add the `DMA_BUF_IOCTL_IMPORT_SYNC_FILE` ioctl + struct so yserver can attach a Vulkan completion fence (exported as sync_file) onto the exported dmabuf as a WRITE fence, making Mesa's implicit-sync GL reads wait on yserver's write.

**Files:**
- Modify: `crates/yserver/src/kms/vk/dri3.rs`
- Test: `crates/yserver/tests/glx_tfp_export.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
#[ignore = "requires a Vulkan device"]
fn import_sync_file_accepts_a_signaled_fence() {
    let vk = common::test_vk_context();
    let img = yserver::kms::vk::target::allocate_exportable(&vk, 16, 16, yserver::kms::vk::target::EXPORT_FORMAT_BGRA8).unwrap();
    let export = yserver::kms::vk::dri3::export_backing(&vk, &img).unwrap();
    // Produce an already-signaled sync_file by exporting a signaled Vulkan semaphore.
    let sync_fd = common::signaled_sync_file(&vk);
    let r = yserver::kms::vk::dri3::import_dmabuf_write_fence(export.fd.as_fd(), sync_fd.as_fd());
    // Either accepted, or Unsupported on a kernel/driver that rejects it — both non-panic.
    assert!(r.is_ok() || matches!(r, Err(ref e) if e.kind() == std::io::ErrorKind::Unsupported));
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --test glx_tfp_export import_sync_file_accepts_a_signaled_fence -- --ignored`
Expected: FAIL — `import_dmabuf_write_fence` not defined.

- [ ] **Step 3: Implement**

```rust
// _IOW('b', 3, struct dma_buf_import_sync_file) where the struct is 8 bytes
// { __u32 flags; __s32 fd }. 'b' = 0x62, dir=W(1), size=8 →
//   (1<<30) | (8<<16) | (0x62<<8) | 3 = 0x4008_6203
const DMA_BUF_IOCTL_IMPORT_SYNC_FILE: libc::c_ulong = 0x4008_6203;

#[repr(C)]
struct DmaBufImportSyncFile {
    flags: u32,
    fd: i32,
}

/// Attach `sync_fd` (a sync_file representing yserver's completed Vulkan write)
/// onto the dmabuf's reservation object as a WRITE fence. Mesa's implicit-sync
/// GL read on the imported dmabuf will then wait on it automatically.
/// The kernel dup()s the fd; caller retains ownership of `sync_fd`.
pub fn import_dmabuf_write_fence(
    dmabuf: BorrowedFd<'_>,
    sync_fd: BorrowedFd<'_>,
) -> io::Result<()> {
    let mut arg = DmaBufImportSyncFile {
        flags: DMA_BUF_SYNC_WRITE,
        fd: sync_fd.as_raw_fd(),
    };
    let rc = unsafe {
        libc::ioctl(dmabuf.as_raw_fd(), DMA_BUF_IOCTL_IMPORT_SYNC_FILE, &mut arg)
    };
    if rc != 0 {
        let err = io::Error::last_os_error();
        // ENOTTY/EINVAL on kernels without IMPORT_SYNC_FILE → Unsupported.
        if matches!(err.raw_os_error(), Some(libc::ENOTTY) | Some(libc::EINVAL)) {
            return Err(io::Error::from(io::ErrorKind::Unsupported));
        }
        return Err(err);
    }
    Ok(())
}
```

> `common::signaled_sync_file(&vk)` in the test: create a binary semaphore with `VkExportSemaphoreCreateInfo{SYNC_FD}`, signal it via an empty `vkQueueSubmit2` (signal-only), then `export_sync_file(&vk, sem)`. (Mirror `PresentCompletionSignal` at `platform.rs:295`.)

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --test glx_tfp_export import_sync_file_accepts_a_signaled_fence -- --ignored`
Expected: PASS.

- [ ] **Step 5: fmt, clippy, commit**

```bash
cargo +nightly fmt && cargo clippy
git add crates/yserver/src/kms/vk/dri3.rs crates/yserver/tests/glx_tfp_export.rs
git commit -m "feat(glx-tfp): dma-buf IMPORT_SYNC_FILE (publish Vulkan write fence to reservation)"
```

---

### Task 2.3: Wire bidirectional sync into the copy path for exported images

Track which exported pixmaps are live-shared, and on every Vulkan write that touches such a pixmap: (read→write) wait on the dmabuf's WRITE-scope fence before submitting; (write→read) after submit, export the completion semaphore as sync_file and IMPORT it onto the dmabuf as a WRITE fence.

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs` (track exported dmabuf fds per DrawableId; hook copy_area / the submit path)
- Modify: `crates/yserver/src/kms/vk/platform.rs` (expose a per-op completion-signal semaphore, like `PresentCompletionSignal`)
- Test: HW (the cinnamon repro is the real test); add a unit test asserting the hooks are invoked on an exported drawable.

- [ ] **Step 1: Track exported dmabufs**

In `backend.rs`, when `dri3_export_pixmap` succeeds, record the exported state so writes know to sync. Add to the backend an **idempotent-per-drawable** record (see Task 2.4 for why repeated exports must NOT re-incref):

```rust
struct ExportedBacking {
    /// A dup of the exported dmabuf fd, for implicit-sync fence I/O.
    /// `None` until the first BufferFromPixmap export (an entry can exist
    /// earlier, created by glXCreatePixmap — see the ordering note below).
    fd: Option<OwnedFd>,
    /// How many live GLXPixmaps reference this backing. Teardown fires when this
    /// reaches 0 — see Task 2.4 Step 3.
    glx_refs: u32,
    /// True once the single lifetime ref has been taken (so teardown releases once).
    lifetime_ref_held: bool,
}

/// DrawableId → export state. AT MOST ONE entry per drawable, regardless of how
/// many times BufferFromPixmap is issued for it (muffin re-exports per damage).
exported_dmabufs: HashMap<DrawableId, ExportedBacking>,
```

**The contradiction codex flagged** (glXCreatePixmap-then-BufferFromPixmap is the COMMON order, so an entry may be created before any export) is resolved by a single helper that creates the entry and takes the lifetime ref **exactly once**, whoever arrives first:

```rust
/// Ensure an ExportedBacking entry exists for `id`, taking the single backing
/// lifetime ref on first creation. Idempotent.
///
/// NOTE: taking the lifetime ref needs `&mut self` (alias_registry / store),
/// which would conflict with an `entry(id).or_insert_with(closure)` borrow of
/// `self.exported_dmabufs`. So spell it as an explicit check → take-ref →
/// insert, then `get_mut` for the return — no nested mutable borrow of self.
fn ensure_exported_entry(&mut self, id: DrawableId, backing: PixmapHandle) -> &mut ExportedBacking {
    if !self.exported_dmabufs.contains_key(&id) {
        // ONE lifetime ref per backing, taken here and released exactly once in teardown.
        // For NameWindowPixmap'd window backings (the muffin MVP path) this is
        // alias_registry.incref(backing); for a plain pixmap exported via glXCreatePixmap
        // it is a DrawableStore refcount on the Drawable. Pick whichever owns the
        // backing's lifetime — see the layering note in Task 2.4.
        self.take_backing_lifetime_ref(id, backing);
        self.exported_dmabufs.insert(
            id,
            ExportedBacking { fd: None, glx_refs: 0, lifetime_ref_held: true },
        );
    }
    self.exported_dmabufs.get_mut(&id).expect("entry just ensured")
}
```

- `dri3_export_pixmap`: `let e = self.ensure_exported_entry(id, backing); if e.fd.is_none() { e.fd = Some(dup); }`. Repeated exports touch nothing else. Always return the original fd to the client.
- `acquire_glx_pixmap_export(host_xid)` (Task 3.4): `self.ensure_exported_entry(id, backing).glx_refs += 1`.

This makes the order irrelevant: whether `glXCreatePixmap` (acquire) or `BufferFromPixmap` (export) runs first, the entry and its one lifetime ref are created exactly once, and `glx_refs`/`fd` are filled independently.

- [ ] **Step 2: Hook ALL writes at the submit chokepoint, not just `copy_area`**

> Codex finding: `copy_area` is only one mutation path. Fills, clears, RENDER/composite, `put_image`/uploads — any submit that writes an exported drawable — need the same read→write wait and write→read publish. Hooking each request handler is brittle; hook once where every write converges.

Every paint op stamps `touch_render_fence(dst_id, ticket)` on its destination, and all submissions converge at `RenderEngine::flush_submit_group` → `vkQueueSubmit2` (`platform.rs:1736`). Use that as the single chokepoint:

1. **Track exported ids on the engine.** Give the engine a `&HashSet<DrawableId>` view of currently-exported drawables (the backend owns `exported_dmabufs`; pass a cheap snapshot/handle into the flush path, or have the backend drive the flush wrapper). When any write stamps `touch_render_fence(dst_id, _)` and `dst_id` is exported, record `dst_id` into the current submit group's `exported_writes: SmallVec<DrawableId>`.
2. **read→write wait before submit.** In `flush_submit_group`, before `vkQueueSubmit2`, for each `dst_id` in `exported_writes` call `wait_dmabuf_write_ready(fd, 50)` (50 ms matches the read path; `TimedOut` → WARN + proceed, no worse than today).
3. **Signal semaphore on the submit.** When `exported_writes` is non-empty, add an exportable SYNC_FD signal semaphore to the `vkQueueSubmit2` signal list.

```rust
// in flush_submit_group, before submit:
for dst_id in &group.exported_writes {
    if let Some(fd) = exported_fd(dst_id) { // backend-provided lookup
        if let crate::kms::vk::dri3::DmabufWait::TimedOut =
            crate::kms::vk::dri3::wait_dmabuf_write_ready(fd, 50)
        {
            warn!("glx-tfp: write-wait on exported {dst_id:?} timed out; proceeding");
        }
    }
}
```

- [ ] **Step 3: write→read publication after submit (same chokepoint)**

After `vkQueueSubmit2`, export the submit's completion SYNC_FD and IMPORT it onto every exported dmabuf the group wrote:

```rust
// in flush_submit_group, after submit, when exported_writes is non-empty:
if let Some(sync_fd) = self.platform.export_last_submit_sync_file()? {
    for dst_id in &group.exported_writes {
        if let Some(fd) = exported_fd(dst_id) {
            if let Err(e) = crate::kms::vk::dri3::import_dmabuf_write_fence(fd, sync_fd.as_fd()) {
                if e.kind() != io::ErrorKind::Unsupported {
                    warn!("glx-tfp: import write fence failed: {e}");
                }
            }
        }
    }
}
```

Add `PlatformBackend::export_last_submit_sync_file(&self) -> io::Result<Option<OwnedFd>>` — attach an exportable SYNC_FD semaphore (mirroring `PresentCompletionSignal`, `platform.rs:275-310`) to the submit's signal list, then `export_sync_file` it.

> Layering note: `exported_dmabufs` lives on the backend, `flush_submit_group` on the engine. Resolve this by either (a) passing a borrowed lookup closure / `&HashMap` into the flush call, or (b) having the backend own a thin flush wrapper that does the wait/publish around `engine.flush_submit_group`. Pick whichever fits the existing engine↔backend boundary — but the wait/publish MUST be at the converged submit point so no mutation path is missed. Correct over clever: it is fine for the first cut to publish on every flush that touched any exported drawable; optimise only if measured.

- [ ] **Step 4: HW validation (user gate)**

Land Phase 3 first (so TFP is actually advertised and muffin uses it), then verify on silence: cinnamon-settings pane-switch redraws live with NO tearing/stale flashes across many switches. This is the gate that proves the sync direction(s) are right. (Per `feedback_no_commit_before_smoke`, do not declare Phase 2 done before this.)

- [ ] **Step 5: fmt, clippy, commit**

```bash
cargo +nightly fmt && cargo clippy
git add crates/yserver/src/kms/v2/backend.rs crates/yserver/src/kms/vk/platform.rs
git commit -m "feat(glx-tfp): bidirectional dma-buf implicit sync on exported-pixmap writes"
```

---

### Task 2.4: Alias lifetime — keep exported backing alive until consumer releases

Refcount the export so the `NameWindowPixmap` alias + exported dmabuf keep the backing alive until the GL consumer releases, mirroring the `alias_registry` incref in `name_window_pixmap`.

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs` (`dri3_export_pixmap` incref; clear `exported_dmabufs` on final decref)
- Modify: `crates/yserver/src/kms/core.rs` (`AliasRegistry` — reuse incref/decref at `core.rs:1452/1462`)

- [ ] **Step 1: Write the failing test**

```rust
#[test]
#[ignore = "requires a Vulkan device"]
fn exported_backing_retained_until_glx_ref_released_then_torn_down() {
    let mut backend = common::test_kms_backend();
    let host_xid = backend.create_test_pixmap(32, 32, 24);

    // Simulate glXCreatePixmap acquiring a GLX ref, then the export.
    backend.acquire_glx_pixmap_export(host_xid);
    let (.., fd) = backend.dri3_export_pixmap(host_xid).unwrap();
    assert!(backend.has_export_entry(host_xid), "export entry should exist after export");

    // RETENTION: client frees the X pixmap while the GLXPixmap still references it.
    // The entry + lifetime ref must survive (defer-destroy-while-referenced), and
    // the dmabuf must still be importable.
    backend.free_pixmap(host_xid);
    assert!(backend.has_export_entry(host_xid),
        "export entry must survive FreePixmap while glx_refs > 0");
    // Re-import via Vulkan (DrawableImage::from_dmabuf) rather than mmap — the
    // memory is DEVICE_LOCAL and may not be CPU-mappable on a dGPU.
    assert!(common::dmabuf_is_importable(&backend.vk(), fd.as_fd(), 32, 32, 24),
        "backing freed while export still GLX-referenced");

    // RELEASE: glXDestroyPixmap drops the last GLX ref → entry + lifetime ref gone.
    backend.release_glx_pixmap_export(host_xid);
    assert!(!backend.has_export_entry(host_xid),
        "export entry must be torn down once glx_refs hits 0 (no leak)");
}

#[test]
#[ignore = "requires a Vulkan device"]
fn export_only_entry_is_cleaned_up_at_free_pixmap() {
    // A bare BufferFromPixmap with no GLX acquire must not leak.
    let mut backend = common::test_kms_backend();
    let host_xid = backend.create_test_pixmap(32, 32, 24);
    let _ = backend.dri3_export_pixmap(host_xid).unwrap();
    assert!(backend.has_export_entry(host_xid));
    backend.free_pixmap(host_xid); // glx_refs == 0 → immediate teardown
    assert!(!backend.has_export_entry(host_xid),
        "export-only entry (glx_refs == 0) must be torn down at FreePixmap");
}
```

> Test helper `has_export_entry(host_xid) -> bool` introspects `exported_dmabufs` (resolve host_xid → DrawableId → `contains_key`); expose it `#[cfg(test)]` on the backend. These two tests together prove the round-3 and round-4 invariants: teardown on `glx_refs → 0` (no leak when GLXPixmap destroyed first), retention across an early `FreePixmap`, and cleanup of export-only entries.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --test glx_tfp_export exported_backing_retained_until_glx_ref_released_then_torn_down export_only_entry_is_cleaned_up_at_free_pixmap -- --ignored`
Expected: FAIL — `acquire_glx_pixmap_export` / `release_glx_pixmap_export` / `has_export_entry` not defined; no export tracking yet.

- [ ] **Step 3: Implement the single lifetime ref + concrete teardown**

The hazard (codex finding round 1): muffin issues `BufferFromPixmap` repeatedly. The `ensure_exported_entry` helper (Task 2.3 Step 1) already guarantees ONE lifetime ref per backing regardless of export/acquire count. This step adds the matching release + teardown.

**Teardown fires on `glx_refs == 0` alone** (codex finding round 3): the export's lifetime ref exists only to *bridge* the case where the client frees the X pixmap while a GLX consumer still references it. It must NOT be held until `FreePixmap` in the normal create→destroy case — that leaks the ref + entry while the X pixmap's own ref already keeps storage alive. Refcounting handles the bridge automatically: if `FreePixmap` arrives while `glx_refs > 0`, the normal decref runs but storage survives because our extra ref is still held; when `glx_refs` hits 0 we release ours and storage frees then.

```rust
fn release_glx_pixmap_export(&mut self, host_xid: u32) {
    let Some(id) = self.store.lookup(host_xid) else { return };
    if let Some(e) = self.exported_dmabufs.get_mut(&id) {
        e.glx_refs = e.glx_refs.saturating_sub(1);
    }
    self.maybe_teardown_export(id);
}

fn maybe_teardown_export(&mut self, id: DrawableId) {
    // No GLX consumer references the backing any more → release our extra ref
    // and drop the entry, whether or not the X pixmap is still alive. While
    // glx_refs > 0 this is a no-op (defer to the eventual release).
    let ready = matches!(self.exported_dmabufs.get(&id), Some(e) if e.glx_refs == 0);
    if !ready { return; }
    if let Some(e) = self.exported_dmabufs.remove(&id) {
        drop(e.fd); // close our dup; kernel buffer survives while any GL consumer holds its own fd
        if e.lifetime_ref_held {
            // The single matching release of the ref taken in ensure_exported_entry.
            // If the X pixmap is still alive, its own ref keeps storage; if it was
            // already freed, this release lets the backing free now.
            self.release_backing_lifetime_ref(id); // alias_registry.decref OR DrawableStore decref
        }
    }
}
```

Teardown is reached from THREE sites, all routing through `maybe_teardown_export` (which gates on `glx_refs == 0`):
- **`glXDestroyPixmap`** → `release_glx_pixmap_export(host_xid)` (Task 3.4): decrements `glx_refs`, then `maybe_teardown_export`. When it hits 0 (last GLXPixmap gone) the entry is dropped even if the X pixmap is still alive.
- **Client `FreePixmap`** (`process_request.rs:948`) and **client disconnect cleanup** MUST call `maybe_teardown_export(id)` (codex finding round 4). This is what cleans up:
  - **export-only entries** — a bare DRI3 `BufferFromPixmap` with no `glXCreatePixmap` acquire, or the Task-2.4 liveness test, creates an entry with `glx_refs == 0`. At `FreePixmap`/disconnect it is torn down immediately (`glx_refs == 0` → ready), releasing the lifetime ref taken in `ensure_exported_entry`. Without this hook the entry + ref would leak forever.
  - **the defer case** — if `FreePixmap` arrives while `glx_refs > 0` (a GLX consumer still references it), `maybe_teardown_export` is a no-op; the lifetime ref keeps `DrawableStore`/`Storage` alive until `glXDestroyPixmap` drops `glx_refs` to 0 (GLX defer-destroy-while-referenced semantics). A re-export (`dri3_export_pixmap`) NEVER calls `maybe_teardown_export` and never decrements `glx_refs`.

> The exported fd is itself a kernel reference to the underlying GBM/DRM buffer, so the *memory* survives even if Vulkan frees the image. But the `DrawableStore` entry + `Storage` must not be torn down while writes are still routed to it — the single lifetime ref is what prevents that during the bridge window. One ref taken (`ensure_exported_entry`), one released (`maybe_teardown_export`): never per-export, and never leaked because all three teardown sites converge on `maybe_teardown_export`.
>
> **Backing lifetime ref — which counter:** for the muffin MVP path the X pixmap is a NameWindowPixmap'd redirect backing already in `alias_registry`, so `take/release_backing_lifetime_ref` = `alias_registry.incref/decref(backing)` (pattern from `name_window_pixmap`, `backend.rs:8999`; on `decref → true` free via `backend.rs:9167`). For a plain (non-redirect) pixmap exported via `glXCreatePixmap`, there is no alias_registry entry — hold a `DrawableStore` `Drawable::refcount` ref instead (via `store.incref(id)` / `decref(id)`). Implement `take/release_backing_lifetime_ref` to pick the right counter based on whether `alias_registry.get(backing).is_some()`.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test --test glx_tfp_export exported_backing_retained_until_glx_ref_released_then_torn_down export_only_entry_is_cleaned_up_at_free_pixmap -- --ignored`
Expected: PASS — retention across FreePixmap while GLX-referenced, teardown on glx_refs→0, and export-only cleanup at FreePixmap.

- [ ] **Step 5: fmt, clippy, commit**

```bash
cargo +nightly fmt && cargo clippy
git add crates/yserver/src/kms/v2/backend.rs crates/yserver/src/kms/core.rs crates/yserver/tests/glx_tfp_export.rs
git commit -m "feat(glx-tfp): refcount + teardown exported-backing lifetime (acquire/release/FreePixmap)"
```

---

# Phase 3 — GLX protocol surface

**Outcome:** the extension is advertised only when the backend can satisfy it; FBConfigs carry the bind-to-texture pairs in Xorg's exact order; GLXPixmaps map to X pixmaps; the indirect `BindTexImageEXT`/`ReleaseTexImageEXT` path works.

**Grounding:**
- GLX ext string is the static `SERVER_EXTENSIONS: &str` at `glx.rs:112` (yserver-protocol), returned verbatim for both `QUERY_SERVER_STRING` (`process_request.rs:8155`) and `QUERY_EXTENSIONS_STRING` (`process_request.rs:8169`). Must become capability-conditional.
- `synthesise_glx_fb_configs` (`process_request.rs:7996`, yserver-core) returns `Vec<Vec<(u32,u32)>>`, 4 configs × 25 pairs. `GLX_DRAWABLE_TYPE` is ALREADY `WINDOW_BIT | PIXMAP_BIT (0x3)`. The bind-to-texture pairs are NOT yet emitted. `encode_get_fb_configs_reply` (`glx.rs:283`) requires every config the same length.
- TFP attribute constants are NOT defined in `glx.rs` — only as locals in `drawable_attributes_for` (`process_request.rs:7904`): `GLX_TEXTURE_TARGET_EXT=0x20D6`, `GLX_TEXTURE_2D_EXT=0x20DC`, `GLX_Y_INVERTED_EXT=0x20D4`.
- `glXCreatePixmap` (`process_request.rs:8311`) inserts a `GlxDrawable{owner, x_drawable, fbconfig, width:0, height:0, attributes}` into `state.glx_drawables: HashMap<u32, GlxDrawable>` (`server.rs:940`/`1048`). `glXDestroyPixmap` (`process_request.rs:8368`) removes it. No separate GLXPixmap type — `x_drawable` is the link to the X pixmap.
- VendorPrivate(16)/VendorPrivateWithReply(17) are rejected at `process_request.rs:8459` with `GLX_FIRST_ERROR(169) + ERROR_GLX_UNSUPPORTED_PRIVATE_REQUEST(8)`, major `GLX_MAJOR_OPCODE(148)`. The body's `vendorCode: u32` is not read.
- Xorg order for the bind-to-texture FBConfig pairs (`glxcmds.c:1094-1100`): `GLX_BIND_TO_TEXTURE_RGB_EXT`, `GLX_BIND_TO_TEXTURE_RGBA_EXT`, `GLX_BIND_TO_MIPMAP_TEXTURE_EXT`(=0), `GLX_BIND_TO_TEXTURE_TARGETS_EXT`, `GLX_Y_INVERTED_EXT`. Targets = `GLX_TEXTURE_2D_BIT_EXT | GLX_TEXTURE_RECTANGLE_BIT_EXT` (`glxdricommon.c:165`). `GLX_Y_INVERTED_EXT` in FBConfig = `GLX_DONT_CARE` (`glxcmds.c:1093`).

---

### Task 3.1: Runtime capability gate for the GLX extension string

Make the advertised GLX extension string append `GLX_EXT_texture_from_pixmap` only when the backend reports the capability (Vulkan + external-memory dmabuf export available).

**Files:**
- Modify: `crates/yserver-core/src/server.rs` (add a `glx_tfp_supported: bool` capability flag on `ServerState`, set at backend init)
- Modify: `crates/yserver-protocol/src/x11/glx.rs` (split the constant: keep base `SERVER_EXTENSIONS`; add `TFP_EXTENSION` token)
- Modify: `crates/yserver-core/src/core_loop/process_request.rs` (`QUERY_SERVER_STRING`/`QUERY_EXTENSIONS_STRING` build the string conditionally)
- Test: `crates/yserver-core` unit test for the string-builder.

- [ ] **Step 1: Write the failing unit test**

In `crates/yserver-core/src/core_loop/process_request.rs` tests (or a focused module test):

```rust
#[test]
fn glx_extension_string_includes_tfp_only_when_capable() {
    let with = glx_extension_string(true);
    let without = glx_extension_string(false);
    assert!(with.contains("GLX_EXT_texture_from_pixmap"));
    assert!(!without.contains("GLX_EXT_texture_from_pixmap"));
    // Base extensions always present.
    assert!(with.contains("GLX_ARB_create_context"));
    assert!(without.contains("GLX_ARB_create_context"));
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p yserver-core glx_extension_string_includes_tfp_only_when_capable`
Expected: FAIL — `glx_extension_string` not defined.

- [ ] **Step 3: Implement**

In `glx.rs`, add:

```rust
pub const TFP_EXTENSION: &str = "GLX_EXT_texture_from_pixmap";
```

In `process_request.rs`, add a small builder and use it in both opcode handlers:

```rust
fn glx_extension_string(tfp_supported: bool) -> String {
    let mut s = String::from(yserver_protocol::x11::glx::SERVER_EXTENSIONS);
    if tfp_supported {
        s.push(' ');
        s.push_str(yserver_protocol::x11::glx::TFP_EXTENSION);
    }
    s
}
```

Add `glx_tfp_supported: bool` to `ServerState` (`server.rs`), default `false`, set ONCE during backend init from a cached backend capability.

**The capability must be a real probe, not just an extension check** (codex finding round 2): advertising TFP is client-visible, so it must reflect that yserver can actually allocate AND export a BGRA8 pixmap via the exact Task-1.1 path. **Probe once at init and store the bool** (codex finding round 3 — do NOT re-probe Vulkan on every protocol query): run `probe_dmabuf_export_support` during backend construction, cache it in a `dmabuf_export_supported: bool` backend field, and have the trait getter return the cached value.

```rust
// Run ONCE during KMS backend init; store the result.
fn probe_dmabuf_export_support(vk: &VkContext) -> bool {
    if vk.external_memory_fd.is_none() { return false; }
    // Probe the real allocate+export path with a 1x1 BGRA8 image.
    match crate::kms::vk::target::allocate_exportable(
        vk, 1, 1, crate::kms::vk::target::EXPORT_FORMAT_BGRA8,
    ) {
        Ok(img) => {
            let ok = crate::kms::vk::dri3::export_backing(vk, &img).is_ok();
            crate::kms::vk::target::destroy_exportable(vk, img); // vkDestroyImage + vkFreeMemory
            ok
        }
        Err(_) => false,
    }
}

// Cheap cached getter used by the protocol path (no Vulkan calls).
fn supports_dmabuf_export(&self) -> bool { self.dmabuf_export_supported }
```

During KMS backend construction: `let dmabuf_export_supported = platform.vk.as_ref().map(probe_dmabuf_export_support).unwrap_or(false);` and store it in the field. Add `target::destroy_exportable(vk, ExportableImage)` (the `vkDestroyImage`+`vkFreeMemory` teardown for a probe image — no views were created). Replace the two verbatim `SERVER_EXTENSIONS` uses at `process_request.rs:8155`/`8169` with `glx_extension_string(state.glx_tfp_supported)`.

> A probe failure here means TFP is never advertised — clients silently fall back, exactly as today. This is strictly safer than advertising on the weaker `is_some()` check and then returning `BadPixmap` at export time.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p yserver-core glx_extension_string_includes_tfp_only_when_capable`
Expected: PASS.

- [ ] **Step 5: fmt, clippy, commit**

```bash
cargo +nightly fmt && cargo clippy
git add crates/yserver-protocol/src/x11/glx.rs crates/yserver-core/src/server.rs crates/yserver-core/src/core_loop/process_request.rs
git commit -m "feat(glx-tfp): runtime-gate GLX_EXT_texture_from_pixmap on dmabuf-export capability"
```

---

### Task 3.2: Define TFP attribute constants in glx.rs

Promote the local TFP token constants into `glx.rs` so both the FBConfig encoder and `drawable_attributes_for` share them.

**Files:**
- Modify: `crates/yserver-protocol/src/x11/glx.rs` (add constants near `glx.rs:215-261`)
- Modify: `crates/yserver-core/src/core_loop/process_request.rs` (`drawable_attributes_for` uses the shared constants instead of its locals)

- [ ] **Step 1: Add constants**

In `glx.rs`:

```rust
// GLX_EXT_texture_from_pixmap tokens.
pub const GLX_TEXTURE_TARGET_EXT: u32 = 0x20D6;
pub const GLX_TEXTURE_2D_EXT: u32 = 0x20DC;
pub const GLX_Y_INVERTED_EXT: u32 = 0x20D4;
pub const GLX_BIND_TO_TEXTURE_RGB_EXT: u32 = 0x20D0;
pub const GLX_BIND_TO_TEXTURE_RGBA_EXT: u32 = 0x20D1;
pub const GLX_BIND_TO_MIPMAP_TEXTURE_EXT: u32 = 0x20D2;
pub const GLX_BIND_TO_TEXTURE_TARGETS_EXT: u32 = 0x20D3;
pub const GLX_TEXTURE_1D_BIT_EXT: u32 = 0x0001;
pub const GLX_TEXTURE_2D_BIT_EXT: u32 = 0x0002;
pub const GLX_TEXTURE_RECTANGLE_BIT_EXT: u32 = 0x0004;
// GLX_DONT_CARE for FBConfig "don't care" values (Xorg glxcmds.c:1093).
pub const GLX_DONT_CARE: u32 = 0xFFFF_FFFF;
```

> Verify these hex values against `/home/jos/Projects/xserver/include/GL/glxext.h` (or `glx.h`) before committing — do not trust recall (see `feedback_no_hallucinated_constants`). `0x20D0..0x20D6` and the `_BIT_` triplet `0x1/0x2/0x4` are the canonical GLX_EXT_texture_from_pixmap enums; confirm.

- [ ] **Step 2: Replace locals in `drawable_attributes_for`**

In `process_request.rs:7904`, delete the three `const GLX_*` locals and reference `glx::GLX_TEXTURE_TARGET_EXT` etc. No behavior change (drawable attr `GLX_Y_INVERTED_EXT` stays `0` = GL_FALSE per Xorg `glxcmds.c:1900`).

- [ ] **Step 3: Build + existing GLX tests still pass**

Run: `cargo test -p yserver-core glx` `&&` `cargo build -p yserver-protocol`
Expected: PASS, no behavior change.

- [ ] **Step 4: fmt, clippy, commit**

```bash
cargo +nightly fmt && cargo clippy
git add crates/yserver-protocol/src/x11/glx.rs crates/yserver-core/src/core_loop/process_request.rs
git commit -m "feat(glx-tfp): centralize GLX_EXT_texture_from_pixmap token constants"
```

---

### Task 3.3: Emit bind-to-texture pairs in FBConfig replies (Xorg order)

Append the five bind-to-texture pairs to each FBConfig in `synthesise_glx_fb_configs`, in Xorg's exact reply order, with yserver's depth-derived RGB/RGBA policy. Only when the TFP capability is on.

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request.rs` (`synthesise_glx_fb_configs` at `7996`; pass the capability flag in)
- Test: `crates/yserver-core` unit test asserting exact order + values + per-config length equality.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn fbconfigs_emit_bind_to_texture_pairs_in_xorg_order() {
    use yserver_protocol::x11::glx as g;
    let configs = synthesise_glx_fb_configs(/*tfp_supported*/ true);
    // All configs same length (wire encoder requirement).
    let len = configs[0].len();
    assert!(configs.iter().all(|c| c.len() == len));

    for c in &configs {
        // Find the bind-to-texture block; it must appear as a contiguous run in this order.
        let idx = c.iter().position(|(k, _)| *k == g::GLX_BIND_TO_TEXTURE_RGB_EXT).expect("RGB present");
        let order = [
            g::GLX_BIND_TO_TEXTURE_RGB_EXT,
            g::GLX_BIND_TO_TEXTURE_RGBA_EXT,
            g::GLX_BIND_TO_MIPMAP_TEXTURE_EXT,
            g::GLX_BIND_TO_TEXTURE_TARGETS_EXT,
            g::GLX_Y_INVERTED_EXT,
        ];
        for (i, key) in order.iter().enumerate() {
            assert_eq!(c[idx + i].0, *key, "wrong order at offset {i}");
        }
        // Values:
        assert_eq!(c[idx + 2].1, 0, "MIPMAP must be 0/false");
        assert_eq!(c[idx + 3].1, g::GLX_TEXTURE_2D_BIT_EXT | g::GLX_TEXTURE_RECTANGLE_BIT_EXT);
        assert_eq!(c[idx + 4].1, g::GLX_DONT_CARE, "Y_INVERTED in FBConfig is GLX_DONT_CARE");
    }

    // Depth policy: depth-24 (visual 0x102) RGB=true RGBA=false; depth-32 (0x103) RGB=true RGBA=true.
    let depth24 = configs.iter().find(|c| c.iter().any(|(k, v)| *k == g::GLX_VISUAL_ID && *v == 0x102)).unwrap();
    let d24 = depth24.iter().position(|(k, _)| *k == g::GLX_BIND_TO_TEXTURE_RGB_EXT).unwrap();
    assert_eq!(depth24[d24].1, 1);       // RGB true
    assert_eq!(depth24[d24 + 1].1, 0);   // RGBA false on depth-24

    let depth32 = configs.iter().find(|c| c.iter().any(|(k, v)| *k == g::GLX_VISUAL_ID && *v == 0x103)).unwrap();
    let d32 = depth32.iter().position(|(k, _)| *k == g::GLX_BIND_TO_TEXTURE_RGB_EXT).unwrap();
    assert_eq!(depth32[d32 + 1].1, 1);   // RGBA true on depth-32
}

#[test]
fn fbconfigs_omit_bind_to_texture_when_tfp_unsupported() {
    use yserver_protocol::x11::glx as g;
    let configs = synthesise_glx_fb_configs(false);
    assert!(configs.iter().all(|c| c.iter().all(|(k, _)| *k != g::GLX_BIND_TO_TEXTURE_RGB_EXT)));
    // Still equal length across configs.
    let len = configs[0].len();
    assert!(configs.iter().all(|c| c.len() == len));
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p yserver-core fbconfigs_emit_bind_to_texture_pairs_in_xorg_order fbconfigs_omit_bind_to_texture_when_tfp_unsupported`
Expected: FAIL — `synthesise_glx_fb_configs` takes no args / pairs absent.

- [ ] **Step 3: Implement**

Change `synthesise_glx_fb_configs()` to `synthesise_glx_fb_configs(tfp_supported: bool)`. After the existing 25 pairs, when `tfp_supported`, append per config (depth known from the visual: `0x102`=24, `0x103`=32):

```rust
if tfp_supported {
    let (rgb, rgba) = match visual_id {
        0x102 => (1u32, 0u32), // depth-24: opaque; sample_view forces α=1 (BgraNoAlpha)
        0x103 => (1u32, 1u32), // depth-32: RGBA
        _ => (1, 0),
    };
    config.push((g::GLX_BIND_TO_TEXTURE_RGB_EXT, rgb));
    config.push((g::GLX_BIND_TO_TEXTURE_RGBA_EXT, rgba));
    config.push((g::GLX_BIND_TO_MIPMAP_TEXTURE_EXT, 0)); // not backed; pair required for contract
    config.push((g::GLX_BIND_TO_TEXTURE_TARGETS_EXT,
        g::GLX_TEXTURE_2D_BIT_EXT | g::GLX_TEXTURE_RECTANGLE_BIT_EXT));
    config.push((g::GLX_Y_INVERTED_EXT, g::GLX_DONT_CARE));
}
```

`GLX_DRAWABLE_TYPE` already includes `GLX_PIXMAP_BIT` (verified — no change). Because every config gets the same 5 appended pairs (or none), the equal-length invariant holds. Thread `state.glx_tfp_supported` into the `GET_FB_CONFIGS` call site (`process_request.rs:8200`).

> RGB/RGBA values are yserver policy, not Xorg-exact (only order is exact). If a client rejects the stricter depth-24-RGBA=false set, revisit to "both true" per spec §3 — leave a comment to that effect.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p yserver-core fbconfigs_emit_bind_to_texture_pairs_in_xorg_order fbconfigs_omit_bind_to_texture_when_tfp_unsupported`
Expected: PASS.

- [ ] **Step 5: fmt, clippy, commit**

```bash
cargo +nightly fmt && cargo clippy
git add crates/yserver-core/src/core_loop/process_request.rs
git commit -m "feat(glx-tfp): emit bind-to-texture FBConfig pairs in Xorg order (depth-derived RGB/RGBA)"
```

---

### Task 3.4: GLXPixmap → X pixmap association for the TFP path

Ensure `glXCreatePixmap` correctly records the GLXPixmap→X-pixmap mapping for TFP, and `glXDestroyPixmap` releases the export ref (Task 2.4). The existing `GlxDrawable.x_drawable` already holds the X pixmap XID — this task wires destroy to the export-lifetime decref and validates the X pixmap exists.

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request.rs` (`glXCreatePixmap` `8311`, `glXDestroyPixmap` `8368`)
- Test: `crates/yserver-core` unit test for the create/destroy bookkeeping.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn glx_create_pixmap_records_x_drawable_and_destroy_clears_it() {
    let mut state = test_server_state();
    let client = test_client(&mut state);
    let x_pixmap = make_test_pixmap(&mut state, client, 64, 32, 24);
    let glx_xid = 0x4000_0001;

    handle_glx_create_pixmap(&mut state, client, /*screen*/0, /*fbconfig*/0x101, x_pixmap, glx_xid);
    let d = state.glx_drawables.get(&glx_xid).expect("glx pixmap recorded");
    assert_eq!(d.x_drawable, x_pixmap);

    handle_glx_destroy_pixmap(&mut state, client, glx_xid);
    assert!(state.glx_drawables.get(&glx_xid).is_none());
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p yserver-core glx_create_pixmap_records_x_drawable_and_destroy_clears_it`
Expected: FAIL — extract `handle_glx_create_pixmap`/`handle_glx_destroy_pixmap` helpers if the dispatch is inline; or adapt the test to call the existing arms.

- [ ] **Step 3: Implement**

The create arm at `8311` already inserts `GlxDrawable{x_drawable: req.x_window, ...}` — confirm it stores the X pixmap XID (it does). Add validation: if `state.resources.pixmap(ResourceId(req.x_window))` is `None`, emit `GLXBadPixmap` (GLX error) rather than silently inserting. On a valid insert, resolve `req.x_window` → host_xid and call `backend.acquire_glx_pixmap_export(host_xid)` (Task 2.4: increments `glx_refs`). The destroy arm at `8368` already `remove`s; extend it so that, when removing a `GlxDrawable`, it resolves `x_drawable` → host_xid and calls `backend.release_glx_pixmap_export(host_xid)` (decrements `glx_refs`, releasing the single alias ref when it reaches 0). The disconnect cleanup (`process_disconnect.rs:205`) that `retain`s `glx_drawables` must call the same `release_glx_pixmap_export` for each removed drawable.

> `acquire_glx_pixmap_export` calls `ensure_exported_entry(id, backing)` (Task 2.3 Step 1) then `glx_refs += 1`. Because `ensure_exported_entry` creates the entry and takes the single lifetime ref on first touch, calling `acquire` BEFORE the first `BufferFromPixmap` is fully defined — the entry exists with `fd: None`, and the later export just fills in `fd`. This is exactly the common `glXCreatePixmap`-then-`BufferFromPixmap` order, with no lost ref and no reconciliation step. `release_glx_pixmap_export` is specified in Task 2.4 Step 3.

> No new resource type needed — `glx_drawables` keyed by GLX XID with `x_drawable` is sufficient (matches Xorg's model where the GLXPixmap is a thin wrapper). The TFP bind path (Task 3.5 + the muffin DRI3 path) resolves `x_drawable` → host pixmap → `dri3_export_pixmap`.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p yserver-core glx_create_pixmap_records_x_drawable_and_destroy_clears_it`
Expected: PASS.

- [ ] **Step 5: fmt, clippy, commit**

```bash
cargo +nightly fmt && cargo clippy
git add crates/yserver-core/src/core_loop/process_request.rs
git commit -m "feat(glx-tfp): validate GLXPixmap X-drawable + release export ref on destroy"
```

---

### Task 3.5: Indirect-context BindTexImageEXT / ReleaseTexImageEXT

Open the VendorPrivate path and implement `glXBindTexImageEXT` (vendor code 1330) and `glXReleaseTexImageEXT` (1331) for indirect contexts. Direct contexts (muffin) ride the DRI3 export and never hit these, but "complete" requires them.

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request.rs` (VendorPrivate arm at `8459`)
- Modify: `crates/yserver-protocol/src/x11/glx.rs` (vendor code constants; request parser)
- Test: `crates/yserver-core` unit test that the vendor codes dispatch (no longer return GLXUnsupportedPrivateRequest).

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn bind_tex_image_ext_is_dispatched_not_rejected() {
    let mut state = test_server_state();
    let client = test_client(&mut state);
    let x_pixmap = make_test_pixmap(&mut state, client, 32, 32, 24);
    let glx_xid = 0x4000_0002;
    handle_glx_create_pixmap(&mut state, client, 0, 0x101, x_pixmap, glx_xid);

    // VendorPrivate body: vendor_code(1330) + context_tag + glx_drawable + buffer.
    let body = build_vendor_private(glx::VENDOR_CODE_BIND_TEX_IMAGE, &[/*ctx_tag*/0, glx_xid, /*buffer*/ glx::GLX_FRONT_LEFT_EXT]);
    let outcome = dispatch_glx_vendor_private(&mut state, client, &body, /*with_reply*/ false);
    assert!(!matches!(outcome, GlxOutcome::Error(code) if code == GLX_FIRST_ERROR + ERROR_GLX_UNSUPPORTED_PRIVATE_REQUEST),
        "BindTexImageEXT must not be rejected as unsupported");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p yserver-core bind_tex_image_ext_is_dispatched_not_rejected`
Expected: FAIL — vendor private still unconditionally rejected.

- [ ] **Step 3: Implement**

In `glx.rs` add:

```rust
pub const VENDOR_CODE_BIND_TEX_IMAGE: u32 = 1330;
pub const VENDOR_CODE_RELEASE_TEX_IMAGE: u32 = 1331;
pub const GLX_FRONT_LEFT_EXT: u32 = 0x20DE;
```

> Confirm `GLX_FRONT_LEFT_EXT = 0x20DE` against `glxext.h`. The `buffer` arg is informational for our backend (single front buffer) — accept any value.

Rewrite the VendorPrivate arm at `8459` to read `vendor_code = u32::from_ne`... (parse `body[0..4]` with the wire byte order used elsewhere — match the existing GLX request parsers in `glx.rs`) and dispatch:

```rust
let vendor_code = read_card32(body, 0);
match vendor_code {
    glx::VENDOR_CODE_BIND_TEX_IMAGE => {
        // body: [vendor_code][context_tag][glx_drawable][buffer]
        let glx_drawable = read_card32(body, 8);
        // Resolve to the underlying X pixmap and ensure it is exportable/up-to-date.
        // For an indirect context yserver IS the GL implementation, so "bind" means:
        // the texture sourced from this GLXPixmap should sample the current pixmap
        // contents. Promotion (Phase 1) already makes the backing the live image;
        // record the binding so the indirect GL texture state samples x_drawable.
        if let Some(d) = state.glx_drawables.get(&glx_drawable) {
            let x_drawable = d.x_drawable;
            backend_bind_tex_image(state, x_drawable)?; // promote-if-needed; mark bound
        } else {
            return glx_error(state, client, sequence, GLXBadPixmap);
        }
        // BindTexImageEXT has no reply.
        return Ok(());
    }
    glx::VENDOR_CODE_RELEASE_TEX_IMAGE => {
        let glx_drawable = read_card32(body, 8);
        if let Some(d) = state.glx_drawables.get(&glx_drawable) {
            backend_release_tex_image(state, d.x_drawable);
        }
        return Ok(());
    }
    _ => {
        // Preserve the existing rejection for genuinely-unsupported codes.
        return emit_x11_error_with_minor(/* ...GLXUnsupportedPrivateRequest as before... */);
    }
}
```

> Indirect-context TFP is a completeness item, not the muffin path. The minimal correct behavior: resolve `glx_drawable` → `x_drawable`, ensure it is promoted (so subsequent indirect GL sampling of that texture reads live content), and track the bind/release for ref accounting. If yserver's indirect GL does not yet implement texture sampling from a pixmap at all, it is acceptable for this first cut to (a) validate + promote + return success without an error (so clients that probe BindTexImageEXT succeed) and (b) leave a `// TODO(glx-tfp): indirect texture sampling` — but DO NOT return a zero/stub that crashes real clients (see `feedback_no_protocol_stubs`). The direct path is what the HW gate exercises.

`VendorPrivateWithReply` for these two codes: neither has a reply in the EXT spec, so they arrive as `VENDOR_PRIVATE` (16). If a client sends them via `VENDOR_PRIVATE_WITH_REPLY` (17), send an empty success reply rather than erroring.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p yserver-core bind_tex_image_ext_is_dispatched_not_rejected`
Expected: PASS.

- [ ] **Step 5: fmt, clippy, commit**

```bash
cargo +nightly fmt && cargo clippy
git add crates/yserver-protocol/src/x11/glx.rs crates/yserver-core/src/core_loop/process_request.rs
git commit -m "feat(glx-tfp): open VendorPrivate path; dispatch BindTexImageEXT/ReleaseTexImageEXT (1330/1331)"
```

---

# Phase 4 — DRI3 export contract

**Outcome:** confirm single-fd `BufferFromPixmap` (op 3) is the correct minimal contract for yserver's single-plane BGRA8 backings; document `BuffersFromPixmap` (op 8) deferral. (The `BadPixmap` error mapping is handled earlier in Task 1.3 Step 5.)

**Grounding:**
- `BufferFromPixmap` (DRI3 op 3) handler at `process_request.rs:7550`; parser `parse_buffer_from_pixmap` (`dri3.rs:186`) reads the 4-byte pixmap XID; resolves X pixmap → host_xid → `backend.dri3_export_pixmap`; on success `encode_buffer_from_pixmap_reply` (`dri3.rs:316`) + `send_reply_with_fd`. **Currently returns `BadAlloc` on export `Err`.**
- `BuffersFromPixmap` is op **8** (`BUFFERS_FROM_PIXMAP = 8`; op 7 is `PIXMAP_FROM_BUFFERS`) — handler at `process_request.rs:7617` returns `BadAlloc` unconditionally (deferred). The spec's "op 7" is a mislabel; the constant is 8.
- Xorg returns `BadPixmap` when `dri3_fd_from_pixmap()` fails (`dri3/dri3_request.c:277`); `BadAlloc` only if *sending the fd* fails.

---

### Task 4.1: Lock in single-plane contract; document BuffersFromPixmap deferral

> The `BadPixmap` error mapping moved to **Task 1.3 Step 5** (it must precede the Phase-3 advertise gate). This task is now just: confirm the single-fd contract is sufficient and document the op-8 deferral.

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request.rs` (`BuffersFromPixmap` arm at `7617`)

- [ ] **Step 1: Confirm `BufferFromPixmap` (op 3) suffices**

Verify on HW (Task 5.1's xtrace) that muffin/Mesa drives the single-fd `BufferFromPixmap` (op 3) path and never needs `BuffersFromPixmap` (op 8) for yserver's single-plane BGRA8 linear backings — Xorg's `dri3_fd_from_pixmap` *prefers* the single-fd interface (`dri3_screen.c:112`). No code change if confirmed.

- [ ] **Step 2: Document BuffersFromPixmap deferral**

In the `BuffersFromPixmap` arm (`7617`), update the `debug!` message and add a code comment: "op 8 (BUFFERS_FROM_PIXMAP); deferred — single-plane BGRA8 exports via BufferFromPixmap (op 3) per Xorg dri3_fd_from_pixmap preference (dri3_screen.c:112). Implement only when yserver exports multi-plane/modifier-bearing buffers." No behavior change (stays the current unsupported response).

- [ ] **Step 3: fmt, clippy, commit**

```bash
cargo +nightly fmt && cargo clippy
git add crates/yserver-core/src/core_loop/process_request.rs
git commit -m "docs(glx-tfp): document DRI3 BuffersFromPixmap (op 8) deferral; single-plane contract confirmed"
```

---

# Integration & HW validation (the real gate)

### Task 5.1: End-to-end cinnamon repro on hardware

**Files:** none (validation). This is the MVP success gate (spec §Goal).

- [ ] **Step 1:** On silence (rx580/RADV) or bee, run yserver with all phases landed. Launch Cinnamon (muffin GLX compositor).
- [ ] **Step 2:** Confirm via `COGL_DEBUG=winsys` that muffin NO LONGER logs "Not using GLX TFP!" and DOES use the TFP path.
- [ ] **Step 3:** xtrace yserver vs Xorg: muffin now issues `glXCreatePixmap` + DRI3 `BufferFromPixmap` (Xorg did 17×, yserver previously 0×). Counts should match Xorg's order of magnitude.
- [ ] **Step 4:** The repro: open cinnamon-settings, switch panes repeatedly. Content must redraw live every time — no stale pane, no tearing/flashing across many rapid switches (validates Phase 2 sync direction).
- [ ] **Step 5:** Regression sweep — chromium (`--use-angle=vulkan`) and a GTK app render unaffected; gtk3-demo still works. (See `project_chromium_glx_pbuffer_gap`, `project_phase31_complete`.)
- [ ] **Step 6:** Update `docs/status.md` with the TFP feature status. Update the `project_cinnamon_settings_norefresh` memory to RESOLVED with the root-cause + fix summary once HW-verified.

> Do NOT squash-merge before Step 4 passes on hardware (`feedback_no_commit_before_smoke`). MVP = Phases 1–3 + this gate. Robust tail (indirect texture sampling, full FBConfig matrix, multi-plane/modifiers) is follow-up.

---

# Self-review (against the spec)

**Spec coverage:**
- Component 1 (exportable-pixmap promotion): Tasks 1.1–1.3 — alloc, copy+layout, storage swap, view-cache invalidation, in-flight CB retire, extend `dri3_export_pixmap`. ✓
- Component 2 (bidirectional dma-buf sync): Tasks 2.0–2.4 — characterize, WRITE-scope export, IMPORT_SYNC_FILE, wire both directions, alias lifetime. ✓
- Component 3 (GLX surface): Tasks 3.1–3.5 — runtime gate, constants, FBConfig pairs in Xorg order, GLXPixmap tracking, VendorPrivate bind/release. ✓
- Component 4 (DRI3): Task 4.1 — BadPixmap mapping, single-plane contract, op-8 deferral. ✓
- Testing (unit + `--ignored` Vulkan + HW): Tasks carry unit tests; 1.x/2.x carry `--ignored` Vulkan tests; Task 5.1 is the HW gate. ✓
- Error handling (BadPixmap on export failure, advertise only when capable): Tasks 4.1 + 3.1. ✓
- Phasing (MVP = 1–3 + HW gate, robust tail follow-up): preserved. ✓

**Known spec deviations folded in:**
- Spec "BuffersFromPixmap op 7" → corrected to op 8 (`BUFFERS_FROM_PIXMAP = 8`).
- Spec "add GLX_PIXMAP_BIT to GLX_DRAWABLE_TYPE" → already present (`0x3`); only bind-to-texture pairs are added.
- `synthesise_glx_fb_configs` / `drawable_attributes_for` live in **yserver-core**, not yserver-protocol.

**Open items deferred to implementation (per spec Open questions):**
- Exact Vulkan↔dma-buf sync mechanism is validated in Task 2.0 before 2.1–2.3 are finalized; may collapse if RADV implicit-syncs same-GPU.
- Reuse existing memory if already exportable vs always realloc — `is_exportable()` short-circuits re-promotion; further optimization only if measured.
- Hex values for GLX TFP tokens (Task 3.2) and `GLX_FRONT_LEFT_EXT` (Task 3.5) must be confirmed against `glxext.h` before commit.
- DRM-format-modifier export path in `allocate_exportable` (Task 1.1): the layout query must use `VK_IMAGE_ASPECT_MEMORY_PLANE_0_BIT_EXT` for `DRM_FORMAT_MODIFIER_EXT` tiling. MVP may ship `LINEAR`-only and defer the modifier branch.

**Codex review round 1 (2026-06-09) folded in:**
1. Export stride/size now carried in `Storage` (`export_stride`/`export_size`) via `adopt_exportable` — no zero-metadata replies (Tasks 1.2/1.3).
2. Export holds exactly ONE lifetime ref per backing, tied to a `glx_refs` count, idempotent across repeated `BufferFromPixmap` (Tasks 2.3/2.4/3.4).
3. Liveness/lifetime tests use GPU-staging readback / Vulkan re-import, not mmap — DEVICE_LOCAL memory isn't CPU-mappable on the dGPU test box (Tasks 1.2/2.4).
4. Sync wait/publish hooked at the `flush_submit_group` chokepoint, covering all mutation paths (not just `copy_area`) (Task 2.3).
5. `BufferFromPixmap` → `BadPixmap` mapping moved to Task 1.3, before the Phase-3 advertise gate.
6. The Phase-2 TFP-advertise experiment is env-gated AND uncommitted, with a mandatory revert before proceeding (Task 2.0).

**Codex review round 2 (2026-06-09) folded in:**
1. Imported storage does NOT read nonexistent `DrawableImage` metadata fields — `export_stride`/`export_size` are promotion-only; imported export keeps the existing `export_dmabuf` (self-querying) path (Task 1.2).
2. Capability gate `supports_dmabuf_export` is now a real allocate+export probe of the Task-1.1 path (1×1 BGRA8), cached at init — not a bare `external_memory_fd.is_some()` check (Task 3.1).
3. Ref accounting made order-independent and concrete: `ensure_exported_entry` takes the single lifetime ref on first touch (export OR `glXCreatePixmap`), `fd: Option`, explicit `maybe_teardown_export` (Tasks 2.3/2.4/3.4) — no placeholder/reconcile ambiguity.
4. `allocate_exportable` is firmly LINEAR-only for the MVP; the `DRM_FORMAT_MODIFIER_EXT` branch is removed from the pseudo-code and explicitly deferred (Task 1.1 + Open items).

**Codex review round 3 (2026-06-09) folded in:**
1. Export teardown fires on `glx_refs == 0` alone (dropped the `x_pixmap_freed` gate + `note_export_pixmap_freed` hook) — a create/destroy GLXPixmap cycle no longer leaks the lifetime ref + entry until `FreePixmap`; the bridge-when-X-pixmap-freed-early case is handled purely by refcounting (Tasks 2.3/2.4).
2. Capability probe split into a one-shot `probe_dmabuf_export_support` (init) + cached `dmabuf_export_supported` field; the protocol getter `supports_dmabuf_export` makes no Vulkan calls (Task 3.1).

**Codex review round 4 (2026-06-09) folded in:**
1. `FreePixmap` and client-disconnect cleanup now call `maybe_teardown_export(id)`, closing the leak for export-only entries (bare `BufferFromPixmap` / no GLX acquire) that sit at `glx_refs == 0` with no other teardown trigger; `glx_refs > 0` stays a no-op (defer to release). Teardown now converges from all three sites — `glXDestroyPixmap`, `FreePixmap`, disconnect (Tasks 2.3/2.4).
2. The lifetime test now proves RELEASE, not just retention: `acquire → export → FreePixmap (retained) → release (entry gone)`, plus a second test that an export-only entry is cleaned up at `FreePixmap`. Adds a `#[cfg(test)] has_export_entry` introspection helper (Task 2.4).

**Codex review round 5 (2026-06-09) folded in:**
1. `ensure_exported_entry` rewritten from `entry().or_insert_with(closure)` (which would borrow-conflict — the closure needs `&mut self` for the lifetime ref while `entry` holds `&mut self.exported_dmabufs`) to an explicit `contains_key` → `take_backing_lifetime_ref` → `insert` → `get_mut` sequence (Task 2.3).
