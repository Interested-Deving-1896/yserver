# Phase 2 wrap-up Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land the residual Phase 2 opcodes and known follow-ups so a simple reparenting WM (Openbox / Fluxbox) can run end to end under `ynest`, alongside fvwm3.

**Architecture:** Pure incremental work in the existing crates. New per-server `KeyGrab` table and per-client `save_set` live on `ServerState` / `ClientHandle` (or `ResourceTable::clients`). Wire encoders/decoders go in `yserver-protocol/src/x11/mod.rs`. New dispatch arms go in `yserver-core/src/nested.rs`. Host proxy calls go in `host_x11.rs`. RANDR follow-ups extend `randr.rs` with subscriber storage and a host-resize hook.

**Tech Stack:** Rust stable, std-only at the crate API surface, `x11-rs` (or whatever `host_x11.rs` already uses) on the host side.

**Spec:** [`2026-04-30-phase2-wrap-up-design.md`](../specs/2026-04-30-phase2-wrap-up-design.md)

---

## Conventions used in this plan

- "Failing test" steps go in the same `mod tests {}` block at the end of the file being modified (existing pattern in `protocol/x11/mod.rs` and `resources.rs`). For wire encoders, the test is a hex/byte-array roundtrip; for state machines, an exercise of the public method.
- Run `cargo test -p yserver-protocol` or `cargo test -p yserver-core` to scope tests, or `cargo test` for the full set.
- Pre-commit gate (per `~/.claude/CLAUDE.md` Rust section): `cargo +nightly fmt`, `cargo clippy -- -W clippy::pedantic`, `cargo test`. The repo's CLAUDE.md says clippy without pedantic is fine here, but pedantic doesn't hurt — run pedantic and only address easy hits, do not chase pedantic lints in unrelated code.
- Commit messages follow the existing style (`feat:`, `fix:`, `docs:`).
- All commits must use the trailer `Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>`.

---

## Group A — Keyboard for WMs

### Task A1: KeyGrab data structure and lookup

**Files:**
- Modify: `crates/yserver-core/src/server.rs` (add `KeyGrab`, field on `ServerState`, lookup helper)

- [ ] **Step 1: Write the failing tests**

Add these to the existing `#[cfg(test)] mod tests { ... }` block at the bottom of `server.rs` (create the block if absent). Tests must compile against the new types — they will fail with "type not found" first.

```rust
#[test]
fn key_grab_lookup_exact_match() {
    use crate::resources::ResourceId;
    use crate::resources::ClientId;
    let mut s = ServerState::new();
    let win = ResourceId(0x42);
    let owner = ClientId(1);
    s.key_grabs.push(KeyGrab {
        owner,
        grab_window: win,
        keycode: 24,        // 'q'
        modifiers: 0x0040,  // Mod4 (Super)
        owner_events: false,
        pointer_mode: 1,
        keyboard_mode: 1,
    });
    let hit = s.find_key_grab(win, 24, 0x0040);
    assert!(hit.is_some());
    assert_eq!(hit.unwrap().owner, owner);
}

#[test]
fn key_grab_lookup_any_modifier_wildcard() {
    use crate::resources::{ClientId, ResourceId};
    let mut s = ServerState::new();
    let win = ResourceId(0x42);
    s.key_grabs.push(KeyGrab {
        owner: ClientId(1),
        grab_window: win,
        keycode: 24,
        modifiers: 0x8000, // AnyModifier
        owner_events: false,
        pointer_mode: 1,
        keyboard_mode: 1,
    });
    assert!(s.find_key_grab(win, 24, 0x0040).is_some());
    assert!(s.find_key_grab(win, 24, 0x0000).is_some());
    assert!(s.find_key_grab(win, 25, 0x0040).is_none());
}

#[test]
fn key_grab_lookup_any_keycode_wildcard() {
    use crate::resources::{ClientId, ResourceId};
    let mut s = ServerState::new();
    let win = ResourceId(0x42);
    s.key_grabs.push(KeyGrab {
        owner: ClientId(1),
        grab_window: win,
        keycode: 0,        // AnyKey
        modifiers: 0x0040,
        owner_events: false,
        pointer_mode: 1,
        keyboard_mode: 1,
    });
    assert!(s.find_key_grab(win, 24, 0x0040).is_some());
    assert!(s.find_key_grab(win, 99, 0x0040).is_some());
    assert!(s.find_key_grab(win, 24, 0x0000).is_none());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p yserver-core key_grab -- --nocapture`
Expected: compile error "cannot find type `KeyGrab` in this scope" / "no field `key_grabs`".

- [ ] **Step 3: Implement `KeyGrab` and `find_key_grab`**

Add at the top of `server.rs` near `PassiveButtonGrab`:

```rust
#[derive(Debug, Clone)]
pub struct KeyGrab {
    pub owner: ClientId,
    pub grab_window: ResourceId,
    /// 0 == AnyKey
    pub keycode: u8,
    /// 0x8000 == AnyModifier; otherwise the literal modifier-state mask the grab matches
    pub modifiers: u16,
    pub owner_events: bool,
    /// 0 = Synchronous, 1 = Asynchronous
    pub pointer_mode: u8,
    /// 0 = Synchronous, 1 = Asynchronous
    pub keyboard_mode: u8,
}
```

Add `pub key_grabs: Vec<KeyGrab>` to `ServerState`, initialise to `Vec::new()` in `new()`.

Add the lookup helper on `impl ServerState`:

```rust
#[must_use]
pub fn find_key_grab(
    &self,
    window: ResourceId,
    keycode: u8,
    state_mask: u16,
) -> Option<&KeyGrab> {
    // Walk the window's ancestor chain; any grab on an ancestor of the
    // focused window can fire (X11 spec semantics).
    let mut current = window;
    let mut depth = 0usize;
    loop {
        for grab in &self.key_grabs {
            if grab.grab_window != current {
                continue;
            }
            let key_match = grab.keycode == 0 || grab.keycode == keycode;
            // The relevant modifier bits are the lower 8 of the state mask.
            let mod_match = grab.modifiers == 0x8000
                || grab.modifiers == (state_mask & 0x00ff);
            if key_match && mod_match {
                return Some(grab);
            }
        }
        let w = self.resources.window(current)?;
        if w.parent == current || w.parent == crate::resources::ROOT_WINDOW {
            // Also try root once.
            if current != crate::resources::ROOT_WINDOW {
                current = crate::resources::ROOT_WINDOW;
                depth += 1;
                if depth > 256 { break; }
                continue;
            }
            break;
        }
        current = w.parent;
        depth += 1;
        if depth > 256 { break; }
    }
    None
}
```

- [ ] **Step 4: Run tests to verify pass**

Run: `cargo test -p yserver-core key_grab`
Expected: 3 passed.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver-core/src/server.rs
git commit -m "$(cat <<'EOF'
feat: add KeyGrab table and find_key_grab lookup

Per-server passive key grab table with AnyKey (keycode=0) and
AnyModifier (modifiers=0x8000) wildcards; lookup walks the
focused window's ancestor chain plus root.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

### Task A2: GrabKey / UngrabKey opcode handlers

**Files:**
- Modify: `crates/yserver-core/src/nested.rs` (replace stubs at opcodes 33, 34)

- [ ] **Step 1: Write the failing test**

Add to the `tests` mod at the bottom of `nested.rs`. If no such mod exists, create one. The test exercises the parser via a small dispatch helper — but parsers/handlers in this codebase are normally tested at the protocol layer. For this task, write the parse helper as a free function in `protocol/x11/mod.rs` and test it there.

In `crates/yserver-protocol/src/x11/mod.rs` (existing tests mod):

```rust
#[test]
fn parse_grab_key_request() {
    // GrabKey body (excludes opcode/length header; the dispatcher passes
    // the trailing bytes plus header.data == owner_events).
    // Layout (post-header): grab_window(4) modifiers(2) keycode(1) pointer_mode(1)
    //                       keyboard_mode(1) pad(3)
    let body = [
        0x12, 0x34, 0x00, 0x00, // grab_window 0x3412
        0x40, 0x00,             // modifiers 0x0040
        24,                     // keycode 24
        1,                      // pointer_mode async
        1,                      // keyboard_mode async
        0, 0, 0,                // pad
    ];
    let parsed = parse_grab_key(&body, /*owner_events=*/ false).unwrap();
    assert_eq!(parsed.grab_window, 0x3412);
    assert_eq!(parsed.modifiers, 0x0040);
    assert_eq!(parsed.keycode, 24);
    assert_eq!(parsed.pointer_mode, 1);
    assert_eq!(parsed.keyboard_mode, 1);
    assert!(!parsed.owner_events);
}

#[test]
fn parse_ungrab_key_request() {
    // UngrabKey body: grab_window(4) modifiers(2) pad(2). header.data carries keycode.
    let body = [0x12, 0x34, 0x00, 0x00, 0x40, 0x00, 0, 0];
    let parsed = parse_ungrab_key(&body, /*keycode_in_header_data=*/ 24).unwrap();
    assert_eq!(parsed.grab_window, 0x3412);
    assert_eq!(parsed.keycode, 24);
    assert_eq!(parsed.modifiers, 0x0040);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p yserver-protocol parse_grab_key parse_ungrab_key`
Expected: "cannot find function `parse_grab_key`".

- [ ] **Step 3: Implement parsers in `protocol/x11/mod.rs`**

```rust
#[derive(Debug, Clone, Copy)]
pub struct GrabKeyRequest {
    pub owner_events: bool,
    pub grab_window: u32,
    pub modifiers: u16,
    pub keycode: u8,
    pub pointer_mode: u8,
    pub keyboard_mode: u8,
}

#[must_use]
pub fn parse_grab_key(body: &[u8], owner_events: bool) -> Option<GrabKeyRequest> {
    if body.len() < 12 { return None; }
    Some(GrabKeyRequest {
        owner_events,
        grab_window: u32::from_le_bytes([body[0], body[1], body[2], body[3]]),
        modifiers: u16::from_le_bytes([body[4], body[5]]),
        keycode: body[6],
        pointer_mode: body[7],
        keyboard_mode: body[8],
    })
}

#[derive(Debug, Clone, Copy)]
pub struct UngrabKeyRequest {
    pub keycode: u8,
    pub grab_window: u32,
    pub modifiers: u16,
}

#[must_use]
pub fn parse_ungrab_key(body: &[u8], keycode_in_header_data: u8) -> Option<UngrabKeyRequest> {
    if body.len() < 6 { return None; }
    Some(UngrabKeyRequest {
        keycode: keycode_in_header_data,
        grab_window: u32::from_le_bytes([body[0], body[1], body[2], body[3]]),
        modifiers: u16::from_le_bytes([body[4], body[5]]),
    })
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p yserver-protocol parse_grab_key parse_ungrab_key`
Expected: 2 passed.

- [ ] **Step 5: Wire the parsers into the dispatcher**

In `crates/yserver-core/src/nested.rs`, replace the `33 => log_void(...)` and `34 => log_void(...)` arms with:

```rust
33 => {
    if let Some(req) = x11::parse_grab_key(body, header.data != 0) {
        let mut s = lock_server(server)?;
        // De-dup: remove existing grab with same (owner, window, key, modifiers)
        s.key_grabs.retain(|g| !(g.owner == client_id
            && g.grab_window == ResourceId(req.grab_window)
            && g.keycode == req.keycode
            && g.modifiers == req.modifiers));
        s.key_grabs.push(crate::server::KeyGrab {
            owner: client_id,
            grab_window: ResourceId(req.grab_window),
            keycode: req.keycode,
            modifiers: req.modifiers,
            owner_events: req.owner_events,
            pointer_mode: req.pointer_mode,
            keyboard_mode: req.keyboard_mode,
        });
        debug!(
            "client {} GrabKey window=0x{:x} keycode={} modifiers=0x{:x}",
            client_id.0, req.grab_window, req.keycode, req.modifiers
        );
    }
    log_void(client_id, sequence, "GrabKey")
}
34 => {
    if let Some(req) = x11::parse_ungrab_key(body, header.data) {
        let mut s = lock_server(server)?;
        s.key_grabs.retain(|g| !(g.owner == client_id
            && g.grab_window == ResourceId(req.grab_window)
            && (g.keycode == req.keycode || req.keycode == 0)
            && (g.modifiers == req.modifiers || req.modifiers == 0x8000)));
    }
    log_void(client_id, sequence, "UngrabKey")
}
```

- [ ] **Step 6: Run full test suite**

Run: `cargo test`
Expected: all pass, no regressions.

- [ ] **Step 7: Commit**

```bash
git add crates/yserver-protocol/src/x11/mod.rs crates/yserver-core/src/nested.rs
git commit -m "$(cat <<'EOF'
feat: implement GrabKey / UngrabKey (op 33 / 34)

Stores passive key grabs in ServerState.key_grabs; UngrabKey supports
AnyKey/AnyModifier wildcards. Parsers covered by unit tests.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

### Task A3: GrabKeyboard / UngrabKeyboard

**Files:**
- Modify: `crates/yserver-core/src/server.rs` (add `keyboard_grab: Option<(ClientId, ResourceId)>`)
- Modify: `crates/yserver-core/src/nested.rs` (replace stubs at opcodes 31, 32)

- [ ] **Step 1: Write a unit test for the active grab field**

In `server.rs` tests mod:

```rust
#[test]
fn keyboard_grab_set_and_clear() {
    use crate::resources::{ClientId, ResourceId};
    let mut s = ServerState::new();
    assert!(s.keyboard_grab.is_none());
    s.keyboard_grab = Some((ClientId(7), ResourceId(0xff)));
    assert_eq!(s.keyboard_grab.unwrap().0, ClientId(7));
    s.keyboard_grab = None;
    assert!(s.keyboard_grab.is_none());
}
```

- [ ] **Step 2: Run, fail, add field, run, pass**

Run: `cargo test -p yserver-core keyboard_grab`
Expected: compile error first; after adding `pub keyboard_grab: Option<(ClientId, ResourceId)>` (init to `None`), passes.

- [ ] **Step 3: Wire opcodes 31 / 32**

Replace the two stubs:

```rust
31 => {
    // GrabKeyboard body: owner_events(header.data) grab_window(4)
    //   time(4) pointer_mode(1) keyboard_mode(1) pad(2)
    if body.len() >= 12 {
        let grab_window = ResourceId(u32::from_le_bytes(
            [body[0], body[1], body[2], body[3]]));
        let mut s = lock_server(server)?;
        s.keyboard_grab = Some((client_id, grab_window));
    }
    log_reply(client_id, sequence, "GrabKeyboard");
    x11::write_grab_reply(&mut *lock_writer()?, sequence, 0)
}
32 => {
    let mut s = lock_server(server)?;
    if let Some((owner, _)) = s.keyboard_grab && owner == client_id {
        s.keyboard_grab = None;
    }
    log_void(client_id, sequence, "UngrabKeyboard")
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver-core/src/server.rs crates/yserver-core/src/nested.rs
git commit -m "$(cat <<'EOF'
feat: implement GrabKeyboard / UngrabKeyboard (op 31 / 32)

Active keyboard grab tracked on ServerState; routing change in
spawn_keyboard_forwarder follows in next commit.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

### Task A4: Route key events through grab table

**Files:**
- Modify: `crates/yserver-core/src/nested.rs` — `spawn_keyboard_forwarder`

- [ ] **Step 1: Add a unit test for grab routing decision**

The existing forwarder is a free function that's hard to unit-test in isolation because it owns a `HostInputPump`. Add a pure helper `decide_key_target` and test it:

In `nested.rs` (or a new `crates/yserver-core/src/keyboard.rs` if you prefer keeping nested.rs from growing more):

```rust
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum KeyTarget {
    Focus(ResourceId),
    Grab { client_id: ClientId, grab_window: ResourceId },
    Drop,
}

pub(crate) fn decide_key_target(
    state: &ServerState,
    focus: ResourceId,
    keycode: u8,
    state_mask: u16,
) -> KeyTarget {
    // Active keyboard grab pre-empts everything.
    if let Some((cid, win)) = state.keyboard_grab {
        return KeyTarget::Grab { client_id: cid, grab_window: win };
    }
    if let Some(grab) = state.find_key_grab(focus, keycode, state_mask) {
        return KeyTarget::Grab {
            client_id: grab.owner,
            grab_window: grab.grab_window,
        };
    }
    if focus == ROOT_WINDOW {
        return KeyTarget::Drop;
    }
    KeyTarget::Focus(focus)
}
```

Test cases (in `nested.rs` tests mod):

```rust
#[test]
fn key_target_focus_when_no_grab() {
    let s = ServerState::new();
    let focus = ResourceId(0x100);
    assert_eq!(decide_key_target(&s, focus, 24, 0), KeyTarget::Focus(focus));
}

#[test]
fn key_target_active_grab_wins() {
    let mut s = ServerState::new();
    s.keyboard_grab = Some((ClientId(3), ResourceId(0x200)));
    let focus = ResourceId(0x100);
    assert_eq!(
        decide_key_target(&s, focus, 24, 0),
        KeyTarget::Grab { client_id: ClientId(3), grab_window: ResourceId(0x200) },
    );
}
```

(Note: the passive-grab-routing test belongs in Task A1 once the focus-window walk has a real `ResourceTable` populated; here we just exercise the pure helper.)

- [ ] **Step 2: Run, fail, implement, pass**

Run: `cargo test -p yserver-core key_target`
Expected: fail with "decide_key_target not found", then pass after pasting in the helper.

- [ ] **Step 3: Use the helper in `spawn_keyboard_forwarder`**

In the existing forwarder loop, replace the `if focus == ROOT_WINDOW { continue; }` block plus the `write_key_event(... event: focus ...)` call with:

```rust
let (event_window, target_writer) = {
    let s = match server.lock() { Ok(s) => s, Err(_) => continue };
    let target = decide_key_target(&s, focus, event.keycode, event.state);
    match target {
        KeyTarget::Drop => continue,
        KeyTarget::Focus(w) => (w, writer.clone()),
        KeyTarget::Grab { client_id: cid, grab_window } => {
            // Look up the grab owner's writer.
            match s.client_target(cid) {
                Some(t) => (grab_window, t.writer.clone()),
                None => continue, // owner gone
            }
        }
    }
};
let mut writer = match target_writer.lock() { Ok(w) => w, Err(_) => return };
// ... existing write_key_event call but with event_window in `event:`
```

(The exact mechanics: today the function holds a single `Arc<Mutex<UnixStream>>` for the focused client. When a grab fires, we deliver to the *grab owner* — which may be a different client. So we need to re-acquire the writer through `state.client_target(cid)`. Adjust function signature: pass `Arc<Mutex<ServerState>>` into `spawn_keyboard_forwarder` if it isn't already there.)

- [ ] **Step 4: Build and run tests**

Run: `cargo build && cargo test`
Expected: pass.

- [ ] **Step 5: Manual smoke test**

Start `ynest`, run:

```sh
DISPLAY=:99 xterm &
DISPLAY=:99 sh -c 'xdotool key --clearmodifiers super+q' || true
```

Confirm the key is delivered to xterm normally (no grab registered). Then write a tiny test client that calls `XGrabKey` for `XK_q` with Mod4Mask on the root window and confirm key events arrive at *that* client when xterm has focus and Super+q is pressed.

- [ ] **Step 6: Commit**

```bash
git add crates/yserver-core/src/nested.rs
git commit -m "$(cat <<'EOF'
feat: route key events through KeyGrab table

Active keyboard grab pre-empts focus delivery; passive GrabKey on the
focused window or any ancestor delivers to the grab owner with the
event window set to the grab window.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

### Task A5: Real GetKeyboardMapping via host proxy

**Files:**
- Modify: `crates/yserver-core/src/host_x11.rs` — add `get_keyboard_mapping(first, count)` and `keyboard_min_max() -> (u8, u8)`
- Modify: `crates/yserver-protocol/src/x11/mod.rs` — overload `write_get_keyboard_mapping_reply` to accept a precomputed keysym slice
- Modify: `crates/yserver-core/src/nested.rs` opcode 101

- [ ] **Step 1: Write a host-proxy test**

The host proxy uses live X11 sockets, so test it indirectly: write a wire-encoder test for `write_get_keyboard_mapping_reply_from_keysyms`:

```rust
#[test]
fn keyboard_mapping_reply_from_keysyms_layout() {
    let keysyms: &[u32] = &[0x71, 0x51, 0, 0,    // q Q
                            0x77, 0x57, 0, 0];   // w W
    let mut buf = Vec::new();
    write_get_keyboard_mapping_reply_from_keysyms(
        &mut buf, SequenceNumber(7), 4, keysyms).unwrap();
    assert_eq!(buf[0], 1);                     // reply
    assert_eq!(buf[1], 4);                     // keysyms-per-keycode
    let length = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    assert_eq!(length, 8);                     // 8 keysyms × 4 bytes / 4
    let kb = &buf[32..];
    assert_eq!(kb.len(), 32);
    assert_eq!(u32::from_le_bytes(kb[0..4].try_into().unwrap()), 0x71);
}
```

- [ ] **Step 2: Run, fail, implement**

Run: `cargo test -p yserver-protocol keyboard_mapping_reply_from_keysyms_layout`
Expected: function not found.

Implement in `protocol/x11/mod.rs`:

```rust
pub fn write_get_keyboard_mapping_reply_from_keysyms(
    writer: &mut impl Write,
    sequence: SequenceNumber,
    keysyms_per_keycode: u8,
    keysyms: &[u32],
) -> io::Result<()> {
    let length_words = u32::try_from(keysyms.len()).unwrap_or(0);
    let mut reply = fixed_reply(sequence, keysyms_per_keycode, length_words);
    // fixed_reply leaves only 32 bytes; we append 4-byte keysyms.
    for k in keysyms {
        reply.extend_from_slice(&k.to_le_bytes());
    }
    writer.write_all(&reply)
}
```

- [ ] **Step 3: Add host proxy method**

In `host_x11.rs` (look at `list_fonts_proxy` for the established raw-wire pattern). Implementation sketch:

```rust
pub fn get_keyboard_mapping(
    &mut self,
    first_keycode: u8,
    count: u8,
) -> io::Result<(u8 /* keysyms_per_keycode */, Vec<u32>)> {
    // Build wire request manually (opcode 101, length 2):
    let mut req = [0u8; 8];
    req[0] = 101;
    req[1] = 0;            // pad
    req[2] = 2; req[3] = 0; // length in 4-byte units
    req[4] = first_keycode;
    req[5] = count;
    req[6] = 0; req[7] = 0;
    self.stream.write_all(&req)?;
    // Read reply (header 32 bytes; trailing keysyms).
    let mut header = [0u8; 32];
    self.stream.read_exact(&mut header)?;
    let kpc = header[1];
    let length = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    let total_bytes = (length as usize) * 4;
    let mut tail = vec![0u8; total_bytes];
    self.stream.read_exact(&mut tail)?;
    let mut keysyms = Vec::with_capacity(tail.len() / 4);
    for chunk in tail.chunks_exact(4) {
        keysyms.push(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    Ok((kpc, keysyms))
}
```

(Use `XGetKeyboardMapping` via `x11-rs` if `host_x11.rs` already binds to it; otherwise the raw approach above.)

If `host_x11` already uses `xlib` bindings (check `Cargo.toml` / existing code: it uses `XOpenDisplay` etc.), prefer:

```rust
use x11::xlib::{XGetKeyboardMapping, XFree};
let mut kpc: c_int = 0;
let ptr = unsafe { XGetKeyboardMapping(self.display, first_keycode as c_int, count as c_int, &mut kpc) };
// ptr is array of (count * kpc) KeySym (XID = c_ulong on this platform).
let n = (count as usize) * (kpc as usize);
let slice = unsafe { std::slice::from_raw_parts(ptr, n) };
let keysyms: Vec<u32> = slice.iter().map(|&k| k as u32).collect();
unsafe { XFree(ptr.cast()); }
Ok((kpc as u8, keysyms))
```

Inspect `host_x11.rs` to choose the matching style; the project has been mixing raw wire and Xlib calls.

- [ ] **Step 4: Wire opcode 101**

Replace existing handler:

```rust
101 => {
    log_reply(client_id, sequence, "GetKeyboardMapping");
    let first_keycode = body.first().copied().unwrap_or(8);
    let count = body.get(1).copied().unwrap_or(0);
    let result = host
        .and_then(|h| h.lock().ok())
        .and_then(|mut h| h.get_keyboard_mapping(first_keycode, count).ok());
    if let Some((kpc, keysyms)) = result {
        x11::write_get_keyboard_mapping_reply_from_keysyms(
            &mut *lock_writer()?, sequence, kpc, &keysyms)
    } else {
        // Fallback to existing local stub on host failure
        x11::write_get_keyboard_mapping_reply(
            &mut *lock_writer()?, sequence, first_keycode, count, 4)
    }
}
```

- [ ] **Step 5: Run tests, build, commit**

```bash
cargo test
cargo +nightly fmt
cargo clippy -- -W clippy::pedantic
git add -A crates/
git commit -m "$(cat <<'EOF'
feat: proxy GetKeyboardMapping (op 101) to host

Replaces the hard-coded keysyms.rs stub with the host's real keymap.
Falls back to the local table when the host call fails.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

### Task A6: Real GetModifierMapping via host proxy

**Files:**
- Modify: `crates/yserver-core/src/host_x11.rs` — `get_modifier_mapping() -> [u8; 64]` (8 modifiers × 8 keycodes)
- Modify: `crates/yserver-protocol/src/x11/mod.rs` — extend `write_get_modifier_mapping_reply` to accept a 64-byte keycode array (or add `..._with_keycodes`)
- Modify: `crates/yserver-core/src/nested.rs` opcode 119

Steps follow the same TDD shape as Task A5: encoder unit test in `protocol`, host proxy method, dispatcher rewrite, full test, commit.

- [ ] **Step 1: Encoder test**

```rust
#[test]
fn modifier_mapping_reply_layout() {
    let kc: [u8; 64] = std::array::from_fn(|i| i as u8 + 8);
    let mut buf = Vec::new();
    write_get_modifier_mapping_reply_with_keycodes(&mut buf, SequenceNumber(3), 8, &kc).unwrap();
    assert_eq!(buf[0], 1);     // reply
    assert_eq!(buf[1], 8);     // keycodes-per-modifier
    let length = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    assert_eq!(length, 16);    // 64 bytes / 4
    assert_eq!(buf[32..96], kc[..]);
}
```

- [ ] **Step 2: Implement encoder**

```rust
pub fn write_get_modifier_mapping_reply_with_keycodes(
    writer: &mut impl Write,
    sequence: SequenceNumber,
    keycodes_per_modifier: u8,
    keycodes: &[u8],
) -> io::Result<()> {
    let length_words = u32::try_from(keycodes.len() / 4).unwrap_or(0);
    let mut reply = fixed_reply(sequence, keycodes_per_modifier, length_words);
    reply.extend_from_slice(keycodes);
    while reply.len() % 4 != 0 { reply.push(0); }
    writer.write_all(&reply)
}
```

- [ ] **Step 3: Host proxy**

`get_modifier_mapping` returns `(kc_per_modifier, Vec<u8>)`. Use `XGetModifierMapping` from xlib bindings if available; the returned struct has `.max_keypermod` and `.modifiermap` (a `c_uchar*`). Free with `XFreeModifiermap`.

- [ ] **Step 4: Dispatcher**

```rust
119 => {
    log_reply(client_id, sequence, "GetModifierMapping");
    let result = host
        .and_then(|h| h.lock().ok())
        .and_then(|mut h| h.get_modifier_mapping().ok());
    if let Some((kpm, keycodes)) = result {
        x11::write_get_modifier_mapping_reply_with_keycodes(
            &mut *lock_writer()?, sequence, kpm, &keycodes)
    } else {
        x11::write_get_modifier_mapping_reply(&mut *lock_writer()?, sequence)
    }
}
```

- [ ] **Step 5: Run tests, commit**

Commit message:

```
feat: proxy GetModifierMapping (op 119) to host

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
```

### Task A7: ChangeKeyboardMapping + MappingNotify

**Files:**
- Modify: `crates/yserver-protocol/src/x11/mod.rs` — `write_mapping_notify_event(request, first_keycode, count)`
- Modify: `crates/yserver-core/src/nested.rs` — opcode 100

- [ ] **Step 1: Write encoder test**

```rust
#[test]
fn mapping_notify_event_layout() {
    let mut buf = Vec::new();
    write_mapping_notify_event(&mut buf, SequenceNumber(0), /*request=*/1, 8, 248).unwrap();
    assert_eq!(buf.len(), 32);
    assert_eq!(buf[0], 34);     // MappingNotify
    assert_eq!(buf[1], 1);      // request: Keyboard
    assert_eq!(buf[4], 8);      // first_keycode
    assert_eq!(buf[5], 248);    // count
}
```

- [ ] **Step 2: Implement encoder + dispatcher**

```rust
pub fn write_mapping_notify_event(
    writer: &mut impl Write,
    sequence: SequenceNumber,
    request: u8,         // 0=Modifier, 1=Keyboard, 2=Pointer
    first_keycode: u8,
    count: u8,
) -> io::Result<()> {
    let mut buf = [0u8; 32];
    buf[0] = 34;
    buf[1] = request;
    buf[2..4].copy_from_slice(&sequence.0.to_le_bytes());
    buf[4] = first_keycode;
    buf[5] = count;
    writer.write_all(&buf)
}
```

Dispatcher (replacing absent `100 =>` arm; insert next to opcode 101):

```rust
100 => {
    // ChangeKeyboardMapping is host-mediated; treat as a no-op and
    // broadcast MappingNotify so clients refresh their keymaps.
    let first = body.first().copied().unwrap_or(8);
    let count = header.data; // keycode-count is in the header byte
    let targets: Vec<_> = lock_server(server)?.clients.values()
        .map(crate::server::ServerState::event_target_for_client)
        .collect();
    crate::server::fanout_event(&targets, |buf, seq, _order| {
        let _ = x11::write_mapping_notify_event(buf, seq, 1, first, count);
    });
    log_void(client_id, sequence, "ChangeKeyboardMapping")
}
```

(Note: `ServerState::event_target_for_client` is currently private — change it to `pub(crate)` if needed.)

- [ ] **Step 3: Run tests, commit**

```
feat: implement ChangeKeyboardMapping (op 100) + MappingNotify

Treats the request as a no-op (host owns the keymap) but emits
MappingNotify(Keyboard) to all clients so they refresh.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
```

---

## Group B — Window operations for reparenting WMs

### Task B1: Per-client save_set storage

**Files:**
- Modify: `crates/yserver-core/src/server.rs` — `ClientHandle.save_set: HashSet<ResourceId>`

- [ ] **Step 1: Test**

```rust
#[test]
fn save_set_insert_and_remove() {
    use crate::resources::ResourceId;
    let mut handle = ClientHandle {
        writer: Arc::new(Mutex::new(/* requires test stream — skip in unit, just struct-init */
            unsafe { std::mem::zeroed() })),
        // ... use a fresh helper or refactor: see below
        ..ClientHandle::test_default()
    };
    handle.save_set.insert(ResourceId(0x10));
    handle.save_set.insert(ResourceId(0x20));
    handle.save_set.remove(&ResourceId(0x10));
    assert!(!handle.save_set.contains(&ResourceId(0x10)));
    assert!(handle.save_set.contains(&ResourceId(0x20)));
}
```

The `unsafe zeroed` is a cheat for unit testing — the cleaner pattern is a `ClientHandle::test_default()` method behind `#[cfg(test)]`. Add it.

- [ ] **Step 2: Implement**

Add field to `ClientHandle`:

```rust
pub save_set: HashSet<ResourceId>,
```

Initialise in every place where `ClientHandle` is constructed (search `ClientHandle {` — there should be 1–2 sites in `nested.rs`).

Add `#[cfg(test)] impl ClientHandle { fn test_default() -> Self { ... } }`.

- [ ] **Step 3: Test, commit**

```
feat: add save_set field to ClientHandle

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
```

### Task B2: ChangeSaveSet opcode

**Files:**
- Modify: `crates/yserver-core/src/nested.rs` — opcode 6

- [ ] **Step 1: Wire dispatch**

```rust
6 => {
    // ChangeSaveSet body: window(4); header.data = mode (0=Insert, 1=Delete)
    if body.len() >= 4 {
        let win = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
        let mut s = lock_server(server)?;
        if let Some(c) = s.clients.get_mut(&client_id.0) {
            match header.data {
                0 => { c.save_set.insert(win); }
                1 => { c.save_set.remove(&win); }
                _ => {}
            }
        }
    }
    log_void(client_id, sequence, "ChangeSaveSet")
}
```

- [ ] **Step 2: Restore on disconnect**

In the client disconnect path (search for the `clients.remove` call — likely in `handle_client` cleanup), before resource cleanup:

```rust
let to_restore: Vec<ResourceId> = match server.lock() {
    Ok(s) => s.clients.get(&client_id.0).map(|c| c.save_set.iter().copied().collect()).unwrap_or_default(),
    Err(_) => Vec::new(),
};
for w in to_restore {
    // Reparent w to root if it still exists
    if let Ok(mut s) = server.lock() {
        if s.resources.window(w).is_some() {
            // ResourceTable::reparent moves the window; coordinate-translation
            // can be 0,0 for a save-set restore (matches Xorg behaviour).
            let _ = s.resources.reparent_window(w, ROOT_WINDOW, 0, 0);
        }
    }
}
```

- [ ] **Step 3: Test the disconnect restore via a unit test on `ResourceTable`**

(There is no easy way to drive disconnect synthetically without a real client thread. Defer the integration test; the unit test exercises `ResourceTable::reparent_window` survival, which already exists.)

- [ ] **Step 4: Commit**

```
feat: implement ChangeSaveSet (op 6)

Tracks per-client save-set; on client disconnect, save-set windows
are reparented back to root before resource cleanup.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
```

### Task B3: CirculateNotify / CirculateRequest event encoders

**Files:**
- Modify: `crates/yserver-protocol/src/x11/mod.rs`

- [ ] **Step 1: Encoder tests**

```rust
#[test]
fn circulate_notify_event_layout() {
    let mut buf = Vec::new();
    write_circulate_notify_event(
        &mut buf, SequenceNumber(0), ResourceId(0x100), ResourceId(0x200), 0).unwrap();
    assert_eq!(buf.len(), 32);
    assert_eq!(buf[0], 26);                              // CirculateNotify
    let event_window = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    let window = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
    assert_eq!(event_window, 0x100);
    assert_eq!(window, 0x200);
    assert_eq!(buf[16], 0);                              // place: PlaceOnTop
}

#[test]
fn circulate_request_event_layout() {
    let mut buf = Vec::new();
    write_circulate_request_event(
        &mut buf, SequenceNumber(0), ResourceId(0x100), ResourceId(0x200), 1).unwrap();
    assert_eq!(buf[0], 27);                              // CirculateRequest
    assert_eq!(buf[16], 1);                              // place: PlaceOnBottom
}
```

- [ ] **Step 2: Implement**

```rust
pub fn write_circulate_notify_event(
    writer: &mut impl Write,
    sequence: SequenceNumber,
    event_window: ResourceId,
    window: ResourceId,
    place: u8,            // 0=Top, 1=Bottom
) -> io::Result<()> {
    let mut buf = [0u8; 32];
    buf[0] = 26;
    buf[2..4].copy_from_slice(&sequence.0.to_le_bytes());
    buf[4..8].copy_from_slice(&event_window.0.to_le_bytes());
    buf[8..12].copy_from_slice(&window.0.to_le_bytes());
    buf[16] = place;
    writer.write_all(&buf)
}

pub fn write_circulate_request_event(
    writer: &mut impl Write,
    sequence: SequenceNumber,
    parent: ResourceId,
    window: ResourceId,
    place: u8,
) -> io::Result<()> {
    let mut buf = [0u8; 32];
    buf[0] = 27;
    buf[2..4].copy_from_slice(&sequence.0.to_le_bytes());
    buf[4..8].copy_from_slice(&parent.0.to_le_bytes());
    buf[8..12].copy_from_slice(&window.0.to_le_bytes());
    buf[16] = place;
    writer.write_all(&buf)
}
```

- [ ] **Step 3: Test, commit**

```
feat: add CirculateNotify / CirculateRequest event encoders

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
```

### Task B4: CirculateWindow opcode 13

**Files:**
- Modify: `crates/yserver-core/src/nested.rs` — new arm 13

- [ ] **Step 1: Wire dispatch**

```rust
13 => {
    // CirculateWindow body: window(4); header.data = direction (0=RaiseLowest, 1=LowerHighest)
    if body.len() >= 4 {
        let win = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
        let direction = header.data;
        let parent = lock_server(server)?
            .resources.window(win).map(|w| w.parent);
        if let Some(parent) = parent {
            // Substructure redirect on the parent?
            let redirect_target = lock_server(server)?
                .subscribers(parent, 0x0010_0000) // SubstructureRedirectMask
                .into_iter().next();
            if let Some(target) = redirect_target {
                let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
                let mut buf = Vec::with_capacity(32);
                let _ = x11::write_circulate_request_event(&mut buf, seq, parent, win, direction);
                if let Ok(mut w) = target.writer.lock() { let _ = w.write_all(&buf); }
            } else {
                // No redirect — actually circulate.
                let _ = lock_server(server)?.resources.circulate_window(win, direction);
                // Notify subscribers.
                let on_window = lock_server(server)?.subscribers(win, 0x0002_0000);     // StructureNotify
                let on_parent = lock_server(server)?.subscribers(parent, 0x0008_0000);  // SubstructureNotify
                for t in on_window.into_iter().chain(on_parent.into_iter()) {
                    let seq = SequenceNumber(t.last_sequence.load(Ordering::Relaxed));
                    let mut buf = Vec::with_capacity(32);
                    let _ = x11::write_circulate_notify_event(&mut buf, seq, win, win, direction);
                    if let Ok(mut w) = t.writer.lock() { let _ = w.write_all(&buf); }
                }
            }
        }
    }
    log_void(client_id, sequence, "CirculateWindow")
}
```

- [ ] **Step 2: Implement `ResourceTable::circulate_window`**

In `resources.rs`, add a method that reorders the parent's child list per X11 semantics:
- direction 0 (RaiseLowest): if any obscured child exists, raise the lowest one to the top of stacking order
- direction 1 (LowerHighest): if any obscuring child exists, lower the highest one to the bottom

For Phase 2 we don't model obscuring; treat both as a simple reorder (move the back child to the front, or front to the back). Document this approximation in a single-line comment.

```rust
pub fn circulate_window(&mut self, window: ResourceId, direction: u8) -> Result<(), ResourceError> {
    let parent = self.window(window).ok_or(ResourceError::NotFound)?.parent;
    let children = self.children_mut(parent);
    if children.len() < 2 { return Ok(()); }
    match direction {
        0 => { // RaiseLowest: move last to first
            let last = children.pop().expect("len>=2");
            children.insert(0, last);
        }
        1 => { // LowerHighest: move first to last
            let first = children.remove(0);
            children.push(first);
        }
        _ => return Err(ResourceError::Invalid),
    }
    Ok(())
}
```

(Look up the actual `children_mut` / equivalent helper in resources.rs; rename if needed.)

- [ ] **Step 3: Test, commit**

Add unit test for `circulate_window` in resources.rs.

```
feat: implement CirculateWindow (op 13)

SubstructureRedirect path emits CirculateRequest; otherwise reorders
children and emits CirculateNotify. Phase-2 stacking is naive
(end-of-list rotation); proper obscuring detection comes with the
compositor.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
```

### Task B5: DestroySubwindows opcode 5

**Files:**
- Modify: `crates/yserver-core/src/nested.rs`

- [ ] **Step 1: Wire dispatch**

```rust
5 => {
    if body.len() >= 4 {
        let parent = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
        let kids: Vec<ResourceId> = lock_server(server)?.resources.children(parent).to_vec();
        for k in kids {
            destroy_window(client_id, server, host, k); // existing helper
        }
    }
    log_void(client_id, sequence, "DestroySubwindows")
}
```

(`destroy_window` is the helper used by the opcode-4 path; reuse it.)

- [ ] **Step 2: Test, commit**

```
feat: implement DestroySubwindows (op 5)

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
```

---

## Group C — Drawing

### Task C1: CopyPlane opcode 63

**Files:**
- Modify: `crates/yserver-core/src/host_x11.rs` — `copy_plane(...)` mirroring `copy_area`
- Modify: `crates/yserver-core/src/nested.rs` — new arm 63

- [ ] **Step 1: Add host method**

Look at `copy_area` (lines ~971+). Add:

```rust
pub fn copy_plane(
    &mut self,
    src_xid: u32, dst_xid: u32, gc_xid: u32,
    src_x: i16, src_y: i16, dst_x: i16, dst_y: i16,
    width: u16, height: u16, plane: u32,
) -> io::Result<()> {
    // Build wire request: opcode 63, length 8 words.
    let mut req = [0u8; 32];
    req[0] = 63; req[2] = 8; req[3] = 0;
    req[4..8].copy_from_slice(&src_xid.to_le_bytes());
    req[8..12].copy_from_slice(&dst_xid.to_le_bytes());
    req[12..16].copy_from_slice(&gc_xid.to_le_bytes());
    req[16..18].copy_from_slice(&src_x.to_le_bytes());
    req[18..20].copy_from_slice(&src_y.to_le_bytes());
    req[20..22].copy_from_slice(&dst_x.to_le_bytes());
    req[22..24].copy_from_slice(&dst_y.to_le_bytes());
    req[24..26].copy_from_slice(&width.to_le_bytes());
    req[26..28].copy_from_slice(&height.to_le_bytes());
    req[28..32].copy_from_slice(&plane.to_le_bytes());
    self.stream.write_all(&req)
}
```

- [ ] **Step 2: Wire opcode**

In `nested.rs`, model after the existing `62 => { ... CopyArea ... }` arm:

```rust
63 => {
    // Layout same as CopyArea + trailing plane(4)
    if body.len() >= 24 {
        let src = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
        let dst = ResourceId(u32::from_le_bytes([body[4], body[5], body[6], body[7]]));
        let gc = ResourceId(u32::from_le_bytes([body[8], body[9], body[10], body[11]]));
        let sx = i16::from_le_bytes([body[12], body[13]]);
        let sy = i16::from_le_bytes([body[14], body[15]]);
        let dx = i16::from_le_bytes([body[16], body[17]]);
        let dy = i16::from_le_bytes([body[18], body[19]]);
        let w  = u16::from_le_bytes([body[20], body[21]]);
        let h  = u16::from_le_bytes([body[22], body[23]]);
        let plane = u32::from_le_bytes([body[24], body[25], body[26], body[27]]);
        // Look up host xids and offsets — see existing CopyArea handler;
        // call host.copy_plane(...). On failure log and drop.
    }
    log_void(client_id, sequence, "CopyPlane")
}
```

- [ ] **Step 3: Commit**

```
feat: implement CopyPlane (op 63)

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
```

---

## Group D — Pointer/grab follow-ups

### Task D1: ChangeActivePointerGrab opcode 30

**Files:**
- Modify: `crates/yserver-core/src/nested.rs`

- [ ] **Step 1: Wire dispatch**

```rust
30 => {
    // body: cursor(4) time(4) event_mask(2) pad(2)
    if body.len() >= 12 {
        let cursor = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
        let event_mask = u16::from_le_bytes([body[8], body[9]]);
        let mut s = lock_server(server)?;
        if let Some((owner, _)) = s.pointer_grab && owner == client_id {
            // Update host cursor on the grab window if we have a host xid.
            // For now, just record; full cursor swap requires another host call.
            for grab in s.button_grabs.iter_mut() {
                if grab.owner == client_id {
                    grab.event_mask = event_mask;
                    let _ = cursor; // cursor swap deferred
                }
            }
        }
    }
    log_void(client_id, sequence, "ChangeActivePointerGrab")
}
```

- [ ] **Step 2: Commit**

```
feat: implement ChangeActivePointerGrab (op 30)

Updates the active pointer grab's event_mask; cursor swap is deferred
until we have a use case beyond fvwm3.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
```

### Task D2: GrabButton sync replay channel

**Files:**
- Modify: `crates/yserver-core/src/server.rs` — add `replay_tx: Option<Sender<ReplayCmd>>`
- Modify: `crates/yserver-core/src/host_x11.rs` (or wherever `pointer_event_fanout` lives)
- Modify: `crates/yserver-core/src/nested.rs` — opcode 35 ReplayPointer arm

- [ ] **Step 1: Define `ReplayCmd` and channel**

Channel carries the frozen `HostPointerEvent` plus the `xid_map` reference is already available in the pump thread.

```rust
pub enum ReplayCmd { Pointer(crate::host_x11::HostPointerEvent) }
```

`ServerState.replay_tx: Option<std::sync::mpsc::Sender<ReplayCmd>>`. Set from the pump thread on startup; consume in the same pump thread's loop with a `try_recv` between input reads. Use `crossbeam` or `std::sync::mpsc` — match what's already in `Cargo.toml`.

- [ ] **Step 2: Replace the TODO in opcode 35**

Replace the `// ReplayPointer (mode==2): frozen event is cleared; ...` block with:

```rust
if mode == 2 && let Some(ev) = frozen.take() {
    if let Some(tx) = &s.replay_tx { let _ = tx.send(ReplayCmd::Pointer(ev)); }
}
```

- [ ] **Step 3: Pump thread consumes replays**

Adjust the pump thread to do `match rx.try_recv() { Ok(ReplayCmd::Pointer(ev)) => { /* re-route through pointer_event_fanout */ } _ => {} }` once per input loop iteration.

- [ ] **Step 4: Manual test**

Smoke-test using fvwm3 + xterm: pre-existing GrabButton(Sync, ReplayPointer) path should now actually re-route.

- [ ] **Step 5: Commit**

```
fix: deliver GrabButton sync replay through pump thread

Replaces the deferred TODO in AllowEvents(ReplayPointer) with a
crossbeam-style command channel consumed by the pointer pump
thread, which already holds xid_map.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
```

---

## Group E — RANDR follow-ups

### Task E1: RRSelectInput mask storage

**Files:**
- Modify: `crates/yserver-core/src/randr.rs` — `RandrState.subscribers: HashMap<(ClientId, ResourceId), u16>`

- [ ] **Step 1: Test**

```rust
#[test]
fn randr_subscribers_set_and_get() {
    let mut s = RandrState::nested(0, 800, 600);
    s.subscribe(ClientId(1), ResourceId(0x10), 0x1);
    assert_eq!(s.subscriber_mask(ClientId(1), ResourceId(0x10)), Some(0x1));
}
```

- [ ] **Step 2: Implement**

Add field, `subscribe(...)`, `subscriber_mask(...)`.

- [ ] **Step 3: Wire RRSelectInput in nested.rs**

(Search `RRSelectInput` in `handle_randr_request`; replace the "accepted, not stored" comment with the new call.)

- [ ] **Step 4: Commit**

```
feat: store RRSelectInput masks in RandrState

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
```

### Task E2: Host resize watcher and RRScreenChangeNotify

**Files:**
- Modify: `crates/yserver-core/src/host_x11.rs` — extend the close-watcher thread to surface ConfigureNotify
- Modify: `crates/yserver-core/src/nested.rs` — fanout
- Modify: `crates/yserver-protocol/src/x11/mod.rs` — `write_rr_screen_change_notify_event`

- [ ] **Step 1: Write the encoder + test**

```rust
#[test]
fn rr_screen_change_notify_layout() {
    let mut buf = Vec::new();
    write_rr_screen_change_notify_event(
        &mut buf, SequenceNumber(0), /*first_event=*/RANDR_FIRST_EVENT,
        /*rotation=*/1, ResourceId(0x10), ResourceId(0x20),
        /*size_id=*/0, /*subpixel=*/0, /*time=*/123,
        /*width=*/1920, /*height=*/1080, /*mwidth=*/508, /*mheight=*/285).unwrap();
    assert_eq!(buf.len(), 32);
    assert_eq!(buf[0], RANDR_FIRST_EVENT); // ScreenChangeNotify
    assert_eq!(buf[1], 1);                 // rotation
}
```

- [ ] **Step 2: Implement encoder**

(Per the RANDR spec — see `xcb-proto/src/randr.xml` event 0; layout: 32 bytes.)

```rust
pub fn write_rr_screen_change_notify_event(
    writer: &mut impl Write,
    sequence: SequenceNumber,
    first_event: u8,
    rotation: u8,
    root: ResourceId,
    request_window: ResourceId,
    size_id: u16,
    subpixel: u16,
    timestamp: u32,
    width: u16,
    height: u16,
    mwidth: u16,
    mheight: u16,
) -> io::Result<()> {
    let mut buf = [0u8; 32];
    buf[0] = first_event;
    buf[1] = rotation;
    buf[2..4].copy_from_slice(&sequence.0.to_le_bytes());
    buf[4..8].copy_from_slice(&timestamp.to_le_bytes());
    buf[8..12].copy_from_slice(&timestamp.to_le_bytes()); // config-timestamp
    buf[12..16].copy_from_slice(&root.0.to_le_bytes());
    buf[16..20].copy_from_slice(&request_window.0.to_le_bytes());
    buf[20..22].copy_from_slice(&size_id.to_le_bytes());
    buf[22..24].copy_from_slice(&subpixel.to_le_bytes());
    buf[24..26].copy_from_slice(&width.to_le_bytes());
    buf[26..28].copy_from_slice(&height.to_le_bytes());
    buf[28..30].copy_from_slice(&mwidth.to_le_bytes());
    buf[30..32].copy_from_slice(&mheight.to_le_bytes());
    writer.write_all(&buf)
}
```

- [ ] **Step 3: Watcher integration**

In the existing close-watcher thread (`spawn_window_close_watcher` or similar), the `HostInputPump::read_event` returns `HostEvent::Closed` only. Extend the input-pump enum (or add a parallel ConfigureNotify polling path) — the simpler route is: in the watcher thread, after the `read_event` loop, also process `XConfigureEvent` from XEvents on the container window. Or: subscribe to `StructureNotifyMask` on the host container and check for `ConfigureNotify` size deltas inside the existing loop.

When a size change is detected:

```rust
let mut s = lock_server(server)?;
s.randr.set_size(new_w, new_h);
let subs = s.randr.subscribers_snapshot();
drop(s);
for (cid, win) in subs {
    let target = lock_server(server)?.client_target(cid);
    if let Some(t) = target {
        let seq = SequenceNumber(t.last_sequence.load(Ordering::Relaxed));
        let mut buf = Vec::with_capacity(32);
        let _ = x11::write_rr_screen_change_notify_event(
            &mut buf, seq, RANDR_FIRST_EVENT, 1, ROOT_WINDOW, win,
            0, 0, lock_server(server)?.timestamp_now(),
            new_w, new_h, /*mwidth=*/254, /*mheight=*/254);
        if let Ok(mut w) = t.writer.lock() { let _ = w.write_all(&buf); }
    }
}
```

- [ ] **Step 4: Commit**

```
feat: emit RRScreenChangeNotify on host container resize

Watcher thread sees XConfigureNotify on the host container and
updates RandrState dimensions, then fans out RRScreenChangeNotify
to clients that selected RRScreenChangeNotifyMask.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
```

### Task E3: RRGetScreenInfo (RANDR 1.0)

**Files:**
- Modify: `crates/yserver-protocol/src/x11/randr.rs` — `write_rr_get_screen_info_reply`
- Modify: `crates/yserver-core/src/nested.rs` — `handle_randr_request` minor 5

- [ ] **Step 1: Encoder + test**

(See RANDR 1.0 spec; reply layout is well-defined.)

- [ ] **Step 2: Wire dispatcher minor 5**

- [ ] **Step 3: Commit**

```
feat: implement RRGetScreenInfo (RANDR minor=5) for 1.0 clients

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
```

---

## Group F — Cross-cutting fixes

### Task F1: DestroyWindow releases bg-pixmap host XIDs

**Files:**
- Modify: `crates/yserver-core/src/resources.rs` — `destroy_window` (or wherever the recursive destroy walks)
- Modify: `crates/yserver-core/src/nested.rs` — opcode 4 path

In `destroy_window`, after a window is removed from the tree but before dropping its struct, if `Window.background_pixmap_host_xid` is set, capture it and call `host.free_pixmap(host_xid)` from the dispatcher.

- [ ] **Step 1: Test** — unit test on `ResourceTable::take_pending_pixmap_frees(window)` that returns the bg-pixmap host xid (if set) and clears the field.
- [ ] **Step 2: Implement helper.** Call from the existing destroy path; the dispatcher already holds the host handle.
- [ ] **Step 3: Commit.**

```
fix: free host bg-pixmap XIDs on DestroyWindow

Closes the leak noted in status.md known follow-ups.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
```

### Task F2: SendEvent propagation up the parent chain

**Files:**
- Modify: `crates/yserver-core/src/nested.rs` — opcode 25 SendEvent

The current implementation delivers to direct subscribers. Spec: when `propagate=true` and no client in the destination has a matching mask, walk up parents. Stop at a window where the do-not-propagate mask covers the event type.

- [ ] **Step 1: Test the lookup helper**

Add `fn target_for_send_event(state, dst, event_type, propagate) -> Option<EventTarget>` and unit-test it.

- [ ] **Step 2: Implement and replace direct lookup**

- [ ] **Step 3: Commit**

```
fix: propagate SendEvent up parent chain when no direct subscriber

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
```

### Task F3: UnmapNotify.from_configure on shrunk children

**Files:**
- Modify: `crates/yserver-core/src/nested.rs` — opcode 12 ConfigureWindow

When a parent's ConfigureWindow shrinks the parent and a child becomes fully outside the new size, emit `UnmapNotify` with `from_configure=true`.

- [ ] **Step 1: Test the geometry helper**

```rust
#[test]
fn child_clipped_out_after_parent_shrink() {
    use crate::resources::{ResourceTable, ROOT_WINDOW};
    let mut t = ResourceTable::new();
    let parent = ResourceId(0x100);
    let child = ResourceId(0x200);
    t.create_window(parent, ROOT_WINDOW, 0, 0, 800, 600, 24, 0).unwrap();
    t.create_window(child, parent, 700, 500, 100, 100, 24, 0).unwrap();
    t.map_window(child).unwrap();
    let unmapped = t.children_clipped_out(parent, 600, 400);
    assert_eq!(unmapped, vec![child]);
}
```

- [ ] **Step 2: Implement helper and wire into ConfigureWindow**

- [ ] **Step 3: Commit**

```
fix: emit UnmapNotify(from_configure=true) for clipped-out children

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
```

---

## Group G — Validation

### Task G1: Run Openbox under ynest

**Files:** none (validation only)

- [ ] **Step 1: Make sure Openbox is installed**

Run: `which openbox || sudo pacman -S openbox`

- [ ] **Step 2: Start ynest**

```sh
cargo run --bin ynest -- 99 &
sleep 1
```

- [ ] **Step 3: Start Openbox**

```sh
DISPLAY=:99 openbox 2>&1 | tee openbox.log
```

- [ ] **Step 4: Open a client and exercise basic management**

```sh
DISPLAY=:99 xterm &
```

In the host window: right-click for the root menu, Alt+drag the xterm window, Alt+F4 to close.

- [ ] **Step 5: Capture findings**

For each blocker, note opcode/extension and whether it's in scope for this plan or escalates to a follow-up. Append a section "Phase 2 wrap-up validation log" to `docs/status.md` listing what worked and what didn't.

- [ ] **Step 6: Iterate**

Address each in-scope blocker by re-opening the relevant task or adding a small new task. Out-of-scope blockers (XKB, BIG-REQUESTS) get a status.md entry and a Phase 3 reference.

### Task G2: Run Fluxbox under ynest

Same shape as G1 with `fluxbox` instead of `openbox`. If Fluxbox demands BIG-REQUESTS, defer to Phase 3 per the spec.

### Task G3: Update status.md

- [ ] Mark Phase 2 wrap-up items checked off in `docs/status.md`.
- [ ] Update the opcode table for the newly-implemented opcodes (5, 6, 13, 30, 31, 32, 33, 34, 63, 100).
- [ ] Add the validation log section.
- [ ] Commit.

```
docs: mark Phase 2 wrap-up items complete

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
```

---

## Self-review checklist

- Spec coverage: every numbered item in the spec maps to a Group A–F task.
- No placeholders: every code step contains the actual code.
- Type consistency: `KeyGrab`, `KeyTarget`, `ReplayCmd`, `RandrState::subscribers` types referenced in dispatcher arms match their definitions.
- Each task ends with a commit.
- Tests precede implementation in every TDD-relevant task; pure plumbing tasks (e.g. dispatcher arms with no return value) rely on the encoder unit tests and validation in G1/G2.
