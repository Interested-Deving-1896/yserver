# Phase 4.2 — DRI3 + Present + GLX (design)

Status: design, awaiting implementation plan
Author: brainstormed 2026-05-09
Branch target: `dri`

## 1. Goals and non-goals

### Goal

Land the wire surface and KMS-side machinery for accelerated clients
on yserver: clients allocate dma-buf-backed buffers GPU-side, hand
them to the server via DRI3, and have them flipped to the screen via
Present. GLX framing on top so OpenGL apps using libGLX_mesa identify
DRI3 and proceed direct-rendering. Phase 4.1's per-window
`DrawableImage` mirrors and `ScanoutBoPool` are the substrate.

### In scope

- **DRI3 v1.4** wire surface on the KMS backend (the protocol has
  exactly one `Open`, with later versions adding new requests around
  it): `QueryVersion`, `Open` (v1.0), `PixmapFromBuffer` (v1.0),
  `BufferFromPixmap` (v1.0), `FenceFromFD` (v1.0), `FDFromFence`
  (v1.0), `GetSupportedModifiers` (v1.2), `PixmapFromBuffers` (v1.2),
  `BuffersFromPixmap` (v1.2), `SetDRMDeviceInUse` (v1.3,
  acknowledged-but-ignored — single-GPU), `ImportSyncobj` (v1.4),
  `FreeSyncobj` (v1.4).
- Client-allocated dma-buf import as a `DrawableImage` attached to a
  `PixmapState` — usable as a CopyArea / RENDER source unchanged.
  RGB single-plane only for Phase 4.2; multi-plane (YCbCr) deferred.
- Modifier negotiation backed by Vulkan format-modifier queries with a
  `LINEAR`-only fallback for hardware lacking
  `VK_EXT_image_drm_format_modifier`.
- **Present v1.4** wire surface: `QueryVersion`, `QueryCapabilities`,
  `SelectInput`, `NotifyMSC`, `PresentPixmap` (v1.0 — binary
  wait/idle fences), `PresentPixmapSynced` (v1.4 — timeline syncobj
  wait/idle with `wait_value` / `idle_value`), with all three
  presentation paths (Copy, Flip, Direct-scanout).
- Sync wiring: XSYNC `Fence` (binary) and `Syncobj` (timeline)
  resources both backed by `VkSemaphore` imports via
  `VK_KHR_external_semaphore_fd`. `Fence` uses `SYNC_FD` handle type
  (binary semaphore from a `sync_file` fd via `FenceFromFD`).
  `Syncobj` uses `OPAQUE_FD` handle type with
  `VK_KHR_timeline_semaphore` (timeline semaphore from a
  `DRM_SYNCOBJ` fd via `ImportSyncobj`).
- **GLX framing** sufficient for libGLX_mesa direct-rendering:
  identification (`QueryVersion`, `QueryServerString`,
  `QueryExtensionsString`), `SetClientInfoARB` /
  `SetClientInfo2ARB` (Mesa sends one of these on connect; without
  a handler init fails), FBConfig enumeration, context lifecycle,
  `IsDirect`, `WaitX`/`WaitGL`, `SwapBuffers`.

### Out of scope

- ynest backend support. ynest was a protocol-iteration tool; not a
  delivery target.
- Indirect GLX (server-side GL execution). Modern Mesa needs direct
  only.
- DRI3 v1.4 cross-GPU import semantics. `SetDRMDeviceInUse` is
  accepted on the wire but ignored — single-GPU only for Phase 4.2.
- Multi-plane (YCbCr / planar) image import. The
  `PixmapFromBuffers` `num_buffers > 1` case is wire-decoded but
  rejected with `BadAlloc`. Single-plane RGB only.
- Vulkan WSI optimisations beyond the three Present paths.
- Composite/cacomposite scheduler batching follow-up from Phase 4.1
  — separate work item.

## 2. Sub-phase decomposition

| Sub-phase | Scope | Smoke test |
|-----------|-------|------------|
| **4.2.1** | DRI3 v1.4 wire surface; dma-buf → `DrawableImage` import; modifier negotiation with LINEAR fallback. | Custom xcb test client: `PixmapFromBuffers` of an externally-rendered checkerboard, `xcb_copy_area` to a window, pixel readback matches. |
| **4.2.2** | Audit + fill XSYNC handlers; DRI3 `FenceFromFD`/`FDFromFence`; `ImportSyncobj`/`FreeSyncobj`. | Two-client fence trigger/await; observable signal via `Sync::AwaitFence`. |
| **4.2.3** | Present v1.4 wire surface; `PresentPixmap` Copy path only; `IdleNotify`/`CompleteNotify` events. | `vkcube` (FIFO mode) renders and flips at vsync via Copy path. |
| **4.2.4** | Present Flip path (alien-BO scanout) + Direct-scanout fast path. | `vkcube --present-mode mailbox`, fullscreen `vkcube` confirms direct-scanout via debug log. |
| **4.2.5** | GLX framing stubs for libGLX_mesa. | `glxinfo` reports vendor + extensions; `glxgears` runs at vsync. |

Each sub-phase ends commit-worthy and ships behind master before the
next starts.

## 3. Architecture

### 3.1 Component map

```
                        ┌──────────────────────────────────────────┐
client (vkcube/glxgears) │ DRI3 PixmapFromBuffers (fd, modifier, …) │
                        │ Present PresentPixmap (pixmap, win, …)   │
                        │ Sync FenceFromFD                         │
                        │ GLX QueryServerString, GetFBConfigs      │
                        └──────────────────┬───────────────────────┘
                                           │ X11 wire + SCM_RIGHTS fds
                                           ▼
                ┌──────────────────────────────────────────┐
                │ yserver-core dispatch (process_request)  │
                │  ├─ DRI3 dispatcher  (new, opcode TBD)   │
                │  ├─ Present dispatcher (existing stub)   │
                │  ├─ GLX dispatcher    (new, opcode TBD)  │
                │  └─ Sync dispatcher   (existing,         │
                │                        gaps filled)      │
                └──────────────────┬───────────────────────┘
                                   │ Backend trait
                                   ▼
                ┌──────────────────────────────────────────┐
                │ KmsBackend  (yserver/src/kms/)           │
                │  ├─ vk::dri3   import_dmabuf →           │
                │  │              DrawableImage            │
                │  ├─ vk::present scheduler +              │
                │  │              path selector            │
                │  ├─ vk::sync   Vk semaphore ↔ fd         │
                │  └─ ScanoutBoPool (Phase 4.1) +          │
                │                  alien-BO support        │
                └──────────────────────────────────────────┘
```

DRI3, Present, GLX dispatchers live in `yserver-core` and call
through the `Backend` trait into KMS-side helpers. The Sync extension
already has a stub dispatcher; 4.2.2 audits and extends it.

### 3.2 DRI3 buffer import

The load-bearing piece. Client passes fd(s), modifier, geometry; we
build a `VkImage`. The `pNext` slots in `VkImageCreateInfo` are a
chain — both structs link via their own `pNext` field, not as
parallel struct fields:

```rust
let plane_layouts: [VkSubresourceLayout; N] = client_planes.map(
    |p| VkSubresourceLayout {
        offset:     p.offset,
        size:       0,           // ignored on import
        rowPitch:   p.pitch,
        arrayPitch: 0, depthPitch: 0,
    });

let modifier_info = VkImageDrmFormatModifierExplicitCreateInfoEXT {
    sType:    DRM_FORMAT_MODIFIER_EXPLICIT_CREATE_INFO_EXT,
    pNext:    null,                       // chain head
    drmFormatModifier:           modifier,
    drmFormatModifierPlaneCount: plane_layouts.len(),
    pPlaneLayouts:               plane_layouts.as_ptr(),
};
let external_info = VkExternalMemoryImageCreateInfo {
    sType:       EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
    pNext:       &modifier_info,          // chain link
    handleTypes: VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
};
let image_info = VkImageCreateInfo {
    sType:     IMAGE_CREATE_INFO,
    pNext:     &external_info,            // chain root
    flags:     0,
    imageType: VK_IMAGE_TYPE_2D,
    format:    negotiated,
    extent:    { width, height, 1 },
    tiling:    VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT,  // LINEAR fallback otherwise
    usage:     SAMPLED | TRANSFER_SRC | TRANSFER_DST | COLOR_ATTACHMENT,
    sharingMode:           EXCLUSIVE,
    initialLayout:         UNDEFINED,
    …
};
```

Memory import:

```rust
let server_fd = libc::dup(client_fd);    // server now owns server_fd
let import_info = VkImportMemoryFdInfoKHR {
    sType:       IMPORT_MEMORY_FD_INFO_KHR,
    pNext:       null,
    handleType:  VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
    fd:          server_fd,
};
let alloc_info = VkMemoryAllocateInfo {
    sType: MEMORY_ALLOCATE_INFO,
    pNext: &import_info,
    …
};
match vkAllocateMemory(&alloc_info, &mut memory) {
    SUCCESS => { /* fd ownership transferred to the VkDeviceMemory */ }
    err     => { libc::close(server_fd); return Err(err); }
}
```

**fd ownership rule.** `vkAllocateMemory` (with
`VkImportMemoryFdInfoKHR`) and the equivalent
`vkImportSemaphoreFdKHR` only consume the fd *on success*. On any
error path between `dup` and a successful import, the server must
`close` the fd itself. The same rule applies to import failures
during VkImage/VkSemaphore creation in §3.4. fd-leak harness
catches violations.

`vkBindImageMemory` then `DrawableImage { image, view, extent,
format, modifier }` is attached to a fresh `PixmapState`. After a
successful bind, fd lifetime is owned by the `VkDeviceMemory`;
`FreePixmap` drops it. No side-channel tracking.

**Format-query / modifier negotiation.**
`vk::dri3::supported_modifiers(format) -> Vec<u64>` is computed once
at backend init. Both `VkPhysicalDeviceImageDrmFormatModifierInfoEXT`
and `VkPhysicalDeviceExternalImageFormatInfo` chain as direct
*siblings* under `VkPhysicalDeviceImageFormatInfo2` (not nested
through each other). For each candidate modifier:

```rust
let modifier_info = VkPhysicalDeviceImageDrmFormatModifierInfoEXT {
    sType:             …DRM_FORMAT_MODIFIER_INFO_EXT,
    pNext:             null,                           // chain head
    drmFormatModifier: modifier,
    sharingMode:       EXCLUSIVE,
    queueFamilyIndexCount: 0,
    pQueueFamilyIndices:   null,
};
let external_info = VkPhysicalDeviceExternalImageFormatInfo {
    sType:      …EXTERNAL_IMAGE_FORMAT_INFO,
    pNext:      &modifier_info,                        // chain link
    handleType: DMA_BUF_BIT_EXT,
};
let format_info = VkPhysicalDeviceImageFormatInfo2 {
    sType:    …IMAGE_FORMAT_INFO_2,
    pNext:    &external_info,                          // chain root
    format,
    type_:    VK_IMAGE_TYPE_2D,
    tiling:   VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT,
    usage:    SAMPLED | TRANSFER_SRC | TRANSFER_DST | COLOR_ATTACHMENT,
    flags:    0,
};

let mut external_props = VkExternalImageFormatProperties { … };
let mut props2 = VkImageFormatProperties2 {
    pNext: &mut external_props, …
};
vkGetPhysicalDeviceImageFormatProperties2(phys, &format_info, &mut props2)
```

We keep modifiers for which the call returns `SUCCESS` and
`external_props.externalMemoryProperties.compatibleHandleTypes`
includes `DMA_BUF_BIT_EXT`. Empty extension support →
`[DRM_FORMAT_MOD_LINEAR]`. Note `DRM_FORMAT_MOD_INVALID` is a Mesa
client-side sentinel (allocate without an explicit modifier); it's
never sent on the wire.

`GetSupportedModifiers` carries both a `window` and a `screen`
parameter — keep the per-window vs. per-screen distinction even when
single-GPU makes the lists equal. The server-modifier list (returned
when `window` is the screen) is the modifier set above; the
window-modifier list (when `window` is a real window) further
restricts to what the window's output can flip-scanout via the
`output.scanout_format_set` from §3.3.1.

**Multi-plane scope.** `PixmapFromBuffers` carries `num_buffers ∈
{1, 2, 3, 4}` plus per-plane `stride` and `offset` arrays. Phase 4.2
implements `num_buffers == 1` (RGB single-plane). `num_buffers > 1`
is wire-parsed but rejected with `BadAlloc`. YCbCr/planar via
multi-plane is a follow-up; the explicit-modifier struct already
takes plane-layout arrays so the extension is mechanical.

`KmsBackend::render_node_fd: OwnedFd` — opened at backend init via
sysfs walk (`/sys/dev/char/<major>:<minor>/device/drm/renderD*`),
falling back to a libdrm linkage if the sysfs path isn't available.
Dup'd per `Open` call to a render-node sibling of the scanout fd.

### 3.3 Present scheduler

#### 3.3.1 Path selection

```rust
enum PresentPath {
    Copy,
    Flip { alien_bo: AlienBo },
    DirectScanout { alien_bo: AlienBo },
}

fn choose_path(req: &PresentPixmap, pixmap: &PixmapState,
               window: &WindowState, output: &OutputState) -> PresentPath {
    if req.options.contains(PresentOptionCopy) {
        return Copy;
    }
    let scanout_compat = is_scanout_compatible(
        pixmap.format, pixmap.modifier, output.scanout_format_set);
    let fullscreen_exact = pixmap.size == output.mode_size
        && window.covers_exactly(output)
        && req.valid_region.is_full_window(window)
        && req.update_region.is_full_window(window)
        && scanout_compat;
    match (fullscreen_exact, scanout_compat) {
        (true,  _   ) => DirectScanout { alien_bo: …},
        (false, true) => Flip          { alien_bo: …},
        (false, false) => Copy,
    }
}
```

`output.scanout_format_set` is the `(format, modifier)` set the
kernel accepted via `add_fb2` at backend init — same set Phase 4.1
already filters against.

When `PresentCaps::flip_path == false` (kernel lacks
`IN_FENCE_FD` / `OUT_FENCE_PTR` on the target output), the
selector short-circuits to `Copy` regardless of the other inputs —
the explicit-fence flip handshake is required for client-imported
BO scanout and there's no degraded variant.

#### 3.3.2 Per-path mechanics

**Copy.** The `composite_pass_record` from Phase 4.1 already samples
each window's `DrawableImage` mirror. The imported pixmap is just a
mirror; PresentPixmap on `Copy` path queues a per-window override
that "use this imported pixmap as the window's content for one
frame". Per `presentproto`, a Copy makes the pixmap idle *as soon as
the operation occurs* — no later than when the GPU has finished
reading the source. We fire `IdleNotify` (and signal `idle_fence` /
`idle_syncobj` if present) at GPU-read-completion, signalled via a
per-frame timeline-semaphore value — *not* gated on flip completion.
`CompleteNotify { mode: Copy }` fires on pageflip-complete of the
composite frame.

**Flip.** Alien BO joins `ScanoutBoPool` with `is_alien: true`. We
record a transfer pass that *writes into* the alien BO — composing
cursor + any decoration on top of the client's content. Atomic-flip
to the alien BO. `CompleteNotify { mode: Flip }` fires on
pageflip-complete.

Per `presentproto`, a flipped pixmap stays in use *until a later
Present on the same window completes*. So when PresentPixmap A
flips alien BO A, and a subsequent PresentPixmap B (any path) later
completes on the same window, *only then* does A's `IdleNotify`
fire and the import release to the client. `KmsBackend::present`
keeps a per-window `last_flipped: Option<AlienBoHandle>`; on the
next CompleteNotify for that window, the previous handle retires.

**Direct-scanout.** Alien BO becomes the scanout image directly; no
composite pass; cursor renders on a separate cursor plane (already
exists per Phase 6.10). Atomic-flip. Same `last_flipped` retire
chain as Flip.

**Teardown rules.** When no subsequent Present arrives, the
retained alien BO must still retire eventually:

| Trigger | Action |
|---|---|
| `DestroyWindow` on the destination window | Drain the per-window FIFO, signal `idle_fence`/`idle_syncobj` for each queued frame, retire `last_flipped` immediately. No `IdleNotify` event because the window is gone. Imports released. |
| Source pixmap `FreePixmap` while in use | Standard X resource lifetime: pixmap reference is decremented but the resource lives until the Present-side reference drops on retire. dma-buf fd stays alive (owned by `VkDeviceMemory`). |
| Client disconnect | Same as DestroyWindow for every window owned by the client; plus drain Presents this client queued against *other* clients' windows (signal idle and retire). |
| Output mode change / unplug | `output.scanout_format_set` re-evaluated; if `last_flipped`'s format/modifier no longer fits, force a fallback Copy frame onto a pool BO so the client BO retires. The flip itself can't unflip cleanly without a follow-up frame. |
| Vulkan device lost | Fatal — yserver restarts. Imports leak at the OS level; kernel cleans up on process exit. |

`last_flipped: Option<AlienBoHandle>` is per-window state that
`process_destroy_window` must walk and retire as part of the
cleanup pass.

#### 3.3.3 Scheduler

Per-window FIFO of queued PresentPixmaps. At each output's vblank
event:

1. Compute `next_msc` from `target_msc / divisor / remainder` for
   each queued frame.
2. Pick the latest frame whose schedule is satisfied; mark earlier
   queued frames for the same window as `Skip` and emit
   `CompleteNotify { mode: Skip }` for them.
3. Submit chosen frame's path; record submission.
4. `Async` frames bypass the queue and submit immediately.
5. `AsyncMayTear` allowed via `DRM_MODE_ATOMIC_NONBLOCK` if the
   kernel reports the capability.

Occluded / unmapped windows: PresentPixmap demoted to `Copy` path so
the composite pass naturally produces no observable change but the
buffer-lifecycle (IdleNotify) still completes correctly.

### 3.4 Sync wiring

Both XSync resource types are backed by `VkSemaphore` (not `VkFence`
— `VK_KHR_external_fence_fd` is for `VkFence` and is not what DRI3 /
Mesa use here). The two import paths differ in handle type and
semaphore type:

- **`Sync::Fence`** — single-shot, binary. `FenceFromFD` takes a
  `sync_file` fd. We import it via `vkImportSemaphoreFdKHR` with
  `handleType = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT` into
  a binary `VkSemaphore` (no `VkSemaphoreTypeCreateInfo`). XSync
  `TriggerFence` schedules a signal on the GPU queue;
  `AwaitFence` schedules a wait. PresentPixmap (v1.0) `wait_fence`
  / `idle_fence` plumb directly into `pWaitSemaphores` /
  `pSignalSemaphores` of the per-frame submit.
- **`Sync::Syncobj`** — DRI3 v1.4. `ImportSyncobj` takes a
  `DRM_SYNCOBJ` fd. We import via `vkImportSemaphoreFdKHR` with
  `handleType = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_OPAQUE_FD_BIT`
  into a timeline `VkSemaphore` created with
  `VkSemaphoreTypeCreateInfo { semaphoreType: TIMELINE }`
  (requires `VK_KHR_timeline_semaphore`). PresentPixmapSynced (v1.4)
  `wait_value` / `idle_value` are 64-bit timeline values fed into
  `VkTimelineSemaphoreSubmitInfo` on the submit.

fd ownership rule from §3.2 applies — `vkImportSemaphoreFdKHR`
consumes the fd only on `SUCCESS`; close the dup'd fd on any other
return.

DRI3 `FenceFromFD` and `ImportSyncobj` allocate XIDs and stash the
imported semaphore on `KmsBackend::sync_resources`. PresentPixmap
and PresentPixmapSynced wait/idle parameters look up by XID and
feed into the per-frame submission.

**Export path (`FDFromFence`).** Symmetric to `FenceFromFD`. Given
an XSync `Fence` XID, look up the backing `VkSemaphore` and call
`vkGetSemaphoreFdKHR { semaphore, handleType: SYNC_FD_BIT_KHR }`,
returning the fd to the client. Per Vulkan spec the returned fd
ownership transfers to the caller (server → client), so it goes
out via the SCM_RIGHTS reply path used by MIT-SHM `CreateSegment`.
Syncobj export is not part of DRI3's request set — clients keep
their own DRM_SYNCOBJ fd.

### 3.5 GLX framing

GLX server-side scope is *identification + bookkeeping*. libGLX_mesa
loads, hits these requests, gets enough metadata to identify our
DRI3 backend, then renders client-side direct-to-GPU.

| Request | Behaviour |
|---|---|
| `QueryVersion` | Reply `(1, 4)`. |
| `QueryServerString` | Vendor: `"yserver"`; version: `"1.4"`; extensions: empty. |
| `QueryExtensionsString` | List of GLX extensions Mesa probes for: `GLX_ARB_create_context`, `GLX_ARB_create_context_profile`, `GLX_EXT_create_context_es2_profile`, `GLX_EXT_buffer_age`, `GLX_EXT_swap_control`, `GLX_INTEL_swap_event`, `GLX_ARB_fbconfig_float`, `GLX_EXT_visual_info`, `GLX_EXT_visual_rating`, `GLX_EXT_import_context`. |
| `ClientInfo` (legacy, GLX 1.1) / `SetClientInfoARB` / `SetClientInfo2ARB` | Mesa sends `SetClientInfoARB` or `SetClientInfo2ARB` on connect carrying client GL/GLX/GLES versions and extension strings; older clients use the legacy `ClientInfo`. We parse + log + drop on the floor (server-side state we don't act on). Without a handler libGLX_mesa fails init. Opcodes per `glxproto.h` — implementation looks them up there rather than carrying numbers in the design. |
| `GetFBConfigs` / `GetVisualConfigs` | Synthesise from each X visual × `(double_buffered, depth_size, stencil_size, sample_count)` combos (~30 configs). |
| `CreateNewContext` / `CreateContextAttribsARB` | Track `(context_xid, share_xid, screen, fbconfig, attribs)` in a `GlxContext` resource. We never execute GL. Both are normal GLX requests in glvnd's dispatch path; opcodes per `glxproto.h`. |
| `DestroyContext` | Free resource. |
| `MakeCurrent` / `MakeContextCurrent` | Bookkeeping — record `(context, drawable)` pair on the client. |
| `IsDirect` | Always `true`. |
| `WaitX` / `WaitGL` | No-op for direct contexts. |
| `SwapBuffers` | Forward as a degraded `PresentPixmap` for legacy clients; modern Mesa never calls this. |

Indirect-mode rendering opcodes (1–198) → `GLXBadRequest` stubs.

**Vendor-private dispatch.** GLX has a `VendorPrivate` /
`VendorPrivateWithReply` forwarding mechanism (opcodes 16, 17 per
`glxproto.h`) that some legacy clients use to reach driver-specific
hooks. yserver responds with `GLXUnsupportedPrivateRequest` for
every vendor-code; modern Mesa direct-rendering doesn't go this
route, so the stub suffices.

## 4. Hardware fallback matrix

| Missing capability | Behaviour |
|---|---|
| `VK_EXT_image_drm_format_modifier` | `GetSupportedModifiers` returns `[LINEAR]` only. Mesa allocates LINEAR; correctness preserved, perf degraded. |
| `VK_KHR_external_memory_fd` | DRI3 filtered out of `EXTENSIONS` (same `availability:` machinery as RENDER/XKB). Clients fall back to MIT-SHM. |
| `VK_KHR_external_semaphore_fd` (`SYNC_FD` handle type) | `FenceFromFD`/`FDFromFence` filtered from DRI3. Clients fall back to roundtrip sync. |
| `VK_KHR_external_semaphore_fd` (`OPAQUE_FD`) or `VK_KHR_timeline_semaphore` | Advertise DRI3 ≤ 1.3; drop syncobj surface; do not advertise `PresentCapabilitySyncobj`. |
| Kernel lacks `DRM_SYNCOBJ` ioctls | Same as above. |
| Kernel lacks atomic modeset (`DRM_CLIENT_CAP_ATOMIC` returns 0) | Phase 4.1 already requires this — yserver fails to start. Documented here for completeness; not a runtime fallback. |
| KMS plane lacks `IN_FENCE_FD` property OR CRTC lacks `OUT_FENCE_PTR` | Flip / DirectScanout disabled at backend init. Present scheduler always picks `Copy`. Phase 4.1's scanout pool keeps working because it owns the explicit-fence handshake; we degrade DRI3-flip path only. |
| Kernel rejects `DRM_MODE_ATOMIC_NONBLOCK` | Don't advertise `PresentCapabilityAsyncMayTear` in `QueryCapabilities`. Per `presentproto`: when the cap is not advertised, the `PresentOptionAsyncMayTear` bit in incoming PresentPixmap requests is **silently ignored** — the option is treated as if unset, *not* downgraded to `PresentOptionAsync`. (Async and AsyncMayTear are distinct options; one means "submit immediately, no tearing"; the other means "submit immediately, tearing allowed". Folding one into the other changes observable behavior.) |

Capability detection happens at `KmsBackend::open`. Two surfaces,
since DRI3 import support and Present scheduling capabilities are
queried independently by clients:

```rust
struct Dri3Caps {
    version:    (u32, u32),  // negotiated max DRI3 version we can serve
    modifiers:  bool,        // VK_EXT_image_drm_format_modifier
    fence_fd:   bool,        // VK_KHR_external_semaphore_fd SYNC_FD handle type
    syncobj:    bool,        // VK_KHR_external_semaphore_fd OPAQUE_FD
                             // + VK_KHR_timeline_semaphore + DRM_SYNCOBJ ioctls
}

struct PresentCaps {
    flip_path:      bool,    // IN_FENCE_FD + OUT_FENCE_PTR present on plane/CRTC
    async_may_tear: bool,    // DRM_MODE_ATOMIC_NONBLOCK accepted by kernel
    syncobj:        bool,    // mirrors Dri3Caps::syncobj — Present syncobj cap
                             // requires DRI3 syncobj support to be useful
}
```

`Backend::dri3_capabilities() -> Dri3Caps` and
`Backend::present_capabilities(window) -> PresentCaps` (the latter
window-keyed because Present's `QueryCapabilities` is per-window).
The DRI3 extension-availability filter consults `Dri3Caps`; the
Present `QueryCapabilities` reply assembles a u32 from
`PresentCaps`.

## 5. Testing strategy

### 5.1 Unit tests

- DRI3 wire decoders in `yserver-protocol::x11::dri3` — table-driven
  parse tests for every request, including `PixmapFromBuffers` with
  `num_buffers ∈ {1, 2, 3, 4}` and `SetDRMDeviceInUse`.
- Present wire decoders in `yserver-protocol::x11::present` —
  `PresentPixmap` (binary fences) and `PresentPixmapSynced`
  (timeline syncobj + wait/idle values), full option/notifies
  surface.
- GLX wire decoders, including `SetClientInfoARB` /
  `SetClientInfo2ARB`.
- Path selector (`choose_path`) — synthetic `(pixmap, window,
  output, options)` triples covering each of Copy/Flip/DirectScanout
  decisions.
- Modifier negotiation logic — given a fake `vkGetImageFormat
  Properties2` responder, check that `supported_modifiers` filters
  correctly.

### 5.2 Integration tests in vng

- DRI3 import smoke: a custom `xcb` client allocates a buffer (via
  `gbm_bo_create` against the render-node fd), draws a checkerboard,
  hands fd to server via `PixmapFromBuffers`, `xcb_copy_area`s to a
  window, server reads back via `xcb_get_image`; pixels match.
- DRI3 `GetSupportedModifiers` per-window vs. per-screen: confirm
  the wire surface preserves the distinction (window-mods is a
  subset of screen-mods, even if equal in single-GPU).
- Sync smoke: client creates a `SYNC_FILE` fence fd, registers via
  `FenceFromFD`, server triggers via `Sync::TriggerFence`; client
  waits and observes the signal.
- Sync round-trip: `FenceFromFD` (import) followed by `FDFromFence`
  (export); the exported fd must signal in lockstep with the
  imported one. Catches `vkGetSemaphoreFdKHR` SCM_RIGHTS reply path.
- Syncobj smoke: same shape, DRM_SYNCOBJ + timeline value.
- Teardown: client imports a pixmap, queues a PresentPixmap that
  takes the Flip path, then either calls `DestroyWindow` or
  disconnects before the next Present. fd count returns to baseline
  and no `IdleNotify` is delivered after the disconnect.
- AsyncMayTear-ignored: on a backend where `PresentCaps::async_
  may_tear == false`, a client PresentPixmap with
  `PresentOptionAsyncMayTear` set must execute as if the bit were
  clear — verified by observing pageflip-complete is vsync-locked,
  not torn (CompleteNotify timestamps).

### 5.3 End-to-end

- `vkcube` under FIFO / Mailbox / Immediate present modes (4.2.3
  Copy path; 4.2.4 Flip + DirectScanout).
- `glxinfo` reports `direct rendering: Yes`, vendor `yserver`,
  extension list (4.2.5).
- `glxgears` runs at vsync (4.2.5).

### 5.4 fd-leak harness

Stress test creates + destroys 10k DRI3 pixmaps in a loop and
asserts `/proc/self/fd` count stays bounded.

### 5.5 Hardware coverage

Run the smoke + integration matrix on:

- Mesa Venus passthrough in vng (lavapipe baseline lacks DRM
  modifiers — exercises the LINEAR fallback path).
- Bare-metal AMD Radeon RX 580 (Polaris): no
  `VK_EXT_image_drm_format_modifier` — full LINEAR fallback path.
- Bare-metal AMD Radeon 680M (Rembrandt): full
  modifier + syncobj path.

## 6. Open questions

- **`BuffersFromPixmap` scope.** Spec'd but rarely used; the only
  realistic consumer is screen recorders. Implement minimally
  (LINEAR / single-plane re-export) at 4.2.1 or stub `BadAlloc`?
- **Cursor on Direct-scanout.** Cursor plane usage is already in
  Phase 6.10 multi-monitor; need to confirm it composes with an
  alien BO on the primary plane. Smoke gate at 4.2.4.
- **PresentPixmap concurrency on a shared window.** Two clients
  Present-ing to the same window: spec is silent on which idle-fence
  fires first. Conservative answer: serialise per-window queue and
  retire imports in submission order. Confirm with Mesa under a
  WM-redirected window.

## 7. Review history

- 2026-05-09 — Codex review (`019e0c29-472f-7fb0-8cb7-15c17b57c387`).
  Folded in: corrected DRI3 v1.4 request set (no `OpenV2`/`OpenV3`,
  added `SetDRMDeviceInUse`); split Present syncobj surface into
  `PresentPixmapSynced`; corrected Vulkan sync mapping
  (`VK_KHR_external_semaphore_fd` for both, with handle-type split);
  narrowed scope to RGB single-plane; rewrote Present buffer
  lifetime to match `presentproto` (Copy idle on GPU-read-complete,
  Flip retains until next-Present-completes); added kernel-side
  capability gate for atomic / `IN_FENCE_FD` / `OUT_FENCE_PTR`;
  added `SetClientInfoARB` / `SetClientInfo2ARB` to GLX surface;
  fixed `VkImageCreateInfo` chain notation + format-query path;
  spelled out fd ownership rule on import-error paths.

- 2026-05-09 — Codex re-review. Of the 9 first-pass corrections, 7
  verified clean; 2 partial (sync mapping missing FDFromFence
  export; lifetime missing teardown). Additional findings folded
  in: the format-query pNext chain was still wrong (siblings under
  `VkPhysicalDeviceImageFormatInfo2`, not nested); AsyncMayTear
  fallback semantics rewritten (the option is **silently ignored**
  when the cap isn't advertised, not downgraded to `Async` —
  `presentproto` distinguishes the two); flip-retention teardown
  rules added for DestroyWindow / FreePixmap / client disconnect /
  output mode change / device-lost; capability surface split into
  `Dri3Caps` and `PresentCaps` (Present caps are per-window);
  `FDFromFence` export path spelled out via `vkGetSemaphoreFdKHR`;
  GLX section corrected — opcode numbers deferred to `glxproto.h`,
  legacy `ClientInfo` request added, vendor-private dispatch path
  noted; per-window vs. per-screen `GetSupportedModifiers` test
  added; round-trip + teardown + AsyncMayTear-ignore tests added
  to integration suite.
