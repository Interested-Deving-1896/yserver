# Phase 6 bootstrap — Implementation plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Replace the placeholder `crates/yserver/src/bin/yserver.rs` with a real DRM/KMS binary that boots in `virtme-ng`, sets a mode on virtio-gpu, runs a libinput-driven single-thread `epoll` loop, and paints a moving rectangle into a dumb-buffer swapchain.

**Architecture:** Single binary crate, modular internal layout (`drm/`, `input/`, `present/`). Single-thread `epoll` over `[libinput.fd, drm.fd, signalfd]` (signalfd added in Step 12); no Backend trait, no `dyn`, no generics. CPU-painted dumb buffers, atomic-commit-only KMS, libinput via udev seat0 (handled transitively by the `input` crate), root-only DRM master with no logind / VT-switch handling.

**Tech Stack:** Rust 2024 edition. Pinned crate versions (verified against crates.io 2026-05-02): `drm = "0.15"`, `input = "0.10"`, `nix = { version = "0.31", features = ["event", "fs", "ioctl", "mman", "poll", "signal"] }`, `signal-hook = "0.4"`. No direct `udev` dep — `input` crate's default `udev` feature handles enumeration. Existing workspace deps unchanged: `log`, `env_logger`, `libc`.

**Branch:** `phase6-bootstrap` (already created, design committed).

**Companion design:** `docs/superpowers/specs/2026-05-02-phase6-bootstrap-design.md`.

---

## Status

Not started. Steps 1–13 pending.

## Strategy

Each numbered Step below is one PR / one commit. The order is chosen so the binary compiles and runs at every step — early steps print log lines and exit; later steps add scanout, then input, then the loop. There is no step that leaves the binary half-broken.

Steps 1, 4, and 7 are pure-logic and are written test-first per the writing-plans TDD discipline (failing test → minimal impl → green → commit). Steps that are dominated by ioctls / FFI (2, 3, 5, 6, 8, 9, 10) are integration-tested via the vng smoke; unit testing them would either mock out the thing under test or require root — both worthless. The design doc explicitly notes this trade-off.

After every step, `cargo build --bin yserver` must succeed. `cargo +nightly fmt` and `cargo clippy --workspace --all-targets` must pass before commit.

## Step 1 — Spike: identify the future C Backend trait boundary

**Goal:** Confirm that a Backend trait *can* be carved out of `nested.rs` / `host_x11.rs` later without rewriting the X11 protocol layer. If the boundary is unfindable, B does not start — that's a Phase 6 prerequisite.

**Output:** A note appended to `docs/superpowers/specs/2026-05-02-phase6-bootstrap-design.md` under a new `## Spike findings — future C trait boundary` heading. The note must answer the four questions enumerated in the design's "Spike during plan-writing phase" section: operation set sketch, call-site uniformity, **resource model coupling and migration approach for opaque backend handles**, cross-connection sync points. A bare "findable" without those four items is not a passing verdict.

**Time budget:** 1–2 hours. If the spike runs longer than 2 hours and the boundary is still unclear, stop and write up the obstruction — that's the answer. **Do not refactor.** Reading-only.

**Files:**
- Read: `crates/yserver-core/src/resources.rs` (2860 lines — start here; the resource-model coupling is the most likely C blocker)
- Read: `crates/yserver-core/src/host_x11.rs` (3637 lines — public surface)
- Read: `crates/yserver-core/src/nested.rs` (10538 lines — sample, don't read end-to-end)
- Modify: `docs/superpowers/specs/2026-05-02-phase6-bootstrap-design.md` (append spike note)

**Step 1.1: Survey resource-model coupling first.** Run `grep -n "host_xid" crates/yserver-core/src/resources.rs` and note every type that embeds a host XID and every function that reads/writes one. This is the most likely blocker — sketch how each becomes an opaque backend handle in C (single `BackendHandle` newtype per resource kind, or a per-kind indirection table).

**Step 1.2: Skim `host_x11.rs` for the public surface.** Run `grep -n "^pub " crates/yserver-core/src/host_x11.rs` and note the top-level `pub fn` and `pub struct` items. That's a starting approximation of the trait operation set.

**Step 1.3: Find call-sites.** Run `grep -n "host_x11::\|host\." crates/yserver-core/src/nested.rs | wc -l` to size the call-site impact, and read ~20 of them across the file to confirm the calls are uniform-shaped (good — a trait will fit) vs. ad-hoc and intermixed with state mutation (bad — refactor pre-work needed).

**Step 1.4: Cross-connection sync points.** Grep for `sync_main_connection`, `pump`, and any place that crosses request/reply boundaries. Note each; the trait's threading model has to accommodate or replace them.

**Step 1.5: Write the note.** Append to the design doc:

```markdown
## Spike findings — future C trait boundary

### Operation set sketch
[trait method signatures]

### Call-site uniformity
[grep counts, sample observations, verdict — "uniform" or "ad-hoc"]

### Resource model coupling
[every host_xid field in resources.rs + sketch of opaque-handle migration]

### Cross-connection sync points
[list of points + how trait absorbs each]

### Verdict
[ go | go-with-prework | no-go ]
```

**Step 1.6: Commit.**

```bash
git add docs/superpowers/specs/2026-05-02-phase6-bootstrap-design.md
git commit -m "docs: Phase 6 bootstrap design — spike findings on future C trait boundary"
```

**Gate:** Note exists with all four sections populated. Verdict explicit. If verdict is `no-go`, **stop the plan here** and surface to user. If `go-with-prework`, **stop the plan here** and surface the prework as its own design/plan pair — B does not start until that lands.

---

## Step 2 — Skeleton: workspace deps + `lib.rs` + entry point

**Goal:** Reorganize the `yserver` binary crate to host a library module (`drm/`, `input/`, `present/` modules will land later, empty for now). The binary becomes a thin shell that calls `yserver::run()` which currently just logs and exits Ok. This step adds the workspace dependencies but does not yet *use* them.

**Files:**
- Modify: `Cargo.toml` (workspace) — add `drm`, `input`, `udev`, `nix` to `[workspace.dependencies]`.
- Modify: `crates/yserver/Cargo.toml` — depend on the new workspace deps; add `[lib]` entry; keep both `[[bin]]` entries.
- Create: `crates/yserver/src/lib.rs` — `pub mod drm; pub mod input; pub mod present; pub fn run() -> Result<()>`.
- Create: `crates/yserver/src/drm/mod.rs` — empty (just a module marker for now).
- Create: `crates/yserver/src/input/mod.rs` — empty.
- Create: `crates/yserver/src/present/mod.rs` — empty.
- Modify: `crates/yserver/src/bin/yserver.rs` — call `yserver::run()`.

**Step 2.1: Pin exact crate versions (verified against crates.io 2026-05-02).**

Add to root `Cargo.toml` `[workspace.dependencies]`:

```toml
drm = "0.15"
input = "0.10"
nix = { version = "0.31", features = ["event", "fs", "ioctl", "mman", "poll", "signal"] }
signal-hook = "0.4"
```

The `event` feature is required for `nix::sys::epoll::Epoll` and for `signalfd`. No direct `udev` dep — the `input` crate's default `udev` feature handles device enumeration. If `cargo update` reveals a newer compatible release, take it; if a major version change has happened, stop and re-validate Steps 5–9 against the new API surface before bumping.

**Step 2.2: Modify `crates/yserver/Cargo.toml`.**

```toml
[package]
name = "yserver"
version.workspace = true
edition.workspace = true
license.workspace = true
repository.workspace = true

[lib]
name = "yserver"
path = "src/lib.rs"

[dependencies]
yserver-core.workspace = true
env_logger.workspace = true
log.workspace = true
drm.workspace = true
input.workspace = true
nix.workspace = true
signal-hook.workspace = true

[[bin]]
name = "ynest"
path = "src/bin/ynest.rs"

[[bin]]
name = "yserver"
path = "src/bin/yserver.rs"
```

**Step 2.3: Create `crates/yserver/src/lib.rs`.**

```rust
pub mod drm;
pub mod input;
pub mod present;

use std::io;

pub fn run() -> io::Result<()> {
    log::info!("yserver: Phase 6 bootstrap — startup");
    log::info!("yserver: nothing implemented yet, exiting");
    Ok(())
}
```

**Step 2.4: Create empty module files.**

`crates/yserver/src/drm/mod.rs`, `crates/yserver/src/input/mod.rs`, `crates/yserver/src/present/mod.rs` each contain a single line:

```rust
// placeholder — populated in later steps
```

**Step 2.5: Replace `crates/yserver/src/bin/yserver.rs`.**

```rust
use std::process::ExitCode;

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    match yserver::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            log::error!("yserver: {err}");
            ExitCode::FAILURE
        }
    }
}
```

**Step 2.6: Build.**

Run: `cargo build --bin yserver`
Expected: clean build (with deprecation warnings from `drm`/`input`/`udev`/`nix` permissible).

**Step 2.7: Run on host (not vng — just confirm it links).**

Run: `cargo run --bin yserver`
Expected stderr: two `INFO` log lines and exit 0.

**Step 2.8: Commit.**

```bash
git add Cargo.toml Cargo.lock crates/yserver/Cargo.toml crates/yserver/src/lib.rs \
        crates/yserver/src/drm/mod.rs crates/yserver/src/input/mod.rs \
        crates/yserver/src/present/mod.rs crates/yserver/src/bin/yserver.rs
git commit -m "feat: Phase 6 Step 2 — yserver crate skeleton + workspace deps"
```

**Gate:** Build green. Binary runs, logs, exits 0. No clippy warnings introduced.

---

## Step 3 — `drm/`: open device, acquire master, RAII Drop

**Goal:** A `Device` type that opens `/dev/dri/card0`, calls `DRM_IOCTL_SET_MASTER`, and on `Drop` calls `DRM_IOCTL_DROP_MASTER` and closes the fd. `yserver::run()` constructs one, logs success, and exits.

**Files:**
- Create: `crates/yserver/src/drm/device.rs` — `Device { fd, ... }`, `Device::open(path)`, `Drop`.
- Modify: `crates/yserver/src/drm/mod.rs` — `pub mod device; pub use device::Device;`.
- Modify: `crates/yserver/src/lib.rs` — `run()` opens `/dev/dri/card0` and logs the device's basic capability info before returning.

**Step 3.1: Implementation sketch.** Use the `drm` crate's `Card` pattern; `Device` wraps a `std::fs::File` whose fd is the DRM fd. Master acquisition is `drm::control::Device::acquire_master_lock()` (or its named equivalent in 0.14 — confirm at coding time). On `Drop`, call `release_master_lock` and let the `File` drop close the fd.

**Step 3.2: Distinct error messages per the design's error principle.**

Match `io::Error` `kind()` values:
- `NotFound` → `"DRM device {path} not found — is virtio-gpu attached? In vng pass --graphics or --qemu-opts=\"-device virtio-gpu-pci\""`
- `PermissionDenied` → `"opening {path} requires root — vng runs as root by default; on host use sudo (but B is vng-only by design)"`
- `Other` with `EBUSY` raw_os_error → `"another DRM master holds {path} — B is vng-only; do not run yserver on a host with an active graphical session"`
- otherwise → `"failed to open {path}: {err}"`

**Step 3.3: Wire into `run()`.**

```rust
pub fn run() -> io::Result<()> {
    log::info!("yserver: Phase 6 bootstrap — startup");
    let device = drm::Device::open("/dev/dri/card0")?;
    log::info!("yserver: opened DRM device, master acquired");
    drop(device);
    log::info!("yserver: master released, exiting");
    Ok(())
}
```

**Step 3.4: Validate in vng.**

Run: `just yserver-headless` (graphics-off recipe — stdout reaches host).
Expected stderr in host terminal: three `INFO` lines, then vng exits.

**Step 3.5: Re-run idempotence check.**

Run `just yserver-headless` twice in a row. Expected: both runs succeed; second run does not see `EBUSY`.

**Step 3.6: Commit.**

**Gate:** Vng harness shows the three log lines. Re-run idempotent. `EBUSY` path is *not* exercised in vng; it's a defensive log message for the (rejected) bare-metal path.

---

## Step 4 — Mode pick policy (pure logic, TDD)

**Goal:** Implement `pick_mode(connector_modes: &[Mode]) -> Option<&Mode>` selecting the connector's preferred mode if any, else the first 1024×768@60 if available, else the first mode in the list, else None. This is pure logic, isolated for unit testing before any DRM ioctls touch it.

**Files:**
- Create: `crates/yserver/src/drm/modeset.rs` — the function + tests.
- Modify: `crates/yserver/src/drm/mod.rs` — `pub mod modeset;`.

**Step 4.1: Write the failing test first.**

```rust
// crates/yserver/src/drm/modeset.rs
#[cfg(test)]
mod tests {
    use super::*;

    fn mode(name: &str, w: u16, h: u16, refresh: u32, preferred: bool) -> Mode {
        Mode { name: name.into(), width: w, height: h, vrefresh: refresh, preferred }
    }

    #[test]
    fn picks_preferred_when_present() {
        let modes = vec![
            mode("800x600", 800, 600, 60, false),
            mode("1024x768", 1024, 768, 60, true),
            mode("1920x1080", 1920, 1080, 60, false),
        ];
        let picked = pick_mode(&modes).unwrap();
        assert_eq!(picked.name, "1024x768");
    }

    #[test]
    fn falls_back_to_1024x768_60_when_no_preferred() {
        let modes = vec![
            mode("800x600", 800, 600, 60, false),
            mode("1024x768", 1024, 768, 60, false),
            mode("1920x1080", 1920, 1080, 60, false),
        ];
        let picked = pick_mode(&modes).unwrap();
        assert_eq!(picked.name, "1024x768");
    }

    #[test]
    fn falls_back_to_first_when_no_preferred_and_no_1024x768() {
        let modes = vec![
            mode("800x600", 800, 600, 60, false),
            mode("1920x1080", 1920, 1080, 60, false),
        ];
        let picked = pick_mode(&modes).unwrap();
        assert_eq!(picked.name, "800x600");
    }

    #[test]
    fn empty_list_returns_none() {
        assert!(pick_mode(&[]).is_none());
    }
}
```

**Step 4.2: Run the failing test.**

Run: `cargo test -p yserver drm::modeset::tests`
Expected: build error — `Mode`, `pick_mode` undefined.

**Step 4.3: Minimal implementation.**

```rust
#[derive(Debug, Clone)]
pub struct Mode {
    pub name: String,
    pub width: u16,
    pub height: u16,
    pub vrefresh: u32,
    pub preferred: bool,
}

pub fn pick_mode(modes: &[Mode]) -> Option<&Mode> {
    if let Some(m) = modes.iter().find(|m| m.preferred) {
        return Some(m);
    }
    if let Some(m) = modes.iter().find(|m| m.width == 1024 && m.height == 768 && m.vrefresh == 60) {
        return Some(m);
    }
    modes.first()
}
```

**Step 4.4: Run tests.**

Run: `cargo test -p yserver drm::modeset::tests`
Expected: 4 passed.

**Step 4.5: Commit.**

**Gate:** All four tests green. The `Mode` struct here is a yserver-local type, distinct from `drm::control::Mode` in the crate; conversion happens in Step 5.

---

## Step 5 — DRM client capabilities + atomic modeset

**Goal:** Set the DRM client capabilities required for atomic modeset, dump connector/CRTC/plane property tables to log (so a future debugger has a record), then enumerate outputs, convert a mode through `pick_mode` from Step 4, and commit the modeset atomically. After this step, `run()` blanks the connected output.

**Files:**
- Modify: `crates/yserver/src/drm/device.rs` — add `enable_atomic_capabilities()` called from inside `Device::open` after master acquire.
- Modify: `crates/yserver/src/drm/modeset.rs` — add `Output { connector, crtc, plane, mode }`, `discover_output(device) -> io::Result<Output>`, `dump_properties(device, output)`, `commit_modeset(device, output, fb_id) -> io::Result<()>`.
- Modify: `crates/yserver/src/lib.rs` — between master-acquire and master-release: discover output, allocate one dumb-buffer-backed framebuffer, commit modeset showing it (cleared to a recognizable colour like `0xFF0080` magenta), sleep 2 seconds (temporary — replaced by event loop in Step 11), then exit.

**Step 5.0: Set client capabilities.** Call `device.set_client_capability(ClientCapability::Atomic, true)` and `device.set_client_capability(ClientCapability::UniversalPlanes, true)` immediately after master acquire. Failure on either is fatal with a message like `"DRM driver does not support atomic / universal planes — virtio-gpu in modern kernels supports both; check kernel and qemu-desktop versions"`. virtio-gpu in any kernel ≥5.x supports both; if this fires in vng it's an environment bug.

**Step 5.1:** `discover_output` walks the connector list, picks the first `Connected` connector, walks its encoder's `possible_crtcs` mask, picks the first CRTC, then enumerates planes whose `possible_crtcs` mask includes this CRTC and `type` property is `Primary`. Calls `pick_mode(&connector.modes())`. If no connector is connected, return `io::Error::other("no connected output — vng with --graphics required for modeset path; headless mode does not exercise this")`.

**Step 5.2: Dump properties for debugging.** `dump_properties` walks each of `output.{connector, crtc, plane}` and logs every property name + current value at `debug` level. Cost is one-time at startup; payoff is the ability to triage opaque atomic-commit `EINVAL`s without a kernel debugger.

**Step 5.3:** `commit_modeset` builds an `AtomicModeReq`. Look up properties by *name* (`drm` crate provides `find_prop_by_name` or equivalent) — connector's `CRTC_ID`, CRTC's `MODE_ID` (a property blob created from the picked mode) and `ACTIVE`, plane's `FB_ID`, `CRTC_ID`, `SRC_X/Y/W/H` (16.16 fixed-point), and `CRTC_X/Y/W/H` (integer pixels). Commit with `AtomicCommitFlags::AllowModeset`. Failure logs the rejected property set if available.

**Step 5.4:** Allocate a single dumb buffer using the `drm` crate's `Card::create_dumb_buffer(size, format, bpp)` (the type is exposed under `drm::control::dumbbuffer`, not `drm::buffer::DumbBuffer`). Format is `DRM_FORMAT_XRGB8888`, bpp is 32. Map via `Card::map_dumb_buffer`. Fill with magenta. Call `Card::add_framebuffer` to register the FB and pass its handle to `commit_modeset`.

**Step 5.5: Validate.**

Run: `just yserver` (graphics-on recipe — paints into the QEMU window).
Expected: QEMU window shows a solid magenta screen for ~2 seconds, then vng exits.
Expected stderr: capability set OK, property dump in debug logs (`RUST_LOG=debug just yserver` to see them), connector list, picked mode.

**Step 5.6: Commit.**

**Gate:** Magenta visible. Capability set succeeds. Property dump exists in debug logs. Run with `RUST_LOG=info just yserver` (default) does *not* dump properties — they're verbose. If `pick_mode` picks something other than `1024x768`, that is information — log it but do not fail.

---

## Step 6 — Dumb buffer wrapper

**Goal:** Extract the dumb-buffer + mmap + framebuffer-add lifecycle from Step 5's inline code into a `Buffer` type with RAII Drop that releases the FB and the dumb buffer.

**Files:**
- Create: `crates/yserver/src/drm/buffer.rs` — `Buffer { id, width, height, stride, ptr, len, fb_id }`, `Buffer::new(device, width, height) -> io::Result<Self>`, `Drop`.
- Modify: `crates/yserver/src/drm/mod.rs` — `pub mod buffer; pub use buffer::Buffer;`.
- Modify: `crates/yserver/src/lib.rs` — replace the inline buffer code with `Buffer::new`.

**Step 6.1:** Implement. `Buffer::new` calls the `drm` crate's `create_dumb_buffer` + `map_dumb_buffer` + `add_framebuffer`. `Drop` calls `destroy_framebuffer`, then unmaps, then destroys the dumb buffer (in that order — destroying the dumb buffer while a FB references it is a use-after-free; the kernel refcounts the BO so this would be silently survived but the order is the documented contract).

**Step 6.2:** Add a `pub fn fill(&mut self, pixel: u32)` helper that writes `pixel` to every 4-byte slot, plus `pub fn pixels_mut(&mut self) -> &mut [u32]` for raw access (used by Step 12's painter).

**Step 6.3:** Validate by re-running Step 5's vng smoke. Expected: same magenta as before. No leak (re-run idempotence still works).

**Step 6.4: Commit.**

**Gate:** vng smoke unchanged. Re-run idempotent.

---

## Step 7 — Swapchain state machine (pure logic, TDD)

**Goal:** A `Swapchain` wrapping N `Buffer`s with a state machine: each buffer is in one of `{Free, Acquired, Submitted, Scanout}`. `acquire()` finds a `Free` buffer and returns it as `Acquired`. `submit(idx)` flips `Acquired → Submitted`. `complete(idx)` flips `Submitted → Scanout` and the previous `Scanout → Free`. Pure-logic state machine — TDD it before wiring to actual DRM.

**Critical invariant:** After Step 5's initial modeset, buffer 0 is on scanout; the swapchain must be constructed with that buffer marked `Scanout`, not `Free`. Otherwise the painter could acquire buffer 0 and tear over the live framebuffer between the modeset and the first page-flip completion.

**Files:**
- Create: `crates/yserver/src/drm/swapchain.rs` — type + state machine + tests; no DRM calls yet.
- Modify: `crates/yserver/src/drm/mod.rs` — `pub mod swapchain; pub use swapchain::Swapchain;`.

**Step 7.1: Write the failing test first.**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_swapchain_has_all_free() {
        let s = SwapState::new(3);
        assert_eq!(s.count_free(), 3);
    }

    #[test]
    fn with_initial_scanout_marks_buffer_busy() {
        let s = SwapState::with_initial_scanout(3, 0);
        assert_eq!(s.count_free(), 2);
        assert_eq!(s.count_scanout(), 1);
    }

    #[test]
    fn acquire_never_returns_initial_scanout_buffer() {
        let mut s = SwapState::with_initial_scanout(2, 0);
        let i = s.acquire().unwrap();
        assert_ne!(i, 0);
    }

    #[test]
    fn acquire_then_submit_then_complete_advances_state() {
        let mut s = SwapState::new(3);
        let i = s.acquire().unwrap();
        s.submit(i).unwrap();
        s.complete(i).unwrap();
        // i is now Scanout; remaining 2 are Free; nothing is Acquired or Submitted
        assert_eq!(s.count_free(), 2);
        assert_eq!(s.count_scanout(), 1);
    }

    #[test]
    fn second_complete_releases_first() {
        let mut s = SwapState::new(3);
        let a = s.acquire().unwrap();
        s.submit(a).unwrap();
        s.complete(a).unwrap();
        let b = s.acquire().unwrap();
        s.submit(b).unwrap();
        s.complete(b).unwrap();
        // a is back to Free; b is Scanout
        assert_eq!(s.count_free(), 2);
        assert_eq!(s.count_scanout(), 1);
    }

    #[test]
    fn acquire_returns_none_when_all_busy() {
        let mut s = SwapState::new(2);
        let _a = s.acquire().unwrap();
        let _b = s.acquire().unwrap();
        assert!(s.acquire().is_none());
    }

    #[test]
    fn submit_unacquired_buffer_errors() {
        let mut s = SwapState::new(2);
        assert!(s.submit(0).is_err());
    }

    #[test]
    fn complete_unsubmitted_buffer_errors() {
        let mut s = SwapState::new(2);
        let i = s.acquire().unwrap();
        assert!(s.complete(i).is_err());
    }
}
```

**Step 7.2:** Run failing test, expect compile error.

**Step 7.3:** Minimal implementation — `SwapState` with `Vec<BufferState>` where `BufferState` is the four-state enum. `count_free` / `count_scanout` walk the vec. `acquire` finds first Free, transitions. Three transitions are wrapped in `Result<(), &'static str>` for error cases.

**Step 7.4:** Run tests, expect 6 passed.

**Step 7.5:** Implement `Swapchain` struct that owns `[Buffer; N]` (use `Vec<Buffer>` since N is runtime in B; pick N=2 in B for double-buffering) plus a `SwapState`. Methods `acquire() -> Option<&mut Buffer>`, `submit(idx)`, `complete(idx)` that delegate to `SwapState` and return buffer references appropriately.

**Step 7.6: Commit.**

**Gate:** All tests green. `Swapchain` compiles but isn't yet wired into `run()`.

---

## Step 8 — Page-flip event handling

**Goal:** Submit an atomic page flip via `commit_with_flags(PageFlipEvent | Nonblock)`. Read DRM events from `device.fd` via `drm::control::Device::receive_events()` (or equivalent) when readable; for each completion event call `swapchain.complete(idx)`. The `idx` is encoded in the page-flip event's `user_data`.

**Files:**
- Create: `crates/yserver/src/drm/page_flip.rs` — `submit_flip(device, output, buffer, user_data) -> io::Result<()>`, `drain_events(device, on_complete: impl FnMut(u64))`.
- Modify: `crates/yserver/src/drm/mod.rs` — `pub mod page_flip;`.
- Modify: `crates/yserver/src/lib.rs` — replace Step 5's modeset-with-fixed-buffer + sleep with: initial modeset, then loop submitting flips with the swapchain (alternating between the two buffers), reading DRM events to release them, sleeping for 5 seconds total. Painter still produces the same magenta — flipping is observable only via log lines for now.

**Step 8.1:** Implement `submit_flip`. The atomic commit replaces the primary plane's FB id only (CRTC and mode unchanged). Set the `DRM_MODE_PAGE_FLIP_EVENT` flag and `DRM_MODE_ATOMIC_NONBLOCK`. Use the buffer's swapchain index (cast to `u64`) as user_data.

**Step 8.2:** Implement `drain_events`. The `drm` crate exposes the kernel event stream via a method on `Device` — call it, iterate, dispatch on event type. PageFlip events carry the user_data we set; pass it back via the closure.

**Step 8.3:** In `lib.rs`, allocate `Swapchain` of 2 buffers. Modeset with buffer 0. Then loop for ~5 seconds:
- Submit flip with buffer 1.
- Block on `device.fd` becoming readable (use `nix::poll::poll` with a 1s timeout).
- `drain_events` to handle completion.
- Acquire next buffer, submit, repeat.

(This is intentionally simplistic and synchronous. The real epoll-driven loop arrives in Step 11.)

**Step 8.4: Validate.**

Run: `just yserver`. Expected: magenta visible for ~5s, log lines show "submitted flip [0|1]" and "completion for [0|1]" alternating roughly at the connector's refresh rate (60Hz on virtio-gpu typically), vng exits.

**Step 8.5: Commit.**

**Gate:** Flip + completion log cadence matches refresh rate within ~10%. No "buffer in unexpected state" panics from the swapchain state machine.

---

## Step 9 — libinput context + dispatch

**Goal:** Open libinput against udev seat0, enumerate input devices, and dispatch events with no thread of its own. Add `input::Context::new()`, `input::Context::fd() -> RawFd`, `input::Context::dispatch() -> Vec<InputEvent>` where `InputEvent` is a yserver-local enum (`KeyPress { keycode }`, `KeyRelease { keycode }`, `PointerMotion { dx, dy }`, `Button { code, pressed }` — keep the variant set minimal). **Keycodes only** — keysyms require xkbcommon, which is C's job.

**Files:**
- Create: `crates/yserver/src/input/context.rs` — wraps `input::Libinput`.
- Create: `crates/yserver/src/input/event.rs` — yserver-local `InputEvent` enum.
- Modify: `crates/yserver/src/input/mod.rs` — pub mod, re-exports.
- Modify: `crates/yserver/src/lib.rs` — Step 8's loop now also pumps `input::Context::dispatch()` once per iteration and logs events to stderr.

**Step 9.1:** Use `input::Libinput::new_with_udev(LibinputInterface)`. The `LibinputInterface::open_restricted(path, flags)` callback receives the flags libinput wants (read or read/write, blocking or non-blocking). **Honour them.** Use `OpenOptions::new()` with `OpenOptionsExt::custom_flags(flags)`, then derive read/write from `flags & O_ACCMODE`. Forcing `O_RDWR | O_NONBLOCK` regardless can fail on read-only devices and diverges from the libinput contract.

**Step 9.2:** Subscribe to seat `seat0`: `lib.udev_assign_seat("seat0")`.

**Step 9.3:** `dispatch()` calls `lib.dispatch()` and iterates events (the `input` 0.10 API yields `Event` values — confirm the iteration shape against `docs.rs/input/0.10` at coding time). For B, only translate Keyboard, Pointer Motion, Pointer Button. Keyboard events expose a Linux input keycode (`KeyboardEvent::key()` returns `u32`); emit it as-is. Ignore touch / tablet / gesture / switch.

**Step 9.4:** Wire into `lib.rs`'s loop — log every event. Validate in vng.

Run: `just yserver` and click + type in the QEMU window.

Expected log lines: `pointer motion dx=… dy=…`, `button 0x110 press`, `key keycode=… press` interleaved with the flip log.

**Step 9.5: Commit.**

**Gate:** Mouse + keyboard generate log lines. No crashes on devices we don't translate (touch etc — they should be silently dropped).

---

## Step 10 — Throwaway painter (pure logic)

**Goal:** A small `present::paint` module with a `State { rect_x: f32, rect_y: f32, vel_x: f32, vel_y: f32, cursor_x: f32, cursor_y: f32 }` and `update(state, dt, events)`/`paint(state, buffer)` functions. State advances by velocity; mouse motion moves cursor; rectangle bounces off framebuffer edges. Paint is a 60×60 magenta rectangle at `(rect_x, rect_y)` and a 4×4 white cursor at `(cursor_x, cursor_y)`.

**Files:**
- Create: `crates/yserver/src/present/state.rs` — `State`, `update`.
- Create: `crates/yserver/src/present/paint.rs` — `paint(&State, &mut Buffer)`.
- Modify: `crates/yserver/src/present/mod.rs` — pub mod, re-exports.

**Step 10.1: Test-drive `update`.**

```rust
#[test]
fn update_advances_position_by_velocity() {
    let mut s = State { rect_x: 0.0, rect_y: 0.0, vel_x: 100.0, vel_y: 50.0, cursor_x: 0.0, cursor_y: 0.0 };
    update(&mut s, 0.1, &[], 1024, 768);
    assert!((s.rect_x - 10.0).abs() < 1e-3);
    assert!((s.rect_y - 5.0).abs() < 1e-3);
}

#[test]
fn rect_bounces_off_right_edge() {
    let mut s = State { rect_x: 1020.0, rect_y: 0.0, vel_x: 100.0, vel_y: 0.0, cursor_x: 0.0, cursor_y: 0.0 };
    update(&mut s, 0.1, &[], 1024, 768);
    assert!(s.vel_x < 0.0, "velocity should flip negative on right-edge bounce");
}

#[test]
fn pointer_motion_moves_cursor() {
    let mut s = State::default();
    update(&mut s, 0.0, &[InputEvent::PointerMotion { dx: 5.0, dy: 3.0 }], 1024, 768);
    assert_eq!(s.cursor_x, 5.0);
    assert_eq!(s.cursor_y, 3.0);
}
```

**Step 10.2:** Run failing tests, implement minimal logic, run green, commit.

**Step 10.3:** `paint` is a tight loop over the buffer writing pixel words. Magenta for the rect, white for the cursor, dark grey background. No fancy clipping — just bounds-check before write.

**Step 10.4:** Wire `paint` into `lib.rs`'s loop body before the flip submit. Validate visually.

Run: `just yserver`. Expected: a magenta rectangle bounces around the screen. Mouse motion in the QEMU window moves a small white square.

**Step 10.5: Commit.**

**Gate:** Visual confirms motion + bounce + cursor follows mouse. Tests green.

---

## Step 11 — Single-thread `epoll` event loop (continuous animation)

**Goal:** Replace Step 8's synchronous `nix::poll` + sleep with the design's single-thread `epoll` loop over `[libinput.fd, drm.fd]` (signalfd added in Step 12). For B, **animation is always dirty**: after every page-flip completion, the loop computes `dt`, advances rectangle state, paints the next free buffer, and submits a flip. CPU is proportional to refresh rate, not to input — that's correct for an always-animating painter.

```
loop:
  epoll_wait(timeout = -1)        # block until something is ready
  drain readable fds:
    libinput → input events → cursor.{x,y} += dt * (dx, dy)
    drm      → page-flip completion → swapchain.complete(idx)
                                    → if free buffer:
                                        dt = now - last_flip_submit
                                        update_rect(state, dt)
                                        paint(buf)
                                        atomic_commit(buf)
  if running flag cleared → break
```

The first flip is submitted from `lib.rs` before entering the loop; thereafter each completion drives the next.

**Files:**
- Create: `crates/yserver/src/present/loop.rs` — `run_loop(device, output, swapchain, input_ctx, running: &AtomicBool) -> io::Result<()>`.
- Modify: `crates/yserver/src/present/mod.rs` — pub mod, re-export.
- Modify: `crates/yserver/src/lib.rs` — `run()` constructs everything, submits the first flip, then delegates to `run_loop`. Remove the temporary 5-second cutoff.

**Step 11.1:** Use `nix::sys::epoll::Epoll` (`event` feature on `nix`). Add `libinput_ctx.fd()` and `device.as_raw_fd()` with `EpollFlags::EPOLLIN`.

**Step 11.2:** Loop:
- `epoll_wait` with `-1` (block forever — animation is event-driven via flip completion).
- For each ready fd, dispatch.
- After a drm page-flip completion: compute `dt = now - last_flip_submit_time`, advance state, if a free buffer exists then paint+submit.
- If `running.load(Ordering::Acquire) == false` → break.

**Step 11.3:** Use `std::time::Instant` for monotonic `dt`.

**Step 11.4:** Validate.

Run: `just yserver`. Expected: smooth ~60 fps motion, runs until killed. Mouse motion moves the cursor stand-in immediately; rectangle motion is independent.

**Step 11.5: Commit.**

**Gate:** Smooth animation at ~refresh rate. The rectangle continues moving when no input is happening (this is the *correct* behaviour for an always-animating painter — the previous low-idle-CPU gate was inconsistent with continuous animation and has been removed). Killing the QEMU window ends the program.

---

## Step 12 — Signal handling, clean shutdown

**Goal:** Add `signalfd` to the epoll set as the third fd so SIGINT / SIGTERM are observed synchronously in normal control flow (no async signal handler, no flag polling, no bounded epoll timeout). On break, **atomic-disable the plane and CRTC before dropping buffers**, then RAII unwinds: swapchain drops (each `Buffer` releases its FB and dumb buffer), device drops (releases master). This sequence prevents the kernel from holding a framebuffer reference while we destroy it — even though the kernel refcounts and survives, the explicit atomic-disable is the documented contract.

**Files:**
- Modify: `crates/yserver/src/present/loop.rs` — add signalfd to epoll, dispatch a third event source. The flag still exists (`signal_hook::flag::register` keeps it pointing at our `Arc<AtomicBool>` for clean shutdown if signalfd dispatch ever misses); the signalfd is the *primary* shutdown signal.
- Modify: `crates/yserver/src/drm/modeset.rs` — add `disable_output(device, output)` that atomic-commits `CRTC_ID=0` on the plane and `ACTIVE=false` on the CRTC.
- Modify: `crates/yserver/src/lib.rs` — block SIGINT/SIGTERM in main thread via `nix::sys::signal::sigprocmask`, create the signalfd, share with loop. After loop exit, call `disable_output` before swapchain/device drop.

**Step 12.1:** Block SIGINT and SIGTERM via `pthread_sigmask` (so they reach signalfd, not the default handler). Create the signalfd via `nix::sys::signalfd::SignalFd::new(&mask)`.

**Step 12.2:** Add the signalfd to epoll. When it fires, read one `siginfo_t`, log the signal name, set `running = false`, fall through to break.

**Step 12.3:** After the loop returns, call `disable_output(device, output)`. Log success or the rejected property set on failure (failure should not abort shutdown — keep going so RAII still runs).

**Step 12.4:** Validate. Send SIGTERM from inside the vng guest (`pkill yserver`) or let vng's exit signal propagate.

Expected log sequence:
- `received SIGTERM via signalfd`
- `exiting loop`
- `disabling plane + CRTC`
- swapchain Drop logs (one per buffer): `framebuffer N destroyed`
- device Drop log: `DRM master released`

**Step 12.5:** Re-run idempotence: confirm Step 3.5's idempotence still holds with the new shutdown path.

**Step 12.6: Commit.**

**Gate:** Full log sequence on every shutdown. Re-run idempotent. If the `disable_output` atomic commit ever fails, the rejected property is logged — that's the failure mode worth seeing.

---

## Step 13 — Validation pass + status.md update

**Goal:** Run through the design's validation checklist and write up the result in `docs/status.md` under a new Phase 6 section. No code changes (or only trivial fixes surfaced by validation).

**Files:**
- Modify: `docs/status.md` — replace the existing "Phase 6 — Standalone DRM/KMS" stub with a real status entry.

**Step 13.1: Run the design's validation checklist.**

- `just yserver` — visual smoke. Capture a `screendump` from QEMU monitor for the doc.
- `just yserver-headless` — log lines smoke. Expected: device-open / master-acquired / capability-set / property-dump (with `RUST_LOG=debug`) / connector-list, then a non-zero exit with `"no connected output"` because the headless recipe has no display backend. **That non-zero exit is success for headless** — the path that exercises modeset is `just yserver`, not `just yserver-headless`.
- `just yserver` twice consecutively — idempotence.

**Step 13.2:** If anything fails, fix it on this branch before writing the status entry. Each fix is its own commit.

**Step 13.3:** Update `docs/status.md`.

The Phase 6 section should mirror the existing Phase 3.x sections in style:
- A "Phase 6.1 — DRM/KMS bootstrap (in progress / complete)" heading
- "Goal" paragraph
- "Landed" bullet list per step
- "Validation" subsection with what was confirmed
- "Phase 6.1 follow-ups" — list of explicit out-of-scope items deferred to later (hotplug, multi-output, GBM/EGL, logind, VT, bare-metal, etc).

**Step 13.4:** Commit.

```bash
git add docs/status.md
git commit -m "docs: status.md — Phase 6.1 DRM/KMS bootstrap landed"
```

**Step 13.5:** Push the branch.

```bash
GIT_SSH_COMMAND="ssh -F /home/jos/realhome/Projects/dotfiles/ssh/config -o UserKnownHostsFile=/home/jos/realhome/.ssh/known_hosts" git push -u origin phase6-bootstrap
```

**Gate:** Status doc reflects reality. Branch pushed.

---

## Notes for the executor

- **Step 1's verdict can stop the plan.** If the spike returns `go-with-prework` or `no-go`, do not proceed to Step 2. Surface the verdict and the prework spec/plan to the user.
- **Re-run idempotence is the cheapest leak detector.** If Step N's vng smoke works the first time and `EBUSY`s the second, you leaked DRM master in some error path. Find it before moving on.
- **`drm` crate API names are sketches.** This plan references `Card::create_dumb_buffer`, `add_framebuffer`, `find_prop_by_name`, `set_client_capability`, etc. The `drm` 0.15 API may have slightly different exact names — check `docs.rs/drm/0.15` at coding time and adjust. The *shapes* (what to call, in what order) are correct; the *names* may need a one-character fix.
- **Don't optimize the painter.** A naive `for pixel in buffer.pixels_mut()` is plenty fast for 1024×768 at 60 Hz on virtio-gpu. SIMD / damage tracking belongs in C.
- **If `cargo clippy` flags lifetime/borrow noise from the `drm` or `input` crates' API shapes, accept them locally with `#[allow(clippy::…)]` rather than restructuring.** Those crates' APIs are what they are.
- **Don't preserve "todo" comments.** Throwaway code is throwaway: paint.rs is going to be deleted in C. No need to mark every line.
- **vng boot is fast after the first run.** First boot decompresses the kernel; subsequent boots are seconds. Don't be tempted to `--keep-running` or skip vng during the inner dev loop unless you're testing pure-logic code (in which case `cargo test` on the host is enough).
