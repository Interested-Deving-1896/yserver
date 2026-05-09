# Phase 4.2 — DRI3 + Present + GLX Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Land the DRI3 v1.4, Present v1.4, and GLX wire surfaces on the KMS
backend so dma-buf clients (vkcube, glxgears, glxinfo) can run direct-rendered
on yserver, with all three Present paths (Copy, Flip, Direct-scanout).

**Architecture:** New per-extension dispatchers in `yserver-core` call through
`Backend` trait into KMS-side helpers in `yserver/src/kms/vk/{dri3,present,sync}.rs`.
DRI3 imports dma-buf fds → `DrawableImage` and attaches to a `PixmapState`.
Present's path selector picks Copy / Flip / Direct-scanout against a
`(format, modifier)` scanout-compat set. XSync gains binary fences and
DRI3 gains timeline syncobjs, both backed by `VkSemaphore` external imports.
GLX is identification + bookkeeping only (no server-side GL).

**Tech Stack:** Rust 2024 edition; `ash` (Vulkan); raw libc for `SCM_RIGHTS` fd
passing; existing yserver wire-protocol crate (`yserver-protocol`); Phase 4.1
substrate (`DrawableImage`, `ScanoutBoPool`); KMS atomic modeset already
established in Phase 6.

**Reference design:** `docs/superpowers/specs/2026-05-09-phase4-2-dri3-present-glx-design.md`.

**Branch:** `dri` (already checked out). One commit per task in this plan.
At the end of each sub-phase: squash via PR or stack of fixups (see
@superpowers:finishing-a-development-branch for the choice).

---

## Pre-flight

**Working directory:** `/home/jos/Projects/yserver`. Already on branch `dri`.

**Style gates (run before every commit):**

```bash
cargo +nightly fmt
cargo clippy --workspace -- -D warnings
cargo test --workspace
```

**Visible smoke (sub-phase 4.2.3+):** vng + virtio-gpu Venus passthrough per
the `reference_vng_vulkan_venus.md` memory. The lavapipe baseline has no
modifiers — exercise the LINEAR fallback there.

**Skill cross-references applied throughout:**
- @superpowers:test-driven-development — every behavior change starts with a
  failing test.
- @superpowers:systematic-debugging — when smoke fails, root-cause before
  patching symptoms.
- @superpowers:verification-before-completion — never claim a task complete
  without `cargo test` + (where applicable) the visible smoke.

---

## Sub-phase 4.2.1 — DRI3 wire surface + dma-buf import

**Outcome:** Custom xcb test client `PixmapFromBuffers` of an
externally-rendered checkerboard, `xcb_copy_area` to a window, pixel readback
matches.

### Task 1: DRI3 wire-protocol skeleton (request opcodes + parse stubs)

**Files:**
- Create: `crates/yserver-protocol/src/x11/dri3.rs`
- Modify: `crates/yserver-protocol/src/x11/mod.rs` (add `pub mod dri3;`)

**Step 1: Write failing test for opcode constants and `parse_query_version`**

```rust
// crates/yserver-protocol/src/x11/dri3.rs
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn opcodes_match_dri3proto() {
        assert_eq!(QUERY_VERSION, 0);
        assert_eq!(OPEN, 1);
        assert_eq!(PIXMAP_FROM_BUFFER, 2);
        assert_eq!(BUFFER_FROM_PIXMAP, 3);
        assert_eq!(FENCE_FROM_FD, 4);
        assert_eq!(FD_FROM_FENCE, 5);
        assert_eq!(GET_SUPPORTED_MODIFIERS, 6);
        assert_eq!(PIXMAP_FROM_BUFFERS, 7);
        assert_eq!(BUFFERS_FROM_PIXMAP, 8);
        assert_eq!(SET_DRM_DEVICE_IN_USE, 9);
        assert_eq!(IMPORT_SYNCOBJ, 10);
        assert_eq!(FREE_SYNCOBJ, 11);
    }

    #[test]
    fn query_version_parses_minor() {
        let mut body = vec![0u8; 8];
        body[0..4].copy_from_slice(&1u32.to_le_bytes());
        body[4..8].copy_from_slice(&4u32.to_le_bytes());
        assert_eq!(parse_query_version(&body), Some((1, 4)));
    }
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p yserver-protocol dri3::tests`
Expected: FAIL with "unresolved import `super::*`" / module not found.

**Step 3: Write minimal implementation**

Constants (`u8` opcodes, `MAJOR_VERSION = 1`, `MINOR_VERSION = 4`),
`parse_query_version(body) -> Option<(u32, u32)>` mirroring
`x11/present.rs:74-79`.

**Step 4: Test passes**

Run: `cargo test -p yserver-protocol dri3::tests`
Expected: PASS.

**Step 5: Commit**

```bash
git add crates/yserver-protocol/src/x11/dri3.rs crates/yserver-protocol/src/x11/mod.rs
git commit -m "feat(dri3): add wire-protocol module skeleton"
```

### Task 2: Wire-decode `Open`, `PixmapFromBuffer`, `PixmapFromBuffers`

**Files:**
- Modify: `crates/yserver-protocol/src/x11/dri3.rs`

**Step 1: Failing tests** for each request struct, exercising:
- `Open { drawable: u32, provider: u32 }` (8-byte body).
- `PixmapFromBuffer { pixmap, drawable, size, width, height, stride, depth, bpp }`
  (24-byte body; fd arrives via `SCM_RIGHTS`).
- `PixmapFromBuffers { pixmap, window, num_buffers ∈ 1..=4, modifier_hi,
  modifier_lo, width, height, stride[4], offset[4], depth, bpp }`
  (sized 56 bytes; `num_buffers` clamps which `stride[i]/offset[i]` are read).

Confirm rejection on: too-short body, `num_buffers` outside `1..=4`.

**Step 2-4: Implement parsers**, copying the pattern from `present.rs:81-126`.
Modifier is two `u32`s (hi/lo) packed; combine into `u64` (DRI3 wire is
big-endian on the wire... actually little-endian per X11 client byte-order;
match `present.rs` convention with `read_u32_le` and assemble
`(hi as u64) << 32 | lo as u64`).

**Step 5: Commit**

```bash
git commit -m "feat(dri3): parse Open / PixmapFromBuffer(s)"
```

### Task 3: Wire-decode remaining DRI3 requests

**Files:**
- Modify: `crates/yserver-protocol/src/x11/dri3.rs`

Cover: `BufferFromPixmap`, `FenceFromFD` (fd-attached),
`FDFromFence`, `GetSupportedModifiers`, `BuffersFromPixmap`,
`SetDRMDeviceInUse`, `ImportSyncobj` (fd-attached), `FreeSyncobj`.

Tests: minimum-size validation + happy path for each.

```bash
git commit -m "feat(dri3): parse remaining v1.4 requests"
```

### Task 4: Encode DRI3 replies

**Files:**
- Modify: `crates/yserver-protocol/src/x11/dri3.rs`

Replies needed:
- `QueryVersion` → `(u32 major, u32 minor)`.
- `Open` → `(u32 nfd=1, padding)`; fd via `SCM_RIGHTS`.
- `BufferFromPixmap` → `(u32 size, u16 width, u16 height, u16 stride, u8
  depth, u8 bpp)`; fd via `SCM_RIGHTS`.
- `BuffersFromPixmap` → `(u8 nfd, u16 width, u16 height, u32[4] strides,
  u32[4] offsets, u64 modifier, u8 depth, u8 bpp)`; fds via `SCM_RIGHTS`.
- `GetSupportedModifiers` → window-modifiers + screen-modifiers as
  `(u32 num_window_mods, u32 num_screen_mods, u64[..] mods)`.
- `FDFromFence` → `(u32 nfd=1)`; fd via `SCM_RIGHTS`.

Tests: encoded length, header bytes, sequence number placement match
`xproto.h` reply-shape conventions used elsewhere in the file.

```bash
git commit -m "feat(dri3): encode replies"
```

### Task 5: Register `DRI3` extension and dispatcher (no-op handlers)

**Files:**
- Modify: `crates/yserver-core/src/nested.rs`:
  - Add the new `Dri3` variant to `ExtensionAvailability` (file already
    enumerates `Always`, `HostRender`, `HostXkb` — copy the pattern).
  - Add `ExtensionMetadata { name: "DRI3", … availability:
    ExtensionAvailability::Dri3 }` to `EXTENSIONS`.
  - Allocate major-opcode constant `DRI3_MAJOR_OPCODE: u8 = 147` (next
    to the existing `PRESENT_MAJOR_OPCODE: u8 = 145` and
    `XTEST_MAJOR_OPCODE: u8 = 146`). 147 is the next free slot in
    yserver's local opcode space — the upstream X.Org assignment is
    143 but yserver already uses 143 for DAMAGE, so opcode numbers
    are local rather than mirrored from X.Org.
- Modify: `crates/yserver-core/src/core_loop/process_request.rs`:
  - **Patch the existing `match ext.availability` arms in
    `extension_query_reply` (line 4131) and `advertised_extension_names`
    (line 4151)** to handle `ExtensionAvailability::Dri3`. For Task 5
    only, treat `Dri3 => true` (always-available); Task 11 will narrow
    this to `backend.dri3_capabilities().version != (0, 0)`. Without
    this patch the new variant is non-exhaustive and the crate
    won't compile.
  - Add `handle_dri3_request` mirroring `handle_present_request:3063-3258`.
  - Add a `147 => handle_dri3_request(...)` arm next to the existing
    `145 => handle_present_request(...)` route.
- Create: `crates/yserver-core/src/dri3.rs` (analogous to `randr.rs` —
  state container for DRI3-side bookkeeping; empty for now).
- Modify: `crates/yserver-core/src/lib.rs` — `pub(crate) mod dri3;`.

For this task only: every minor returns `Handled` after a `debug!()` log;
`QueryVersion` returns the encoded reply; everything else logs and
swallows. The behavior of the wire surface is verified by Task 6+.

**Step 1: Failing test** — `cargo test extension_query_dri3` in
`process_request` test module: send a `QueryExtension { name: "DRI3" }`
and assert `present == 1`, `major_opcode == 147`.

**Step 2-4: Implement**

**Step 5: Commit**

```bash
git commit -m "feat(dri3): register extension + skeleton dispatcher"
```

### Task 6: Render-node fd inventory at backend init

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs` — `KmsBackend::open` adds a
  `render_node_fd: OwnedFd` field, populated by walking
  `/sys/dev/char/<major>:<minor>/device/drm/renderD*`.

**Step 1: Failing unit test** for a small helper
`fn render_node_path_for(card_major_minor: (u32, u32)) -> io::Result<PathBuf>`.
Mock the sysfs root via a temp dir.

**Step 2-4: Implement.** Per design §3.2: if the sysfs walk fails, fall
back to libdrm enumeration via `drmGetDevices2` (the `drm` crate already
in use exposes this) and pick the render node sibling of the scanout
device's `device_node_path`. **Do not** hardcode `/dev/dri/renderD128`
— a multi-GPU host or a different bus order routes that to the wrong
device; the design calls out the libdrm fallback specifically because
of this.

**Step 5: Commit**

```bash
git commit -m "feat(kms): inventory render-node fd at backend init"
```

### Task 7: Backend trait — `dri3_open` returning a render-node fd

**Files:**
- Modify: `crates/yserver-core/src/backend/trait_def.rs` — add
  `fn dri3_open(&mut self, drawable: u32) -> io::Result<OwnedFd>;`
  Default impl: `Err(io::Error::other("DRI3 unsupported"))`. KmsBackend
  overrides; HostX11Backend keeps the default. The drawable is currently
  unused (single-GPU); document with a `// _drawable: ` rename so clippy
  is happy.
- Modify: `crates/yserver/src/kms/backend.rs` — implement by
  `dup`-ing `self.render_node_fd`. **fd ownership rule**: ownership of
  the dup'd fd transfers to the caller (the dispatcher will hand it to
  the client via `SCM_RIGHTS`).

**Step 1: Failing test** in `kms::backend::tests` (or a new
`tests/dri3_open.rs` integration test) — assert two successive
`dri3_open` calls return distinct fds and both refer to a render node
(`fstat` major matches the DRM major, dev minor ≥ 128).

**Step 2-4: Implement.**

**Step 5: Commit**

```bash
git commit -m "feat(kms): Backend::dri3_open dup's render-node fd"
```

### Task 8: DRI3 dispatcher — `Open` / `QueryVersion`

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request.rs`

`QueryVersion`: reply with the negotiated min of client and server
versions per `Backend::dri3_capabilities()` (added later — for now hard-code
`(1, 4)`). Negotiate but don't store on the client.

`Open`: call `backend.dri3_open(drawable)`. On `Err`, emit `BadAlloc`. On
`Ok(fd)`, build the `Open` reply (nfd=1) and use `send_reply_with_fd`
(see `process_request.rs:2191`) to dispatch the bytes + fd as one
SCM_RIGHTS frame.

**Step 1: Failing integration test** under `tests/` — open a Unix-socket
client, run `QueryExtension/QueryVersion/Open`, confirm an fd arrived and
its `fstat` is a render-node character device.

**Step 2-4: Implement.**

**Step 5: Commit**

```bash
git commit -m "feat(dri3): Open + QueryVersion dispatch"
```

### Task 9: KMS-side modifier query helper

**Files:**
- Create: `crates/yserver/src/kms/vk/dri3.rs`
- Modify: `crates/yserver/src/kms/vk/mod.rs` — `pub mod dri3;`

`pub fn supported_modifiers(vk: &VkContext, format: vk::Format) -> Vec<u64>`
implementing the §3.2 algorithm: enumerate via
`vkGetPhysicalDeviceImageFormatProperties2` with
`VkPhysicalDeviceImageDrmFormatModifierInfoEXT` and
`VkPhysicalDeviceExternalImageFormatInfo` chained as **siblings** under
`VkPhysicalDeviceImageFormatInfo2`.

For the candidate set: query
`VkDrmFormatModifierPropertiesListEXT` with a one-shot
`vkGetPhysicalDeviceFormatProperties2` to get all DRM modifiers the GPU
supports for the format, then filter via the per-modifier
`vkGetPhysicalDeviceImageFormatProperties2` call against
`compatibleHandleTypes & DMA_BUF_BIT_EXT`. If
`VK_EXT_image_drm_format_modifier` is unsupported, return
`vec![DRM_FORMAT_MOD_LINEAR]` (`= 0`).

**Step 1: Failing unit test** with a mock `VkContext` that records the
chained pNext slots and confirms the chain shape (header → external_info
→ modifier_info as siblings? **No** — re-read §3.2: under
`VkPhysicalDeviceImageFormatInfo2`, the `external_info` and
`modifier_info` chain as siblings. The chain is
`format_info.pNext = &external_info; external_info.pNext = &modifier_info`
— so they are linked but read as a flat list by the driver. Test
asserts both structs reachable from the format_info pNext walk).

**Step 2-4: Implement.** This is the trickiest unsafe in the plan; small,
focused helpers (`fn make_format_info_chain(...)`) keep the lifetime
plumbing readable.

**Step 5: Commit**

```bash
git commit -m "feat(kms/dri3): supported_modifiers query"
```

### Task 10: KMS-side dma-buf import → `DrawableImage`

**Files:**
- Modify: `crates/yserver/src/kms/vk/dri3.rs`

```rust
pub struct ImportRequest<'a> {
    pub fd: BorrowedFd<'a>,    // duped client fd
    pub width: u32, pub height: u32,
    pub stride: u32, pub offset: u32,
    pub format: vk::Format,
    pub modifier: u64,
}
pub fn import_dmabuf(vk: &VkContext, req: ImportRequest) -> Result<DrawableImage, ImportError>;
```

Mirrors `scanout.rs:allocate_vk_scanout_image:554` but with
`VkImageDrmFormatModifierExplicitCreateInfoEXT` chained under
`VkExternalMemoryImageCreateInfo` chained under `VkImageCreateInfo`.
For modifier `DRM_FORMAT_MOD_LINEAR` (or when the modifier extension
isn't loaded), use `VK_IMAGE_TILING_LINEAR` and skip the explicit-modifier
chain; otherwise `VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT`.

**Critical fd-ownership rule (§3.2):** `import_dmabuf` `dup`s the
client fd; on **any** error path between `dup` and a successful
`vkAllocateMemory`, `close` the dup'd fd. `vkAllocateMemory` consumes
the fd only on `VK_SUCCESS`. Once consumed, the fd's lifetime is
owned by the resulting `VkDeviceMemory` — `DrawableImage::Drop` releases
it via `vkFreeMemory`.

**Step 1: Failing integration test** under
`crates/yserver/tests/dri3_import.rs`. Use the existing vng/Venus
harness: allocate a `VkBuffer + VkImage TILING_LINEAR` via the test's
own VkContext, export to a dma-buf fd via `VK_KHR_external_memory_fd`,
import via `import_dmabuf` into a separate `VkContext`, sample the
import in a `vkCmdCopyImage` to a host-readable buffer, assert pixels
match. (The lavapipe leg of the matrix exercises the LINEAR fallback;
the Venus leg exercises the modifier path.)

**Step 2-4: Implement.** Wire up an `fd_leak_check` helper that snapshots
`/proc/self/fd` count before/after to catch ownership-rule slips.

**Step 5: Commit**

```bash
git commit -m "feat(kms/dri3): import dma-buf into DrawableImage"
```

### Task 11: Dispatcher — `PixmapFromBuffer(s)` + capability gating

**Files:**
- Modify: `crates/yserver-core/src/backend/trait_def.rs` — add:
  - ```rust
    #[derive(Clone, Copy, Debug, Default)]
    pub struct Dri3Caps {
        pub version: (u32, u32),  // (0, 0) sentinel = unsupported
        pub modifiers: bool,
        pub fence_fd: bool,
        pub syncobj: bool,
    }
    impl Dri3Caps {
        pub const fn unsupported() -> Self {
            Self { version: (0, 0), modifiers: false, fence_fd: false, syncobj: false }
        }
    }
    ```
    `Default` is provided so `Dri3Caps::default()` is also valid; the
    explicit `unsupported()` constructor is referenced by name elsewhere
    in this plan.
  - `fn dri3_capabilities(&self) -> Dri3Caps { Dri3Caps::unsupported() }`
    — default impl returns the unsupported sentinel; KmsBackend overrides;
    HostX11Backend and `RecordingBackend` (`#[cfg(test)]`) keep the
    default (so the workspace still compiles).
  - `fn dri3_import_pixmap(&mut self, …) -> io::Result<PixmapHandle>` —
    default impl `Err(io::Error::other("DRI3 unsupported"))`.
- Modify: `crates/yserver/src/kms/backend.rs` — implement, calling
  `kms::vk::dri3::import_dmabuf` and stashing the returned `DrawableImage`
  on `PixmapState`.
- Modify: `crates/yserver-core/src/core_loop/process_request.rs` —
  - Dispatch `PixmapFromBuffer` (single-fd) and `PixmapFromBuffers`
    (multi-fd). Only `num_buffers == 1` is accepted; `num_buffers > 1`
    emits `BadAlloc` (multi-plane is out-of-scope).
  - Tighten the `Dri3` arm in `extension_query_reply` (line 4131) and
    `advertised_extension_names` (line 4151) — replace the
    Task 5 always-true placeholder with
    `backend.dri3_capabilities().version != (0, 0)`. DRI3 is hidden
    from `EXTENSIONS` **only when** `VK_KHR_external_memory_fd` is
    missing (i.e. version is `(0, 0)`).
  - Per-request gates inside `handle_dri3_request`:
    - `fence_fd == false` → reject `FENCE_FROM_FD` and `FD_FROM_FENCE`
      with `BadImplementation` (matches design §4 row for missing
      SYNC_FD handle type — clients fall back to roundtrip sync). The
      gate is implemented here, not in Task 19.
    - `syncobj == false` → reject `IMPORT_SYNCOBJ` / `FREE_SYNCOBJ`
      with `BadImplementation` (Task 20 wires the dispatch arm; this
      task installs the gate it consults).
  - Version negotiation in `QueryVersion`: `min(client, server)` where
    server = `Dri3Caps::version`. The version cap is set by
    `KmsBackend::dri3_capabilities()` per design §4: with `syncobj`
    advertise `(1, 4)`; without `syncobj` cap at `(1, 3)`. `fence_fd`
    does **not** affect the advertised version — `FenceFromFD` /
    `FDFromFence` are 1.0 requests; missing the capability filters
    those individual requests via `BadImplementation` per the rule
    above, but the rest of DRI3 1.4 keeps working.
- Modify: `crates/yserver-core/src/nested.rs` — extend
  `ExtensionAvailability` enum with a `Dri3` variant. The discrimination
  itself is `nested.rs`'s job; the resolution lives in
  `extension_query_reply` in `process_request.rs:4128` (mirrors how
  `HostRender` calls `backend.render_opcode().is_some()` from there
  rather than from `nested.rs` itself).

**Step 1: Failing tests:**
- Caps gating — when `Dri3Caps::modifiers == false`,
  `GetSupportedModifiers` for any format returns `[LINEAR]`.
- Multi-plane rejection — `PixmapFromBuffers { num_buffers: 2 }` returns
  `BadAlloc`.
- Wire-level smoke — a python-xcb test using `xcb.dri3` connects,
  `PixmapFromBuffer`s a precomputed checkerboard buffer, then `GetImage`s
  a region of the resulting pixmap and asserts pixel equality.

**Step 2-4: Implement.**

**Step 5: Commit**

```bash
git commit -m "feat(dri3): import pixmap dispatcher with caps gating"
```

### Task 12: `GetSupportedModifiers` per-window vs. per-screen

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs` — track
  `output.scanout_format_set: HashSet<(vk::Format, u64)>` from the
  `add_fb2` probe at backend init (Phase 4.1 already does the probe;
  expose the set as a backend method).
- Modify: `crates/yserver-core/src/core_loop/process_request.rs` —
  `GetSupportedModifiers` reply distinguishes the two lists per §3.2.

**Step 1: Failing test** — request twice on the same backend, one with
`window == screen`, one with `window == real_window`. Assert the
window-list is a subset of the screen-list.

**Step 2-4: Implement.**

**Step 5: Commit**

```bash
git commit -m "feat(dri3): GetSupportedModifiers per-window vs per-screen"
```

### Task 13: `BufferFromPixmap` + `BuffersFromPixmap` (best-effort export)

**Files:**
- Modify: `crates/yserver/src/kms/vk/dri3.rs` — add `export_dmabuf`:
  `vkGetMemoryFdKHR` on the backing `VkDeviceMemory`. Returns one fd
  per plane (always 1 for Phase 4.2).
- Modify: `crates/yserver-core/src/core_loop/process_request.rs` —
  dispatcher uses `send_reply_with_fd`.

`BuffersFromPixmap` may legitimately be deferred to a `BadAlloc`
stub; design §6 lists this as an open question. **Decision (defer):**
implement `BufferFromPixmap` (single plane) only, return `BadAlloc`
for `BuffersFromPixmap`. Note this in a doc-comment so the follow-up
is discoverable.

```bash
git commit -m "feat(dri3): BufferFromPixmap export path"
```

### Task 14: fd-leak harness

**Files:**
- Create: `crates/yserver/tests/dri3_fd_leak.rs`

10k iterations of `(allocate gbm bo → PixmapFromBuffers → FreePixmap)`;
assert `/proc/self/fd` count returns to baseline after the loop.

```bash
git commit -m "test(dri3): fd-leak harness for 10k import cycles"
```

### Task 15: Sub-phase 4.2.1 smoke + status update

**Files:**
- Modify: `docs/status.md` — flip "Phase 4.2.1" to ✅.

Run the visible smoke per the design:

```bash
cargo build --release --bin yserver
tools/vng-vulkan-smoke.sh         # if it exists, otherwise the
                                  # raw vng invocation from
                                  # reference_vng_vulkan_venus.md
# Inside vng:
DISPLAY=:1 yserver &
DISPLAY=:1 ./target/release/dri3-checkerboard-test  # Task 11 binary
```

Expected: a window with a centered checkerboard renders, byte-for-byte
match against the source buffer.

```bash
git commit -m "chore(status): mark Phase 4.2.1 complete"
```

---

## Sub-phase 4.2.2 — XSync fence audit + DRI3 syncobj

**Outcome:** Two-client fence trigger / await observable via `Sync::AwaitFence`.

### Task 16: Audit existing XSync handlers

**Files:**
- Read: `crates/yserver-protocol/src/x11/sync.rs`,
  `crates/yserver-core/src/core_loop/process_request.rs` (sync handler).

**Output:** A short note in `docs/superpowers/notes/2026-05-09-sync-audit.md`
listing every XSync request and its current status (handled / stub /
unimplemented). No code change yet.

```bash
git commit -m "docs(sync): handler audit baseline"
```

### Task 17: `CreateFence` / `DestroyFence` / `TriggerFence` / `ResetFence` / `AwaitFence`

**Files:**
- Modify: `crates/yserver-protocol/src/x11/sync.rs` — add request opcodes
  (`CREATE_FENCE = 14`, `DESTROY_FENCE = 15`, `TRIGGER_FENCE = 18`,
  `RESET_FENCE = 19`, `QUERY_FENCE = 20`, `AWAIT_FENCE = 21`) and
  parsers/encoders. (Existing file stops at `GET_PRIORITY = 13`.)
- Modify: `crates/yserver-core/src/core_loop/process_request.rs` — extend
  the SYNC dispatcher.

For Phase 4.2.2 these are server-only fences (no fd backing yet). The
backing storage is a `bool triggered` field per fence XID kept on
`ServerState::sync_fences: HashMap<ResourceId, FenceState>`.

**Step 1: Failing tests** — wire-decode round trip for each opcode;
two-client behavior (client A `TriggerFence`, client B `AwaitFence`
returns control after the trigger) via the existing test harness.

**Step 2-4: Implement.**

**Step 5: Commit**

```bash
git commit -m "feat(sync): server-only Fence lifecycle + AwaitFence"
```

### Task 18: KMS-side `VkSemaphore` import (sync-fd handle type)

**Files:**
- Create: `crates/yserver/src/kms/vk/sync.rs`
- Modify: `crates/yserver/src/kms/vk/mod.rs` — `pub mod sync;`

```rust
pub fn import_sync_file(vk: &VkContext, fd: BorrowedFd) -> Result<vk::Semaphore, …>;
pub fn import_drm_syncobj(vk: &VkContext, fd: BorrowedFd) -> Result<vk::Semaphore, …>;
pub fn export_sync_file(vk: &VkContext, sem: vk::Semaphore) -> Result<OwnedFd, …>;
```

Both imports go through `vkImportSemaphoreFdKHR`; the difference is
`handleType = SYNC_FD_BIT_KHR` vs `OPAQUE_FD_BIT_KHR` and (for syncobj)
the semaphore was created with `VkSemaphoreTypeCreateInfo {
semaphoreType: TIMELINE }`. Same fd-ownership rule as §3.2:
close on any non-`SUCCESS` return.

**Step 1: Failing integration test** — create a binary semaphore on the
test VkContext, export to sync_file fd, import on a fresh semaphore,
signal on the source queue, wait on the destination, observe the wait
returns within a timeout.

**Step 2-4: Implement.**

**Step 5: Commit**

```bash
git commit -m "feat(kms/sync): VkSemaphore external import/export"
```

### Task 19: Wire `FenceFromFD` / `FDFromFence` into XSync

**Files:**
- Modify: `crates/yserver-core/src/backend/trait_def.rs`. All four new
  methods get default impls returning `Err(io::Error::other("sync
  unsupported"))` so `HostX11Backend` and `RecordingBackend` continue to
  compile without per-method stubs:
  - `fn sync_fence_from_fd(&mut self, fd: OwnedFd) -> io::Result<SyncSemaphoreHandle>`
  - `fn sync_fd_from_fence(&mut self, h: SyncSemaphoreHandle) -> io::Result<OwnedFd>`
  - `fn sync_trigger_fence(&mut self, h: SyncSemaphoreHandle) -> io::Result<()>`
  - `fn sync_await_fence(&mut self, h: SyncSemaphoreHandle, …) -> io::Result<…>`.
- Modify: `crates/yserver-core/src/core_loop/process_request.rs` — DRI3
  dispatcher routes `FENCE_FROM_FD` / `FD_FROM_FENCE` here. The
  capability gate (`Dri3Caps::fence_fd == false → BadImplementation`)
  was installed in Task 11; this task implements the actual handler
  behind that gate.
- Modify: `crates/yserver/src/kms/backend.rs` — table of
  `Dri3SyncResources: HashMap<ResourceId, SemaphoreEntry>` keyed by the
  XID the client passed.

**Step 1: Failing integration test** (per design §5.2 round-trip case) —
client `FenceFromFD` (import a sync_file fd) then `FDFromFence` (export
the same XID); the exported fd signals when the imported one signals.
This catches the SCM_RIGHTS reply path on `FDFromFence`.

**Step 2-4: Implement.**

**Step 5: Commit**

```bash
git commit -m "feat(dri3): FenceFromFD + FDFromFence round-trip"
```

### Task 20: `ImportSyncobj` / `FreeSyncobj`

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs` — `Dri3SyncResources`
  variant for timeline semaphores.
- Modify: `crates/yserver-core/src/core_loop/process_request.rs` —
  implement the `IMPORT_SYNCOBJ` / `FREE_SYNCOBJ` handlers. The
  `Dri3Caps::syncobj` gate (`BadImplementation` when missing) was
  installed in Task 11; this task fills in the behind-gate behaviour.

**Step 1: Failing tests** — happy path + caps-gated rejection.

**Step 2-4: Implement.**

**Step 5: Commit**

```bash
git commit -m "feat(dri3): ImportSyncobj / FreeSyncobj"
```

### Task 21: 4.2.2 smoke + status update

Run the two-client fence smoke from §5.2 manually (a small `xcb` rust
program in `tools/`). Update `docs/status.md`.

```bash
git commit -m "chore(status): mark Phase 4.2.2 complete"
```

---

## Sub-phase 4.2.3 — Present v1.4 Copy path

**Outcome:** `vkcube --present-mode fifo` renders and flips at vsync via
the Copy path; `IdleNotify` and `CompleteNotify` fire correctly.

### Task 22: Wire-decode `PresentPixmapSynced` (existing `PIXMAP_SYNCED`)

**Files:**
- Modify: `crates/yserver-protocol/src/x11/present.rs`

Add `PixmapSyncedRequest` (binary fields like `PixmapRequest` but with
`(acquire_syncobj, release_syncobj, acquire_value: u64, release_value: u64)`
in place of `(wait_fence, idle_fence)`). Bump `MINOR_VERSION = 4`.

`MAJOR_VERSION` already `1`.

**Step 1: Failing tests** — wire-decode happy path + length validation.

**Step 2-4: Implement.**

**Step 5: Commit**

```bash
git commit -m "feat(present): wire-decode PresentPixmapSynced"
```

### Task 23: `PresentCaps` + `QueryCapabilities`

**Files:**
- Modify: `crates/yserver-core/src/backend/trait_def.rs`:
  ```rust
  pub struct PresentCaps { pub flip_path: bool, pub async_may_tear: bool, pub syncobj: bool }
  // default impl: all-false (Copy-only, no syncobj). KmsBackend overrides;
  // HostX11Backend / RecordingBackend keep the default.
  fn present_capabilities(&self, _window: u32) -> PresentCaps { PresentCaps::default() }
  ```
- Modify: `crates/yserver/src/kms/backend.rs` — populate from KMS
  property probe (`IN_FENCE_FD` on the window's CRTC's primary plane,
  `OUT_FENCE_PTR` on the CRTC, `DRM_MODE_ATOMIC_NONBLOCK` accepted by a
  test commit), and mirror `Dri3Caps::syncobj`.
- Modify: `crates/yserver-core/src/core_loop/process_request.rs` —
  `QueryCapabilities` reply assembled from `PresentCaps` per the
  `presentproto` flag layout.

**Step 1: Failing tests** — caps reply byte layout; per-window dispatch.

**Step 5: Commit**

```bash
git commit -m "feat(present): PresentCaps + QueryCapabilities"
```

### Task 24: Present scheduler skeleton

**Files:**
- Modify: `crates/yserver/src/present/state.rs` — per-window FIFO of
  queued presents:
  ```rust
  pub struct QueuedPresent {
      pub serial: u32, pub pixmap: ResourceId, pub window: ResourceId,
      pub options: u32, pub target_msc: u64, pub divisor: u64, pub remainder: u64,
      pub wait_sem: Option<SyncSemaphoreHandle>, pub idle_sem: Option<SyncSemaphoreHandle>,
      pub path: PresentPath,           // chosen at queue time, not vblank
      pub valid_region: u32, pub update_region: u32,
  }
  pub enum PresentPath { Copy, Flip { alien_bo: AlienBoHandle }, DirectScanout { alien_bo: AlienBoHandle } }
  ```
- Modify: `crates/yserver/src/present/event_loop.rs` — at each vblank
  event, drain windows whose schedule predicate
  (`current_msc >= target_msc` after the divisor/remainder formula in
  §3.3.3) is satisfied, pick latest, mark earlier with `Skip`, submit.

**Step 1: Failing unit tests** — scheduler: `(target_msc=0)` immediate;
`(divisor=2, remainder=0)` even-MSC alignment; multiple queued frames
on one window collapse to `latest + Skip`.

**Step 2-4: Implement.**

**Step 5: Commit**

```bash
git commit -m "feat(present): scheduler skeleton + path enum"
```

### Task 25: `choose_path` + scanout-compat predicate

**Files:**
- Create: `crates/yserver/src/present/path_selector.rs`
- Modify: `crates/yserver/src/present/mod.rs` — `pub mod path_selector;`

```rust
pub fn choose_path(req: &PresentPixmapRequest, pixmap: &PixmapState,
                   window: &WindowState, output: &OutputState,
                   caps: &PresentCaps) -> PresentPath
```

per §3.3.1. Honour the `caps.flip_path == false` short-circuit.

**Step 1: Failing tests** covering each branch in the decision table:
- `PresentOptionCopy` set → Copy.
- Fullscreen + scanout-compat + window-covers-output → DirectScanout.
- Non-fullscreen but scanout-compat → Flip.
- Otherwise → Copy.
- `flip_path == false` → Copy regardless.

**Step 5: Commit**

```bash
git commit -m "feat(present): choose_path selector"
```

### Task 26: `IdleNotify` / `CompleteNotify` event encoders

**Files:**
- Modify: `crates/yserver-protocol/src/x11/present.rs` — the GE Generic
  Event encoders for these two events (event types 1 and 2 per
  `presentproto`).

**Step 1: Failing tests** — byte layout matches `presentproto`. These
encoders are a prerequisite for Task 27's Copy-path test, which observes
the events; do them first so the Task 27 test can reference real
encoder output instead of hand-rolled byte fixtures.

**Step 5: Commit**

```bash
git commit -m "feat(present): IdleNotify/CompleteNotify event encoders"
```

### Task 27: Copy-path implementation

**Files:**
- Modify: `crates/yserver/src/present/paint.rs` — when a window has a
  queued `PresentPath::Copy`, the composite pass overrides the window's
  `DrawableImage` source with the imported pixmap's `DrawableImage` for
  exactly one frame.
- Modify: `crates/yserver/src/kms/vk/scanout.rs` — when the composite
  pass completes (per-frame timeline-semaphore signal),
  `present::on_gpu_read_complete` fires `IdleNotify` for queued Copy
  frames and signals `idle_fence` / `idle_syncobj` semaphores.
- Modify: `crates/yserver-core/src/core_loop/process_request.rs` —
  `PIXMAP` and `PIXMAP_SYNCED` dispatch:
  - Look up `PresentCaps` for the target window.
  - Call `choose_path`.
  - For `PresentPath::Copy`, push onto the per-window FIFO via
    `Backend::present_queue(...)`.
  - Plumb the `wait_fence` / `wait_value` semaphore as a queue-submit
    `pWaitSemaphores` entry.

**Step 1: Failing integration test** — `vkcube`-equivalent rust binary
that imports a pixmap, calls `PresentPixmap` with `PresentOptionCopy`,
asserts (a) pixels reach the framebuffer this frame, (b) `IdleNotify`
event arrives, (c) `CompleteNotify { mode: Copy }` arrives at next
vblank.

**Step 2-4: Implement.**

**Step 5: Commit**

```bash
git commit -m "feat(present): Copy-path with IdleNotify/CompleteNotify"
```

### Task 28: 4.2.3 smoke + status update

```bash
# In vng:
DISPLAY=:1 yserver &
DISPLAY=:1 vkcube --present-mode fifo
```

Expected: vkcube renders, vsynced. `RUST_LOG=yserver::present=debug`
shows `Copy` path chosen for every frame.

Update `docs/status.md`.

```bash
git commit -m "chore(status): mark Phase 4.2.3 complete"
```

---

## Sub-phase 4.2.4 — Flip + Direct-scanout paths

**Outcome:** `vkcube --present-mode mailbox` runs Flip; fullscreen
`vkcube` runs Direct-scanout.

### Task 29: `AlienBoHandle` + extension to `ScanoutBoPool`

**Files:**
- Modify: `crates/yserver/src/kms/vk/scanout.rs` — `ScanoutBo` gains
  `is_alien: bool`. Alien BOs are added on demand from a client-imported
  `DrawableImage` rather than allocated by the pool. They share the
  pool's `add_fb2` registration code path but skip the allocator.
- Add: `pub fn ScanoutBoPool::register_alien(&mut self, drawable: &DrawableImage)
  -> io::Result<AlienBoHandle>` and `unregister_alien`.

**Step 1: Failing test** — register an alien BO from a synthetic
`DrawableImage`, observe the `add_fb2` ID is allocated, then
`unregister_alien` releases it without affecting the pool's owned BOs.

**Step 5: Commit**

```bash
git commit -m "feat(kms): AlienBo registration on ScanoutBoPool"
```

### Task 30: `AsyncMayTear` cap probe + silent-ignore

This lands before the Flip / DirectScanout paths because the silent-clear
must happen at request-ingress (before `choose_path`), and the Flip-path
test in Task 31 is the easiest place to incidentally verify it.

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs` — probe
  `DRM_MODE_ATOMIC_NONBLOCK` via a no-op test commit at backend init;
  store the result on `PresentCaps::async_may_tear`.
- Modify: `crates/yserver-core/src/core_loop/process_request.rs` —
  inside `handle_present_request` for `PIXMAP` / `PIXMAP_SYNCED`, after
  parsing `req.options`, mask off `PresentOptionAsyncMayTear` when
  `present_capabilities(window).async_may_tear == false`. Per design §4:
  the bit is **silently cleared**, *not* downgraded to
  `PresentOptionAsync`. Async and AsyncMayTear are distinct request
  options; folding one into the other changes observable behavior.

**Step 1: Failing test** — call `handle_present_request` with
`PresentOptionAsyncMayTear | PresentOptionCopy` on a backend whose
`PresentCaps::async_may_tear == false`; observe that the queued frame's
options field has the AsyncMayTear bit cleared and the Async bit also
clear.

**Step 5: Commit**

```bash
git commit -m "feat(present): AsyncMayTear cap probe + silent-ignore"
```

### Task 31: Flip path

**Files:**
- Modify: `crates/yserver/src/present/paint.rs` — for
  `PresentPath::Flip`, the composite pass writes into the alien BO
  (cursor + decorations on top of client content). Atomic-flip the alien
  BO. Track `last_flipped: Option<AlienBoHandle>` per window.
- Modify: `crates/yserver/src/present/event_loop.rs` —
  `CompleteNotify { mode: Flip }` on pageflip-complete; `IdleNotify` for
  the *previous* `last_flipped` (per `presentproto` Flip lifetime —
  retained until next-Present-completes).

**Step 1: Failing test** — queue PresentPixmap A (Flip) then B (any).
After B completes, A's `IdleNotify` fires. Before B completes, it does
not.

**Step 5: Commit**

```bash
git commit -m "feat(present): Flip path with deferred IdleNotify"
```

### Task 32: Direct-scanout path

**Files:**
- Modify: `crates/yserver/src/present/paint.rs` — for
  `PresentPath::DirectScanout`, skip composite entirely; the alien BO
  becomes the scanout image. Cursor renders on the cursor plane (already
  exists per Phase 6.10).

**Step 1: Failing test** — fullscreen test client; assert
`RUST_LOG=yserver::present=debug` reports `DirectScanout`; readback
via DRM `getfb2` shows the alien BO ID, not a pool BO.

**Step 5: Commit**

```bash
git commit -m "feat(present): Direct-scanout path"
```

### Task 33: Teardown rules

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request.rs` —
  `process_destroy_window` walks `last_flipped` and per-window FIFO,
  retires Presents (drain queued, signal idle semaphores, no
  `IdleNotify` because window is gone).
- Modify: `crates/yserver-core/src/core_loop/client_close.rs` (or
  wherever client-disconnect cleanup lives) — drain Presents the
  closing client queued against other clients' windows.
- Modify: `crates/yserver/src/kms/backend.rs` — on output mode change
  / unplug, re-evaluate `output.scanout_format_set`; if `last_flipped`
  no longer fits, force one Copy frame onto a pool BO so the alien
  retires.

**Step 1: Failing tests** — the §5.2 teardown matrix:
- `DestroyWindow` mid-Flip.
- Source pixmap `FreePixmap` mid-Flip.
- Client disconnect mid-Flip.

(`AsyncMayTear`-ignored already verified in Task 30.)

**Step 5: Commit**

```bash
git commit -m "feat(present): teardown rules for Flip retention"
```

### Task 34: 4.2.4 smoke + status update

```bash
# In vng with hardware that supports atomic + IN_FENCE_FD:
DISPLAY=:1 yserver &
DISPLAY=:1 vkcube --present-mode mailbox            # Flip
DISPLAY=:1 vkcube --present-mode fifo --fullscreen  # DirectScanout
```

Verify via `RUST_LOG=yserver::present=debug` that the path picked is
the expected one for each mode.

Update `docs/status.md`.

```bash
git commit -m "chore(status): mark Phase 4.2.4 complete"
```

---

## Sub-phase 4.2.5 — GLX framing

**Outcome:** `glxinfo` reports `direct rendering: Yes`, vendor `yserver`,
extensions list. `glxgears` runs at vsync.

### Task 35: GLX wire-protocol skeleton

**Files:**
- Create: `crates/yserver-protocol/src/x11/glx.rs`
- Modify: `crates/yserver-protocol/src/x11/mod.rs` — `pub mod glx;`

Constants and parsers for `QueryVersion`, `QueryServerString`,
`QueryExtensionsString`, `ClientInfo` (legacy), `SetClientInfoARB`,
`SetClientInfo2ARB`, `GetFBConfigs`, `GetVisualConfigs`,
`CreateNewContext`, `CreateContextAttribsARB`, `DestroyContext`,
`MakeCurrent`, `MakeContextCurrent`, `IsDirect`, `WaitX`, `WaitGL`,
`SwapBuffers`, `VendorPrivate`, `VendorPrivateWithReply`. Opcode
numbers per `glxproto.h` — write a small comment at the top of the file
pointing readers to the upstream header rather than duplicating numbers
in the design.

**Step 1-4: Tests + parsers** following `dri3.rs` shape.

**Step 5: Commit**

```bash
git commit -m "feat(glx): wire-protocol module skeleton"
```

### Task 36: GLX dispatcher with stub handlers

**Files:**
- Create: `crates/yserver-core/src/glx.rs` — state container
  (`GlxContext` resource table).
- Modify: `crates/yserver-core/src/core_loop/process_request.rs` — add
  `handle_glx_request`. Allocate `GLX_MAJOR_OPCODE: u8 = 148` in
  `nested.rs` (next free slot after `DRI3_MAJOR_OPCODE = 147` from
  Task 5). The upstream X.Org assignment is 149, but yserver's local
  opcode space puts COMPOSITE at 144 (already taken), so 148 is what
  fits. Add a `148`-arm in the dispatcher match.
- Modify: `crates/yserver-core/src/nested.rs` — register `GLX`
  extension; availability `Always` for the framing surface (the cap
  matrix doesn't gate GLX itself, only DRI3 underneath).

For every minor:
- `QueryVersion` → reply `(1, 4)`.
- `QueryServerString` → vendor `yserver`, version `1.4`, extensions empty.
- `QueryExtensionsString` → the §3.5 list.
- `ClientInfo / SetClientInfoARB / SetClientInfo2ARB` → parse, debug-log,
  drop.
- `GetFBConfigs / GetVisualConfigs` → synthesise from each X visual ×
  `(double_buffered, depth_size, stencil_size, sample_count)` (~30
  configs).
- `CreateNewContext / CreateContextAttribsARB` → allocate a `GlxContext`
  resource, store `(share_xid, screen, fbconfig, attribs)`.
- `DestroyContext` → free.
- `MakeCurrent / MakeContextCurrent` → record `(context, drawable)` per
  client.
- `IsDirect` → reply `is_direct = 1`.
- `WaitX / WaitGL` → no-op.
- `SwapBuffers` → forward as a degraded `PresentPixmap` (Copy path) for
  legacy clients.
- `VendorPrivate / VendorPrivateWithReply` → reply
  `GLXUnsupportedPrivateRequest`.
- Indirect rendering opcodes (1..=198) → `GLXBadRequest`.

**Step 1: Failing tests** — opcode-by-opcode, asserting reply bytes
match `glxproto.h` shape. Plus an integration smoke that drives
`glxinfo` against the server and asserts the textual output contains
`direct rendering: Yes` and `Vendor: yserver`.

**Step 2-4: Implement.**

**Step 5: Commit**

```bash
git commit -m "feat(glx): identification + bookkeeping dispatcher"
```

### Task 37: 4.2.5 smoke + status update

```bash
# In vng:
DISPLAY=:1 yserver &
DISPLAY=:1 glxinfo | grep -E '(rendering|Vendor|^GLX extensions)'
DISPLAY=:1 glxgears
```

Expected: `direct rendering: Yes`, vendor `yserver`, extension list
matches §3.5. `glxgears` runs.

Update `docs/status.md`.

```bash
git commit -m "chore(status): mark Phase 4.2.5 complete"
```

---

## Wrap-up

**Files:**
- Modify: `docs/status.md` — flip Phase 4.2 to ✅, link to this plan and the design.
- Optionally: open a follow-up issue for the deferred items
  (`BuffersFromPixmap`, multi-plane import, indirect GLX, per-window
  Present concurrency confirmation).

Then per @superpowers:finishing-a-development-branch decide between
PR-merge or squash-and-merge (already on a feature branch per the
`branching_for_multi_commit_work` memory; squash-collapse at PR is the
norm here per the `amend_only_with_permission` memory).

```bash
git commit -m "chore(status): mark Phase 4.2 complete"
```

---

## Risk / open items folded from design §6

- **`BuffersFromPixmap` scope** — Task 13 stubs it to `BadAlloc`. Open a
  follow-up if a screen-recorder use case lands.
- **Cursor on Direct-scanout** — verified at Task 31's smoke; fix here if
  cursor disappears.
- **PresentPixmap concurrency on a shared window** — Task 33's teardown
  matrix exercises one-client retire; multi-client serialisation is
  per-window-queue by construction. Confirm under a WM-redirected
  window if a regression surfaces.

---

## Review history

- 2026-05-09 — Codex review (first pass). Folded in: opcode collision
  fixes (DRI3 `146` → `147` because XTEST already owns 146; GLX `144`
  → `148` because COMPOSITE owns 144); render-node fallback rewritten
  to use libdrm `drmGetDevices2` rather than hardcoded
  `/dev/dri/renderD128`; Task 11 capability filtering moved from
  `nested.rs` to `process_request.rs:4128` and clarified that only
  missing `VK_KHR_external_memory_fd` hides DRI3 from `EXTENSIONS`,
  while `fence_fd` / `syncobj` narrow `Dri3Caps::version` instead;
  Tasks 26 ↔ 27 swapped so the `IdleNotify` / `CompleteNotify` event
  encoders land before the Copy-path test that observes them; Task 33
  (`AsyncMayTear` silent-ignore) hoisted to Task 30 so the bit is
  cleared before the Flip / DirectScanout / teardown tasks run; default
  trait impls spelled out for every new `Backend` method (Tasks 7, 11,
  19, 23) so `HostX11Backend` and `RecordingBackend` continue to
  compile without per-method overrides.

- 2026-05-09 — Codex re-review (second pass). Folded in: Task 5 now
  also patches the `match ext.availability` arms in
  `extension_query_reply` (line 4131) and `advertised_extension_names`
  (line 4151) to handle the new `Dri3` variant — without that the
  Task 5 `QueryExtension("DRI3")` smoke wouldn't compile; Task 11
  spells out the `Dri3Caps::unsupported()` constructor that was
  previously referenced but never defined; per-request capability gates
  (`fence_fd` for `FenceFromFD` / `FDFromFence`, `syncobj` for
  `ImportSyncobj` / `FreeSyncobj`) installed at Task 11 and
  cross-referenced from Tasks 19 / 20 — earlier draft only narrowed
  the version reply which left the request handlers ungated.
