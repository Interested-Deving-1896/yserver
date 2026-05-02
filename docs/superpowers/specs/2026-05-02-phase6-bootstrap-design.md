# Phase 6 bootstrap — DRM/KMS first slice ("Hello rectangle")

Replaces the placeholder `crates/yserver/src/bin/yserver.rs` with a real DRM/KMS
binary that boots in `virtme-ng`, sets a mode on the virtio-gpu output, runs a
libinput-driven event loop, and paints a moving rectangle into a dumb-buffer
swapchain. The goal is to validate the bare-metal lifecycle and the reusable
infrastructure layers, not to land any X11 protocol code yet.

This is **slice B** of a B → C trajectory. C (real X clients on KMS) is a
follow-up phase scoped separately.

## Goal

Prove that the bare-metal lifecycle works end-to-end:

- `just yserver` boots a vng guest.
- The binary opens `/dev/dri/card0`, acquires DRM master, enumerates
  outputs, picks a mode, and atomic-commits.
- An event loop pumps libinput and DRM page-flip events from a single
  thread via `epoll`.
- A throwaway painter draws a moving rectangle into a dumb-buffer
  swapchain; submitted frames are page-flipped to the connected output.
- Mouse/keyboard input from the QEMU window reaches the binary; mouse
  motion moves the rectangle (cursor stand-in); keys log to stderr.
- SIGINT / SIGTERM exit cleanly: release DRM master, drop swapchain
  buffers, close fds.

## Scope decisions

| Topic | Choice | Reason |
|---|---|---|
| Success bar | "Hello rectangle" (B) | Smaller than landing real X clients (C); larger than just painting one solid colour (A). |
| Target platform | vng-only | Iteration speed; bare metal becomes Phase 6.x once B works. |
| Render path | Dumb buffers + CPU painting | Always works on virtio-gpu, no GL stack to debug. Likely the permanent answer for "old 2D desktops on metal" — GL is a future optional optimization, not a Phase 6 requirement. |
| Session/seat | `open(/dev/dri/card0)` + `DRM_IOCTL_SET_MASTER` as root | vng guest is single-session. logind / VT-switching is Phase 6.x. |
| Input | libinput via udev seat0 | Re-implementing keymap/accel on raw evdev is a rabbit hole. |
| Code layout | Single binary crate, modular internal layout | Avoids premature crate split / Backend-trait guess; promotion to a separate crate is a `git mv` once C tells us where the boundary should sit. |
| Loop topology | Single-thread epoll over `[libinput.fd, drm.fd, signalfd]` | One ordering authority; no inter-thread channels; matches what real X servers and Wayland compositors do. |

## Out of scope

Explicitly deferred to Phase 6.x or later:

- Any X11 protocol code; any `yserver-core` integration.
- Hotplug response (and detection — even no-op detection buys nothing here).
- Multi-output, multi-plane, modifier/format negotiation.
- GBM / EGL / GLES / Vulkan.
- logind seat handing, VT-switch handling, console restore on exit.
- Suspend/resume.
- Bare-metal targets (real Intel/AMD/NVIDIA on the CachyOS host).
- Frame timeout / hang detection — if virtio-gpu hangs, B hangs (visible).

## Architecture

`crates/yserver/src/`:

```
bin/yserver.rs        — argv, env_logger init, calls yserver::run()
lib.rs                — pub mod {drm, input, present}; pub fn run()
drm/
  mod.rs              — Device::open, master acquire/release, RAII Drop
  modeset.rs          — connector + crtc + plane discovery, atomic commit, mode pick
  swapchain.rs        — N dumb buffers, framebuffer add/rm, queue/release
  page_flip.rs        — drmHandleEvent loop, completion → buffer release
input/
  mod.rs              — libinput context init + dispatch fn (no thread)
present/
  mod.rs              — frame loop: input → state → paint → submit
  paint.rs            — throwaway moving rectangle + cursor stand-in
```

### Module responsibilities

- **`drm/`** — owns `/dev/dri/card0` lifetime. Atomic-commit-only (no
  legacy modeset fallback). Buffers are dumb buffers with mmap'd CPU
  pointers. RAII `Drop` releases master.
- **`input/`** — owns the libinput context and its udev seat. Exposes a
  dispatch function called by the present loop when libinput's fd is
  ready; no thread of its own. Keymap interpretation (keycode →
  keysym) is C's problem and requires xkbcommon; B emits and logs
  Linux input keycodes only.
- **`present/`** — the main thread. Owns the rectangle's state and the
  swapchain. Pulls input non-blocking, advances state, paints when a
  buffer is free and state is dirty.
- **`yserver::run()`** — top-level orchestrator: open DRM, pick output,
  init libinput, register a `signalfd` for SIGINT/SIGTERM, enter event
  loop. On loop exit: atomic-disable the plane and CRTC, drop the
  swapchain, drop the device.

No traits, no `dyn`, no generics. Concrete types throughout. The
backend abstraction is C's problem.

## Event loop topology

Single thread, single `epoll_wait`, three fds:

```
libinput.fd ─┐
             │
drm.fd ──────┼─→ epoll_wait (block forever) ─→ dispatch:
             │                                 - libinput → state.cursor += dt * (dx, dy)
signalfd ────┘                                 - drm     → release flipped-out buffer
                                               - signal  → break

after each drm completion:
   advance rectangle state by dt
   acquire next free buffer
   paint(buf)
   atomic page-flip commit(buf)
```

No fixed frame rate; no `sleep`/`timerfd`/manual vsync. **B's painter
declares state always-dirty as long as the loop is running**: the
rectangle has autonomous velocity, so a new flip is submitted on
every page-flip completion (so long as a free buffer exists). This is
the standard "continuous animation" atomic-commit cadence; CPU is
proportional to refresh rate, not to input. The shape (epoll →
dispatch → maybe-submit) survives into C unchanged; C will gate
"submit" on real damage instead of always-dirty.

`signalfd` is added to epoll in Step 12; Steps 8–11 work without it
and the loop wakes up via the natural fd readiness from drm/libinput.

## Error handling

All startup errors are fatal with a clear log message naming the
failing syscall. Mid-run errors log and exit. No retry, no recovery —
B is infrastructure validation, so an unexpected error is signal we
want to see.

Specific cases:

- **DRM open** — `ENOENT` / `EBUSY` / `EACCES` each get distinct
  messages. `EBUSY` hints "B is vng-only — try `just yserver`, not
  bare-metal `cargo run`."
- **No connected output / no compatible mode** — fail with connector
  and mode list dumped to log.
- **Atomic commit rejected** — fail. No legacy-modeset fallback.
- **libinput / udev** — log a warning and continue with no input.
  Rectangle still moves under its own velocity, so visual smoke
  remains testable.
- **Page-flip never completes** — no timeout. We hang visibly.

## Validation

- **Manual smoke (primary):** `just yserver` boots vng; QEMU window
  shows a smoothly moving rectangle. Mouse motion moves a small
  cursor stand-in. Keys log to stderr. Window-close exits cleanly.
- **Headless smoke (no graphics):** `just yserver-headless` shows the
  device-open / master-acquired / capability-dump / connector-list
  log lines and exits non-zero with a clear "no connected output"
  message — that's the *expected* outcome with virtio-gpu when no
  display backend is attached. The graphics path is the only path
  that exercises modeset; if a future Justfile recipe adds an
  off-screen virtual display, this gate gets revised. Until then
  "headless" is open + master + capability + enumeration only.
- **Re-run idempotence:** consecutive `just yserver` runs work — first
  run cleans up DRM master so the second can acquire.
- **Unit tests:** only for pure logic (mode-pick policy, swapchain
  buffer state machine). Don't force tests on syscall-bound code.
- **Optional follow-up:** automated smoke recipe that runs the binary
  for ~3s in vng, captures a QEMU `screendump`, asserts non-uniform
  buffer. Not blocking.

## Spike during plan-writing phase

Budget ~1–2 hours reading `crates/yserver-core/src/nested.rs`,
`crates/yserver-core/src/host_x11.rs`, and **`crates/yserver-core/src/resources.rs`**
to identify where the future Backend trait would sit. Output: a note
appended to this design — *not* a code extraction. The note must
cover four things, not just operation enumeration:

1. **Operation set sketch** — the rough trait method signatures C
   would need (window create/destroy, draw primitives, cursor,
   property/event paths, sync points).
2. **Call-site uniformity** — sample 20 `host_x11::*` call-sites in
   `nested.rs`; uniform-shaped (good — trait fits) vs. ad-hoc and
   intermixed with state mutation (bad — refactor pre-work needed).
3. **Resource model coupling** — `resources::{Window,Pixmap,Font,Cursor}`
   embed `host_xid: Option<u32>` fields today. Sketch how these
   become opaque backend handles in C (e.g. `BackendHandle` newtype
   per resource kind, or a single `BackendId` indirection table).
   This is the most likely C blocker — call it out explicitly.
4. **Cross-connection sync points** — note where `nested.rs` relies
   on host-X11 round-trips (`sync_main_connection`, the pump thread,
   request-reply correlations). The trait's threading model has to
   accommodate or replace each.

**Verdict required:** one of
- *go* — boundary findable, no resource-model prework needed before B.
- *go with prework* — boundary findable but the resource-model
  refactor (item 3) must land before B starts. List the prework as a
  separate plan/spec.
- *no-go* — boundary unfindable as the code stands. Stop the plan
  here and surface to user.

A bare "findable" without items 1–4 above is not a passing verdict.

## Forward compatibility with C

B is structured so the infrastructure (drm/, input/, present's loop
topology) survives into C. The throwaway pieces (`present/paint.rs`,
the rectangle state) are explicitly leaves and will be replaced by a
compositor that consumes window-tree state from yserver-core. The
swapchain's "produce a buffer, scan it out" interface is the seam.

## Spike findings — future C trait boundary

Read-only survey of `resources.rs` (2860 LOC), `host_x11.rs` (3637 LOC),
and `nested.rs` (10538 LOC) to confirm a `Backend` trait can later be
carved out without rewriting the X11 protocol layer.

### Operation set sketch

`HostX11` exposes **87 `pub fn` methods** (grep `^    pub fn` in
`host_x11.rs`). They cluster into seven groups, each of which becomes
a coherent block of trait methods in C. **Allocation note:** the
current code uses a *two-phase* pattern — `host.allocate_xid()`
returns a fresh handle, then `create_*(xid, ...)` consumes it, then
the caller stashes the xid into `resources.rs` after both succeed
(see `nested.rs:5085-5114` for `create_subwindow`, also used for
`create_pixmap` and the COMPOSITE `NameWindowPixmap` path). The
trait must reflect this with explicit handle allocation (e.g.
`Backend::allocate_handle(kind) -> BackendHandle`) separate from
`create_*`, or fold both into a single `create_*` that allocates
internally and only returns on success.

```rust
trait Backend {
    // --- init / capability discovery ---
    fn open() -> io::Result<Self>;                               // open_from_env analogue
    fn root_visual(&self) -> u32;
    fn argb_visual(&self) -> Option<u32>;
    fn argb_colormap(&self) -> Option<u32>;
    fn render_opcode(&self) -> Option<u8>;                       // and xkb / shape / xfixes / composite
    fn render_format_for_id(&self, ynest_fmt: u32) -> Option<u32>;

    // --- window/pixmap/cursor lifecycle ---
    fn create_window(&mut self, parent: BackendHandle, cfg: SubwindowConfig)
        -> io::Result<BackendHandle>;
    fn destroy_window(&mut self, h: BackendHandle) -> io::Result<()>;
    fn configure_window(&mut self, h: BackendHandle, cfg: ConfigureRequest) -> io::Result<()>;
    fn reparent_window(&mut self, h: BackendHandle, new_parent: BackendHandle, x: i16, y: i16)
        -> io::Result<()>;
    fn change_window_attributes(&mut self, h: BackendHandle, mask: u32, values: &[u32])
        -> io::Result<()>;
    fn map_window(&mut self, h: BackendHandle) -> io::Result<()>;
    fn unmap_window(&mut self, h: BackendHandle) -> io::Result<()>;
    fn create_pixmap(&mut self, depth: u8, width: u16, height: u16)
        -> io::Result<BackendHandle>;
    fn free_pixmap(&mut self, h: BackendHandle) -> io::Result<()>;
    fn create_cursor(&mut self, ...) -> io::Result<BackendHandle>;
    fn define_cursor(&mut self, win: BackendHandle, cursor: BackendHandle) -> io::Result<()>;
    fn open_font(&mut self, name: &str) -> io::Result<(BackendHandle, FontMetrics)>;
    fn close_font(&mut self, h: BackendHandle) -> io::Result<()>;

    // --- drawing primitives ---
    fn copy_area(&mut self, src: BackendHandle, dst: BackendHandle, ...) -> io::Result<()>;
    fn copy_plane(&mut self, ...) -> io::Result<()>;
    fn put_image(&mut self, dst: BackendHandle, ...) -> io::Result<()>;
    fn get_image(&mut self, src: BackendHandle, ...) -> io::Result<Vec<u8>>;
    fn poly_fill_rectangle(&mut self, dst: BackendHandle, fg: u32, rects: &[u8])
        -> io::Result<()>;
    fn poly_line/segment/arc/point/rectangle(&mut self, ...) -> io::Result<()>;  // 8 more
    fn poly_text8/16(&mut self, ...) -> io::Result<()>;
    fn image_text8/16(&mut self, ...) -> io::Result<()>;
    fn fill_poly(&mut self, ...) -> io::Result<()>;

    // --- shared GC state (current host model) ---
    // C may eliminate this group entirely by switching to per-client GCs;
    // see Phase 3.7 follow-up "Per-client GC mirroring".
    fn set_clip_rectangles(&mut self, clip: Option<ClipRectangles>) -> io::Result<()>;
    fn clear_clip_rectangles(&mut self) -> io::Result<()>;
    fn set_clip_pixmap(&mut self, ...) -> io::Result<()>;
    fn set_gc_fill_tiled(&mut self, ...) -> io::Result<()>;
    fn set_gc_fill_solid(&mut self) -> io::Result<()>;

    // --- RENDER subset (~20 methods) ---
    fn render_create_picture / change_picture / free_picture / composite /
        composite_glyphs / fill_rectangles / create_glyphset / free_glyphset /
        add_glyphs / free_glyphs / create_solid_fill / create_linear_gradient /
        create_radial_gradient / set_picture_transform / set_picture_filter /
        set_picture_clip_rectangles / trapezoids / create_cursor / query_version
        (..., ...) -> io::Result<...>;

    // --- SHAPE / XFIXES / COMPOSITE / container ---
    fn set_shape_rectangles(&mut self, win: BackendHandle, ...) -> io::Result<()>;
    fn xfixes_change_cursor_by_name(&mut self, cursor: BackendHandle, name: &[u8])
        -> io::Result<()>;
    fn name_window_pixmap(&mut self, win: BackendHandle, pix: BackendHandle)
        -> io::Result<()>;
    fn set_container_background_pixmap/pixel(&mut self, ...) -> io::Result<()>;

    // --- input / sync ---
    fn warp_pointer(&mut self, dst: BackendHandle, x: i16, y: i16) -> io::Result<()>;
    fn query_pointer(&mut self) -> io::Result<PointerPosition>;
    fn get_atom_name(&mut self, atom: u32) -> io::Result<Option<String>>;
    fn get_keyboard_mapping(&mut self, ...) -> io::Result<...>;
    fn get_modifier_mapping(&mut self) -> io::Result<...>;
    fn xkb_proxy(&mut self, minor: u8, body: &[u8]) -> io::Result<Option<Vec<u8>>>;
    fn ping(&mut self) -> io::Result<()>;
    fn sync(&mut self) -> io::Result<()>;             // replaces sync_main_connection
}

// Companion event-source trait absorbs HostInputPump:
trait BackendEventSource {
    fn fd(&self) -> RawFd;                            // for epoll registration
    fn dispatch(&mut self) -> io::Result<Vec<BackendEvent>>;
    fn register_window(&mut self, h: BackendHandle, kind: SubwindowKind) -> io::Result<()>;
    fn unregister_window(&mut self, h: BackendHandle) -> io::Result<()>;
}
```

Trait surface: ~60–80 methods on `Backend` plus ~5 on `BackendEventSource`.
Big but tractable. Drawing primitives dominate the count and are
shape-uniform, so most of the trait writes itself.

### Call-site uniformity

`nested.rs` holds `Arc<Mutex<HostX11>>` and reaches it through one
pattern, repeated 52 times (`grep -c "host\.lock()"`):

```rust
if let Some(host) = host
    && let Ok(mut h) = host.lock()
{
    let _ = h.METHOD(args);
}
// or equivalently:
host.lock().ok().map(|mut h| h.METHOD(args));
```

Sample read across the file confirms the pattern is *mostly* uniform
but with a load-bearing minority:

- **Majority — single-call leaves with errors swallowed.** The
  bulk of sites do "lock, call one method, drop lock, `let _ =`"
  (e.g. `change_subwindow_attributes`, `define_cursor`,
  `destroy_subwindow`, `xfixes_change_cursor_by_name`). State
  mutation lives on the ynest side; the host call is a leaf.
  Counts: ~40 of 52 lock sites match this shape.
- **Minority — multi-call transactions with `?` propagation under
  one lock.** Real, and the trait must accommodate them:
  - MIT-SHM `PutImage` does `clear_clip_rectangles()?` then
    `put_image()?` under one lock (`nested.rs:3137-3151`) so a
    stale clip-mask doesn't restrict the upload.
  - `ClearArea` does `clear_clip_rectangles()?` then either
    `copy_area()?` (bg-pixmap) or `fill_rectangle()?` (bg-pixel)
    under one lock (`nested.rs:7273-7300`).
  - Tiled-fill draws set GC state, draw, and reset-to-Solid under
    one lock (`nested.rs:7588`).
  - `ListFonts` / `ListFontsWithInfo` proxies hold the host lock
    while writing client replies (`nested.rs:7078`) — host
    streaming + client write are interleaved.
  These are genuine compositional patterns, not accidents. In C
  they should become *composite* trait methods
  (`Backend::put_image_with_clear`, `clear_area_with_bg`,
  `fill_with_state`) so the transaction stays inside the trait
  impl rather than being open-coded at every call site. Treating
  each leaf as a standalone trait method would force callers
  back into the multi-call shape and re-introduce the lock
  ordering as a public concern.
- Free functions in `host_x11::` (e.g. `connect_to_host`,
  `read_setup_reply`, `create_window`, `select_pointer_events_on_container`)
  are internal helpers with no callers outside the module — they
  vanish into the trait impl, not the trait surface.

Verdict on uniformity: **trait fits, with a documented set of
composite methods for the multi-call paths.** Not pre-work for B;
input to C's trait design.

### Resource model coupling

`resources.rs` embeds host XIDs in **16 named places** plus the
input-pump inverse map. Each is `u32` (or `Option<u32>`) — sometimes
behind a setter, sometimes a public field:

| Resource | Field | Access | Notes |
|---|---|---|---|
| `Window` | `host_xid: Option<u32>` | **`pub` field, direct assignment** at `nested.rs:268, 5114, 9545, 10100` | the mirror in the host tree |
| `Window` | `background_pixmap_host_xid: Option<u32>` | `pub` field | retained across `FreePixmap` |
| `Window` | `border_pixmap_host_xid: Option<u32>` | `pub` field | parallel to above |
| `Pixmap` | `host_xid: Option<u32>` | setter `set_pixmap_host_xid` | depth-1/24/32 backed |
| `Font` | `host_xid: u32` | `pub` field | always present |
| `Cursor` | `host_xid: Option<u32>` | setter `set_cursor_host_xid` | |
| `Visual` | `host_visual_xid: Option<u32>` | setter `set_visual_host_xid` | seeded at HostX11 init |
| `Colormap` | `host_colormap_xid: Option<u32>` | setter `set_colormap_host_xid` | ARGB seeded at init |
| `PictureState` | `host_picture_xid: u32`, `host_owned_pixmap: Option<u32>` | `pub` fields | RENDER |
| `GlyphSetState` | `host_glyphset_xid: u32` | `pub` field | RENDER |
| `ResourceTable` | `host_glyphset_refcounts: HashMap<u32, usize>` | private | ReferenceGlyphSet |
| `NamedCompositePixmap` | `host_pixmap: u32` | `pub` field | `Window.composite_named_pixmaps: Vec<...>` |
| `GcClipState::Pixmap` | `{ host_pixmap: u32, clip_x_origin, clip_y_origin }` | enum variant | derived per-draw from GC + Pixmap |
| `GcFillState::Tiled` | `{ host_pixmap: u32, tile_x_origin, tile_y_origin }` | enum variant | derived per-draw from GC + Pixmap |
| `HostDrawableTarget` | enum carries `host_xid: u32` (Window+Pixmap) | enum variant | resolution helper |
| `ReparentResult` | `host_xid: Option<u32>` | return value | returned from reparent |

Plus the inverse map for the input pump:
`HostXidMap = Arc<Mutex<HashMap<u32, ResourceId>>>` (`host_x11.rs:150`).

**Migration sketch — opaque `BackendHandle` per resource kind.**

The shape of `Option<u32>` fields keeps this refactor mostly
mechanical for the setter-routed slots; see the caveats below for
the `pub`-field assignments and derived enum variants. Replace
`u32` with kind-tagged newtypes:

```rust
pub struct WindowBackend(BackendId);    // BackendId is the backend's
pub struct PixmapBackend(BackendId);    // opaque cookie — u32 today,
pub struct FontBackend(BackendId);      // u64 or pointer in C
pub struct CursorBackend(BackendId);
pub struct VisualBackend(BackendId);
pub struct ColormapBackend(BackendId);
pub struct PictureBackend(BackendId);
pub struct GlyphSetBackend(BackendId);
```

Each existing field rewrites in one line:

```rust
// before
pub host_xid: Option<u32>,
// after
pub backend: Option<WindowBackend>,
```

Setters/getters translate one-for-one (`set_pixmap_host_xid` →
`set_pixmap_backend`). The `HostDrawableTarget` enum loses the raw
`u32` and carries `WindowBackend` / `PixmapBackend` directly. The
pump's `HostXidMap` becomes `BackendIdMap = HashMap<BackendId,
ResourceId>` keyed by the same opaque ID.

Verdict on resource coupling: **not a B blocker, real C work.**
The fields are centralized into 16 named slots; none participate
in arithmetic. But migration is *not* pure search-and-replace —
several `pub` fields are assigned directly (especially `Window.host_xid`
at four nested.rs sites) and would either need to gain setters
(`pub(crate)` + `set_*`) or accept that the public API surface
includes the new newtype. The derived enum variants
(`GcClipState::Pixmap`, `GcFillState::Tiled`) carry raw `u32`
host pixmaps materialized per-draw from a GC + Pixmap pair; they
need to be re-typed in lockstep with `Pixmap.host_xid`.
Realistic effort: 2–3 days of normalization (audit direct
assignments, route through setters, re-type the GC-state enums,
add NamedCompositePixmap to the migration), no design decisions
on the critical path. Stays a C concern.

### Cross-connection sync points

`nested.rs` rides on three live X11 connections to the host:

1. **Main** (`HostX11`, behind `Arc<Mutex<>>`) — every protocol
   handler calls in here. Single-threaded callers serialize via
   the `Mutex`.
2. **Pump** (`HostInputPump` thread) — reads host events and routes
   them via `xid_map` to client subscribers. Writes
   `ChangeWindowAttributes(ExposureMask)` to register/unregister
   per-window event masks.
3. **Per-client keyboard pump** (one thread per nested client).
   Selects `KeyPress | KeyRelease | StructureNotify` on the
   container. Pointer events are owned by (1)+(2) only because
   X11 ButtonPress is exclusive — see Phase 3.7's
   `select_keyboard_events` regression.

The dual main-vs-pump model creates one race: pump's
`CWA(ExposureMask)` on a freshly-created subwindow can arrive at
the host *before* main's `CreateWindow`, host returns `BadWindow`,
pump silently absorbs it, ExposureMask is lost. Phase 3.6's fix is
`sync_main_connection` (`host_x11.rs:1330`) — a synchronous
GetInputFocus round-trip on the main connection that fences
CreateWindow before pump's CWA. The doc comment at `host_x11.rs:1306-1315`
explicitly calls this out as a duct-tape fix:

> One round-trip per window-create is the price for the dual-connection
> model; fixing it cheaper means folding the pump and main connections
> into one (Phase 3.7+ work).

The `reply_buffer: Vec<HostResponse>` field (`host_x11.rs:69`) is a
related accommodation: out-of-order replies arriving on the main
connection during a sync are stashed and re-delivered. RENDER errors
interleaved with sync replies were the failure mode that motivated it.

**How the trait absorbs each sync point:**

| Sync point | Trait absorption |
|---|---|
| `host.lock()` per call | `&mut self` on every method; no special API |
| `sync_main_connection` fence | `Backend::sync(&mut self) -> io::Result<()>` — explicit, callable from anywhere a fence is needed |
| `reply_buffer` reordering | Internal to `Backend` impl; callers don't see it |
| Pump connection | `BackendEventSource` companion trait with `fd() + dispatch()` for epoll integration; collapses into the same fd as `Backend` if the impl chooses single-connection |
| Per-client keyboard pump | Either folded into `BackendEventSource` (preferred — one connection per backend) or remains an X-host-specific implementation detail. C/B don't have host X servers, so this collapses to nothing. |

**Critical observation for the C trajectory.** B is standalone
DRM/KMS — *no host X11 connection*, so the dual-connection race
does not exist. C ("real X clients on KMS") will be a single
backend (KMS+input+composite) running in-process; the pump/main
duality and the `sync_main_connection` fence dissolve naturally.
The trait can be designed around a single synchronous backend
without preserving the duct-tape. The X-host backend remains the
odd one out, and its `sync()` impl does the GetInputFocus
round-trip; every other backend is a no-op `sync()`.

### Verdict

**go for B.**

The verdict criteria from the plan-writing spike (above) are
specifically about whether the resource-model refactor must land
**before B starts**. B is the standalone DRM/KMS bootstrap
(this plan's 13 steps) and does not touch `yserver-core` at all
— the design's "Out of scope" line "Any X11 protocol code; any
`yserver-core` integration" applies. Nothing in this spike's
findings is a dependency of B.

For B specifically:
- Operation set: ~60–80 trait methods on `Backend` plus ~5 on
  `BackendEventSource`. Tractable for C, irrelevant to B.
- Call-sites: 52 host-lock sites; majority uniform single-call,
  minority multi-call transactions that become composite trait
  methods in C. Not pre-work for B.
- Resource coupling: 16 host-XID slots; mostly-mechanical
  re-typing (caveats: `pub`-field direct assignments + derived
  GC-state enums need lockstep changes) with two design choices
  (allocate-handle vs. internal alloc; composite vs. leaf trait
  methods). Not pre-work for B.
- Cross-connection sync: `sync_main_connection` duct-tape
  dissolves naturally for non-X-host backends. Not relevant to B.

### C-prework reading list (deferred, not gating B)

When the C plan/spec is drafted, this work should land *before*
extracting the `Backend` trait, not as part of the same change:

1. **Normalize the host-handle allocation model.** Either fold
   `allocate_xid` into `create_*` (preferred — atomic on success,
   no orphan handles) or formalize the two-phase pattern as
   explicit `Backend::allocate_handle(kind) → Backend::commit(handle)`
   with an explicit rollback path.
2. **Promote the multi-call transactions to composite trait
   methods.** `put_image_with_clear`, `clear_area_with_bg`,
   `fill_with_state`, `list_fonts_proxy` (full streaming, not
   "host call + client write under one lock"). This keeps the
   trait surface honest about which operations are leaves and
   which are sequences.
3. **Audit direct `pub` field assignments.** `Window.host_xid`
   is assigned in 4 sites; same pattern likely exists for
   `Font.host_xid`, `PictureState.*`, `GlyphSetState.host_glyphset_xid`,
   `NamedCompositePixmap.host_pixmap`. Either route them all
   through setters, or accept that the new `BackendHandle` newtypes
   are part of the public API.
4. **Re-type the GC-state enums in lockstep.** `GcClipState::Pixmap.host_pixmap`
   and `GcFillState::Tiled.host_pixmap` are derived per-draw and
   reference `Pixmap.host_xid`; their type must change with it.
5. **Decide the threading-model collapse.** C has no host X server,
   so the pump/main duality goes away. Decide up-front whether
   `BackendEventSource` is a separate trait or `Backend` itself
   exposes `fd() + dispatch()`. Affects whether C's KMS+input
   backend is one type or two.

None of (1)–(5) is a B blocker. All are C inputs.

No prework required before B. Proceeding with Step 2.
