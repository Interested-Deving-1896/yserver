# Phase 6.5 — WM-readiness on KMS Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make fvwm3 fully usable on the bare-metal `KmsBackend` (with wmaker / e16 as smoke regressions) by closing the notify-event delivery gap, wiring GC `function` → pixman op (XOR rubber-band, xclock seconds hand), and fixing xterm glyph baseline.

**Architecture:** Three independent fix tracks plus a final smoke matrix.
1. *Notify-event delivery* — diagnose the fvwm3-module wedge, then patch whatever is missing. The plan does not pre-commit to a fix shape because `nested.rs` already synthesizes ConfigureNotify / MapNotify / UnmapNotify for both StructureNotify and SubstructureNotify subscribers regardless of backend (verified at `nested.rs:5610-5648`, `5694-5722`, `5926-5963`); the documented "wedge" symptom contradicts that, so step 0 is "find out what's actually missing" before writing code.
2. *GC function plumbing* — `KmsBackend::apply_draw_state` ignores `state.function` (currently always composites with `Operation::Src`). Add a `current_function: GcFunction` field, translate to a pixman op per primitive.
3. *xterm glyph baseline* — `render_text_string` phase-2 composite sign / offset is wrong; glyphs appear above/below the visible row.

**Tech Stack:** Rust 2024, Pixman 0.2.1, freetype, xkbcommon. No new deps.

**Branch:** `phase6-5-wm-readiness`

**Worktree:** Create via `superpowers:using-git-worktrees` from current `master`.

---

## Status

Not started. Depends on Phase 6.4 (commit `7763055`).

## Strategy

Each numbered Step is one logical commit. `cargo build` + `cargo test` green at every commit. Manual smoke gates at Step 1 (fvwm3 module wedge fixed), Step 2 (xclock seconds hand visible), Step 3 (xterm glyphs legible), Step 4 (full WM matrix).

After every commit:
```sh
cargo +nightly fmt
cargo clippy --workspace --all-targets
cargo test --workspace
```

Self-review (no codex — token budget exhausted until Wednesday) at the end of every Step before merging.

## Reference points (read these once before starting)

- `crates/yserver/src/kms/backend.rs:1172-1387` — KmsBackend lifecycle hooks (create / destroy / map / unmap / configure / reparent_subwindow).
- `crates/yserver/src/kms/backend.rs:640-652` — `synthesize_expose` pattern (existing event-sink usage in KmsBackend).
- `crates/yserver-core/src/nested.rs:5610-5648` — MapNotify synthesis to StructureNotify + SubstructureNotify subscribers. Backend-agnostic.
- `crates/yserver-core/src/nested.rs:5694-5722` — Same pattern for the children-of-a-just-mapped-parent path.
- `crates/yserver-core/src/nested.rs:5754-5825` — UnmapNotify synthesis (window-driven and parent-driven).
- `crates/yserver-core/src/nested.rs:5926-5963` — ConfigureNotify synthesis (window + parent).
- `crates/yserver-core/src/host_x11/pump.rs:120-138` — `HostEvent` enum: only `Key | Pointer | Expose | Configure | Closed` exist today.
- `crates/yserver-core/src/server.rs:803-826` — `HostPumpEventSink::handle_backend_event`. Note: `HostEvent::Configure` is interpreted only as a *container window* resize, not a per-client ConfigureNotify trigger.
- `crates/yserver-core/src/backend/params.rs:140-160` — `GcFunction` enum (16 X11 functions). Phase 6.5 only needs `Copy → Src` and `Xor → Xor`.
- `docs/known-issues.md:109-192` — full Phase 6.4 known-issues list including the fvwm3-modules entry that motivates this phase.
- `crates/yserver/src/kms/backend.rs:953-1079` — `render_text_string`. Glyph composite is `(g.dst_x, g.dst_y)` where `g.dst_y = y - glyph.bitmap_top()`. Suspect off-by-`font_ascent`.

---

## Step 0 — Diagnostic: capture the fvwm3-module event-delivery gap

**Files:**
- Create: `docs/superpowers/notes/2026-05-04-phase6-5-fvwm3-trace.md` (a working note, deleted at end of Step 1)

**Goal:** Identify exactly which event the fvwm3 module is waiting for that yserver doesn't deliver. Do not write any fix code in this step. The premise from the 6.4 known-issues entry ("synthesize ConfigureNotify from `KmsBackend::configure_subwindow`") is *probably* wrong because nested.rs already synthesizes ConfigureNotify regardless of backend; verify what the actual gap is.

- [ ] **Step 0.1: Capture host-backend (ynest) reference trace**

Run fvwm3 under ynest with full nested.rs event-emission tracing on. Record every event delivered to fvwm3 and to its modules during a 30-second session that exercises the wedge condition (start fvwm3 with the same config that wedges under yserver — typically the default config with FvwmIconMan/FvwmPager/FvwmButtons enabled).

```sh
cd /home/jos/Projects/yserver
just ynest-fvwm3 2>&1 | tee /tmp/phase6-5-ynest-fvwm3.log
# In another terminal, attach x11trace to fvwm3's $DISPLAY for an authoritative wire trace:
x11trace -d $YNEST_DISPLAY -o /tmp/phase6-5-ynest-fvwm3.x11trace -- fvwm3
```

If `just ynest-fvwm3` does not exist, check `justfile` for the closest fvwm3 recipe and adapt; if no recipe exists, run `cargo run --bin ynest -- --display :3` and `DISPLAY=:3 fvwm3 &` manually.

Record in `docs/superpowers/notes/2026-05-04-phase6-5-fvwm3-trace.md`:
- Counts of each event type delivered (MapNotify, ConfigureNotify, UnmapNotify, ReparentNotify, CreateNotify, DestroyNotify, GravityNotify, CirculateNotify, FocusIn/Out, PropertyNotify, ClientMessage).
- Which window each event was delivered to (root vs. fvwm3 main process windows vs. module windows).
- Any non-notify events that look load-bearing (e.g. SelectionNotify for fvwm's `_FVWM_*` selections).

- [ ] **Step 0.2: Capture KMS-backend (yserver) trace under the same workload**

Run fvwm3 under yserver in QEMU/vng with the same fvwm3 config. Record the same data into the working note as a parallel column.

```sh
cd /home/jos/Projects/yserver
just yserver-vng 2>&1 | tee /tmp/phase6-5-yserver-fvwm3.log
# Inside the guest:
DISPLAY=:7 fvwm3 &
DISPLAY=:7 x11trace -o /tmp/phase6-5-yserver-fvwm3.x11trace -- sleep 30
```

If the existing recipe runs xterm/xeyes instead of fvwm3, copy it and substitute fvwm3 as the launched client.

- [ ] **Step 0.3: Diff the two traces**

In the working note, produce a side-by-side comparison: events delivered by ynest that yserver does not deliver, by event type and target window. Identify the smallest set of missing events that would unblock the module wedge.

Hypotheses to evaluate explicitly (rule each in or out with trace evidence):

- (H1) Event types that exist in nested.rs synthesis but never fire on KMS because some upstream gate is false (e.g. `configure` is `None`).
- (H2) Event types that nested.rs *never* synthesizes for any backend, that the host X server emits for free (e.g. GravityNotify, CirculateNotify, possibly ReparentNotify on certain paths, possibly CreateNotify/DestroyNotify on certain paths).
- (H3) Events delivered to a different window on KMS than on host (e.g. SubstructureNotify on root vs. parent).
- (H4) Event delivery race: events are emitted but the sink consumes them in an order that breaks fvwm3's expectations.

- [ ] **Step 0.4: Decide fix shape**

Pick exactly one of these, based on Step 0.3 evidence:

- **(A)** A specific event class is never synthesized in nested.rs → extend `nested.rs` synthesis to cover it (backend-agnostic; both ynest and yserver benefit). Most likely candidates: CreateNotify on `create_window`, ReparentNotify on `reparent_window`, DestroyNotify on `destroy_window`, GravityNotify on parent-resize-driven child movement.
- **(B)** A nested.rs synthesis path exists but its gating condition fails on KMS specifically → fix the gate, not the backend.
- **(C)** The event must be backend-emitted (the event depends on backend-specific state) → add a new `HostEvent` variant + matching `KmsBackend` emit + matching nested.rs sink handler.

Record the chosen shape in the working note. If the trace points to multiple gaps, plan one Step (1.1, 1.2, …) per gap; otherwise one is enough.

- [ ] **Step 0.5: Commit the working note (no code yet)**

```bash
git add docs/superpowers/notes/2026-05-04-phase6-5-fvwm3-trace.md
git commit -m "docs(phase6-5): fvwm3 trace diff vs ynest"
```

The note will be deleted at the end of Step 1; the commit lives only on the feature branch.

---

## Step 1 — Close the notify-event delivery gap

**Files:**
- Modify: depends on Step 0.4. For shape (A) or (B), `crates/yserver-core/src/nested.rs`. For shape (C), `crates/yserver-core/src/host_x11/pump.rs` (add HostEvent variant), `crates/yserver-core/src/server.rs` (sink handler), `crates/yserver/src/kms/backend.rs` (emitter).
- Test: `crates/yserver-core/src/nested.rs` (tests are colocated; see existing `tests` modules near `nested.rs:9662`).

**Goal:** Every fvwm3 module reaches its idle state without busy-looping. The `ConfigureWindow → ChangeWindowAttributes → GetInputFocus` retry pattern from `known-issues.md:172-186` no longer appears in the trace.

- [ ] **Step 1.1: Write a regression test for the gap**

Pick the colocated test pattern at `nested.rs:9662-9745` (the existing root-resize ConfigureNotify test) as a template.

Write a test that:
1. Builds a `ServerState` with a parent window and one child.
2. Subscribes a fake client to the relevant event mask on the relevant window (StructureNotify on the affected window AND SubstructureNotify on the parent — match what fvwm3 selects).
3. Drives the request handler that exercises the missing-event path identified in Step 0.4.
4. Asserts the wire-byte event header (event-type byte 0, window field) for each expected notify.

Code skeleton (adapt event-type and window-id assertions to whatever Step 0.4 found):

```rust
#[test]
fn step_1_<short_name>_emits_missing_notify_to_subscribers() {
    let server = test_server();
    let parent = create_window(&server, ROOT_WINDOW, /*…*/);
    let child = create_window(&server, parent, /*…*/);
    select_input(&server, CLIENT_A, child, 0x0002_0000); // StructureNotify
    select_input(&server, CLIENT_B, parent, 0x0008_0000); // SubstructureNotify

    // Drive the request that should produce the missing notify
    drive_<request>(&server, child, /*…*/);

    // Assert CLIENT_A received the notify on `child`
    let buf_a = drain_client_writes(CLIENT_A);
    assert_eq!(buf_a[0], <EXPECTED_EVENT_TYPE>);
    assert_eq!(read_u32(&buf_a[8..12]), child.0);

    // Assert CLIENT_B received the notify on `parent`-as-event-window, child-as-target
    let buf_b = drain_client_writes(CLIENT_B);
    assert_eq!(buf_b[0], <EXPECTED_EVENT_TYPE>);
    assert_eq!(read_u32(&buf_b[8..12]), parent.0);
    assert_eq!(read_u32(&buf_b[12..16]), child.0);
}
```

- [ ] **Step 1.2: Run the test, verify it fails**

```sh
cargo test -p yserver-core step_1_<short_name>
```

Expected: FAIL with the assertion on `buf_a[0]` (the notify is never written).

- [ ] **Step 1.3: Implement the fix per Step 0.4 decision**

If shape (A) — extend `nested.rs` synthesis: locate the request handler for the missing event class. Add an `emit_window_event(server, window, 0x0002_0000, |buf, seq, order| { x11::encode_<event>_notify_event(...) })` block plus the matching SubstructureNotify variant on the parent (mask `0x0008_0000`). Use the existing MapNotify block at `nested.rs:5610-5648` as a structural template.

If shape (B) — fix the gating condition: locate the `if let Some(...) = configure` (or analogous) pattern and identify why the gate is false on the failing path. Adjust the gate or the upstream computation that feeds it.

If shape (C) — add a HostEvent variant:

```rust
// In crates/yserver-core/src/host_x11/pump.rs near line 133:
pub enum HostEvent {
    Key(HostKeyEvent),
    Pointer(HostPointerEvent),
    Expose(HostExposeEvent),
    Configure(HostConfigureEvent),
    <NewVariant>(<NewEventStruct>),
    Closed,
}
```

Add a matching `<NewEventStruct>` with the fields the sink needs. Wire a sink handler in `crates/yserver-core/src/server.rs` (near line 813). Emit from `crates/yserver/src/kms/backend.rs` at the lifecycle hook (`map_subwindow` / `configure_subwindow` / etc.) that produces the state change. **Do not** also emit from `HostX11Backend` unless Step 0.4 explicitly requires it — duplicate notifies are worse than missing ones.

- [ ] **Step 1.4: Run the test, verify it passes**

```sh
cargo test -p yserver-core step_1_<short_name>
```

Expected: PASS.

- [ ] **Step 1.5: Run the full test suite**

```sh
cargo test --workspace
```

Expected: 360+ tests pass, no regressions.

- [ ] **Step 1.6: Manual smoke gate — fvwm3 modules under yserver**

```sh
just yserver-vng
# In guest:
DISPLAY=:7 fvwm3 &
sleep 10
# Verify in the host log: no busy-loop pattern of the form
# ConfigureWindow → ChangeWindowAttributes → GetInputFocus on the same window.
grep -c "ConfigureWindow.*0x.*-1.*-1" /tmp/yserver.log
```

Expected: count is bounded (single-digit, not 1000+).

Also verify FvwmPager / FvwmIconMan / FvwmButtons render (whichever your config enables). Take a screenshot of the running session for the validation section of `status.md`.

- [ ] **Step 1.7: Delete the working note**

```sh
rm docs/superpowers/notes/2026-05-04-phase6-5-fvwm3-trace.md
```

- [ ] **Step 1.8: Commit**

```bash
git add -A
git commit -m "$(cat <<'EOF'
fix(phase6-5): close fvwm3 module notify-event gap

<one-paragraph description of the gap and the fix shape (A/B/C).
Reference the trace evidence from Step 0.>

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Step 2 — GC function plumbing → XOR

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs` (struct field, `apply_draw_state`, every drawing primitive that calls `composite32` / `fill_rectangles`).
- Test: `crates/yserver/src/kms/backend.rs` (colocated, near existing tests).

**Goal:** xclock's seconds hand is visible; XOR rubber-band selection works under fvwm3 / wmaker.

Background: `GcFunction` has 16 variants (`crates/yserver-core/src/backend/params.rs:140-160`). Pixman supports a corresponding `Operation` enum but the mapping is only well-defined for `Copy → Src` and `Xor → Xor`. Other variants either have no clean pixman equivalent (Clear/Set are trivial; And/Or/Nor/Nand/Equiv have no direct counterpart in pre-multiplied RGB) or are vanishingly rare. **Phase 6.5 only implements `Copy` and `Xor`**; everything else falls back to `Src` with a `debug!("GC function {:?} not implemented, falling back to Copy", function)` log on first use.

- [ ] **Step 2.1: Write a failing test for XOR drawing**

Test that drawing a `poly_segment` with `function = Xor` over a known-color background produces the XOR'd pixel value, not a `Src` overwrite.

```rust
#[test]
fn poly_segment_xor_inverts_destination_pixels() {
    let mut backend = test_kms_backend();
    let win = create_test_window(&mut backend, 16, 16);
    fill_window_with_color(&mut backend, win, 0x00ff_ffff); // white

    let state = DrawState {
        function: GcFunction::Xor,
        foreground: 0x00ff_00ff, // magenta
        ..DrawState::default()
    };
    backend.apply_draw_state(None, &state).unwrap();
    backend.poly_segment(None, win, &[Segment { x1: 0, y1: 8, x2: 15, y2: 8 }]).unwrap();

    // White (0xFFFFFF) XOR magenta (0xFF00FF) = green (0x00FF00).
    let pixel = read_window_pixel(&backend, win, 8, 8);
    assert_eq!(pixel & 0x00ff_ffff, 0x0000_ff00);
}
```

You may need helper `fill_window_with_color`, `read_window_pixel`, `create_test_window`. If they don't exist, write them in the test module above the test (do not pollute the production module).

- [ ] **Step 2.2: Run the test, verify it fails**

```sh
cargo test -p yserver kms::backend::poly_segment_xor
```

Expected: FAIL — the pixel is magenta (0xFF00FF), not green.

- [ ] **Step 2.3: Add `current_function` to KmsBackend struct**

In `crates/yserver/src/kms/backend.rs` near the existing struct fields (around `current_font`):

```rust
current_function: GcFunction,
```

Initialize in the constructor to `GcFunction::Copy`.

- [ ] **Step 2.4: Update `apply_draw_state` to capture function**

`crates/yserver/src/kms/backend.rs:1621`:

```rust
fn apply_draw_state(
    &mut self,
    _origin: Option<OriginContext>,
    state: &DrawState,
) -> io::Result<()> {
    if let Some(font) = state.font {
        self.current_font = Some(font.as_raw());
    }
    self.current_function = state.function;
    Ok(())
}
```

- [ ] **Step 2.5: Add a translation helper**

In the same file, alongside `color_from_u32`:

```rust
fn pixman_op_for(function: GcFunction) -> Operation {
    match function {
        GcFunction::Copy => Operation::Src,
        GcFunction::Xor => Operation::Xor,
        other => {
            log::debug!("GC function {:?} not implemented, falling back to Copy", other);
            Operation::Src
        }
    }
}
```

- [ ] **Step 2.6: Route every drawing primitive through `pixman_op_for(self.current_function)`**

Replace each hard-coded `Operation::Src` in `composite32` / `fill_rectangles` calls inside drawing primitives (`poly_line`, `poly_segment`, `poly_arc`, `poly_fill_arc`, `fill_rectangle`, `poly_fill_rectangle`, `fill_poly`, `image_text8` background rect, `render_text_string` foreground composite) with `pixman_op_for(self.current_function)`.

Do NOT change `Operation::Src` calls in `create_subwindow` / `configure_subwindow` (those are server-internal background fills, not client draws).

Do NOT change `Operation::Over` in `render_text_string`'s glyph composite (that's an alpha-blend op, orthogonal to the GC function).

Search-and-replace pattern (verify each hit by hand before saving):

```sh
grep -n "Operation::Src" crates/yserver/src/kms/backend.rs
```

Audit each hit: client-draw primitives flip to `pixman_op_for(...)`; server-internal fills stay as `Operation::Src`.

- [ ] **Step 2.7: Run the test, verify it passes**

```sh
cargo test -p yserver kms::backend::poly_segment_xor
```

Expected: PASS.

- [ ] **Step 2.8: Run the full test suite**

```sh
cargo test --workspace
```

- [ ] **Step 2.9: Manual smoke gate — xclock seconds hand**

```sh
just yserver-vng
# In guest:
DISPLAY=:7 xclock &
sleep 65
```

Expected: the seconds hand sweeps once per second and is *visible* (it draws over the previous position with XOR, which on a white face produces a dark line).

- [ ] **Step 2.10: Commit**

```bash
git add -A
git commit -m "$(cat <<'EOF'
feat(phase6-5): plumb GC function → pixman op (Copy + Xor)

Adds current_function on KmsBackend, captured in apply_draw_state,
read by every client-draw primitive via pixman_op_for(). Other
GcFunction variants log-and-fall-back to Src for now.

Unblocks: xclock seconds hand, XOR rubber-band selection.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Step 3 — xterm glyph baseline fix

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs:953-1079` (`render_text_string`).
- Test: `crates/yserver/src/kms/backend.rs` (colocated).

**Goal:** xterm renders legible text. The `bitmap_top` / baseline math in the phase-2 composite is correct.

Diagnosis: `render_text_string` at line 1016 uses `dst_y: y - glyph.bitmap_top()`. In freetype, `bitmap_top` is the *signed offset from the baseline to the top of the bitmap*, positive upward. So if the baseline is at pixel `y`, the top row of the bitmap belongs at `y - bitmap_top`. That is correct.

But `image_text8` and `poly_text8` callers pass `y` as the *baseline*, not the top of the cell. If a caller is passing the top of the cell instead, the glyph lands `font_ascent` pixels too high. Check the call sites at `kms/backend.rs:2286` and `kms/backend.rs:2356`.

The fix is most likely in *one* of:
- (i) The call site computes `y` incorrectly (e.g. uses cell-top instead of baseline). Look at `kms/backend.rs:2337` — `let ascent = font_state.metrics.font_ascent as i32;` — and verify it's added to `y` before calling `render_text_string`.
- (ii) `render_text_string` itself uses the wrong sign (unlikely, the code reads correctly).
- (iii) `bitmap_top` is being interpreted with the wrong sign convention (also unlikely; freetype docs are unambiguous).

- [ ] **Step 3.1: Write a failing test for glyph baseline**

```rust
#[test]
fn render_text_string_places_glyph_baseline_at_y() {
    let mut backend = test_kms_backend_with_font();
    let win = create_test_window(&mut backend, 64, 32);
    fill_window_with_color(&mut backend, win, 0x00ff_ffff); // white

    // Draw "X" with baseline at y=20. The font's cap-height is roughly
    // font_ascent * 0.7 — if baseline is correct, pixel (5, 18) is dark
    // (inside the glyph) and pixel (5, 5) is white (above the cap).
    backend.render_text_string(win.as_raw(), 0x0000_0000, 5, 20, b"X").unwrap();

    let pixel_inside = read_window_pixel(&backend, win, 5, 18);
    let pixel_above = read_window_pixel(&backend, win, 5, 5);
    assert!(pixel_inside & 0x00ff_ffff != 0x00ff_ffff,
            "expected dark pixel inside glyph at (5,18), got {:06x}", pixel_inside);
    assert_eq!(pixel_above & 0x00ff_ffff, 0x00ff_ffff,
               "expected white pixel above cap at (5,5)");
}
```

- [ ] **Step 3.2: Run the test, verify it fails**

```sh
cargo test -p yserver kms::backend::render_text_string_places_glyph_baseline
```

Expected: FAIL on the "dark pixel inside glyph" assertion (pixel is white because the glyph landed elsewhere).

- [ ] **Step 3.3: Add a debug print to localize the bug**

Temporarily, at `render_text_string` line 1016, log the inputs and the computed `dst_y`:

```rust
log::debug!("render_text_string: y={} bitmap_top={} dst_y={} h={}",
            y, glyph.bitmap_top(), y - glyph.bitmap_top(), bitmap.rows());
```

Run the test once with `RUST_LOG=debug` and inspect. Compare the printed `dst_y` against the expected baseline. The discrepancy will identify which of (i)/(ii)/(iii) above is wrong.

- [ ] **Step 3.4: Apply the minimal fix indicated by Step 3.3**

If (i): adjust the call site at `kms/backend.rs:2337` (likely `y + ascent` is missing or doubled).

If (ii) or (iii): adjust the offset in `render_text_string` itself.

Remove the temporary debug print added in Step 3.3.

- [ ] **Step 3.5: Run the test, verify it passes**

```sh
cargo test -p yserver kms::backend::render_text_string_places_glyph_baseline
```

Expected: PASS.

- [ ] **Step 3.6: Run the full test suite**

```sh
cargo test --workspace
```

- [ ] **Step 3.7: Manual smoke gate — xterm legibility**

```sh
just yserver-vng
# In guest:
DISPLAY=:7 xterm &
# Type: echo "Hello, world."
```

Expected: the typed string is legible. Glyphs land on a coherent baseline. Take a screenshot for the validation section of `status.md`.

- [ ] **Step 3.8: Commit**

```bash
git add -A
git commit -m "$(cat <<'EOF'
fix(phase6-5): xterm glyph baseline — <which-of-i-ii-iii>

<one paragraph: where the off-by-font_ascent / sign-flip lived
and why the fix corrects it.>

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Step 4 — WM smoke matrix + status.md update

**Files:**
- Modify: `docs/status.md` (Phase 6.5 retrospective in the same shape as 6.2/6.3/6.4).
- Modify: `docs/known-issues.md` (delete the now-fixed entries from the "KMS backend (Phase 6.4)" section: GC function, xterm glyph baseline, fvwm3 modules wedge).

**Goal:** Full WM matrix runs cleanly under yserver/KMS. Documentation reflects the new ground truth.

- [ ] **Step 4.1: fvwm3 smoke (gate)**

```sh
just yserver-vng
# In guest:
DISPLAY=:7 fvwm3 &
sleep 10
DISPLAY=:7 xterm &
DISPLAY=:7 xclock &
DISPLAY=:7 xeyes &
```

Verify:
- All three clients render.
- fvwm3 modules render and stay idle (no busy loop).
- xclock's seconds hand sweeps and is visible.
- xterm shows legible text.
- xeyes' pupils track the cursor.

Capture screenshot at `/tmp/phase6-5-fvwm3.png`.

- [ ] **Step 4.2: wmaker smoke (regression)**

Repeat under wmaker:

```sh
DISPLAY=:7 wmaker &
sleep 10
DISPLAY=:7 xterm &
DISPLAY=:7 xclock &
```

Verify the dock and clip render. Capture `/tmp/phase6-5-wmaker.png`. If anything new is broken under wmaker that worked under fvwm3, log it as a fresh entry in `known-issues.md` under "WM-specific behaviour" (not gating for 6.5).

- [ ] **Step 4.3: e16 smoke (regression, only if installed)**

If `which enlightenment-16` returns a path, run the same matrix. Otherwise skip — e16 is not a 6.5 gate.

- [ ] **Step 4.4: Update status.md**

Convert the "Phase 6.5 — WM-readiness on KMS (planned)" section to "(complete)". Add `#### Landed (commit <sha>)` and `#### Validation (commit <sha>)` subsections in the shape of the existing 6.4 entry. Reference the screenshots captured in Steps 4.1–4.3.

- [ ] **Step 4.5: Update known-issues.md**

Delete the now-fixed entries from `docs/known-issues.md:109-192`:
- "GC `function` not honoured."
- "xterm glyph baseline / placement."
- "fvwm3 modules wedge on missing ConfigureNotify."

Keep the remaining entries (poly_arc partial-angle clipping, fill_rectangles segfault, cursor drift, list_fonts empty, line_width thick lines, SetDashes, InstallColormap) — those are out of scope for 6.5 unless one of the stretch tasks below ships.

- [ ] **Step 4.6: Commit docs**

```bash
git add docs/status.md docs/known-issues.md
git commit -m "docs(phase6-5): retro + retire fixed known-issues"
```

- [ ] **Step 4.7: Self-review (no codex)**

Walk the diff `git log master..HEAD -p` and check for:
- Dead code added during diagnosis (debug logs, scratch helpers).
- Inconsistent use of `Operation::Src` vs `pixman_op_for(...)` after Step 2.
- Test helpers that leaked from the test module into the production module.
- Any `// TODO` or `// XXX` left behind.

Fix anything found inline; commit with `chore(phase6-5): self-review cleanup`.

---

## Stretch (drop if scope grows)

These are independent and each is a separate commit. Take them only if Steps 0–4 land cleanly with time to spare.

### Stretch A — `SetDashes` no-op + reply

`crates/yserver/src/kms/backend.rs`. Accept opcode 58, store `dashes: Vec<u8>` in DrawState (already there at `params.rs:316`), no rasterisation change. Removes the "unsupported opcode 58" log from fvwm3.

### Stretch B — `InstallColormap` no-op

`crates/yserver/src/kms/backend.rs`. Accept opcode 81, do nothing (TrueColor backend). Removes the "unsupported opcode 81" log.

### Stretch C — `line_width` thick lines

`crates/yserver/src/kms/backend.rs:1871` (`poly_line`) + `:1912` (`poly_segment`). Pixman has no native thick-line primitive; the cheapest implementation is to inflate to a thin polygon (4 vertices per segment) and route through `fill_poly`. Skip if `line_width <= 1`. Add a test that `line_width=3` paints a 3-pixel-wide horizontal line.

### Stretch D — Partial-angle clipping for `poly_arc` / `poly_fill_arc`

Per the existing `known-issues.md:133-144` description: angular mask via `atan2(py-cy, px-cx)` against `[angle1, angle1+angle2)` with X11's "0 = 3 o'clock, counter-clockwise" convention. Add a test that draws a quarter-arc and asserts only one quadrant of pixels is set.

---

## Out of scope (defer to 6.6+)

- Real font enumeration / RENDER extension stubs on KMS.
- Host (GTK) cursor and guest cursor drift / lock.
- pixman `fill_rectangles` partly-out-of-bounds segfault root-cause investigation (the `clip_rects_to_image` workaround stays).
- VT_SETMODE / logind / suspend-resume / hotplug polish.
- xterm scrollback misbehaviour.
- e16 popup rounded corners.
- openbox frame chrome.
- COMPOSITE NameWindowPixmap on un-redirected windows.

---

## Risks

- **Step 0 may surface multiple gaps.** If the trace diff shows three independent missing notify classes, Step 1 expands to 1.1/1.2/1.3 — one fix per gap, one commit each. Not a scope blocker, just more steps.
- **GC function `Xor` over an `X8R8G8B8` buffer.** Pixman `Operation::Xor` operates on the full 32-bit word including the `X` byte. The `X` byte is undefined per the format, so XOR'ing it is harmless, but if any downstream code (cursor compositor?) reads it as an alpha channel, things break. Check the scanout-composite path doesn't depend on the X byte being zero.
- **`render_text_string` fix may interact with FontMetrics.** If the baseline bug is in the call site rather than `render_text_string`, the fix may also affect `image_text8`'s background rect calculation. Re-verify the white-background-behind-text behaviour after Step 3.
- **Manual smoke depends on QEMU/vng.** No automated KMS smoke yet. If the vng image lacks fvwm3 / wmaker, Step 4 stalls; the install dance is not in this plan's scope.
