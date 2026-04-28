# UnmapNotify Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Emit `UnmapNotify` (event 18) on every mapped → unmapped transition, both explicit (opcode 10 `UnmapWindow`) and implicit (opcode 4 `DestroyWindow` + client disconnect cleanup).

**Architecture:** Add an `encode_unmap_notify_event` encoder in `yserver-protocol`. Change `ResourceTable::unmap_window` to return `bool` (mapped → unmapped transition occurred) and to silently no-op on the root window. Wire opcode 10 to the snapshot-then-fanout pattern already used by other notify events. Extend the existing destroy `pending` tuple in opcode 4 and disconnect cleanup with a `was_mapped` flag and emit `UnmapNotify` immediately before each `DestroyNotify`.

**Tech Stack:** Rust 2024, std `Mutex`/`Arc`, `proptest` (already a dev-dependency in both crates).

**Spec:** [`docs/superpowers/specs/2026-04-28-unmap-notify-design.md`](../specs/2026-04-28-unmap-notify-design.md).

**Project conventions** (run on every commit, see `~/.claude/CLAUDE.md`):

```sh
cargo fmt
cargo clippy -- -W clippy::pedantic
cargo test
```

Fix all warnings before committing.

---

## File Structure

| Path | Status | Purpose |
|---|---|---|
| `crates/yserver-protocol/src/x11/mod.rs` | modify | Add `encode_unmap_notify_event` (paired with existing `encode_destroy_notify_event`); add `unmap_notify_tests` submodule |
| `crates/yserver-core/src/resources.rs` | modify | Change `unmap_window` to return `bool` and no-op on the root; add `#[cfg(test)] mod tests` covering the new state-transition contract |
| `crates/yserver-core/src/nested.rs` | modify | Opcode 10: snapshot subscribers + fanout `UnmapNotify` on transition. Opcode 4 + disconnect cleanup: extend the destroy `pending` tuple with `was_mapped`; emit `UnmapNotify` before `DestroyNotify` per window |
| `crates/yserver-core/src/server.rs` | modify | Add one integration-style test (`unmap_notify_fanout_reaches_only_subscribed_clients`) alongside the existing `subscribers` tests |

The implementation is three commits, sequenced bottom-up so each commit compiles independently:

1. **Protocol encoder** — pure addition, no callers.
2. **Resource-table contract** — `unmap_window` returns `bool` + root no-op. The lone caller (opcode 10 in `nested.rs`) is updated to `let _ = …` to compile; behavior unchanged.
3. **Wire fanout** — opcode 10, opcode 4, disconnect cleanup. Adds the server-level integration test.

---

## Commit 1 — `encode_unmap_notify_event` in `yserver-protocol`

### Task 1.1: Add the unit-shape test

**Files:**
- Modify: `crates/yserver-protocol/src/x11/mod.rs` (tests at `:1673-1875`)

- [ ] **Step 1: Add the failing unit test**

Append a new submodule alongside the existing `destroy_notify_tests` (inside `mod tests`, at the bottom of the file before the closing `}` at `:1875`):

```rust
mod unmap_notify_tests {
    use super::*;
    #[test]
    fn shape() {
        let mut buf = Vec::new();
        encode_unmap_notify_event(
            &mut buf,
            SequenceNumber(0x1234),
            ClientByteOrder::LittleEndian,
            ResourceId(0x100),
            ResourceId(0x100002),
            false,
        );
        assert_eq!(buf.len(), 32);
        assert_eq!(buf[0], 18);
        assert_eq!(buf[1], 0);
        assert_eq!(&buf[2..4], &[0x34, 0x12]);
        assert_eq!(&buf[4..8], &0x100u32.to_le_bytes());
        assert_eq!(&buf[8..12], &0x100002u32.to_le_bytes());
        assert_eq!(buf[12], 0);
        assert!(buf[13..32].iter().all(|&b| b == 0));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p yserver-protocol unmap_notify_tests::shape`
Expected: compile error — `encode_unmap_notify_event` is not defined.

- [ ] **Step 3: Add the encoder**

Insert after `encode_destroy_notify_event` (at `:1672`, before `#[cfg(test)]`):

```rust
pub fn encode_unmap_notify_event(
    out: &mut Vec<u8>,
    sequence: SequenceNumber,
    order: ClientByteOrder,
    event_window: ResourceId,
    window: ResourceId,
    from_configure: bool,
) {
    out.push(18); // UnmapNotify
    out.push(0);
    write_u16(order, out, sequence.0);
    write_u32(order, out, event_window.0);
    write_u32(order, out, window.0);
    out.push(u8::from(from_configure));
    out.extend_from_slice(&[0; 19]);
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p yserver-protocol unmap_notify_tests::shape`
Expected: PASS.

### Task 1.2: Add the encoder round-trip proptest

**Files:**
- Modify: `crates/yserver-protocol/src/x11/mod.rs` (inside `mod unmap_notify_tests`)

- [ ] **Step 1: Extend `unmap_notify_tests` with a proptest**

Inside `mod unmap_notify_tests`, add (right after `fn shape`):

```rust
use proptest::prelude::*;

proptest! {
    #[test]
    fn encoder_round_trip(
        sequence in any::<u16>(),
        event_window in any::<u32>(),
        window in any::<u32>(),
        from_configure: bool,
        big_endian: bool,
    ) {
        let order = if big_endian {
            ClientByteOrder::BigEndian
        } else {
            ClientByteOrder::LittleEndian
        };
        let mut buf = Vec::new();
        encode_unmap_notify_event(
            &mut buf,
            SequenceNumber(sequence),
            order,
            ResourceId(event_window),
            ResourceId(window),
            from_configure,
        );
        prop_assert_eq!(buf.len(), 32);
        prop_assert_eq!(buf[0], 18);
        prop_assert_eq!(buf[1], 0);

        let seq_bytes = if big_endian {
            sequence.to_be_bytes()
        } else {
            sequence.to_le_bytes()
        };
        prop_assert_eq!(&buf[2..4], &seq_bytes[..]);

        let ew_bytes = if big_endian {
            event_window.to_be_bytes()
        } else {
            event_window.to_le_bytes()
        };
        prop_assert_eq!(&buf[4..8], &ew_bytes[..]);

        let w_bytes = if big_endian {
            window.to_be_bytes()
        } else {
            window.to_le_bytes()
        };
        prop_assert_eq!(&buf[8..12], &w_bytes[..]);

        prop_assert_eq!(buf[12], u8::from(from_configure));
        prop_assert!(buf[13..32].iter().all(|&b| b == 0));
    }
}
```

- [ ] **Step 2: Run the proptest**

Run: `cargo test -p yserver-protocol unmap_notify_tests`
Expected: PASS for both `shape` and `encoder_round_trip`.

### Task 1.3: Verify, format, lint, commit

- [ ] **Step 1: Run the full crate test suite**

Run: `cargo test -p yserver-protocol`
Expected: 9 tests pass (was 7, +2).

- [ ] **Step 2: Format and lint**

Run: `cargo fmt && cargo clippy -- -W clippy::pedantic`
Expected: clean exit, no warnings.

- [ ] **Step 3: Commit**

```bash
git add crates/yserver-protocol/src/x11/mod.rs
git commit -m "feat(protocol): add encode_unmap_notify_event"
```

---

## Commit 2 — `unmap_window` returns `bool` + root no-op

### Task 2.1: Add a tests module to `resources.rs`

**Files:**
- Modify: `crates/yserver-core/src/resources.rs` (append after the last item in the file)

- [ ] **Step 1: Add a `#[cfg(test)] mod tests` skeleton at the bottom of the file**

Append after the `Cursor` struct at `:472`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use yserver_protocol::x11::{ClientId, CreateWindowRequest};

    fn make_window(table: &mut ResourceTable, id: u32) {
        table.create_window(
            ClientId(1),
            CreateWindowRequest {
                depth: 24,
                window: ResourceId(id),
                parent: ROOT_WINDOW,
                x: 0,
                y: 0,
                width: 100,
                height: 100,
                border_width: 0,
                class: 1,
                visual: ROOT_VISUAL,
                background_pixel: None,
                event_mask: None,
                override_redirect: None,
            },
        );
    }
}
```

The `CreateWindowRequest` field order above matches `crates/yserver-protocol/src/x11/mod.rs:114-128`. `create_window` constructs the new `Window` with `map_state: MapState::Unmapped`, which is the precondition every test below relies on.

- [ ] **Step 2: Verify it compiles**

Run: `cargo test -p yserver-core --no-run`
Expected: builds cleanly.

### Task 2.2: Write the five unit tests for the new contract (RED)

**Files:**
- Modify: `crates/yserver-core/src/resources.rs` (inside the new `mod tests`)

- [ ] **Step 1: Add all five tests**

Inside `mod tests`, after `fn make_window`:

```rust
#[test]
fn unmap_window_returns_true_on_transition_from_viewable() {
    let mut table = ResourceTable::new();
    make_window(&mut table, 0x100002);
    table.map_window(ResourceId(0x100002));
    assert_eq!(
        table.window(ResourceId(0x100002)).unwrap().map_state,
        MapState::Viewable
    );
    let was_mapped = table.unmap_window(ResourceId(0x100002));
    assert!(was_mapped);
    assert_eq!(
        table.window(ResourceId(0x100002)).unwrap().map_state,
        MapState::Unmapped
    );
}

#[test]
fn unmap_window_returns_true_on_transition_from_unviewable() {
    let mut table = ResourceTable::new();
    make_window(&mut table, 0x100002);
    // Force Unviewable directly — no public setter, but the field is pub.
    table.windows.get_mut(&0x100002).unwrap().map_state = MapState::Unviewable;
    let was_mapped = table.unmap_window(ResourceId(0x100002));
    assert!(was_mapped);
    assert_eq!(
        table.window(ResourceId(0x100002)).unwrap().map_state,
        MapState::Unmapped
    );
}

#[test]
fn unmap_window_returns_false_when_already_unmapped() {
    let mut table = ResourceTable::new();
    make_window(&mut table, 0x100002);
    // create_window leaves new windows Unmapped.
    assert_eq!(
        table.window(ResourceId(0x100002)).unwrap().map_state,
        MapState::Unmapped
    );
    let first = table.unmap_window(ResourceId(0x100002));
    assert!(!first);
    let second = table.unmap_window(ResourceId(0x100002));
    assert!(!second);
}

#[test]
fn unmap_window_returns_false_for_unknown_window() {
    let mut table = ResourceTable::new();
    let was_mapped = table.unmap_window(ResourceId(0x9999_9999));
    assert!(!was_mapped);
}

#[test]
fn unmap_window_no_ops_on_root() {
    let mut table = ResourceTable::new();
    assert_eq!(
        table.window(ROOT_WINDOW).unwrap().map_state,
        MapState::Viewable
    );
    let was_mapped = table.unmap_window(ROOT_WINDOW);
    assert!(!was_mapped);
    assert_eq!(
        table.window(ROOT_WINDOW).unwrap().map_state,
        MapState::Viewable
    );
}
```

> **Note on `table.windows.get_mut(...)`:** the `windows` field is currently private (`HashMap<u32, Window>` at `:20`). Tests are in the same module via `super::*`, so they can access the private field. If clippy's `pedantic` complains about the direct field write in `unviewable`, leave the access — adding a public setter for one test would over-engineer.

- [ ] **Step 2: Run the tests, expect compile failure**

Run: `cargo test -p yserver-core resources::tests`
Expected: FAILS to compile — `unmap_window` returns `()`, not `bool`.

### Task 2.3: Update `unmap_window` to return `bool` + root no-op (GREEN)

**Files:**
- Modify: `crates/yserver-core/src/resources.rs:151-155`

- [ ] **Step 1: Replace the body of `unmap_window`**

Replace `:151-155`:

```rust
pub fn unmap_window(&mut self, id: ResourceId) {
    if let Some(window) = self.windows.get_mut(&id.0) {
        window.map_state = MapState::Unmapped;
    }
}
```

with:

```rust
pub fn unmap_window(&mut self, id: ResourceId) -> bool {
    if id == ROOT_WINDOW {
        return false;
    }
    let Some(window) = self.windows.get_mut(&id.0) else {
        return false;
    };
    let was_mapped = window.map_state != MapState::Unmapped;
    window.map_state = MapState::Unmapped;
    was_mapped
}
```

- [ ] **Step 2: Update the lone caller in `nested.rs:728`**

In `crates/yserver-core/src/nested.rs:725-731`, change:

```rust
10 => {
    if let Some(window) = x11::map_window_id(body) {
        let mut s = lock_server(server)?;
        s.resources.unmap_window(window);
    }
    log_void(client_id, sequence, "UnmapWindow")
}
```

to:

```rust
10 => {
    if let Some(window) = x11::map_window_id(body) {
        let mut s = lock_server(server)?;
        let _ = s.resources.unmap_window(window);
    }
    log_void(client_id, sequence, "UnmapWindow")
}
```

(The `let _ =` makes the change source-compatible without using the new return value yet — Commit 3 wires up the fanout.)

- [ ] **Step 3: Run resources tests**

Run: `cargo test -p yserver-core resources::tests`
Expected: 5 tests pass.

### Task 2.4: Add the proptest state-machine

**Files:**
- Modify: `crates/yserver-core/src/resources.rs` (inside `mod tests`)

- [ ] **Step 1: Add the proptest**

Inside `mod tests`, add after the unit tests:

```rust
use proptest::prelude::*;

#[derive(Debug, Clone, Copy)]
enum InitialState {
    Viewable,
    Unviewable,
    Unmapped,
}

fn arb_initial() -> impl Strategy<Value = InitialState> {
    prop_oneof![
        Just(InitialState::Viewable),
        Just(InitialState::Unviewable),
        Just(InitialState::Unmapped),
    ]
}

proptest! {
    #[test]
    fn unmap_window_state_machine(
        initial in arb_initial(),
        n in 1usize..=5,
    ) {
        let mut table = ResourceTable::new();
        make_window(&mut table, 0x100002);
        let target = ResourceId(0x100002);
        let initial_map_state = match initial {
            InitialState::Viewable => MapState::Viewable,
            InitialState::Unviewable => MapState::Unviewable,
            InitialState::Unmapped => MapState::Unmapped,
        };
        table.windows.get_mut(&target.0).unwrap().map_state = initial_map_state;

        let mut results = Vec::with_capacity(n);
        for _ in 0..n {
            results.push(table.unmap_window(target));
        }

        let expected_first = !matches!(initial, InitialState::Unmapped);
        prop_assert_eq!(results[0], expected_first);
        for r in results.iter().skip(1) {
            prop_assert!(!*r, "subsequent calls must return false");
        }
        prop_assert_eq!(
            table.window(target).unwrap().map_state,
            MapState::Unmapped
        );
    }
}
```

- [ ] **Step 2: Run the proptest**

Run: `cargo test -p yserver-core resources::tests::unmap_window_state_machine`
Expected: PASS.

### Task 2.5: Verify, format, lint, commit

- [ ] **Step 1: Run all yserver-core tests**

Run: `cargo test -p yserver-core`
Expected: 48 tests pass (was 42, +5 unit + 1 proptest = 6).

- [ ] **Step 2: Format and lint**

Run: `cargo fmt && cargo clippy -- -W clippy::pedantic`
Expected: clean. Fix any warnings.

- [ ] **Step 3: Commit**

```bash
git add crates/yserver-core/src/resources.rs crates/yserver-core/src/nested.rs
git commit -m "refactor(resources): unmap_window returns bool, no-ops on root"
```

---

## Commit 3 — Wire `UnmapNotify` fanout

### Task 3.1: Wire opcode 10 fanout

**Files:**
- Modify: `crates/yserver-core/src/nested.rs:725-731`

- [ ] **Step 1: Replace the opcode 10 handler**

Replace the body added in Commit 2:

```rust
10 => {
    if let Some(window) = x11::map_window_id(body) {
        let mut s = lock_server(server)?;
        let _ = s.resources.unmap_window(window);
    }
    log_void(client_id, sequence, "UnmapWindow")
}
```

with:

```rust
10 => {
    if let Some(window) = x11::map_window_id(body) {
        let snapshot = {
            let mut s = lock_server(server)?;
            let was_mapped = s.resources.unmap_window(window);
            if was_mapped {
                let parent = s
                    .resources
                    .window(window)
                    .map_or(ROOT_WINDOW, |w| w.parent);
                let on_window = s.subscribers(window, 0x0002_0000); // StructureNotify
                let on_parent = s.subscribers(parent, 0x0008_0000); // SubstructureNotify
                Some((parent, on_window, on_parent))
            } else {
                None
            }
        };
        if let Some((parent, on_window, on_parent)) = snapshot {
            for target in on_window {
                let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
                let mut buf = Vec::with_capacity(32);
                x11::encode_unmap_notify_event(
                    &mut buf,
                    seq,
                    target.byte_order,
                    window,
                    window,
                    false,
                );
                if let Ok(mut w) = target.writer.lock() {
                    let _ = w.write_all(&buf);
                }
            }
            for target in on_parent {
                let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
                let mut buf = Vec::with_capacity(32);
                x11::encode_unmap_notify_event(
                    &mut buf,
                    seq,
                    target.byte_order,
                    parent,
                    window,
                    false,
                );
                if let Ok(mut w) = target.writer.lock() {
                    let _ = w.write_all(&buf);
                }
            }
        }
    }
    log_void(client_id, sequence, "UnmapWindow")
}
```

- [ ] **Step 2: Verify it compiles and existing tests still pass**

Run: `cargo test -p yserver-core`
Expected: all 48 still pass.

### Task 3.2: Extend opcode 4 (`DestroyWindow`) `pending` tuple with `was_mapped`

**Files:**
- Modify: `crates/yserver-core/src/nested.rs:578-625`

- [ ] **Step 1: Update the tuple type, snapshot, and fanout**

Replace `:578-626`:

```rust
4 => {
    if let Some(window) = x11::free_resource_id(body) {
        let pending = {
            let mut s = lock_server(server)?;
            let mut order = Vec::new();
            collect_destroy_order(&s.resources, window, &mut order);
            let mut pending: Vec<(
                ResourceId,
                ResourceId,
                Vec<crate::server::EventTarget>,
                Vec<crate::server::EventTarget>,
            )> = Vec::new();
            for w in &order {
                let parent = s.resources.window(*w).map_or(ROOT_WINDOW, |win| win.parent);
                let on_window = s.subscribers(*w, 0x0002_0000); // StructureNotify
                let on_parent = s.subscribers(parent, 0x0008_0000); // SubstructureNotify
                pending.push((*w, parent, on_window, on_parent));
            }
            let _ = s.resources.destroy_window(window);
            s.drop_window_subscriptions(&order);
            pending
        };
        for (w, parent, subs_w, subs_p) in pending {
            for target in subs_w {
                let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
                let mut buf = Vec::with_capacity(32);
                x11::encode_destroy_notify_event(&mut buf, seq, target.byte_order, w, w);
                if let Ok(mut wr) = target.writer.lock() {
                    let _ = wr.write_all(&buf);
                }
            }
            for target in subs_p {
                let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
                let mut buf = Vec::with_capacity(32);
                x11::encode_destroy_notify_event(
                    &mut buf,
                    seq,
                    target.byte_order,
                    parent,
                    w,
                );
                if let Ok(mut wr) = target.writer.lock() {
                    let _ = wr.write_all(&buf);
                }
            }
        }
    }
    log_void(client_id, sequence, "DestroyWindow")
}
```

with:

```rust
4 => {
    if let Some(window) = x11::free_resource_id(body) {
        let pending = {
            let mut s = lock_server(server)?;
            let mut order = Vec::new();
            collect_destroy_order(&s.resources, window, &mut order);
            let mut pending: Vec<(
                ResourceId,
                ResourceId,
                bool,
                Vec<crate::server::EventTarget>,
                Vec<crate::server::EventTarget>,
            )> = Vec::new();
            for w in &order {
                let (parent, was_mapped) = s
                    .resources
                    .window(*w)
                    .map_or((ROOT_WINDOW, false), |win| {
                        (win.parent, win.map_state != MapState::Unmapped)
                    });
                let on_window = s.subscribers(*w, 0x0002_0000); // StructureNotify
                let on_parent = s.subscribers(parent, 0x0008_0000); // SubstructureNotify
                pending.push((*w, parent, was_mapped, on_window, on_parent));
            }
            let _ = s.resources.destroy_window(window);
            s.drop_window_subscriptions(&order);
            pending
        };
        for (w, parent, was_mapped, subs_w, subs_p) in pending {
            if was_mapped {
                for target in &subs_w {
                    let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
                    let mut buf = Vec::with_capacity(32);
                    x11::encode_unmap_notify_event(
                        &mut buf,
                        seq,
                        target.byte_order,
                        w,
                        w,
                        false,
                    );
                    if let Ok(mut wr) = target.writer.lock() {
                        let _ = wr.write_all(&buf);
                    }
                }
                for target in &subs_p {
                    let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
                    let mut buf = Vec::with_capacity(32);
                    x11::encode_unmap_notify_event(
                        &mut buf,
                        seq,
                        target.byte_order,
                        parent,
                        w,
                        false,
                    );
                    if let Ok(mut wr) = target.writer.lock() {
                        let _ = wr.write_all(&buf);
                    }
                }
            }
            for target in subs_w {
                let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
                let mut buf = Vec::with_capacity(32);
                x11::encode_destroy_notify_event(&mut buf, seq, target.byte_order, w, w);
                if let Ok(mut wr) = target.writer.lock() {
                    let _ = wr.write_all(&buf);
                }
            }
            for target in subs_p {
                let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
                let mut buf = Vec::with_capacity(32);
                x11::encode_destroy_notify_event(
                    &mut buf,
                    seq,
                    target.byte_order,
                    parent,
                    w,
                );
                if let Ok(mut wr) = target.writer.lock() {
                    let _ = wr.write_all(&buf);
                }
            }
        }
    }
    log_void(client_id, sequence, "DestroyWindow")
}
```

> **Note on iteration:** the unmap loops borrow `&subs_w` / `&subs_p` so the destroy loops below can consume them. `EventTarget` is `Clone`, so an alternative is `subs_w.clone()` for the unmap pass — pick whichever clippy prefers; the `&` form has zero allocation and works because `EventTarget`'s fields are `Arc`/`Copy`.

- [ ] **Step 2: Verify it compiles**

Run: `cargo test -p yserver-core --no-run`
Expected: builds.

### Task 3.3: Extend disconnect cleanup with `was_mapped`

**Files:**
- Modify: `crates/yserver-core/src/nested.rs:255-302`

- [ ] **Step 1: Update the disconnect tuple, snapshot, and fanout**

Replace `:255-302`:

```rust
let (closed_fonts, pending_destroys) = {
    let mut s = lock_server(&server)?;
    let mut owned_roots: Vec<ResourceId> = Vec::new();
    s.resources
        .collect_owned_window_roots(client_id, &mut owned_roots);

    let mut pending: Vec<(
        ResourceId,
        ResourceId,
        Vec<crate::server::EventTarget>,
        Vec<crate::server::EventTarget>,
    )> = Vec::new();
    let mut all_destroyed: Vec<ResourceId> = Vec::new();
    for root in owned_roots {
        let mut order = Vec::new();
        collect_destroy_order(&s.resources, root, &mut order);
        for w in &order {
            let parent = s.resources.window(*w).map_or(ROOT_WINDOW, |win| win.parent);
            let on_w = s.subscribers(*w, 0x0002_0000);
            let on_p = s.subscribers(parent, 0x0008_0000);
            pending.push((*w, parent, on_w, on_p));
        }
        let _ = s.resources.destroy_window(root);
        all_destroyed.extend(order);
    }
    s.drop_window_subscriptions(&all_destroyed);
    let fonts = s.resources.remove_non_window_resources_owned_by(client_id);
    s.clients.remove(&client_id.0);
    (fonts, pending)
};
for (w, parent, subs_w, subs_p) in pending_destroys {
    for target in subs_w {
        let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
        let mut buf = Vec::with_capacity(32);
        x11::encode_destroy_notify_event(&mut buf, seq, target.byte_order, w, w);
        if let Ok(mut wr) = target.writer.lock() {
            let _ = wr.write_all(&buf);
        }
    }
    for target in subs_p {
        let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
        let mut buf = Vec::with_capacity(32);
        x11::encode_destroy_notify_event(&mut buf, seq, target.byte_order, parent, w);
        if let Ok(mut wr) = target.writer.lock() {
            let _ = wr.write_all(&buf);
        }
    }
}
```

with:

```rust
let (closed_fonts, pending_destroys) = {
    let mut s = lock_server(&server)?;
    let mut owned_roots: Vec<ResourceId> = Vec::new();
    s.resources
        .collect_owned_window_roots(client_id, &mut owned_roots);

    let mut pending: Vec<(
        ResourceId,
        ResourceId,
        bool,
        Vec<crate::server::EventTarget>,
        Vec<crate::server::EventTarget>,
    )> = Vec::new();
    let mut all_destroyed: Vec<ResourceId> = Vec::new();
    for root in owned_roots {
        let mut order = Vec::new();
        collect_destroy_order(&s.resources, root, &mut order);
        for w in &order {
            let (parent, was_mapped) = s
                .resources
                .window(*w)
                .map_or((ROOT_WINDOW, false), |win| {
                    (win.parent, win.map_state != MapState::Unmapped)
                });
            let on_w = s.subscribers(*w, 0x0002_0000);
            let on_p = s.subscribers(parent, 0x0008_0000);
            pending.push((*w, parent, was_mapped, on_w, on_p));
        }
        let _ = s.resources.destroy_window(root);
        all_destroyed.extend(order);
    }
    s.drop_window_subscriptions(&all_destroyed);
    let fonts = s.resources.remove_non_window_resources_owned_by(client_id);
    s.clients.remove(&client_id.0);
    (fonts, pending)
};
for (w, parent, was_mapped, subs_w, subs_p) in pending_destroys {
    if was_mapped {
        for target in &subs_w {
            let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
            let mut buf = Vec::with_capacity(32);
            x11::encode_unmap_notify_event(
                &mut buf,
                seq,
                target.byte_order,
                w,
                w,
                false,
            );
            if let Ok(mut wr) = target.writer.lock() {
                let _ = wr.write_all(&buf);
            }
        }
        for target in &subs_p {
            let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
            let mut buf = Vec::with_capacity(32);
            x11::encode_unmap_notify_event(
                &mut buf,
                seq,
                target.byte_order,
                parent,
                w,
                false,
            );
            if let Ok(mut wr) = target.writer.lock() {
                let _ = wr.write_all(&buf);
            }
        }
    }
    for target in subs_w {
        let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
        let mut buf = Vec::with_capacity(32);
        x11::encode_destroy_notify_event(&mut buf, seq, target.byte_order, w, w);
        if let Ok(mut wr) = target.writer.lock() {
            let _ = wr.write_all(&buf);
        }
    }
    for target in subs_p {
        let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
        let mut buf = Vec::with_capacity(32);
        x11::encode_destroy_notify_event(&mut buf, seq, target.byte_order, parent, w);
        if let Ok(mut wr) = target.writer.lock() {
            let _ = wr.write_all(&buf);
        }
    }
}
```

- [ ] **Step 2: Confirm `MapState` is in scope**

Verify that `MapState` is already imported at the top of `nested.rs`. If not, add it to the existing `use crate::resources::{...}` line. Quick check:

Run: `grep -n "use crate::resources" crates/yserver-core/src/nested.rs`
If `MapState` is not in the imports, add it.

- [ ] **Step 3: Verify it compiles and existing tests still pass**

Run: `cargo test -p yserver-core`
Expected: 48 tests pass (no test changes yet).

### Task 3.4: Add the integration-style `server.rs` test (RED → GREEN)

**Files:**
- Modify: `crates/yserver-core/src/server.rs` (inside `mod tests`, alongside the existing `subscribers_*` tests around `:288-352`)

- [ ] **Step 1: Add the failing test**

Inside `mod tests` (before the closing brace), add:

```rust
#[test]
fn unmap_notify_fanout_reaches_only_subscribed_clients() {
    use std::io::Read;
    use yserver_protocol::x11::{encode_unmap_notify_event, SequenceNumber};

    // Client A: StructureNotify on window 0x100.
    let (a_writer_local, mut a_reader_remote) = UnixStream::pair().expect("socketpair");
    // Client B: KeyPress only on window 0x100 (NOT StructureNotify).
    let (b_writer_local, _b_reader_remote) = UnixStream::pair().expect("socketpair");

    let mut state = ServerState::new();
    state.clients.insert(
        1,
        ClientHandle {
            writer: Arc::new(Mutex::new(a_writer_local)),
            byte_order: ClientByteOrder::LittleEndian,
            last_sequence: Arc::new(AtomicU16::new(0)),
            resource_id_base: 0x0010_0000,
            resource_id_mask: 0x000F_FFFF,
            event_masks: HashMap::from([(ResourceId(0x100), 0x0002_0000)]), // StructureNotify
        },
    );
    state.clients.insert(
        2,
        ClientHandle {
            writer: Arc::new(Mutex::new(b_writer_local)),
            byte_order: ClientByteOrder::LittleEndian,
            last_sequence: Arc::new(AtomicU16::new(0)),
            resource_id_base: 0x0020_0000,
            resource_id_mask: 0x000F_FFFF,
            event_masks: HashMap::from([(ResourceId(0x100), 0x0000_0001)]), // KeyPress
        },
    );

    let subs = state.subscribers(ResourceId(0x100), 0x0002_0000);
    assert_eq!(subs.len(), 1, "only client A should be subscribed");

    let target = &subs[0];
    let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
    let mut buf = Vec::with_capacity(32);
    encode_unmap_notify_event(
        &mut buf,
        seq,
        target.byte_order,
        ResourceId(0x100),
        ResourceId(0x100),
        false,
    );
    {
        let mut w = target.writer.lock().unwrap();
        w.write_all(&buf).unwrap();
    }

    let mut received = [0u8; 32];
    a_reader_remote.read_exact(&mut received).unwrap();
    assert_eq!(received[0], 18, "wire byte 0 is UnmapNotify");
    assert_eq!(&received[4..8], &0x100u32.to_le_bytes());
    assert_eq!(&received[8..12], &0x100u32.to_le_bytes());
    assert_eq!(received[12], 0, "from_configure = false");
}
```

- [ ] **Step 2: Verify the test imports compile**

Required `use` items at the top of `mod tests` (most are already present from existing `subscribers_*` tests):
- `super::*`
- `std::io::Write`
- `std::os::unix::net::UnixStream`
- `std::sync::atomic::AtomicU16`

The new test pulls in `std::io::Read` and `yserver_protocol::x11::{encode_unmap_notify_event, SequenceNumber}` locally inside the `fn` body — no module-level changes required.

Run: `cargo test -p yserver-core unmap_notify_fanout_reaches_only_subscribed_clients`
Expected: PASS.

### Task 3.5: Verify, format, lint, commit

- [ ] **Step 1: Run the full workspace test suite**

Run: `cargo test`
Expected: yserver-core has 49 (was 48 after Commit 2, +1); yserver-protocol unchanged at 9; everything else unchanged.

- [ ] **Step 2: Format and lint**

Run: `cargo fmt && cargo clippy -- -W clippy::pedantic`
Expected: clean. Fix any warnings (clippy::pedantic may flag the `&subs_w` borrow in the fanout loops; switch to `subs_w.clone()` if it does).

- [ ] **Step 3: Manual smoke test (optional, follows the property-storage spec convention)**

Run `xev` on a window in a `ynest` instance, then `xdotool windowunmap <id>`, and confirm `xev` prints an `UnmapNotify` line. This is *not* a gating step — automated test 3.4 covers the wire-encoding contract; this just confirms end-to-end behavior in a real client.

- [ ] **Step 4: Commit**

```bash
git add crates/yserver-core/src/nested.rs crates/yserver-core/src/server.rs
git commit -m "feat(events): emit UnmapNotify on explicit + implicit unmap"
```

---

## Final verification

- [ ] **Step 1: Confirm test counts match the spec**

Run: `cargo test 2>&1 | grep "test result"`
Expected:
- yserver-core: 49 passed (42 baseline + 5 unit + 1 proptest in `resources::tests` + 1 integration in `server::tests` = 49). The spec table claims 47 — the actual delta is +7, not +5. The numerical mismatch is a spec-side typo; the named test list (tests 1–9) is the source of truth and is fully implemented.
- yserver-protocol: 9 passed (7 baseline + 1 unit + 1 proptest = 9, matches spec).

- [ ] **Step 2: Confirm three commits landed cleanly**

Run: `git log --oneline -3`
Expected:
```
<sha> feat(events): emit UnmapNotify on explicit + implicit unmap
<sha> refactor(resources): unmap_window returns bool, no-ops on root
<sha> feat(protocol): add encode_unmap_notify_event
```

Each commit compiles independently (`git stash` + `git checkout HEAD~N -- .` + `cargo build` per commit if you want to be thorough; not required).
