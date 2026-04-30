# Phase 3.1 Toolkit Compatibility Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run a simple GTK3 application, preferably `gtk3-demo`, interactively
under `ynest`: windows render, keyboard input is correct, mouse clicks work,
menus open, text entry works, and the app closes cleanly.

**Architecture:** Add the minimum toolkit-facing extensions from the design:
BIG-REQUESTS, XKB, and XInput2. Keep protocol wire encoding in
`yserver-protocol`; keep per-client extension state in `ClientHandle`; keep
host-proxied XKB behavior in `HostX11`; keep nested dispatch and event fanout
in `nested.rs` / `server.rs`. Do not implement XFIXES, DAMAGE, COMPOSITE,
MIT-SHM, SHAPE, SYNC, or PRESENT in this phase.

**Spec:** [`docs/superpowers/specs/2026-04-30-phase3-toolkit-compat-design.md`](../specs/2026-04-30-phase3-toolkit-compat-design.md).

**Project conventions:**

```sh
RUSTC_WRAPPER= cargo fmt --all -- --check
RUSTC_WRAPPER= cargo check --workspace
RUSTC_WRAPPER= cargo test --workspace
RUSTC_WRAPPER= cargo clippy --workspace
```

Manual validation is mandatory because success depends on real Xlib/GDK
startup behavior and input delivery.

---

## File Structure

| Path | Status | Purpose |
|---|---|---|
| `crates/yserver-core/src/server.rs` | modify | Add per-client `big_requests_enabled` and `xi2_masks`; add XI2 subscriber helpers |
| `crates/yserver-core/src/host_x11.rs` | modify | Query host XKB extension and proxy selected XKB requests |
| `crates/yserver-core/src/nested.rs` | modify | Extended request reader, extension registration, BIG-REQUESTS/XKB/XI2 dispatch, XI2 event fanout wiring |
| `crates/yserver-protocol/src/x11/mod.rs` | modify | Extension constants/helpers, BIG-REQUESTS reply, XI2 request parsers and event encoders |
| `docs/status.md` | modify | Mark Phase 3.1 progress after GTK3 validation and record remaining blockers |

The implementation is seven compile-safe commits:

1. **Extension registry cleanup** — centralize advertised extension metadata and update `QueryExtension` / `ListExtensions`.
2. **BIG-REQUESTS** — enable per-client state and extended-length request reads.
3. **XKB host discovery and proxy** — query host XKB and proxy reply-producing minors with sequence rewrite.
4. **XKB no-reply/minimal minors** — handle `SelectEvents`, unsupported minors, and deadlock-safe forwarding.
5. **XI2 request path** — implement query/select requests and per-client mask storage.
6. **XI2 event delivery** — encode and fan out GenericEvent-wrapped keyboard/pointer/focus events.
7. **GTK3 validation and docs** — run manual tests, tune compatibility, update status.

---

## Commit 1 — Extension Registry Cleanup

### Task 1.1: Centralize extension metadata

**Files:**
- Modify: `crates/yserver-core/src/nested.rs`
- Modify: `crates/yserver-protocol/src/x11/mod.rs` if protocol-side constants are preferred

- [ ] **Step 1: Locate current `QueryExtension` and `ListExtensions` handling.**

Identify where `BIG-REQUESTS`, `XKEYBOARD`, and unknown extensions are currently
reported absent/present.

- [ ] **Step 2: Add extension metadata records.**

Use the nested major opcodes from the design as preferred values, but allocate
them through one central registry so collisions are impossible:

```rust
BIG-REQUESTS     major=135 first_event=0  first_error=0
XKEYBOARD        major=136 first_event=host first_error=host
XInputExtension  major=137 first_event=90 first_error=next_available
```

XKB is conditional: it is advertised only if the host X server reports
`XKEYBOARD` present.

- [ ] **Step 2a: Assert extension uniqueness.**

Add a test or startup assertion that every advertised extension has a unique
major opcode, first-event range, and first-error range where those bases are
non-zero. Include RANDR if it has already landed. If a preferred value collides,
move the later extension to the next free major opcode and make `QueryExtension`,
`ListExtensions`, and the dispatcher use the allocated value rather than a
hardcoded literal.

- [ ] **Step 3: Update `QueryExtension`.**

Return present metadata for the Phase 3.1 extensions. Keep unsupported
extensions absent.

- [ ] **Step 4: Update `ListExtensions` opcode 99.**

Include all advertised Phase 3.1 extensions:

- `BIG-REQUESTS`
- `XKEYBOARD` only when host XKB is available
- `XInputExtension`

Preserve any existing extension names already intentionally advertised, such as
RANDR if implemented by then.

- [ ] **Step 5: Add focused tests if lookup is extracted.**

Test advertised vs absent extensions and conditional XKB behavior.

---

## Commit 2 — BIG-REQUESTS

### Task 2.1: Add per-client state

**Files:**
- Modify: `crates/yserver-core/src/server.rs`

- [ ] **Step 1: Add `big_requests_enabled: bool` to `ClientHandle`.**

Default it to `false` for new clients.

- [ ] **Step 2: Ensure mutation is client-local.**

Enabling BIG-REQUESTS for one client must not affect any other client.

### Task 2.2: Add BigReqEnable reply

**Files:**
- Modify: `crates/yserver-protocol/src/x11/mod.rs`
- Modify: `crates/yserver-core/src/nested.rs`

- [ ] **Step 1: Add encoder for BigReqEnable.**

Reply body:

- Standard 32-byte reply.
- CARD32 `maximum_request_length = 0x0003_FFFF` at bytes 8-11.

- [ ] **Step 2: Dispatch major opcode 135 minor 0.**

When received:

1. Set `ClientHandle.big_requests_enabled = true`.
2. Send the enable reply.
3. Log `BIG-REQUESTS::Enable`.

- [ ] **Step 3: Return an error or empty unsupported reply for unknown BIG-REQUESTS minors.**

There is only one required minor for this phase.

### Task 2.3: Support extended request lengths

**Files:**
- Modify: `crates/yserver-core/src/nested.rs`

- [ ] **Step 1: Update the per-client request reader.**

When `length == 0` and `big_requests_enabled == true`, read four additional
bytes as a CARD32 extended length in 4-byte units.

- [ ] **Step 2: Compute body length correctly.**

Extended body length is:

```text
(extended_length - 2) * 4
```

The `-2` accounts for the normal 4-byte request header plus the 4-byte extended
length field.

- [ ] **Step 3: Reject malformed extended lengths.**

If `extended_length < 2`, disconnect the client or emit a protocol error
consistent with existing malformed-request behavior. Avoid underflow.

- [ ] **Step 4: Preserve standard request parsing.**

If BIG-REQUESTS is disabled, length zero is invalid/malformed.

- [ ] **Step 5: Add unit tests around request length calculation if the reader logic is factored.**

At minimum test standard length, valid extended length, and invalid extended
length.

---

## Commit 3 — XKB Host Discovery and Reply Proxy

### Task 3.1: Query host XKB at startup

**Files:**
- Modify: `crates/yserver-core/src/host_x11.rs`

- [ ] **Step 1: Add host metadata type.**

```rust
pub struct HostXkbInfo {
    pub opcode: u8,
    pub first_event: u8,
    pub first_error: u8,
}
```

- [ ] **Step 2: Add `xkb: Option<HostXkbInfo>` to `HostX11`.**

Initialize it in `open_from_env`.

- [ ] **Step 3: Implement host `QueryExtension("XKEYBOARD")`.**

Use the existing host X11 stream and sequence tracking. Store host opcode,
first event, and first error when present.

- [ ] **Step 4: Keep XKB absent if host lacks it.**

`ynest` must not advertise XKB when the host cannot service proxy requests.

### Task 3.2: Add reply-producing XKB proxy helper

**Files:**
- Modify: `crates/yserver-core/src/host_x11.rs`

- [ ] **Step 1: Add `send_xkb_request`.**

```rust
pub fn send_xkb_request(&mut self, minor: u8, body: &[u8]) -> io::Result<Vec<u8>>
```

Build a host request using the host XKB major opcode and the nested minor/body.

- [ ] **Step 2: Wait for the matching host sequence.**

Drain responses until the host reply/error for the proxied request sequence is
received. Return raw bytes starting at byte 0.

- [ ] **Step 3: Handle non-matching host responses deliberately.**

Do not silently discard unrelated host responses. For the first cut:

- ignore host events only if the existing host connection already treats them
  as non-authoritative for nested clients.
- preserve or log any non-matching replies/errors so debugging does not lose
  protocol evidence.
- if the matching host response is an error, rewrite bytes 2-3 to the nested
  sequence and forward the error to the nested client rather than converting it
  into an `io::Error`.

- [ ] **Step 4: Handle missing host XKB.**

Return `io::ErrorKind::Unsupported` if called while `xkb` is `None`.

### Task 3.3: Dispatch reply-producing XKB minors

**Files:**
- Modify: `crates/yserver-core/src/nested.rs`

- [ ] **Step 1: Route nested XKB major opcode 136.**

Dispatch on `header.data` minor.

- [ ] **Step 2: Proxy reply-producing minors.**

Proxy:

- `0` `UseExtension`
- `4` `GetState`
- `6` `GetControls`
- `8` `GetMap`
- `10` `GetCompatMap`
- `12` `GetIndicatorState`
- `17` `GetNames`
- `24` `GetDeviceInfo`

- [ ] **Step 3: Rewrite reply sequence numbers.**

Before writing a raw host reply to the nested client, overwrite bytes 2-3 with
the nested request sequence in the client's byte order.

- [ ] **Step 4: Do not translate host atom ids in `GetNames` yet.**

Document as a known limitation; the design says GTK3/Xlib does not round-trip
these atoms in the validation path.

---

## Commit 4 — XKB No-Reply and Unsupported Minors

### Task 4.1: Handle `SelectEvents` without deadlock

**Files:**
- Modify: `crates/yserver-core/src/server.rs`
- Modify: `crates/yserver-core/src/host_x11.rs`
- Modify: `crates/yserver-core/src/nested.rs`

- [ ] **Step 1: Add optional XKB event-mask storage if useful.**

The design says masks are stored but not acted on. A minimal map/list on
`ClientHandle` is enough, or this can be a TODO if the request is consumed
locally.

- [ ] **Step 2: Handle XKB minor `1` `SelectEvents`.**

Do not call the reply-waiting proxy helper. Either:

- forward fire-and-forget to host, or
- consume locally and reply void.

Prefer local consume for Phase 3.1 unless a client requires host-side XKB event
selection.

- [ ] **Step 3: Log `XKB::SelectEvents`.**

This makes future event/state issues easier to diagnose.

### Task 4.2: Unsupported XKB minors

**Files:**
- Modify: `crates/yserver-core/src/nested.rs`

- [ ] **Step 1: Maintain an explicit minor semantics table.**

Before adding fallback behavior, encode known XKB minor semantics in one table
or match arm annotation:

- reply-producing minors from Commit 3.
- no-reply minors such as `SelectEvents`.
- any additional minor observed in logs while running Xlib/GDK.

Do not guess reply-vs-void behavior for an unknown minor; guessing can
desynchronize Xlib.

- [ ] **Step 2: Return a benign unsupported-minor reply only for known reply-producing minors.**

Avoid hard protocol errors for unknown XKB minors during GTK3 probing.

- [ ] **Step 3: For known void minors, reply void.**

Use logs to identify minor/opcode behavior. Do not block waiting for a host
reply for unknown no-reply minors.

- [ ] **Step 4: For truly unknown minors, log and return a protocol error.**

Prefer a clear error over sending a reply with the wrong shape. If GTK3 exits
on a specific unknown minor, identify that minor's wire semantics and add it to
the explicit table before changing behavior.

- [ ] **Step 5: Record any observed missing minors in `docs/status.md`.**

---

## Commit 5 — XI2 Request Path

### Task 5.1: Add XI2 per-client masks

**Files:**
- Modify: `crates/yserver-core/src/server.rs`

- [ ] **Step 1: Add `xi2_masks` to `ClientHandle`.**

```rust
pub xi2_masks: HashMap<(ResourceId, u16), u32>
```

Key is `(window_id, device_id)`. Device ids `0` and `1` are wildcard values
(`AllDevices`, `AllMasterDevices`).

- [ ] **Step 2: Initialize empty for every client.**

- [ ] **Step 3: Add helper methods if useful.**

Helpers should answer whether a client selected a given XI2 mask for a window
and device.

### Task 5.2: Add XI2 request parsers/replies

**Files:**
- Modify: `crates/yserver-protocol/src/x11/mod.rs`

- [ ] **Step 1: Add constants for XI2 minor opcodes and masks.**

Include minors:

- `44` `SetClientPointer`
- `45` `GetClientPointer`
- `46` `SelectEvents`
- `47` `QueryVersion`
- `48` `QueryDevice`
- `60` `GetSelectedEvents`

Include mask bits from the design.

- [ ] **Step 2: Add parsers.**

Parse:

- `XISelectEvents`: window plus variable device-mask records.
- `XIGetSelectedEvents`: window.
- `XIQueryVersion`: requested major/minor.
- `XIQueryDevice`: device id.

- [ ] **Step 3: Add reply encoders.**

Implement:

- `write_xi_query_version_reply` returning `2.2`.
- `write_xi_get_client_pointer_reply` returning master pointer id `2`.
- `write_xi_query_device_reply` with two master devices.
- `write_xi_get_selected_events_reply` using stored masks.
- simple success/no-op handling for `SetClientPointer`.

- [ ] **Step 4: Add wire tests.**

Test reply lengths, device names, selected-event round-trip encoding, and
short-body parser failures.

### Task 5.3: Dispatch XI2 requests

**Files:**
- Modify: `crates/yserver-core/src/nested.rs`

- [ ] **Step 1: Route major opcode 137.**

Dispatch on `header.data`.

- [ ] **Step 2: Implement `XIQueryVersion`.**

Return major `2`, minor `2` regardless of higher requested versions.

- [ ] **Step 3: Implement `XISelectEvents`.**

Store masks in `ClientHandle.xi2_masks`. Mask zero removes the entry.

- [ ] **Step 4: Implement `XIGetSelectedEvents`.**

Return masks selected by that client for the requested window.

- [ ] **Step 5: Implement `XIQueryDevice`.**

Return the two synthetic master devices for `AllDevices`,
`AllMasterDevices`, or specific ids `2`/`3`. Unknown ids return an XI2 or core
BadValue-style error.

- [ ] **Step 6: Implement `SetClientPointer` / `GetClientPointer`.**

No-op set; get returns pointer id `2`.

- [ ] **Step 7: Unknown XI2 minors.**

Return an empty generic reply or benign unsupported behavior rather than
blocking. Log the minor.

---

## Commit 6 — XI2 Event Delivery

### Task 6.0: Audit modifier-state source

**Files:**
- Inspect: `crates/yserver-core/src/server.rs`
- Inspect: `crates/yserver-core/src/nested.rs`

- [ ] **Step 1: Identify existing modifier tracking.**

Find where core key events derive or store Shift/Ctrl/Alt/Super state. If there
is no authoritative state, add a small per-server or per-client modifier-state
tracker updated from forwarded key press/release events before encoding XI2
events.

- [ ] **Step 2: Define the Phase 3.1 fallback explicitly.**

If accurate modifier tracking is not available yet, set `mods_effective = 0`
and document that shifted text correctness depends on XKB/core keycode mapping
rather than XI2 modifier bits. Do not leave the encoder reading an undefined
or stale modifier value.

- [ ] **Step 3: Add a validation item for shifted input.**

Manual GTK validation must include shifted characters and a modifier shortcut
such as Ctrl+A in a text entry.

### Task 6.1: Add XI2 GenericEvent encoders

**Files:**
- Modify: `crates/yserver-protocol/src/x11/mod.rs`

- [ ] **Step 1: Add `encode_xi2_device_event`.**

Emit the 84-byte `XI_DeviceEvent` format from the design for:

- `XI_KeyPress`
- `XI_KeyRelease`
- `XI_ButtonPress`
- `XI_ButtonRelease`
- `XI_Motion`

Use length `13` and event type `35`.

- [ ] **Step 2: Add `encode_xi2_enter_leave_event`.**

Emit the 76-byte enter/leave/focus layout from the design for:

- `XI_Enter`
- `XI_Leave`
- `XI_FocusIn`
- `XI_FocusOut`

Use length `11` and event type `35`.

- [ ] **Step 3: Encode coordinates as FP1616.**

Use `coord << 16`. Preserve sign for negative coordinates if they appear.

- [ ] **Step 4: Include effective modifiers.**

Use the currently tracked core modifier state for `mods_effective`; base,
latched, and locked can be zero in Phase 3.1 unless already tracked.

- [ ] **Step 5: Add wire tests.**

Assert total byte length, event type, extension byte, evtype, device id,
detail, root/event ids, coordinates, and length field.

### Task 6.2: Add XI2 subscriber fanout helpers

**Files:**
- Modify: `crates/yserver-core/src/server.rs`

- [ ] **Step 1: Add a helper to find XI2 subscribers.**

Inputs:

- event window
- device id (`2` pointer or `3` keyboard)
- XI2 mask bit

Match entries for exact device id and wildcards `0`/`1`.

- [ ] **Step 2: Preserve direct-client sequence behavior.**

Reuse the same `EventTarget` / `fanout_event` pattern as core events so each
client sequence is correct.

- [ ] **Step 3: Add unit tests.**

Test exact device, wildcard device, missing mask, missing client, and unrelated
window.

### Task 6.3: Dual-deliver pointer events

**Files:**
- Modify: `crates/yserver-core/src/server.rs`
- Modify: `crates/yserver-core/src/nested.rs`

- [ ] **Step 1: Keep existing core event delivery unchanged.**

Core clients (`xeyes`, `xterm`, `xev`) must still receive core
Button/Motion/Enter/Leave events.

- [ ] **Step 2: Add XI2 delivery beside core delivery.**

For the same selected target window:

- ButtonPress -> `XI_ButtonPressMask`
- ButtonRelease -> `XI_ButtonReleaseMask`
- Motion -> `XI_MotionMask`
- Enter -> `XI_EnterMask`
- Leave -> `XI_LeaveMask`

- [ ] **Step 3: Use device id `2` for pointer events.**

Set detail to button number for button events and `0` for motion/enter/leave.

- [ ] **Step 4: Validate with `xev -event input` if available.**

Prefer `xinput test-xi2 --root` because it reports XI2 events directly. If
`xinput` is unavailable, use `xev -event input`. If neither tool is installed,
record that limitation and use GTK3 behavior only as end-to-end validation.

### Task 6.4: Dual-deliver keyboard and focus events

**Files:**
- Modify: `crates/yserver-core/src/server.rs`
- Modify: `crates/yserver-core/src/nested.rs`

- [ ] **Step 1: Keep existing core key delivery unchanged.**

- [ ] **Step 2: Add XI2 delivery beside core delivery.**

For the focused/target window:

- KeyPress -> `XI_KeyPressMask`
- KeyRelease -> `XI_KeyReleaseMask`
- FocusIn -> `XI_FocusInMask`
- FocusOut -> `XI_FocusOutMask`

- [ ] **Step 3: Use device id `3` for keyboard events.**

Set detail to keycode for key events and `0` for focus events.

- [ ] **Step 4: Set focus event coords to zero.**

Use `same_screen=1`, `focus=1` for focus events as the design specifies.

---

## Commit 7 — GTK3 Validation and Docs

### Task 7.1: Automated checks

Run:

```sh
RUSTC_WRAPPER= cargo fmt --all -- --check
RUSTC_WRAPPER= cargo check --workspace
RUSTC_WRAPPER= cargo test --workspace
RUSTC_WRAPPER= cargo clippy --workspace
```

- [ ] **Step 1: Fix all failures.**

Existing stable-rustfmt warnings about nightly-only import options are
acceptable if already present.

### Task 7.2: Manual smoke tests

Run `ynest`:

```sh
RUST_LOG=debug cargo run --release --bin ynest 99
```

In another shell:

```sh
DISPLAY=:99 xeyes
DISPLAY=:99 xclock
DISPLAY=:99 xterm
DISPLAY=:99 xinput test-xi2 --root
DISPLAY=:99 gtk3-demo
```

- [ ] **Step 1: Regression-test Phase 1 clients.**

`xeyes`, `xclock`, and `xterm` should retain current behavior.

- [ ] **Step 2: Validate GTK3 startup.**

`gtk3-demo` should open without hanging in Xlib/GDK initialization.

- [ ] **Step 3: Validate pointer input.**

Click buttons, open menus, and interact with widgets.

- [ ] **Step 4: Validate keyboard input.**

Type in text fields. Test lowercase, uppercase via Shift, arrow navigation, and
Enter activation. Also test at least one modifier shortcut such as Ctrl+A in a
text entry.

- [ ] **Step 5: Validate clean close.**

Close the window and ensure no stuck client/server thread remains.

- [ ] **Step 6: Validate BIG-REQUESTS extended length directly.**

Do not rely on `gtk3-demo` accidentally sending a large request. Use a targeted
test client, a small temporary diagnostic client, or a request-reader unit test
that constructs an extended-length request. Confirm the log shows the extended
length path or that the test covers the exact reader branch.

### Task 7.3: Debug expected blockers

If `gtk3-demo` still fails:

- [ ] **Step 1: Check extension probes.**

Search logs for missing/unsupported `BIG-REQUESTS`, XKB, and XI2 minors.

- [ ] **Step 2: Check for absent deferred extensions.**

If GDK logs mention XFIXES, DAMAGE, COMPOSITE, MIT-SHM, SHAPE, SYNC, or
PRESENT, confirm whether it degrades gracefully or hard-fails.

- [ ] **Step 3: Check input selection.**

If rendering works but input does not, verify `XISelectEvents` masks and
GenericEvent fanout before changing core events.

- [ ] **Step 4: Check sequence numbers.**

If Xlib hangs after an XKB request, verify proxied XKB replies have nested
sequence numbers, not host sequence numbers.

### Task 7.4: Update status

**Files:**
- Modify: `docs/status.md`

- [ ] **Step 1: Mark Phase 3.1 complete only if GTK3 is interactive.**

- [ ] **Step 2: Record observed unsupported minors or deferred extension blockers.**

- [ ] **Step 3: Add Phase 3.2 follow-ups.**

Likely follow-ups:

- XFIXES.
- SHAPE.
- RENDER.
- MIT-SHM.
- DAMAGE/COMPOSITE.
- XI2 scroll valuators and raw events.
- XKB event forwarding.

---

## Done Criteria

- `QueryExtension` reports BIG-REQUESTS, XKB when host-supported, and
  XInputExtension.
- `ListExtensions` includes the same advertised Phase 3.1 extensions.
- BIG-REQUESTS enable succeeds and extended-length requests are parsed.
- XKB reply-producing minors are proxied with nested sequence numbers.
- XI2 query/select paths work and store masks per client/window/device.
- XI2 GenericEvents are delivered for selected key, pointer, enter/leave, and
  focus events.
- `gtk3-demo` starts and is interactive under `ynest`.
- `xeyes`, `xclock`, and `xterm` still work.
- Full Rust checks pass.
- `docs/status.md` records the final validation result and remaining blockers.
