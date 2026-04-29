# Nested RANDR Compatibility Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a minimal RANDR extension implementation for `ynest` so simple
window managers that probe RANDR can start. The first cut exposes one connected
output, one CRTC, and one mode matching the nested screen size.

**Architecture:** Keep extension protocol parsing/encoding in
`yserver-protocol`; keep synthetic RANDR display state in `yserver-core`; route
extension requests through a dedicated extension dispatcher rather than adding
RANDR minor opcodes to the core opcode table. No host RANDR calls are needed for
the first cut.

**Spec:** [`docs/superpowers/specs/2026-04-29-nested-randr-compat-design.md`](../specs/2026-04-29-nested-randr-compat-design.md).

**Project conventions:**

```sh
RUSTC_WRAPPER= cargo fmt --all -- --check
RUSTC_WRAPPER= cargo check --workspace
RUSTC_WRAPPER= cargo test --workspace
RUSTC_WRAPPER= cargo clippy --workspace
```

Manual validation is required with `xrandr -q` and `fvwm3` against `ynest`.

---

## File Structure

| Path | Status | Purpose |
|---|---|---|
| `crates/yserver-protocol/src/x11/mod.rs` | modify | Extension registry constants and `QueryExtension` integration if this is where extension lookup currently lives |
| `crates/yserver-protocol/src/x11/randr.rs` | add | RANDR request parsers, reply encoders, constants, and wire tests |
| `crates/yserver-protocol/src/x11/mod.rs` | modify | Export the `randr` module |
| `crates/yserver-core/src/randr.rs` | add | One-output nested RANDR state and reply-building helpers |
| `crates/yserver-core/src/lib.rs` | modify | Export the core RANDR module |
| `crates/yserver-core/src/nested.rs` | modify | Advertise RANDR and route extension requests to the RANDR handler |
| `crates/yserver-core/src/server.rs` or `resources.rs` | modify if needed | Store display/RANDR state if existing server state is the right ownership point |
| `docs/status.md` | modify | Mark Phase 2 RANDR task complete only after `xrandr -q` and `fvwm3` validation |

The implementation is five compile-safe commits:

1. **Protocol wire module** — RANDR constants, parsers, encoders, and tests.
2. **Extension registry** — advertise `RANDR` with stable major/event/error bases.
3. **Core one-output state** — model nested output/CRTC/mode and timestamps.
4. **Nested dispatcher** — route RANDR minor opcodes and emit replies/errors.
5. **Smoke validation and docs** — run clients, tune compatibility, update status.

---

## Commit 1 — Protocol Wire Module

### Task 1.1: Add RANDR module skeleton

**Files:**
- Add: `crates/yserver-protocol/src/x11/randr.rs`
- Modify: `crates/yserver-protocol/src/x11/mod.rs`

- [ ] **Step 1: Add `pub mod randr;` from `x11/mod.rs`.**

- [ ] **Step 2: Define supported version constants.**

Use RANDR 1.2 for the compatibility stub unless a tested client requires a
higher advertised version.

```rust
pub const MAJOR_VERSION: u32 = 1;
pub const MINOR_VERSION: u32 = 2;
```

- [ ] **Step 3: Define minor opcode constants for the first cut.**

Include at least:

- `RR_QUERY_VERSION`
- `RR_SET_SCREEN_CONFIG` (for explicit unsupported handling)
- `RR_SELECT_INPUT`
- `RR_GET_SCREEN_INFO` if legacy probing appears
- `RR_GET_SCREEN_SIZE_RANGE`
- `RR_GET_SCREEN_RESOURCES_CURRENT`
- `RR_GET_OUTPUT_INFO`
- `RR_GET_CRTC_INFO`
- `RR_SET_CRTC_CONFIG` (for explicit unsupported handling)

Use the official RANDR minor opcode values, not local arbitrary values.

### Task 1.2: Add request parsers

**Files:**
- Modify: `crates/yserver-protocol/src/x11/randr.rs`

- [ ] **Step 1: Add parser types.**

```rust
pub struct QueryVersionRequest {
    pub major: u32,
    pub minor: u32,
}

pub struct ScreenRequest {
    pub window: ResourceId,
}

pub struct OutputRequest {
    pub output: u32,
    pub config_timestamp: u32,
}

pub struct CrtcRequest {
    pub crtc: u32,
    pub config_timestamp: u32,
}

pub struct SelectInputRequest {
    pub window: ResourceId,
    pub enable: u16,
}
```

- [ ] **Step 2: Parse fixed fields defensively.**

Return `None` on short bodies. Do not allocate in parsers except where a future
request needs variable payload.

- [ ] **Step 3: Add parser tests.**

Cover valid little-endian bodies and short-body failures.

### Task 1.3: Add reply encoders

**Files:**
- Modify: `crates/yserver-protocol/src/x11/randr.rs`

- [ ] **Step 1: Add pure reply data structs.**

Suggested structs:

```rust
pub struct ModeInfo {
    pub id: u32,
    pub width: u16,
    pub height: u16,
    pub dot_clock: u32,
    pub hsync_start: u16,
    pub hsync_end: u16,
    pub htotal: u16,
    pub hskew: u16,
    pub vsync_start: u16,
    pub vsync_end: u16,
    pub vtotal: u16,
    pub name: String,
}

pub struct ScreenResources {
    pub timestamp: u32,
    pub config_timestamp: u32,
    pub crtcs: Vec<u32>,
    pub outputs: Vec<u32>,
    pub modes: Vec<ModeInfo>,
}
```

Keep owned `Vec`s acceptable for the first cut; this path is not hot.

- [ ] **Step 2: Implement encoders for required replies.**

Add functions:

- `write_query_version_reply`
- `write_get_screen_size_range_reply`
- `write_get_screen_resources_current_reply`
- `write_get_output_info_reply`
- `write_get_crtc_info_reply`

Each function takes `ClientByteOrder`, `SequenceNumber`, and a `Write` target or
`Vec<u8>` buffer consistent with existing protocol style.

- [ ] **Step 3: Encode alignment correctly.**

Validate:

- 32-byte reply header base.
- Reply length is in 4-byte units after the first 32 bytes.
- Output names are padded to 4 bytes.
- Mode names are concatenated and padded according to RANDR layout.

- [ ] **Step 4: Add wire tests.**

Assert:

- Reply code is `1`.
- Sequence is written in client byte order.
- Reply length matches actual payload size.
- Counts match array lengths.
- Total byte length is `32 + reply_length * 4`.

Run:

```sh
RUSTC_WRAPPER= cargo test -p yserver-protocol randr
```

---

## Commit 2 — Extension Registry

### Task 2.1: Locate current `QueryExtension` implementation

**Files:**
- Inspect: `crates/yserver-core/src/nested.rs`
- Inspect: `crates/yserver-protocol/src/x11/mod.rs`

- [ ] **Step 1: Identify how `BIG-REQUESTS` / `XKEYBOARD` are currently answered.**

Current behavior likely hardcodes absent/present replies in the core request
handler. Replace or extend this with a small registry rather than adding more
string branches.

### Task 2.2: Add extension metadata

**Files:**
- Modify: protocol/core location chosen in Task 2.1

- [ ] **Step 1: Define stable bases.**

Example:

```rust
pub const RANDR_MAJOR_OPCODE: u8 = 128;
pub const RANDR_FIRST_EVENT: u8 = 89;
pub const RANDR_FIRST_ERROR: u8 = 147;
```

Use values that do not collide with existing extension bases. If no extensions
are present yet, these conventional low extension values are fine.

- [ ] **Step 2: Return present for `RANDR`.**

`QueryExtension("RANDR")` must return:

- present = true
- major opcode = `RANDR_MAJOR_OPCODE`
- first event = `RANDR_FIRST_EVENT`
- first error = `RANDR_FIRST_ERROR`

- [ ] **Step 3: Keep unsupported extensions absent.**

Do not accidentally advertise `XKEYBOARD`, `RENDER`, `SHAPE`, or `XInput2`.

- [ ] **Step 4: Add tests if extension lookup is pure.**

At minimum test `RANDR` present and an unknown extension absent.

---

## Commit 3 — Core One-Output State

### Task 3.1: Add nested RANDR state

**Files:**
- Add: `crates/yserver-core/src/randr.rs`
- Modify: `crates/yserver-core/src/lib.rs`
- Modify: `crates/yserver-core/src/server.rs` or `resources.rs`

- [ ] **Step 1: Add state struct.**

```rust
pub struct RandrState {
    pub timestamp: u32,
    pub config_timestamp: u32,
    pub screen_width: u16,
    pub screen_height: u16,
    pub output_id: u32,
    pub crtc_id: u32,
    pub mode_id: u32,
}
```

- [ ] **Step 2: Add constructor for nested mode.**

```rust
impl RandrState {
    pub fn nested(width: u16, height: u16) -> Self { ... }
}
```

Use stable non-zero ids, for example:

- output id: `1`
- crtc id: `2`
- mode id: `3`

These ids are RANDR namespace ids, not client resource ids.

- [ ] **Step 3: Derive physical size.**

For 96 DPI:

```text
mm = pixels * 25.4 / 96
```

Clamp to at least 1 mm.

### Task 3.2: Build reply data from state

**Files:**
- Modify: `crates/yserver-core/src/randr.rs`

- [ ] **Step 1: Add methods returning protocol reply structs.**

Methods:

- `screen_size_range()`
- `screen_resources_current()`
- `output_info(output_id, config_timestamp)`
- `crtc_info(crtc_id, config_timestamp)`

- [ ] **Step 2: Return `None` for unknown synthetic ids.**

Unknown output/CRTC/mode handling should be converted to RANDR-specific errors
later. For the first cut, the dispatcher can translate `None` to `BadValue`.

- [ ] **Step 3: Use a plausible mode timing.**

For 60 Hz compatibility, it is acceptable to use simple synthetic timing:

- dot clock roughly `width * height * 60`
- sync/total values greater than active width/height

Clients generally care more about width/height/name than precise timings in
this path.

### Task 3.3: Store state in server

**Files:**
- Modify: `crates/yserver-core/src/server.rs` or `resources.rs`
- Modify: server initialization path in `nested.rs` if needed

- [ ] **Step 1: Add `randr: RandrState` to the central state.**

Initialize it from the same root dimensions used in setup/root window creation.

- [ ] **Step 2: Keep state immutable initially.**

Do not wire host resize yet. This avoids partial resize behavior that may be
worse than a stable fixed screen.

- [ ] **Step 3: Add unit tests.**

Test one-output ids, dimensions, physical size, and unknown-id `None`.

---

## Commit 4 — Nested Dispatcher

### Task 4.1: Route extension major opcode

**Files:**
- Modify: `crates/yserver-core/src/nested.rs`

- [ ] **Step 1: Detect `RANDR_MAJOR_OPCODE` before core opcode matching falls through.**

The core request header opcode should route to:

```rust
handle_randr_request(...)
```

The RANDR minor opcode is `header.data`.

- [ ] **Step 2: Preserve sequence/logging behavior.**

Log as:

```text
client N #SEQ RANDR::<name>
```

This is important for diagnosing future WM probes.

### Task 4.2: Implement RANDR request handler

**Files:**
- Modify: `crates/yserver-core/src/nested.rs`
- Modify: `crates/yserver-core/src/randr.rs` if helper placement is cleaner

- [ ] **Step 1: `RRQueryVersion`.**

Parse requested version and reply with supported version. A client asking for
`1.5` should still get `1.2` if that is the advertised implementation.

- [ ] **Step 2: `RRGetScreenSizeRange`.**

Return current nested size as minimum and either current size or a large safe
maximum. Prefer current size/current size for first cut unless a client expects
resizability.

- [ ] **Step 3: `RRGetScreenResourcesCurrent`.**

Return one CRTC, one output, one mode, and one mode name.

- [ ] **Step 4: `RRGetOutputInfo`.**

Return connected output `ynest-0`, bound to the single CRTC and mode.

- [ ] **Step 5: `RRGetCrtcInfo`.**

Return `(0, 0)`, current dimensions, current mode, normal rotation, and the
single output.

- [ ] **Step 6: `RRSelectInput`.**

Accept and ignore initially. This lets clients subscribe without failure. Add
a TODO to store masks before resize events are implemented.

- [ ] **Step 7: Unsupported setters.**

For `RRSetScreenConfig`, `RRSetCrtcConfig`, and unimplemented minors:

- Return a protocol error if client behavior tolerates it.
- If a real WM exits on the error, switch specific setters to benign replies
  that preserve current configuration.

Document whichever behavior is chosen in `status.md`.

### Task 4.3: Error handling

**Files:**
- Modify: `crates/yserver-core/src/nested.rs`
- Modify: `crates/yserver-protocol/src/x11/randr.rs` if needed

- [ ] **Step 1: Use extension-specific errors if easy.**

RANDR defines `BadRROutput`, `BadRRCrtc`, and `BadRRMode` offsets from the
extension first-error base. Use them for unknown synthetic ids if the existing
error writer accepts arbitrary error codes.

- [ ] **Step 2: Fall back to `BadValue` for first cut if needed.**

This is acceptable for the compatibility stub; correct extension-specific
errors can be a follow-up.

---

## Commit 5 — Validation and Docs

### Task 5.1: Automated checks

Run:

```sh
RUSTC_WRAPPER= cargo fmt --all -- --check
RUSTC_WRAPPER= cargo check --workspace
RUSTC_WRAPPER= cargo test --workspace
RUSTC_WRAPPER= cargo clippy --workspace
```

- [ ] **Step 1: Fix all failures.**

Warnings from stable rustfmt about nightly-only import options are acceptable
if already present.

### Task 5.2: Manual smoke tests

Run `ynest`:

```sh
RUST_LOG=debug cargo run --release --bin ynest 99
```

In another shell:

```sh
DISPLAY=:99 xrandr -q
DISPLAY=:99 xeyes
DISPLAY=:99 xclock
DISPLAY=:99 xterm
DISPLAY=:99 fvwm3
```

- [ ] **Step 1: Verify `xrandr -q`.**

Expected: one connected output named `ynest-0` with one current mode matching
the nested screen size.

- [ ] **Step 2: Verify existing clients still work.**

`xeyes`, `xclock`, and `xterm` should retain current behavior.

- [ ] **Step 3: Verify `fvwm3` gets past missing RANDR.**

If `fvwm3` fails on the next missing feature, capture the log and update
`docs/status.md` with the new blocker.

### Task 5.3: Update documentation

**Files:**
- Modify: `docs/status.md`

- [ ] **Step 1: Mark the Phase 2 RANDR task complete only after smoke tests pass.**

- [ ] **Step 2: Add follow-ups discovered during validation.**

Likely follow-ups:

- host-window resize propagation.
- `RRScreenChangeNotify`.
- Xinerama compatibility.
- additional RANDR setter no-ops if a WM probes them.

---

## Done Criteria

- `QueryExtension("RANDR")` reports present.
- `xrandr -q` prints one connected `ynest-0` output.
- `fvwm3` no longer exits because RANDR is absent.
- Existing Phase 1 clients still run.
- Full Rust checks pass.
- `docs/status.md` reflects final behavior and remaining blockers.
