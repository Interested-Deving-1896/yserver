# Per-Window Clipping + Pointer Events Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give each nested top-level window its own X11 subwindow on the host display, route drawing into the right subwindow, and deliver host pointer events (`ButtonPress`, `ButtonRelease`, `MotionNotify`, `EnterNotify`, `LeaveNotify`) back to nested clients via the existing per-(client, window) event-mask fanout.

**Architecture:** Add a `host_xid: Option<u32>` to `Window` and a `top_level_host_target` helper that walks parents accumulating offsets. Refactor the single `HostX11` window into a "container" parent with five new subwindow-lifecycle methods (`create_subwindow` / `destroy_subwindow` / `configure_subwindow` / `map_subwindow` / `unmap_subwindow`); drawing methods grow a leading `host_xid` parameter so the handler can route into the correct subwindow. Replace the per-client `HostKeyboard` with a `HostInputPump` that selects pointer events on each new subwindow via a separate write-side handle and resolves incoming events (codes 4–8) through a shared `xid_map: Arc<Mutex<HashMap<u32, ResourceId>>>` before fanning out to subscribers. Five new 32-byte encoders in `yserver-protocol` produce the wire events.

**Tech Stack:** Rust 2024, std `Mutex`/`Arc`, `proptest` (existing dev-dependency in both crates).

**Spec:** [`docs/superpowers/specs/2026-04-28-clipping-and-pointer-events-design.md`](../specs/2026-04-28-clipping-and-pointer-events-design.md).

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
| `crates/yserver-core/src/resources.rs` | modify | Add `Window.host_xid: Option<u32>`, `TopLevelTarget` struct, `top_level_host_target` helper, and 6 unit tests + 1 proptest |
| `crates/yserver-protocol/src/x11/mod.rs` | modify | Add five `encode_*_event` functions for events 4–8 and the `pointer_event_tests` submodule (5 unit + 1 proptest) |
| `crates/yserver-core/src/host_x11.rs` | modify | Make `allocate_xid` public; add five subwindow-lifecycle methods on `HostX11`; thread `host_xid: u32` through every drawing method; replace `HostKeyboard` with `HostInputPump` + `HostInputPumpHandle` + `HostPointerEvent` + `PointerEventKind`; extend `HostEvent` with a `Pointer` variant |
| `crates/yserver-core/src/server.rs` | modify | Add `pointer_event_fanout` helper; add 2 unit tests |
| `crates/yserver-core/src/nested.rs` | modify | Wire opcodes 1, 4, 8, 9, 10, 12 + disconnect cleanup to the host subwindow APIs and `register_top_level` / `unregister_top_level`; route every drawing handler through `top_level_host_target`; replace the per-client `HostKeyboard` forwarder with the `HostInputPump` adapter |

The implementation is five commits, sequenced bottom-up so each commit compiles and passes tests on its own:

1. **Resources data + helper** — pure addition. Tests 1–6, 14.
2. **Pointer event encoders** — pure addition. Tests 7–11, 15.
3. **HostX11 refactor** — drawing methods grow `host_xid` arg; add subwindow-lifecycle methods. All call sites in `nested.rs` pass `host.window_id()` as the placeholder so behavior is identical to today. No test changes.
4. **Pump + xid_map + fanout** — `HostInputPump`, `HostInputPumpHandle`, shared `xid_map`, `pointer_event_fanout`. The map is empty; nothing registers yet. Tests 12, 13.
5. **Wire it all up** — opcodes 1, 4, 8, 9, 10, 12 + disconnect + every drawing handler. Replace the placeholder `host.window_id()` with `top_level_host_target(...)` resolution. Real subwindows + real pointer events. Manual smoke checklist.

---

## Commit 1 — `Window.host_xid` + `top_level_host_target`

### Task 1.1: Add the `host_xid` field

**Files:**
- Modify: `crates/yserver-core/src/resources.rs:355-374` (`Window` struct), `:62-93` (`create_window`), `:30-50` (root window initializer), `:376-398` (`Window::placeholder`)

- [ ] **Step 1: Add `host_xid: Option<u32>` to the `Window` struct**

In `crates/yserver-core/src/resources.rs:355-374`, change:

```rust
#[derive(Clone, Debug)]
pub struct Window {
    pub id: ResourceId,
    pub parent: ResourceId,
    pub children: Vec<ResourceId>,
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    pub border_width: u16,
    pub depth: u8,
    pub visual: ResourceId,
    pub class: WindowClass,
    pub map_state: MapState,
    pub background_pixel: u32,
    pub override_redirect: bool,
    pub cursor: Option<ResourceId>,
    pub owner: ClientId,
    pub properties: HashMap<AtomId, PropertyValue>,
}
```

to add the new field at the end:

```rust
#[derive(Clone, Debug)]
pub struct Window {
    pub id: ResourceId,
    pub parent: ResourceId,
    pub children: Vec<ResourceId>,
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    pub border_width: u16,
    pub depth: u8,
    pub visual: ResourceId,
    pub class: WindowClass,
    pub map_state: MapState,
    pub background_pixel: u32,
    pub override_redirect: bool,
    pub cursor: Option<ResourceId>,
    pub owner: ClientId,
    pub properties: HashMap<AtomId, PropertyValue>,
    pub host_xid: Option<u32>,
}
```

- [ ] **Step 2: Initialize `host_xid: None` in the root window initializer**

In `:30-50`, add `host_xid: None,` as the last field of the root `Window { ... }` literal (between `properties: HashMap::new(),` and the closing brace). The root never gets a host xid.

- [ ] **Step 3: Initialize `host_xid: None` in `create_window`**

In `:62-93`, the `Window { ... }` literal in `create_window` builds new windows with `host_xid: None`. Add `host_xid: None,` as the last field, after `properties: HashMap::new(),`. (Commit 5 will set this to `Some(_)` after a successful `host.create_subwindow(...)`.)

- [ ] **Step 4: Initialize `host_xid: None` in `Window::placeholder`**

In `:376-398`, the `Self { ... }` literal in `Window::placeholder` similarly needs `host_xid: None,` appended after `properties: HashMap::new(),`.

- [ ] **Step 5: Verify build**

Run: `cargo build -p yserver-core`
Expected: compiles cleanly. (No tests touched yet; the existing 49 tests still pass because `host_xid` defaults to `None` everywhere.)

### Task 1.2: Add `TopLevelTarget` and the failing helper signature

**Files:**
- Modify: `crates/yserver-core/src/resources.rs` (top of file, alongside `ResourceTable`)

- [ ] **Step 1: Add the `TopLevelTarget` struct**

Insert between `pub const ROOT_VISUAL: ResourceId = ResourceId(0x102);` (`:16`) and `#[derive(Debug)] pub struct ResourceTable { ... }` (`:18`):

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TopLevelTarget {
    pub top_level: ResourceId,
    pub host_xid: u32,
    pub x_offset: i16,
    pub y_offset: i16,
}
```

- [ ] **Step 2: Add the `top_level_host_target` helper as a stub returning `None`**

Inside `impl ResourceTable { ... }`, after the `unmap_window` method at `:152-162` (i.e., right before `pub fn window(&self, id: ResourceId) -> Option<&Window>` at `:164`), insert:

```rust
#[must_use]
pub fn top_level_host_target(&self, id: ResourceId) -> Option<TopLevelTarget> {
    let mut current = self.windows.get(&id.0)?;
    if current.id == ROOT_WINDOW {
        return None;
    }
    let mut x_offset: i16 = 0;
    let mut y_offset: i16 = 0;
    while current.parent != ROOT_WINDOW {
        x_offset = x_offset.wrapping_add(current.x);
        y_offset = y_offset.wrapping_add(current.y);
        let next = self.windows.get(&current.parent.0)?;
        if next.id == ROOT_WINDOW {
            // Parent chain points at root through a missing or self-loop entry.
            return None;
        }
        current = next;
    }
    // current is now the top-level (parent == ROOT_WINDOW).
    let host_xid = current.host_xid?;
    Some(TopLevelTarget {
        top_level: current.id,
        host_xid,
        x_offset,
        y_offset,
    })
}
```

- [ ] **Step 3: Verify the new helper compiles**

Run: `cargo build -p yserver-core`
Expected: compiles cleanly.

### Task 1.3: Add the six unit tests for the helper

**Files:**
- Modify: `crates/yserver-core/src/resources.rs` (inside the existing `mod tests` at `:481-623`)

- [ ] **Step 1: Add a helper that creates a top-level with a host xid**

Inside `mod tests`, after `fn make_window` (currently at `:487-506`), insert:

```rust
fn make_top_level_with_host_xid(table: &mut ResourceTable, id: u32, host_xid: u32) {
    make_window(table, id);
    table.windows.get_mut(&id).unwrap().host_xid = Some(host_xid);
}

fn make_child(table: &mut ResourceTable, id: u32, parent: u32, x: i16, y: i16) {
    table.create_window(
        ClientId(1),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(id),
            parent: ResourceId(parent),
            x,
            y,
            width: 50,
            height: 50,
            border_width: 0,
            class: 1,
            visual: ROOT_VISUAL,
            background_pixel: None,
            event_mask: None,
            override_redirect: None,
        },
    );
}
```

- [ ] **Step 2: Add the six unit tests**

Inside `mod tests`, after the existing `unmap_window_*` tests and before the `proptest! { ... }` block at `:591`, insert:

```rust
#[test]
fn top_level_host_target_for_top_level_returns_self() {
    let mut table = ResourceTable::new();
    make_top_level_with_host_xid(&mut table, 0x100002, 0xAA);
    let target = table.top_level_host_target(ResourceId(0x100002));
    assert_eq!(
        target,
        Some(TopLevelTarget {
            top_level: ResourceId(0x100002),
            host_xid: 0xAA,
            x_offset: 0,
            y_offset: 0,
        })
    );
}

#[test]
fn top_level_host_target_for_child_accumulates_offset() {
    let mut table = ResourceTable::new();
    make_top_level_with_host_xid(&mut table, 0x100002, 0xAA);
    make_child(&mut table, 0x100003, 0x100002, 10, 20);
    let target = table.top_level_host_target(ResourceId(0x100003));
    assert_eq!(
        target,
        Some(TopLevelTarget {
            top_level: ResourceId(0x100002),
            host_xid: 0xAA,
            x_offset: 10,
            y_offset: 20,
        })
    );
}

#[test]
fn top_level_host_target_for_grandchild_sums_offsets() {
    let mut table = ResourceTable::new();
    make_top_level_with_host_xid(&mut table, 0x100002, 0xAA);
    make_child(&mut table, 0x100003, 0x100002, 10, 20);
    make_child(&mut table, 0x100004, 0x100003, 5, 5);
    let target = table.top_level_host_target(ResourceId(0x100004));
    assert_eq!(
        target,
        Some(TopLevelTarget {
            top_level: ResourceId(0x100002),
            host_xid: 0xAA,
            x_offset: 15,
            y_offset: 25,
        })
    );
}

#[test]
fn top_level_host_target_returns_none_for_root() {
    let table = ResourceTable::new();
    assert_eq!(table.top_level_host_target(ROOT_WINDOW), None);
}

#[test]
fn top_level_host_target_returns_none_when_top_level_has_no_host_xid() {
    let mut table = ResourceTable::new();
    make_window(&mut table, 0x100002); // no host_xid set
    make_child(&mut table, 0x100003, 0x100002, 10, 20);
    assert_eq!(table.top_level_host_target(ResourceId(0x100002)), None);
    assert_eq!(table.top_level_host_target(ResourceId(0x100003)), None);
}

#[test]
fn top_level_host_target_returns_none_for_orphaned_window() {
    let mut table = ResourceTable::new();
    // Build a child whose parent is a non-existent window (chain breaks).
    make_child(&mut table, 0x100003, 0x9999_9999, 10, 20);
    assert_eq!(table.top_level_host_target(ResourceId(0x100003)), None);
}
```

> **Note on test 6:** `make_child` calls `create_window` with a non-existent parent. `ResourceTable::create_window` (`:62-93`) will insert a `Window::placeholder` into the parent slot via `entry().or_insert_with(...)`. That placeholder has `parent: ROOT_WINDOW` and `host_xid: None`, so the chain *does* terminate cleanly at the placeholder — which has no `host_xid` and therefore returns `None`. This still tests the "broken chain" case: the leaf can't reach a real top-level with a host xid.

- [ ] **Step 3: Run the new tests**

Run: `cargo test -p yserver-core resources::tests::top_level_host_target`
Expected: 6 tests pass.

### Task 1.4: Add the offset-accumulation proptest

**Files:**
- Modify: `crates/yserver-core/src/resources.rs` (inside the existing `proptest! { ... }` block at `:591-622`)

- [ ] **Step 1: Add the proptest**

Inside the existing `proptest! { ... }` block, after the `unmap_window_state_machine` test (after the closing `}` at `:621` of that test, before the closing `}` of `proptest!`), add:

```rust
#[test]
fn top_level_host_target_offset_proptest(
    n in 1usize..=8,
    offsets in proptest::collection::vec((any::<i16>(), any::<i16>()), 1..=8),
) {
    let depth = n.min(offsets.len());
    let mut table = ResourceTable::new();
    let top_level_id: u32 = 0x100_0000;
    let host_xid: u32 = 0xCAFE;
    make_top_level_with_host_xid(&mut table, top_level_id, host_xid);

    let mut parent = top_level_id;
    let mut expected_x: i16 = 0;
    let mut expected_y: i16 = 0;
    let mut leaf = top_level_id;
    for (i, (x, y)) in offsets.iter().take(depth).enumerate() {
        let id: u32 = 0x100_0001 + i as u32;
        make_child(&mut table, id, parent, *x, *y);
        // The child contributes its own (x, y) to the offset only on the
        // way *up* — the helper walks from leaf to top-level and skips the
        // top-level's own (x, y), matching the spec.
        if parent != top_level_id {
            // Add the *previous* parent's (x, y) — i.e., the segment now
            // sitting between the new child and its grandparent.
            // (Already accumulated in the previous iteration.)
        }
        expected_x = expected_x.wrapping_add(*x);
        expected_y = expected_y.wrapping_add(*y);
        leaf = id;
        parent = id;
    }
    // The helper accumulates only ancestor offsets up to (but not
    // including) the top-level. So the leaf's own (x, y) is included
    // only if there is at least one intermediate ancestor between it
    // and the top-level — i.e., depth >= 2. For depth == 1, the leaf
    // is a direct child of the top-level and its (x, y) is the
    // accumulated offset.
    let target = table.top_level_host_target(ResourceId(leaf)).unwrap();
    prop_assert_eq!(target.top_level, ResourceId(top_level_id));
    prop_assert_eq!(target.host_xid, host_xid);
    prop_assert_eq!(target.x_offset, expected_x);
    prop_assert_eq!(target.y_offset, expected_y);
}
```

> **Note:** `proptest::collection::vec` is already in scope via `use proptest::prelude::*;` at the top of the existing `mod tests`.

- [ ] **Step 2: Run the proptest**

Run: `cargo test -p yserver-core resources::tests::top_level_host_target_offset_proptest`
Expected: PASS.

### Task 1.5: Verify, format, lint, commit

- [ ] **Step 1: Run the full crate test suite**

Run: `cargo test -p yserver-core`
Expected: 56 tests pass (was 49, +6 unit + 1 proptest = +7).

- [ ] **Step 2: Format and lint**

Run: `cargo fmt && cargo clippy -- -W clippy::pedantic`
Expected: clean exit, no warnings.

- [ ] **Step 3: Commit**

```bash
git add crates/yserver-core/src/resources.rs
git commit -m "feat(resources): add Window.host_xid and top_level_host_target"
```

---

## Commit 2 — Pointer event encoders in `yserver-protocol`

### Task 2.1: Add the `PointerEvent` and `CrossingEvent` arg structs

**Files:**
- Modify: `crates/yserver-protocol/src/x11/mod.rs` (after the `KeyEvent` struct around `:99-111`)

- [ ] **Step 1: Add struct definitions**

Insert after the `KeyEvent` struct definition (right after the closing `}` at `:111`, before the next `#[derive]` at `:113`):

```rust
#[derive(Clone, Copy, Debug)]
pub struct PointerEvent {
    pub sequence: SequenceNumber,
    pub detail: u8,
    pub time: u32,
    pub root: ResourceId,
    pub event: ResourceId,
    pub root_x: i16,
    pub root_y: i16,
    pub event_x: i16,
    pub event_y: i16,
    pub state: u16,
}

#[derive(Clone, Copy, Debug)]
pub struct CrossingEvent {
    pub sequence: SequenceNumber,
    pub time: u32,
    pub root: ResourceId,
    pub event: ResourceId,
    pub root_x: i16,
    pub root_y: i16,
    pub event_x: i16,
    pub event_y: i16,
    pub state: u16,
}
```

- [ ] **Step 2: Verify build**

Run: `cargo build -p yserver-protocol`
Expected: compiles cleanly.

### Task 2.2: Add the failing shape tests for ButtonPress, ButtonRelease, MotionNotify

**Files:**
- Modify: `crates/yserver-protocol/src/x11/mod.rs` (inside `mod tests`, near the existing `mod unmap_notify_tests` at `:1893-1969`)

- [ ] **Step 1: Add a `pointer_event_tests` submodule with three failing shape tests**

Inside `mod tests`, after the closing brace of `mod unmap_notify_tests` (at `:1969`), insert:

```rust
mod pointer_event_tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn button_press_event_shape() {
        let mut buf = Vec::new();
        encode_button_press_event(
            &mut buf,
            ClientByteOrder::LittleEndian,
            PointerEvent {
                sequence: SequenceNumber(0x1234),
                detail: 1,
                time: 0xdead_beef,
                root: ResourceId(0x100),
                event: ResourceId(0x100002),
                root_x: 100,
                root_y: 200,
                event_x: 10,
                event_y: 20,
                state: 0x0010,
            },
        );
        assert_eq!(buf.len(), 32);
        assert_eq!(buf[0], 4); // ButtonPress
        assert_eq!(buf[1], 1); // detail
        assert_eq!(&buf[2..4], &0x1234u16.to_le_bytes());
        assert_eq!(&buf[4..8], &0xdead_beefu32.to_le_bytes());
        assert_eq!(&buf[8..12], &0x100u32.to_le_bytes());
        assert_eq!(&buf[12..16], &0x100002u32.to_le_bytes());
        assert_eq!(&buf[16..20], &0u32.to_le_bytes()); // child = 0
        assert_eq!(&buf[20..22], &100i16.to_le_bytes());
        assert_eq!(&buf[22..24], &200i16.to_le_bytes());
        assert_eq!(&buf[24..26], &10i16.to_le_bytes());
        assert_eq!(&buf[26..28], &20i16.to_le_bytes());
        assert_eq!(&buf[28..30], &0x0010u16.to_le_bytes());
        assert_eq!(buf[30], 1); // same_screen
        assert_eq!(buf[31], 0); // pad
    }

    #[test]
    fn button_release_event_shape() {
        let mut buf = Vec::new();
        encode_button_release_event(
            &mut buf,
            ClientByteOrder::LittleEndian,
            PointerEvent {
                sequence: SequenceNumber(0),
                detail: 2,
                time: 0,
                root: ResourceId(0x100),
                event: ResourceId(0x100002),
                root_x: 0,
                root_y: 0,
                event_x: 0,
                event_y: 0,
                state: 0,
            },
        );
        assert_eq!(buf.len(), 32);
        assert_eq!(buf[0], 5); // ButtonRelease
        assert_eq!(buf[1], 2); // detail
        assert_eq!(buf[30], 1); // same_screen
    }

    #[test]
    fn motion_notify_event_shape() {
        let mut buf = Vec::new();
        encode_motion_notify_event(
            &mut buf,
            ClientByteOrder::LittleEndian,
            PointerEvent {
                sequence: SequenceNumber(0),
                detail: 0,
                time: 0,
                root: ResourceId(0x100),
                event: ResourceId(0x100002),
                root_x: 0,
                root_y: 0,
                event_x: 0,
                event_y: 0,
                state: 0,
            },
        );
        assert_eq!(buf.len(), 32);
        assert_eq!(buf[0], 6); // MotionNotify
        assert_eq!(buf[1], 0); // detail = 0 for motion
        assert_eq!(buf[30], 1); // same_screen
    }
}
```

- [ ] **Step 2: Run the tests, expect compile failure**

Run: `cargo test -p yserver-protocol pointer_event_tests`
Expected: FAILS to compile — `encode_button_press_event`, `encode_button_release_event`, `encode_motion_notify_event` are not defined.

### Task 2.3: Implement the three pointer-button/motion encoders

**Files:**
- Modify: `crates/yserver-protocol/src/x11/mod.rs` (after `encode_unmap_notify_event` at `:1673-1688`)

- [ ] **Step 1: Add the three encoders**

Insert after the closing `}` of `encode_unmap_notify_event` (at `:1688`), before the `#[cfg(test)]` at `:1690`:

```rust
fn encode_pointer_event(
    out: &mut Vec<u8>,
    event_code: u8,
    order: ClientByteOrder,
    event: PointerEvent,
) {
    out.push(event_code);
    out.push(event.detail);
    write_u16(order, out, event.sequence.0);
    write_u32(order, out, event.time);
    write_u32(order, out, event.root.0);
    write_u32(order, out, event.event.0);
    write_u32(order, out, 0); // child — descendant hit-testing not implemented
    write_i16(order, out, event.root_x);
    write_i16(order, out, event.root_y);
    write_i16(order, out, event.event_x);
    write_i16(order, out, event.event_y);
    write_u16(order, out, event.state);
    out.push(1); // same_screen
    out.push(0); // pad
}

pub fn encode_button_press_event(
    out: &mut Vec<u8>,
    order: ClientByteOrder,
    event: PointerEvent,
) {
    encode_pointer_event(out, 4, order, event);
}

pub fn encode_button_release_event(
    out: &mut Vec<u8>,
    order: ClientByteOrder,
    event: PointerEvent,
) {
    encode_pointer_event(out, 5, order, event);
}

pub fn encode_motion_notify_event(
    out: &mut Vec<u8>,
    order: ClientByteOrder,
    event: PointerEvent,
) {
    encode_pointer_event(out, 6, order, event);
}
```

- [ ] **Step 2: Run the tests, expect PASS**

Run: `cargo test -p yserver-protocol pointer_event_tests`
Expected: 3 tests pass.

### Task 2.4: Add the failing shape tests for EnterNotify and LeaveNotify

**Files:**
- Modify: `crates/yserver-protocol/src/x11/mod.rs` (inside `mod pointer_event_tests`)

- [ ] **Step 1: Add the two new failing tests**

Inside `mod pointer_event_tests`, after `motion_notify_event_shape` (before the closing `}` of the submodule), insert:

```rust
#[test]
fn enter_notify_event_shape() {
    let mut buf = Vec::new();
    encode_enter_notify_event(
        &mut buf,
        ClientByteOrder::LittleEndian,
        CrossingEvent {
            sequence: SequenceNumber(0x1234),
            time: 0xdead_beef,
            root: ResourceId(0x100),
            event: ResourceId(0x100002),
            root_x: 100,
            root_y: 200,
            event_x: 10,
            event_y: 20,
            state: 0,
        },
    );
    assert_eq!(buf.len(), 32);
    assert_eq!(buf[0], 7); // EnterNotify
    assert_eq!(buf[1], 0); // detail = NotifyAncestor
    assert_eq!(&buf[2..4], &0x1234u16.to_le_bytes());
    assert_eq!(&buf[4..8], &0xdead_beefu32.to_le_bytes());
    assert_eq!(&buf[8..12], &0x100u32.to_le_bytes());
    assert_eq!(&buf[12..16], &0x100002u32.to_le_bytes());
    assert_eq!(&buf[16..20], &0u32.to_le_bytes()); // child = 0
    assert_eq!(&buf[20..22], &100i16.to_le_bytes());
    assert_eq!(&buf[22..24], &200i16.to_le_bytes());
    assert_eq!(&buf[24..26], &10i16.to_le_bytes());
    assert_eq!(&buf[26..28], &20i16.to_le_bytes());
    assert_eq!(&buf[28..30], &0u16.to_le_bytes());
    assert_eq!(buf[30], 0); // mode = NotifyNormal
    assert_eq!(buf[31], 0x03); // same_screen,focus = 0x01 | 0x02
}

#[test]
fn leave_notify_event_shape() {
    let mut buf = Vec::new();
    encode_leave_notify_event(
        &mut buf,
        ClientByteOrder::LittleEndian,
        CrossingEvent {
            sequence: SequenceNumber(0),
            time: 0,
            root: ResourceId(0x100),
            event: ResourceId(0x100002),
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            state: 0,
        },
    );
    assert_eq!(buf.len(), 32);
    assert_eq!(buf[0], 8); // LeaveNotify
    assert_eq!(buf[1], 0); // detail = NotifyAncestor
    assert_eq!(buf[30], 0); // mode = NotifyNormal
    assert_eq!(buf[31], 0x03); // same_screen,focus
}
```

- [ ] **Step 2: Run, expect compile failure**

Run: `cargo test -p yserver-protocol pointer_event_tests`
Expected: compile fails — `encode_enter_notify_event`, `encode_leave_notify_event` not defined.

### Task 2.5: Implement the crossing encoders

**Files:**
- Modify: `crates/yserver-protocol/src/x11/mod.rs` (after `encode_motion_notify_event`)

- [ ] **Step 1: Add the two encoders**

After `encode_motion_notify_event` (the one added in Task 2.3), insert:

```rust
fn encode_crossing_event(
    out: &mut Vec<u8>,
    event_code: u8,
    order: ClientByteOrder,
    event: CrossingEvent,
) {
    out.push(event_code);
    out.push(0); // detail = NotifyAncestor
    write_u16(order, out, event.sequence.0);
    write_u32(order, out, event.time);
    write_u32(order, out, event.root.0);
    write_u32(order, out, event.event.0);
    write_u32(order, out, 0); // child — descendant hit-testing not implemented
    write_i16(order, out, event.root_x);
    write_i16(order, out, event.root_y);
    write_i16(order, out, event.event_x);
    write_i16(order, out, event.event_y);
    write_u16(order, out, event.state);
    out.push(0); // mode = NotifyNormal
    out.push(0x03); // same_screen + focus
}

pub fn encode_enter_notify_event(
    out: &mut Vec<u8>,
    order: ClientByteOrder,
    event: CrossingEvent,
) {
    encode_crossing_event(out, 7, order, event);
}

pub fn encode_leave_notify_event(
    out: &mut Vec<u8>,
    order: ClientByteOrder,
    event: CrossingEvent,
) {
    encode_crossing_event(out, 8, order, event);
}
```

- [ ] **Step 2: Run all five shape tests**

Run: `cargo test -p yserver-protocol pointer_event_tests`
Expected: 5 tests pass.

### Task 2.6: Add the encoder round-trip proptest

**Files:**
- Modify: `crates/yserver-protocol/src/x11/mod.rs` (inside `mod pointer_event_tests`)

- [ ] **Step 1: Add the proptest**

Inside `mod pointer_event_tests`, after `leave_notify_event_shape` (before the closing `}` of the submodule), insert:

```rust
proptest! {
    #[test]
    fn pointer_encoder_round_trip(
        sequence in any::<u16>(),
        detail in any::<u8>(),
        time in any::<u32>(),
        root in any::<u32>(),
        event_window in any::<u32>(),
        root_x in any::<i16>(),
        root_y in any::<i16>(),
        event_x in any::<i16>(),
        event_y in any::<i16>(),
        state in any::<u16>(),
        big_endian: bool,
        encoder_choice in 0u8..5,
    ) {
        let order = if big_endian {
            ClientByteOrder::BigEndian
        } else {
            ClientByteOrder::LittleEndian
        };
        let mut buf = Vec::new();
        let expected_code: u8;
        let expected_detail: u8;
        let expected_state_offset: usize = 28;
        match encoder_choice {
            0 => {
                expected_code = 4;
                expected_detail = detail;
                encode_button_press_event(
                    &mut buf,
                    order,
                    PointerEvent {
                        sequence: SequenceNumber(sequence),
                        detail,
                        time,
                        root: ResourceId(root),
                        event: ResourceId(event_window),
                        root_x,
                        root_y,
                        event_x,
                        event_y,
                        state,
                    },
                );
            }
            1 => {
                expected_code = 5;
                expected_detail = detail;
                encode_button_release_event(
                    &mut buf,
                    order,
                    PointerEvent {
                        sequence: SequenceNumber(sequence),
                        detail,
                        time,
                        root: ResourceId(root),
                        event: ResourceId(event_window),
                        root_x,
                        root_y,
                        event_x,
                        event_y,
                        state,
                    },
                );
            }
            2 => {
                expected_code = 6;
                expected_detail = detail;
                encode_motion_notify_event(
                    &mut buf,
                    order,
                    PointerEvent {
                        sequence: SequenceNumber(sequence),
                        detail,
                        time,
                        root: ResourceId(root),
                        event: ResourceId(event_window),
                        root_x,
                        root_y,
                        event_x,
                        event_y,
                        state,
                    },
                );
            }
            3 => {
                expected_code = 7;
                expected_detail = 0;
                encode_enter_notify_event(
                    &mut buf,
                    order,
                    CrossingEvent {
                        sequence: SequenceNumber(sequence),
                        time,
                        root: ResourceId(root),
                        event: ResourceId(event_window),
                        root_x,
                        root_y,
                        event_x,
                        event_y,
                        state,
                    },
                );
            }
            _ => {
                expected_code = 8;
                expected_detail = 0;
                encode_leave_notify_event(
                    &mut buf,
                    order,
                    CrossingEvent {
                        sequence: SequenceNumber(sequence),
                        time,
                        root: ResourceId(root),
                        event: ResourceId(event_window),
                        root_x,
                        root_y,
                        event_x,
                        event_y,
                        state,
                    },
                );
            }
        }

        prop_assert_eq!(buf.len(), 32);
        prop_assert_eq!(buf[0], expected_code);
        prop_assert_eq!(buf[1], expected_detail);

        let seq_bytes = if big_endian {
            sequence.to_be_bytes()
        } else {
            sequence.to_le_bytes()
        };
        prop_assert_eq!(&buf[2..4], &seq_bytes[..]);

        let time_bytes = if big_endian { time.to_be_bytes() } else { time.to_le_bytes() };
        prop_assert_eq!(&buf[4..8], &time_bytes[..]);

        let root_bytes = if big_endian { root.to_be_bytes() } else { root.to_le_bytes() };
        prop_assert_eq!(&buf[8..12], &root_bytes[..]);

        let event_bytes = if big_endian { event_window.to_be_bytes() } else { event_window.to_le_bytes() };
        prop_assert_eq!(&buf[12..16], &event_bytes[..]);

        prop_assert_eq!(&buf[16..20], &[0u8; 4][..]); // child = 0

        let rx = if big_endian { root_x.to_be_bytes() } else { root_x.to_le_bytes() };
        prop_assert_eq!(&buf[20..22], &rx[..]);
        let ry = if big_endian { root_y.to_be_bytes() } else { root_y.to_le_bytes() };
        prop_assert_eq!(&buf[22..24], &ry[..]);
        let ex = if big_endian { event_x.to_be_bytes() } else { event_x.to_le_bytes() };
        prop_assert_eq!(&buf[24..26], &ex[..]);
        let ey = if big_endian { event_y.to_be_bytes() } else { event_y.to_le_bytes() };
        prop_assert_eq!(&buf[26..28], &ey[..]);

        let state_bytes = if big_endian { state.to_be_bytes() } else { state.to_le_bytes() };
        prop_assert_eq!(&buf[expected_state_offset..expected_state_offset + 2], &state_bytes[..]);

        match expected_code {
            4 | 5 | 6 => {
                prop_assert_eq!(buf[30], 1); // same_screen
                prop_assert_eq!(buf[31], 0); // pad
            }
            7 | 8 => {
                prop_assert_eq!(buf[30], 0); // mode = NotifyNormal
                prop_assert_eq!(buf[31], 0x03); // same_screen + focus
            }
            _ => unreachable!(),
        }
    }
}
```

- [ ] **Step 2: Run the proptest**

Run: `cargo test -p yserver-protocol pointer_event_tests`
Expected: 5 unit + 1 proptest = 6 tests pass.

### Task 2.7: Verify, format, lint, commit

- [ ] **Step 1: Run the full crate test suite**

Run: `cargo test -p yserver-protocol`
Expected: 15 tests pass (was 9, +6).

- [ ] **Step 2: Format and lint**

Run: `cargo fmt && cargo clippy -- -W clippy::pedantic`
Expected: clean exit, no warnings.

- [ ] **Step 3: Commit**

```bash
git add crates/yserver-protocol/src/x11/mod.rs
git commit -m "feat(protocol): add pointer + crossing event encoders"
```

---

## Commit 3 — `HostX11` refactor: drawing methods take `host_xid` + subwindow lifecycle

This commit is mostly mechanical. Behavior is identical to today because every call site in `nested.rs` passes `host.window_id()` (the existing public accessor at `crates/yserver-core/src/host_x11.rs:158-160`) as the `host_xid` argument. Commit 5 replaces those with real `top_level_host_target` resolution.

### Task 3.1: Make `allocate_xid` public

**Files:**
- Modify: `crates/yserver-core/src/host_x11.rs:57-61`

- [ ] **Step 1: Change visibility**

In `:57-61`, change:

```rust
fn allocate_xid(&mut self) -> u32 {
    let xid = self.next_xid;
    self.next_xid = self.next_xid.wrapping_add(1);
    xid
}
```

to:

```rust
pub fn allocate_xid(&mut self) -> u32 {
    let xid = self.next_xid;
    self.next_xid = self.next_xid.wrapping_add(1);
    xid
}
```

- [ ] **Step 2: Verify build**

Run: `cargo build -p yserver-core`
Expected: compiles cleanly.

### Task 3.2: Add the five subwindow-lifecycle methods on `HostX11`

**Files:**
- Modify: `crates/yserver-core/src/host_x11.rs` (inside `impl HostX11`, after the `query_pointer` method that ends around `:195`)

- [ ] **Step 1: Add the five methods**

Insert after `query_pointer` (the closing `}` at `:195`), before `pub fn poly_fill_arc(...)` at `:197`:

```rust
pub fn create_subwindow(
    &mut self,
    host_xid: u32,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
) -> io::Result<()> {
    // 1. CreateWindow request — parent is the container (self.window_id).
    let create_seq = self.sequence.wrapping_add(1);
    self.sequence = create_seq;
    let mut out = Vec::new();
    out.push(1); // CreateWindow opcode
    out.push(0); // depth = CopyFromParent
    write_u16(&mut out, 8); // length: 8 units * 4 = 32 bytes
    write_u32(&mut out, host_xid);
    write_u32(&mut out, self.window_id); // parent = container
    write_i16(&mut out, x);
    write_i16(&mut out, y);
    let safe_width = width.max(1);
    let safe_height = height.max(1);
    write_u16(&mut out, safe_width);
    write_u16(&mut out, safe_height);
    write_u16(&mut out, 0); // border_width
    write_u16(&mut out, 0); // class = CopyFromParent
    write_u32(&mut out, 0); // visual = CopyFromParent
    write_u32(&mut out, 0); // value-mask = 0
    self.stream.write_all(&out)?;

    // 2. GetGeometry round-trip — forces the host to commit CreateWindow
    //    before any later request (e.g. ChangeWindowAttributes from the
    //    pump's connection) can be processed. See spec §"Cross-connection
    //    ordering hazard".
    let geom_seq = self.sequence.wrapping_add(1);
    self.sequence = geom_seq;
    let mut geom = Vec::new();
    geom.push(14); // GetGeometry opcode
    geom.push(0);
    write_u16(&mut geom, 2);
    write_u32(&mut geom, host_xid);
    self.stream.write_all(&geom)?;
    self.stream.flush()?;

    // Drain replies/errors until we see geom_seq.
    loop {
        let resp = read_response(&mut self.stream)?;
        if resp.sequence == geom_seq {
            return Ok(());
        }
        // Ignore any earlier responses (e.g. an error on CreateWindow);
        // GetGeometry will then also fail and we'll see its reply here.
    }
}

pub fn destroy_subwindow(&mut self, host_xid: u32) -> io::Result<()> {
    self.sequence = self.sequence.wrapping_add(1);
    let mut out = Vec::new();
    out.push(4); // DestroyWindow
    out.push(0);
    write_u16(&mut out, 2);
    write_u32(&mut out, host_xid);
    self.stream.write_all(&out)?;
    self.stream.flush()
}

pub fn configure_subwindow(
    &mut self,
    host_xid: u32,
    x: Option<i16>,
    y: Option<i16>,
    width: Option<u16>,
    height: Option<u16>,
) -> io::Result<()> {
    let mut value_mask: u16 = 0;
    let mut values: Vec<u8> = Vec::new();
    if let Some(x) = x {
        value_mask |= 1 << 0;
        write_u32(&mut values, x as i32 as u32);
    }
    if let Some(y) = y {
        value_mask |= 1 << 1;
        write_u32(&mut values, y as i32 as u32);
    }
    if let Some(width) = width {
        value_mask |= 1 << 2;
        write_u32(&mut values, u32::from(width.max(1)));
    }
    if let Some(height) = height {
        value_mask |= 1 << 3;
        write_u32(&mut values, u32::from(height.max(1)));
    }
    if value_mask == 0 {
        return Ok(());
    }

    let length_units = 3 + u16::try_from(values.len() / 4).map_err(|_| {
        io::Error::new(
            ErrorKind::InvalidInput,
            "too many ConfigureWindow values",
        )
    })?;
    self.sequence = self.sequence.wrapping_add(1);
    let mut out = Vec::new();
    out.push(12); // ConfigureWindow
    out.push(0);
    write_u16(&mut out, length_units);
    write_u32(&mut out, host_xid);
    write_u16(&mut out, value_mask);
    write_u16(&mut out, 0); // pad
    out.extend_from_slice(&values);
    self.stream.write_all(&out)?;
    self.stream.flush()
}

pub fn map_subwindow(&mut self, host_xid: u32) -> io::Result<()> {
    self.sequence = self.sequence.wrapping_add(1);
    let mut out = Vec::new();
    out.push(8); // MapWindow
    out.push(0);
    write_u16(&mut out, 2);
    write_u32(&mut out, host_xid);
    self.stream.write_all(&out)?;
    self.stream.flush()
}

pub fn unmap_subwindow(&mut self, host_xid: u32) -> io::Result<()> {
    self.sequence = self.sequence.wrapping_add(1);
    let mut out = Vec::new();
    out.push(10); // UnmapWindow
    out.push(0);
    write_u16(&mut out, 2);
    write_u32(&mut out, host_xid);
    self.stream.write_all(&out)?;
    self.stream.flush()
}
```

> **Note on `as i32 as u32`:** the X11 wire format encodes signed `i16` `x`/`y` as 32-bit signed integers in `ConfigureWindow` value-list slots. `value as i32 as u32` performs the sign-extending cast. Clippy may flag this; the cast is intentional.

- [ ] **Step 2: Verify build**

Run: `cargo build -p yserver-core`
Expected: compiles cleanly.

### Task 3.3: Thread `host_xid: u32` through every drawing method

**Files:**
- Modify: `crates/yserver-core/src/host_x11.rs` (drawing methods at `:197-324` and `:326-347`)

- [ ] **Step 1: Update method signatures and bodies**

Replace every internal use of `self.window_id` in drawing methods with the passed `host_xid`. Specifically, modify:

`:197` `pub fn poly_fill_arc(&mut self, foreground: u32, arcs: &[u8])` → `pub fn poly_fill_arc(&mut self, host_xid: u32, foreground: u32, arcs: &[u8])`. Pass `host_xid` to `self.draw_arcs(...)`.

`:201` `pub fn poly_arc(&mut self, foreground: u32, arcs: &[u8])` → `pub fn poly_arc(&mut self, host_xid: u32, foreground: u32, arcs: &[u8])`. Same.

`:205` `pub fn poly_fill_rectangle(&mut self, foreground: u32, rectangles: &[u8])` → `pub fn poly_fill_rectangle(&mut self, host_xid: u32, foreground: u32, rectangles: &[u8])`. Inside, replace `write_u32(&mut out, self.window_id);` with `write_u32(&mut out, host_xid);`.

`:231` `pub fn fill_rectangle(&mut self, foreground: u32, x, y, width, height)` → `pub fn fill_rectangle(&mut self, host_xid: u32, foreground: u32, x: i16, y: i16, width: u16, height: u16)`. Pass `host_xid` to the inner `self.poly_fill_rectangle(...)` call.

`:247` `pub fn poly_line(&mut self, foreground: u32, coordinate_mode: u8, points: &[u8])` → `pub fn poly_line(&mut self, host_xid: u32, foreground: u32, coordinate_mode: u8, points: &[u8])`. Replace `write_u32(&mut out, self.window_id);` with `write_u32(&mut out, host_xid);`.

`:278` `pub fn image_text8(&mut self, foreground: u32, background: u32, text_len: u8, body: &[u8])` → `pub fn image_text8(&mut self, host_xid: u32, foreground: u32, background: u32, text_len: u8, body: &[u8])`. Replace `write_u32(&mut out, self.window_id);` with `write_u32(&mut out, host_xid);`.

`:306` `pub fn poly_text8(&mut self, foreground: u32, body: &[u8])` → `pub fn poly_text8(&mut self, host_xid: u32, foreground: u32, body: &[u8])`. Replace `write_u32(&mut out, self.window_id);` with `write_u32(&mut out, host_xid);`.

`:326` `fn draw_arcs(&mut self, opcode: u8, foreground: u32, arcs: &[u8])` (private helper) → `fn draw_arcs(&mut self, host_xid: u32, opcode: u8, foreground: u32, arcs: &[u8])`. Replace `write_u32(&mut out, self.window_id);` with `write_u32(&mut out, host_xid);`.

> **Note:** `query_pointer` at `:167-195` keeps its current behavior — still queries against `self.window_id` (the container) — per the spec. Do not add a `host_xid` arg there.

- [ ] **Step 2: Update every call site in `nested.rs` to pass `host.window_id()`**

In `crates/yserver-core/src/nested.rs`, every `host.poly_*(...)`, `host.fill_rectangle(...)`, `host.image_text8(...)`, and `host.poly_text8(...)` call needs `host.window_id()` prepended as the first argument. Specifically:

- `:1409` `host.fill_rectangle(background_pixel, request.x, request.y, width, height)?` → add `host.window_id()` as first arg. *But* `host` is borrowed mutably here via `host.lock()` and `host.window_id()` borrows immutably — so capture the id first:
  ```rust
  let host_id = host.window_id();
  host.fill_rectangle(host_id, background_pixel, request.x, request.y, width, height)?;
  ```
  Apply the same `let host_id = host.window_id();` pattern at every call site below to avoid borrow-checker conflicts.
- `:1426` `host.poly_line(foreground, header.data, points)?` → `host.poly_line(host_id, foreground, header.data, points)?`
- `:1442` `host.poly_arc(foreground, arcs)?` → `host.poly_arc(host_id, foreground, arcs)?`
- `:1457` `host.poly_fill_rectangle(foreground, rectangles)?` → `host.poly_fill_rectangle(host_id, foreground, rectangles)?`
- `:1471` `host.poly_fill_arc(foreground, arcs)?` → `host.poly_fill_arc(host_id, foreground, arcs)?`
- `:1486` `host.poly_text8(foreground, text_body)?` → `host.poly_text8(host_id, foreground, text_body)?`
- `:1503` `host.image_text8(foreground, background, header.data, text_body)?` → `host.image_text8(host_id, foreground, background, header.data, text_body)?`

For each block where `host_id` is needed, hoist the `let host_id = host.window_id();` line right after the `let Ok(mut host) = host.lock()` guard, before any mutable host call.

- [ ] **Step 3: Verify build and run all existing tests**

Run: `cargo build -p yserver-core && cargo test -p yserver-core`
Expected: builds cleanly; all 56 tests still pass (no test changes — behavior is identical).

### Task 3.4: Verify, format, lint, commit

- [ ] **Step 1: Format and lint**

Run: `cargo fmt && cargo clippy -- -W clippy::pedantic`
Expected: clean exit, no warnings. (Clippy may flag the `as i32 as u32` cast in `configure_subwindow` — if so, add `#[allow(clippy::cast_sign_loss, clippy::cast_possible_wrap)]` on that method.)

- [ ] **Step 2: Run the full workspace tests**

Run: `cargo test`
Expected: yserver-core 56, yserver-protocol 15. No regressions.

- [ ] **Step 3: Commit**

```bash
git add crates/yserver-core/src/host_x11.rs crates/yserver-core/src/nested.rs
git commit -m "refactor(host_x11): subwindow lifecycle + host_xid arg on draw"
```

---

## Commit 4 — `HostInputPump` + `xid_map` + `pointer_event_fanout`

### Task 4.1: Replace `HostKeyboard` with `HostInputPump` skeleton + `HostPointerEvent`

**Files:**
- Modify: `crates/yserver-core/src/host_x11.rs:22-24` (struct), `:413-451` (impl + types)

- [ ] **Step 1: Replace the `HostKeyboard` struct and add new types**

Replace `:22-24`:

```rust
pub struct HostKeyboard {
    stream: UnixStream,
}
```

with:

```rust
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use yserver_protocol::x11::ResourceId;

pub type HostXidMap = Arc<Mutex<HashMap<u32, ResourceId>>>;

pub struct HostInputPump {
    read_stream: UnixStream,
    handle: HostInputPumpHandle,
}

#[derive(Clone)]
pub struct HostInputPumpHandle {
    write_stream: Arc<Mutex<UnixStream>>,
    xid_map: HostXidMap,
}
```

> **Note on the import for `ResourceId`:** `host_x11.rs` already has `use yserver_protocol::x11::{self, FontMetrics};` at the top (`:8`). Add `ResourceId` to that line: `use yserver_protocol::x11::{self, FontMetrics, ResourceId};`. The other two `use` lines (`HashMap`, `Arc`, `Mutex`) go at the top of the file alongside the other `use` statements.

- [ ] **Step 2: Update `HostKeyboard` impl block to be `HostInputPump` impl**

Replace `:413-445` (the existing `impl HostKeyboard { ... }` block) with:

```rust
impl HostInputPump {
    pub fn open_from_env(window_id: u32) -> io::Result<Self> {
        let mut stream = connect_to_host()?;
        let _setup = read_setup_reply(&mut stream)?;
        select_keyboard_events(&mut stream, window_id)?;
        stream.flush()?;
        let read_stream = stream.try_clone()?;
        let handle = HostInputPumpHandle {
            write_stream: Arc::new(Mutex::new(stream)),
            xid_map: Arc::new(Mutex::new(HashMap::new())),
        };
        Ok(Self { read_stream, handle })
    }

    #[must_use]
    pub fn handle(&self) -> HostInputPumpHandle {
        self.handle.clone()
    }

    pub fn read_event(&mut self) -> io::Result<HostEvent> {
        loop {
            let mut event = [0; 32];
            self.read_stream.read_exact(&mut event)?;
            let event_type = event[0] & 0x7f;
            match event_type {
                2 | 3 => {
                    return Ok(HostEvent::Key(HostKeyEvent {
                        pressed: event_type == 2,
                        keycode: event[1],
                        time: read_u32(&event[4..8]),
                        root_x: read_i16(&event[20..22]),
                        root_y: read_i16(&event[22..24]),
                        event_x: read_i16(&event[24..26]),
                        event_y: read_i16(&event[26..28]),
                        state: read_u16(&event[28..30]),
                    }));
                }
                4 | 5 | 6 => {
                    let kind = match event_type {
                        4 => PointerEventKind::ButtonPress,
                        5 => PointerEventKind::ButtonRelease,
                        _ => PointerEventKind::MotionNotify,
                    };
                    return Ok(HostEvent::Pointer(HostPointerEvent {
                        kind,
                        host_xid: read_u32(&event[12..16]), // event window
                        detail: event[1],
                        time: read_u32(&event[4..8]),
                        root_x: read_i16(&event[20..22]),
                        root_y: read_i16(&event[22..24]),
                        event_x: read_i16(&event[24..26]),
                        event_y: read_i16(&event[26..28]),
                        state: read_u16(&event[28..30]),
                    }));
                }
                7 | 8 => {
                    let kind = if event_type == 7 {
                        PointerEventKind::EnterNotify
                    } else {
                        PointerEventKind::LeaveNotify
                    };
                    return Ok(HostEvent::Pointer(HostPointerEvent {
                        kind,
                        host_xid: read_u32(&event[12..16]),
                        detail: 0,
                        time: read_u32(&event[4..8]),
                        root_x: read_i16(&event[20..22]),
                        root_y: read_i16(&event[22..24]),
                        event_x: read_i16(&event[24..26]),
                        event_y: read_i16(&event[26..28]),
                        state: read_u16(&event[28..30]),
                    }));
                }
                17 => return Ok(HostEvent::Closed),
                _ => continue,
            }
        }
    }
}

const POINTER_EVENT_MASK: u32 = 0x0000_0004 // ButtonPress
    | 0x0000_0008 // ButtonRelease
    | 0x0000_0010 // EnterWindow
    | 0x0000_0020 // LeaveWindow
    | 0x0000_0040; // PointerMotion

impl HostInputPumpHandle {
    pub fn register_top_level(
        &self,
        nested_id: ResourceId,
        host_xid: u32,
    ) -> io::Result<()> {
        // ChangeWindowAttributes — value-mask = (1<<11) (event-mask), value = pointer mask.
        let mut out = Vec::new();
        out.push(2); // ChangeWindowAttributes
        out.push(0);
        write_u16(&mut out, 4);
        write_u32(&mut out, host_xid);
        write_u32(&mut out, 1 << 11);
        write_u32(&mut out, POINTER_EVENT_MASK);
        {
            let mut stream = self
                .write_stream
                .lock()
                .map_err(|_| io::Error::new(ErrorKind::BrokenPipe, "host pump stream poisoned"))?;
            stream.write_all(&out)?;
            stream.flush()?;
        }
        if let Ok(mut map) = self.xid_map.lock() {
            map.insert(host_xid, nested_id);
        }
        Ok(())
    }

    pub fn unregister_top_level(&self, host_xid: u32) {
        if let Ok(mut map) = self.xid_map.lock() {
            map.remove(&host_xid);
        }
    }

    #[must_use]
    pub fn xid_map(&self) -> HostXidMap {
        self.xid_map.clone()
    }
}
```

- [ ] **Step 3: Extend `HostEvent` with the `Pointer` variant + add `HostPointerEvent` and `PointerEventKind`**

Replace `:447-451` (the existing `HostEvent` enum):

```rust
#[derive(Clone, Copy, Debug)]
pub enum HostEvent {
    Key(HostKeyEvent),
    Closed,
}
```

with:

```rust
#[derive(Clone, Copy, Debug)]
pub enum HostEvent {
    Key(HostKeyEvent),
    Pointer(HostPointerEvent),
    Closed,
}

#[derive(Clone, Copy, Debug)]
pub enum PointerEventKind {
    ButtonPress,
    ButtonRelease,
    MotionNotify,
    EnterNotify,
    LeaveNotify,
}

#[derive(Clone, Copy, Debug)]
pub struct HostPointerEvent {
    pub kind: PointerEventKind,
    pub host_xid: u32,
    pub detail: u8,
    pub time: u32,
    pub root_x: i16,
    pub root_y: i16,
    pub event_x: i16,
    pub event_y: i16,
    pub state: u16,
}
```

- [ ] **Step 4: Update `nested.rs` imports**

In `crates/yserver-core/src/nested.rs:20`, the existing line:

```rust
use crate::{
    host_x11::{HostEvent, HostKeyboard, HostX11},
    ...
};
```

Replace `HostKeyboard` with `HostInputPump, HostInputPumpHandle, HostPointerEvent, PointerEventKind`:

```rust
use crate::{
    host_x11::{HostEvent, HostInputPump, HostInputPumpHandle, HostPointerEvent, HostX11, PointerEventKind},
    ...
};
```

- [ ] **Step 5: Replace `HostKeyboard::open_from_env` call sites with `HostInputPump`**

In `nested.rs`, two places use `HostKeyboard::open_from_env(...)`:

- `:319` (inside `spawn_window_close_watcher`): `let mut watcher = match HostKeyboard::open_from_env(window_id) {` → keep this unchanged for now by adding a temporary alias OR replace with `HostInputPump`. Since the watcher only consumes events and exits on `Closed`, switching is safe. Replace with:
  ```rust
  let mut watcher = match HostInputPump::open_from_env(window_id) {
  ```
- `:221` (inside `handle_client`, the per-client keyboard forwarder): replace `HostKeyboard::open_from_env(host_window_id)` with `HostInputPump::open_from_env(host_window_id)` and update the call to `spawn_keyboard_forwarder` to take `HostInputPump`.

Update the `spawn_keyboard_forwarder` signature at `:343-401` from:

```rust
fn spawn_keyboard_forwarder(
    client_id: ClientId,
    mut keyboard: HostKeyboard,
    ...
)
```

to:

```rust
fn spawn_keyboard_forwarder(
    client_id: ClientId,
    mut keyboard: HostInputPump,
    ...
)
```

The body already calls `keyboard.read_event()` which still returns `HostEvent`; the existing `match` on `Ok(HostEvent::Key(event))` / `Ok(HostEvent::Closed)` keeps compiling, but a new arm is needed for `Ok(HostEvent::Pointer(_))`. For Commit 4, just drop pointer events on the floor inside the per-client forwarder — the real fanout via `pointer_event_fanout` is wired in Commit 5. Add this arm to the inner match:

```rust
Ok(HostEvent::Pointer(_)) => continue,
```

inserted between the `Ok(HostEvent::Key(event)) => event,` and `Ok(HostEvent::Closed) => { ... }` arms. Use `continue` because the match is inside a `loop` — spelling matches the rest of the file.

> **Wait** — the existing match isn't inside a `loop` in the way that allows `continue`. Look at `:351-362`:
> ```rust
> let event = match keyboard.read_event() {
>     Ok(HostEvent::Key(event)) => event,
>     Ok(HostEvent::Closed) => { ... process::exit(0); }
>     Err(err) => { ... process::exit(0); }
> };
> ```
> The match assigns to `event`. To skip pointer events, the cleanest fix is to wrap the read in `loop { match keyboard.read_event() { ... } }` and `break` with the value when a Key arrives, or use a `continue`-friendly outer `loop`. The existing code is *already* inside the outer `loop {}` at `:350`, so add the arm using `continue` directly:
>
> ```rust
> let event = loop {
>     match keyboard.read_event() {
>         Ok(HostEvent::Key(event)) => break event,
>         Ok(HostEvent::Pointer(_)) => continue,
>         Ok(HostEvent::Closed) => { /* exit */ }
>         Err(err) => { /* exit */ }
>     }
> };
> ```

Restructure `:351-362` to:

```rust
let event = loop {
    match keyboard.read_event() {
        Ok(HostEvent::Key(event)) => break event,
        Ok(HostEvent::Pointer(_)) => continue,
        Ok(HostEvent::Closed) => {
            info!("host window closed, exiting");
            std::process::exit(0);
        }
        Err(err) => {
            info!("host connection lost ({err}), exiting");
            std::process::exit(0);
        }
    }
};
```

Similarly, in `spawn_window_close_watcher` at `:327-339`, the existing match arms are:

```rust
match watcher.read_event() {
    Ok(HostEvent::Key(_)) => {}
    Ok(HostEvent::Closed) => { ... }
    Err(err) => { ... }
}
```

Add a `Ok(HostEvent::Pointer(_)) => {}` arm to keep the compiler happy.

- [ ] **Step 6: Verify build**

Run: `cargo build -p yserver-core`
Expected: builds cleanly. The watcher and per-client keyboard forwarder still work as before; pointer events are silently dropped (Commit 5 wires them up).

### Task 4.2: Add `pointer_event_fanout` in `server.rs`

**Files:**
- Modify: `crates/yserver-core/src/server.rs` (after `emit_window_event` at `:197-208`)

- [ ] **Step 1: Add the helper**

Insert after the closing `}` of `emit_window_event` at `:208`, before `#[cfg(test)]` at `:210`:

```rust
pub fn pointer_event_fanout(
    state: &Mutex<ServerState>,
    xid_map: &crate::host_x11::HostXidMap,
    event: crate::host_x11::HostPointerEvent,
) {
    use crate::host_x11::PointerEventKind;
    let nested_id = match xid_map.lock() {
        Ok(map) => match map.get(&event.host_xid).copied() {
            Some(id) => id,
            None => return,
        },
        Err(_) => return,
    };
    let mask_bit: u32 = match event.kind {
        PointerEventKind::ButtonPress => 0x0000_0004,
        PointerEventKind::ButtonRelease => 0x0000_0008,
        PointerEventKind::MotionNotify => 0x0000_0040,
        PointerEventKind::EnterNotify => 0x0000_0010,
        PointerEventKind::LeaveNotify => 0x0000_0020,
    };
    let targets = match state.lock() {
        Ok(g) => g.subscribers(nested_id, mask_bit),
        Err(_) => return,
    };
    for target in targets {
        let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
        let mut buf = Vec::with_capacity(32);
        match event.kind {
            PointerEventKind::ButtonPress => x11::encode_button_press_event(
                &mut buf,
                target.byte_order,
                x11::PointerEvent {
                    sequence: seq,
                    detail: event.detail,
                    time: event.time,
                    root: crate::resources::ROOT_WINDOW,
                    event: nested_id,
                    root_x: event.root_x,
                    root_y: event.root_y,
                    event_x: event.event_x,
                    event_y: event.event_y,
                    state: event.state,
                },
            ),
            PointerEventKind::ButtonRelease => x11::encode_button_release_event(
                &mut buf,
                target.byte_order,
                x11::PointerEvent {
                    sequence: seq,
                    detail: event.detail,
                    time: event.time,
                    root: crate::resources::ROOT_WINDOW,
                    event: nested_id,
                    root_x: event.root_x,
                    root_y: event.root_y,
                    event_x: event.event_x,
                    event_y: event.event_y,
                    state: event.state,
                },
            ),
            PointerEventKind::MotionNotify => x11::encode_motion_notify_event(
                &mut buf,
                target.byte_order,
                x11::PointerEvent {
                    sequence: seq,
                    detail: 0,
                    time: event.time,
                    root: crate::resources::ROOT_WINDOW,
                    event: nested_id,
                    root_x: event.root_x,
                    root_y: event.root_y,
                    event_x: event.event_x,
                    event_y: event.event_y,
                    state: event.state,
                },
            ),
            PointerEventKind::EnterNotify => x11::encode_enter_notify_event(
                &mut buf,
                target.byte_order,
                x11::CrossingEvent {
                    sequence: seq,
                    time: event.time,
                    root: crate::resources::ROOT_WINDOW,
                    event: nested_id,
                    root_x: event.root_x,
                    root_y: event.root_y,
                    event_x: event.event_x,
                    event_y: event.event_y,
                    state: event.state,
                },
            ),
            PointerEventKind::LeaveNotify => x11::encode_leave_notify_event(
                &mut buf,
                target.byte_order,
                x11::CrossingEvent {
                    sequence: seq,
                    time: event.time,
                    root: crate::resources::ROOT_WINDOW,
                    event: nested_id,
                    root_x: event.root_x,
                    root_y: event.root_y,
                    event_x: event.event_x,
                    event_y: event.event_y,
                    state: event.state,
                },
            ),
        }
        if let Ok(mut w) = target.writer.lock() {
            let _ = w.write_all(&buf);
        }
    }
}
```

- [ ] **Step 2: Verify build**

Run: `cargo build -p yserver-core`
Expected: builds cleanly.

### Task 4.3: Add the failing fanout test (mask filtering)

**Files:**
- Modify: `crates/yserver-core/src/server.rs` (inside `mod tests`, after `unmap_notify_fanout_reaches_only_subscribed_clients` at `:368-427`)

- [ ] **Step 1: Add the failing test**

Inside `mod tests`, after the closing `}` of `unmap_notify_fanout_reaches_only_subscribed_clients` (currently `:427`), insert:

```rust
#[test]
fn pointer_event_fanout_filters_by_mask() {
    use std::collections::HashMap as StdHashMap;
    use std::io::Read;
    use std::sync::Mutex as StdMutex;

    use crate::host_x11::{HostPointerEvent, PointerEventKind};

    // Client A: ButtonPress on window 0x100002.
    let (a_writer_local, mut a_reader_remote) = UnixStream::pair().expect("socketpair");
    // Client B: MotionNotify on window 0x100002.
    let (b_writer_local, mut b_reader_remote) = UnixStream::pair().expect("socketpair");
    // Client C: no pointer events at all.
    let (c_writer_local, _c_reader_remote) = UnixStream::pair().expect("socketpair");

    let state = StdMutex::new(ServerState::new());
    {
        let mut s = state.lock().unwrap();
        s.clients.insert(
            1,
            ClientHandle {
                writer: Arc::new(Mutex::new(a_writer_local)),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0010_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::from([(ResourceId(0x100002), 0x0000_0004)]), // ButtonPress
            },
        );
        s.clients.insert(
            2,
            ClientHandle {
                writer: Arc::new(Mutex::new(b_writer_local)),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0020_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::from([(ResourceId(0x100002), 0x0000_0040)]), // PointerMotion
            },
        );
        s.clients.insert(
            3,
            ClientHandle {
                writer: Arc::new(Mutex::new(c_writer_local)),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0030_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::new(),
            },
        );
    }

    let mut map = StdHashMap::new();
    map.insert(0xCAFE_u32, ResourceId(0x100002));
    let xid_map = Arc::new(StdMutex::new(map));

    pointer_event_fanout(
        &state,
        &xid_map,
        HostPointerEvent {
            kind: PointerEventKind::ButtonPress,
            host_xid: 0xCAFE,
            detail: 1,
            time: 0,
            root_x: 1,
            root_y: 2,
            event_x: 3,
            event_y: 4,
            state: 0,
        },
    );

    let mut received = [0u8; 32];
    a_reader_remote.read_exact(&mut received).unwrap();
    assert_eq!(received[0], 4, "client A should receive ButtonPress");
    assert_eq!(received[1], 1, "detail = 1");
    assert_eq!(&received[12..16], &0x0010_0002u32.to_le_bytes());

    // Client B should *not* have received anything; setting the socket to
    // non-blocking and reading should fail.
    b_reader_remote.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 32];
    let result = b_reader_remote.read(&mut buf);
    assert!(
        matches!(&result, Err(e) if e.kind() == std::io::ErrorKind::WouldBlock)
            || matches!(&result, Ok(0)),
        "client B should not have received any pointer event, got {:?}",
        result
    );
}

#[test]
fn pointer_event_fanout_drops_unknown_host_xid() {
    use std::collections::HashMap as StdHashMap;
    use std::io::Read;
    use std::sync::Mutex as StdMutex;

    use crate::host_x11::{HostPointerEvent, PointerEventKind};

    let (a_writer_local, mut a_reader_remote) = UnixStream::pair().expect("socketpair");

    let state = StdMutex::new(ServerState::new());
    {
        let mut s = state.lock().unwrap();
        s.clients.insert(
            1,
            ClientHandle {
                writer: Arc::new(Mutex::new(a_writer_local)),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0010_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::from([(ResourceId(0x100002), 0x0000_0004)]),
            },
        );
    }

    let xid_map: crate::host_x11::HostXidMap =
        Arc::new(StdMutex::new(StdHashMap::new())); // empty

    pointer_event_fanout(
        &state,
        &xid_map,
        HostPointerEvent {
            kind: PointerEventKind::ButtonPress,
            host_xid: 0xCAFE, // not in map
            detail: 1,
            time: 0,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            state: 0,
        },
    );

    a_reader_remote.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 32];
    let result = a_reader_remote.read(&mut buf);
    assert!(
        matches!(&result, Err(e) if e.kind() == std::io::ErrorKind::WouldBlock)
            || matches!(&result, Ok(0)),
        "no client should have received anything, got {:?}",
        result
    );
}
```

- [ ] **Step 2: Run the new tests**

Run: `cargo test -p yserver-core server::tests::pointer_event_fanout`
Expected: 2 tests pass.

### Task 4.4: Verify, format, lint, commit

- [ ] **Step 1: Run the full crate test suite**

Run: `cargo test -p yserver-core`
Expected: 58 tests pass (was 56, +2).

- [ ] **Step 2: Format and lint**

Run: `cargo fmt && cargo clippy -- -W clippy::pedantic`
Expected: clean exit, no warnings.

- [ ] **Step 3: Commit**

```bash
git add crates/yserver-core/src/host_x11.rs crates/yserver-core/src/server.rs crates/yserver-core/src/nested.rs
git commit -m "feat(host_x11): HostInputPump + xid_map + pointer_event_fanout"
```

---

## Commit 5 — Wire opcodes 1, 4, 8, 9, 10, 12 + draw routing + pump adapter

### Task 5.1: Wire opcode 1 (`CreateWindow`) to `host.create_subwindow` + `register_top_level`

**Files:**
- Modify: `crates/yserver-core/src/nested.rs:463-518` (opcode 1 handler)

Goal: when `request.parent == ROOT_WINDOW` and `request.window` is a top-level, allocate a host xid, call `create_subwindow`, set `windows[id].host_xid`, and register the xid → nested-id mapping for pointer events.

- [ ] **Step 1: Plumb the `HostInputPumpHandle` to the opcode 1 handler**

The `HostInputPumpHandle` is created once in `run(...)` and shared across clients. Add an `Option<HostInputPumpHandle>` field to the parameters threaded into `handle_client` and `handle_request`.

In `nested.rs:35` (`pub fn run`), after the `host` setup at `:49-58`, add:

```rust
let input_pump = host_window_id.and_then(|window_id| {
    HostInputPump::open_from_env(window_id)
        .ok()
        .map(|pump| {
            let handle = pump.handle();
            // Spawn the pump-reading thread that fans pointer events out
            // to subscribers.
            let server = server.clone();
            let xid_map = handle.xid_map();
            thread::spawn(move || {
                let mut pump = pump;
                loop {
                    match pump.read_event() {
                        Ok(HostEvent::Key(_)) => continue,
                        Ok(HostEvent::Pointer(event)) => {
                            crate::server::pointer_event_fanout(&server, &xid_map, event);
                        }
                        Ok(HostEvent::Closed) => {
                            info!("host pump: window closed, exiting");
                            std::process::exit(0);
                        }
                        Err(err) => {
                            info!("host pump: connection lost ({err}), exiting");
                            std::process::exit(0);
                        }
                    }
                }
            });
            handle
        })
});
```

> **Important:** the `let mut pump = pump;` shadowing inside the closure is needed because `pump` is moved into the `thread::spawn`. Adjust the surrounding block so the closure captures `pump` correctly. Re-arrange:
>
> ```rust
> let input_pump_handle: Option<HostInputPumpHandle> = match host_window_id {
>     Some(window_id) => match HostInputPump::open_from_env(window_id) {
>         Ok(mut pump) => {
>             let handle = pump.handle();
>             let server_for_thread = server.clone();
>             let xid_map = handle.xid_map();
>             thread::spawn(move || {
>                 loop {
>                     match pump.read_event() {
>                         Ok(HostEvent::Key(_)) => continue,
>                         Ok(HostEvent::Pointer(event)) => {
>                             crate::server::pointer_event_fanout(
>                                 &server_for_thread, &xid_map, event,
>                             );
>                         }
>                         Ok(HostEvent::Closed) => {
>                             info!("host pump: window closed, exiting");
>                             std::process::exit(0);
>                         }
>                         Err(err) => {
>                             info!("host pump: connection lost ({err}), exiting");
>                             std::process::exit(0);
>                         }
>                     }
>                 }
>             });
>             Some(handle)
>         }
>         Err(err) => {
>             warn!("could not start host input pump: {err}");
>             None
>         }
>     },
>     None => None,
> };
> ```

This replaces the per-client `HostKeyboard::open_from_env(...)` for *pointer* events. The per-client keyboard forwarder at `:220-231` still exists for keyboard events — but it now becomes redundant for pointer events since the pump above handles them.

- [ ] **Step 2: Pass `input_pump_handle` into `handle_client`**

Modify the `for stream in listener.incoming()` loop at `:72-87`:

```rust
for stream in listener.incoming() {
    match stream {
        Ok(stream) => {
            let client_id = ClientId(NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed));
            let host = host.clone();
            let server = server.clone();
            let input_handle = input_pump_handle.clone();
            thread::spawn(move || {
                if let Err(err) = handle_client(client_id, stream, server, host, host_window_id, input_handle)
                {
                    info!("client {} disconnected: {err}", client_id.0);
                }
            });
        }
        Err(err) => error!("accept failed: {err}"),
    }
}
```

Add `input_handle: Option<HostInputPumpHandle>` to the `handle_client` signature at `:125-131`:

```rust
fn handle_client(
    client_id: ClientId,
    mut stream: UnixStream,
    server: Arc<Mutex<ServerState>>,
    host: Option<Arc<Mutex<HostX11>>>,
    host_window_id: Option<u32>,
    input_handle: Option<HostInputPumpHandle>,
) -> io::Result<()>
```

- [ ] **Step 3: Pass `input_handle` into `handle_request`**

Update the `handle_request` call at `:242-251` to take an additional `input_handle: Option<&HostInputPumpHandle>` arg, and update its signature at `:447-456`:

```rust
fn handle_request(
    client_id: ClientId,
    server: &Arc<Mutex<ServerState>>,
    host: Option<&Arc<Mutex<HostX11>>>,
    input_handle: Option<&HostInputPumpHandle>,
    writer: &Arc<Mutex<UnixStream>>,
    focused_window: &Arc<Mutex<ResourceId>>,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<()>
```

The call site in `handle_client` becomes:

```rust
handle_request(
    client_id,
    &server,
    host.as_ref(),
    input_handle.as_ref(),
    &writer,
    &focused_window,
    sequence,
    header,
    &body,
)?;
```

- [ ] **Step 4: Add the host-subwindow logic to opcode 1**

Replace the body of opcode 1 (currently `:463-518`) with:

```rust
1 => {
    if let Some(request) = x11::create_window_request(header.data, body) {
        debug!(
            "client {} create window 0x{:x} parent=0x{:x} mask=0x{:x}",
            client_id.0,
            request.window.0,
            request.parent.0,
            request.event_mask.unwrap_or(0)
        );
        let new_id = request.window.0;
        let mask = request.event_mask.unwrap_or(0);
        let window_id = request.window;
        let parent = request.parent;
        let geometry = (request.x, request.y, request.width, request.height);
        let validation_failed = {
            let s = lock_server(server)?;
            let handle = s.clients.get(&client_id.0).expect("client registered");
            let owned = crate::server::IdAllocator::validate_owned(
                new_id,
                handle.resource_id_base,
                handle.resource_id_mask,
            );
            let in_use = s.resources.any_resource_exists(request.window);
            !owned || in_use
        };
        if validation_failed {
            return emit_x11_error(writer, sequence, x11::error::BAD_ID_CHOICE, new_id, 1);
        }
        {
            let mut s = lock_server(server)?;
            s.resources.create_window(client_id, request);
            if mask != 0 {
                s.clients
                    .get_mut(&client_id.0)
                    .expect("client registered")
                    .event_masks
                    .insert(window_id, mask);
            }
        }
        // Top-level only: allocate host xid + create host subwindow + register.
        if parent == ROOT_WINDOW
            && let Some(host) = host
        {
            let allocated_xid: Option<u32> = host.lock().ok().map(|mut h| {
                let xid = h.allocate_xid();
                if let Err(err) = h.create_subwindow(
                    xid,
                    geometry.0,
                    geometry.1,
                    geometry.2,
                    geometry.3,
                ) {
                    warn!(
                        "client {} create_subwindow for 0x{:x} failed: {err}",
                        client_id.0, new_id
                    );
                    return None;
                }
                Some(xid)
            }).flatten();

            if let Some(host_xid) = allocated_xid {
                {
                    let mut s = lock_server(server)?;
                    if let Some(w) = s.resources.window_mut(window_id) {
                        w.host_xid = Some(host_xid);
                    }
                }
                if let Some(input_handle) = input_handle
                    && let Err(err) = input_handle.register_top_level(window_id, host_xid)
                {
                    warn!(
                        "client {} register_top_level for 0x{:x} failed: {err}",
                        client_id.0, new_id
                    );
                }
            }
        }
        let wants_focus = {
            let s = lock_server(server)?;
            let mask = s
                .clients
                .get(&client_id.0)
                .and_then(|c| c.event_masks.get(&window_id).copied())
                .unwrap_or(0);
            let viewable = s
                .resources
                .window(window_id)
                .is_some_and(|w| w.map_state == MapState::Viewable);
            viewable && (mask & 0x3) != 0
        };
        if wants_focus {
            set_focused_window(focused_window, server, window_id)?;
        }
    }
    log_void(client_id, sequence, "CreateWindow")
}
```

- [ ] **Step 5: Add `window_mut` accessor to `ResourceTable`**

In `crates/yserver-core/src/resources.rs`, after `pub fn window(&self, id: ResourceId) -> Option<&Window>` at `:164-166`, insert:

```rust
pub fn window_mut(&mut self, id: ResourceId) -> Option<&mut Window> {
    self.windows.get_mut(&id.0)
}
```

- [ ] **Step 6: Verify build**

Run: `cargo build -p yserver-core`
Expected: builds cleanly. Existing 58 tests still pass.

### Task 5.2: Wire opcode 4 (`DestroyWindow`) to `destroy_subwindow` + `unregister_top_level`

**Files:**
- Modify: `crates/yserver-core/src/nested.rs:581-626` (opcode 4)

- [ ] **Step 1: Extend the `pending` tuple with `host_xid`**

Replace `:581-626` (the entire opcode 4 block) with:

```rust
4 => {
    if let Some(window) = x11::free_resource_id(body) {
        let pending = {
            let mut s = lock_server(server)?;
            let mut order = Vec::new();
            collect_destroy_order(&s.resources, window, &mut order);
            #[allow(clippy::type_complexity)]
            let mut pending: Vec<(
                ResourceId,
                ResourceId,
                bool,
                Option<u32>,
                Vec<crate::server::EventTarget>,
                Vec<crate::server::EventTarget>,
            )> = Vec::new();
            for w in &order {
                let (parent, was_mapped, host_xid) =
                    s.resources.window(*w).map_or((ROOT_WINDOW, false, None), |win| {
                        (win.parent, win.map_state != MapState::Unmapped, win.host_xid)
                    });
                let on_window = s.subscribers(*w, 0x0002_0000);
                let on_parent = s.subscribers(parent, 0x0008_0000);
                pending.push((*w, parent, was_mapped, host_xid, on_window, on_parent));
            }
            let _ = s.resources.destroy_window(window);
            s.drop_window_subscriptions(&order);
            pending
        };
        for (w, parent, was_mapped, host_xid, subs_w, subs_p) in pending {
            if let Some(xid) = host_xid {
                if let Some(host) = host
                    && let Ok(mut h) = host.lock()
                {
                    let _ = h.destroy_subwindow(xid);
                }
                if let Some(input_handle) = input_handle {
                    input_handle.unregister_top_level(xid);
                }
            }
            if was_mapped {
                fanout_event(&subs_w, |buf, seq, order| {
                    x11::encode_unmap_notify_event(buf, seq, order, w, w, false);
                });
                fanout_event(&subs_p, |buf, seq, order| {
                    x11::encode_unmap_notify_event(buf, seq, order, parent, w, false);
                });
            }
            fanout_event(&subs_w, |buf, seq, order| {
                x11::encode_destroy_notify_event(buf, seq, order, w, w);
            });
            fanout_event(&subs_p, |buf, seq, order| {
                x11::encode_destroy_notify_event(buf, seq, order, parent, w);
            });
        }
    }
    log_void(client_id, sequence, "DestroyWindow")
}
```

- [ ] **Step 2: Apply the same change to disconnect cleanup at `:255-305`**

Replace the disconnect cleanup block at `:255-305` (currently capturing `(ResourceId, ResourceId, bool, ..., ...)`) with the version that also captures `host_xid: Option<u32>`. Specifically, change:

```rust
#[allow(clippy::type_complexity)]
let mut pending: Vec<(
    ResourceId,
    ResourceId,
    bool,
    Vec<crate::server::EventTarget>,
    Vec<crate::server::EventTarget>,
)> = Vec::new();
```

to:

```rust
#[allow(clippy::type_complexity)]
let mut pending: Vec<(
    ResourceId,
    ResourceId,
    bool,
    Option<u32>,
    Vec<crate::server::EventTarget>,
    Vec<crate::server::EventTarget>,
)> = Vec::new();
```

And inside the inner loop, change:

```rust
let (parent, was_mapped) =
    s.resources.window(*w).map_or((ROOT_WINDOW, false), |win| {
        (win.parent, win.map_state != MapState::Unmapped)
    });
let on_w = s.subscribers(*w, 0x0002_0000);
let on_p = s.subscribers(parent, 0x0008_0000);
pending.push((*w, parent, was_mapped, on_w, on_p));
```

to:

```rust
let (parent, was_mapped, host_xid) =
    s.resources.window(*w).map_or((ROOT_WINDOW, false, None), |win| {
        (win.parent, win.map_state != MapState::Unmapped, win.host_xid)
    });
let on_w = s.subscribers(*w, 0x0002_0000);
let on_p = s.subscribers(parent, 0x0008_0000);
pending.push((*w, parent, was_mapped, host_xid, on_w, on_p));
```

And the post-lock fanout loop at `:290-305`:

```rust
for (w, parent, was_mapped, subs_w, subs_p) in pending_destroys {
```

becomes:

```rust
for (w, parent, was_mapped, host_xid, subs_w, subs_p) in pending_destroys {
    if let Some(xid) = host_xid {
        if let Some(host) = host.as_ref()
            && let Ok(mut h) = host.lock()
        {
            let _ = h.destroy_subwindow(xid);
        }
        if let Some(input_handle) = input_handle.as_ref() {
            input_handle.unregister_top_level(xid);
        }
    }
```

(the rest of the loop body is unchanged).

- [ ] **Step 3: Verify build**

Run: `cargo build -p yserver-core && cargo test -p yserver-core`
Expected: builds cleanly; 58 tests still pass.

### Task 5.3: Wire opcodes 8, 9, 10, 12 to subwindow lifecycle

**Files:**
- Modify: `crates/yserver-core/src/nested.rs:628-781`

- [ ] **Step 1: Opcode 8 (`MapWindow`) — call `host.map_subwindow(host_xid)` for top-levels**

In the opcode 8 block at `:628-681`, after the existing `s.resources.map_window(window);` (around `:632`), capture `host_xid`:

Replace:

```rust
let map_info = {
    let mut s = lock_server(server)?;
    s.resources.map_window(window);
    s.resources
        .window(window)
        .map(|w| (w.parent, w.override_redirect, w.width, w.height))
};
```

with:

```rust
let (map_info, host_xid) = {
    let mut s = lock_server(server)?;
    s.resources.map_window(window);
    let host_xid = s.resources.window(window).and_then(|w| w.host_xid);
    let map_info = s.resources
        .window(window)
        .map(|w| (w.parent, w.override_redirect, w.width, w.height));
    (map_info, host_xid)
};
if let Some(xid) = host_xid
    && let Some(host) = host
    && let Ok(mut h) = host.lock()
{
    let _ = h.map_subwindow(xid);
}
```

- [ ] **Step 2: Opcode 9 (`MapSubwindows`) — for each top-level child, `map_subwindow`**

In the opcode 9 block at `:682-724`, the existing loop maps each `child`. Inside that loop, after `s.resources.map_window(child);` (around `:691`), capture and apply `host_xid`:

Replace:

```rust
for child in children {
    let extents = {
        let mut s = lock_server(server)?;
        s.resources.map_window(child);
        s.resources.window(child).map(|w| (w.width, w.height))
    };
    ...
}
```

with:

```rust
for child in children {
    let (extents, host_xid) = {
        let mut s = lock_server(server)?;
        s.resources.map_window(child);
        let host_xid = s.resources.window(child).and_then(|w| w.host_xid);
        let extents = s.resources.window(child).map(|w| (w.width, w.height));
        (extents, host_xid)
    };
    if let Some(xid) = host_xid
        && let Some(host) = host
        && let Ok(mut h) = host.lock()
    {
        let _ = h.map_subwindow(xid);
    }
    ...
}
```

(Keep the rest of the loop body — focus check + Expose synthesis — unchanged.)

- [ ] **Step 3: Opcode 10 (`UnmapWindow`) — `unmap_subwindow` for top-levels**

In the opcode 10 block at `:725-749`, change the `lock_server` snapshot to also capture `host_xid`:

Replace:

```rust
let snapshot = {
    let mut s = lock_server(server)?;
    let was_mapped = s.resources.unmap_window(window);
    if was_mapped {
        let parent = s.resources.window(window).map_or(ROOT_WINDOW, |w| w.parent);
        let on_window = s.subscribers(window, 0x0002_0000); // StructureNotify
        let on_parent = s.subscribers(parent, 0x0008_0000); // SubstructureNotify
        Some((parent, on_window, on_parent))
    } else {
        None
    }
};
```

with:

```rust
let (snapshot, host_xid) = {
    let mut s = lock_server(server)?;
    let host_xid = s.resources.window(window).and_then(|w| w.host_xid);
    let was_mapped = s.resources.unmap_window(window);
    let snapshot = if was_mapped {
        let parent = s.resources.window(window).map_or(ROOT_WINDOW, |w| w.parent);
        let on_window = s.subscribers(window, 0x0002_0000);
        let on_parent = s.subscribers(parent, 0x0008_0000);
        Some((parent, on_window, on_parent))
    } else {
        None
    };
    (snapshot, host_xid)
};
if let Some(xid) = host_xid
    && let Some(host) = host
    && let Ok(mut h) = host.lock()
{
    let _ = h.unmap_subwindow(xid);
}
```

(The fanout loop below this stays identical.)

- [ ] **Step 4: Opcode 12 (`ConfigureWindow`) — `configure_subwindow`**

In the opcode 12 block at `:750-778`, after the existing `s.resources.configure_window(request)` snapshot:

Replace:

```rust
let configure = {
    let mut s = lock_server(server)?;
    s.resources
        .configure_window(request)
        .map(|w| (w.id, window_geometry(w), w.override_redirect))
};
```

with:

```rust
let (configure, host_xid) = {
    let mut s = lock_server(server)?;
    let configure = s.resources
        .configure_window(request)
        .map(|w| (w.id, window_geometry(w), w.override_redirect));
    let host_xid = configure
        .as_ref()
        .and_then(|(id, _, _)| s.resources.window(*id).and_then(|w| w.host_xid));
    (configure, host_xid)
};
if let Some(xid) = host_xid
    && let Some(host) = host
    && let Ok(mut h) = host.lock()
{
    let _ = h.configure_subwindow(xid, request.x, request.y, request.width, request.height);
}
```

- [ ] **Step 5: Verify build**

Run: `cargo build -p yserver-core && cargo test -p yserver-core`
Expected: builds cleanly; 58 tests still pass.

### Task 5.4: Route every drawing handler through `top_level_host_target`

**Files:**
- Modify: `crates/yserver-core/src/nested.rs` (drawing handlers at `:1393-1507`: opcodes 61, 65, 68, 70, 71, 74, 76)

Each drawing handler today does:

```rust
let host_id = host.window_id();
host.poly_<op>(host_id, foreground, ...)?;
```

Replace each with the `top_level_host_target`-driven prelude:

```rust
let target = {
    let s = lock_server(server)?;
    s.resources.top_level_host_target(ResourceId(drawable))
};
let Some(target) = target else {
    return log_void(client_id, sequence, "<OpName>");
};
// ... apply target.x_offset / target.y_offset to per-op coordinates ...
host.poly_<op>(target.host_xid, foreground, ...)?;
```

The translation of coordinates depends on the drawing primitive. For Phase 1, since `(x_offset, y_offset)` is `(0, 0)` for all top-level drawables, **omit coordinate translation for now** — child windows with non-zero offsets are out of scope (Phase 1 apps use one top-level per process). Document this in a comment in each handler.

> **Spec compliance note:** the spec says draw routing should translate by `(target.x_offset, target.y_offset)`. Implementing actual translation requires walking the drawing payload and adding offsets to each (x, y) pair, per-opcode. For Phase 1 this code path is not exercised (no nested children draw — `xeyes`/`xterm` draw directly into their top-levels). The `top_level_host_target` helper *returns* the offsets so we can wire them in later without changing the call sites. For now: drop drawing into child windows by only routing when `target.x_offset == 0 && target.y_offset == 0`. Anything else: log a warning, drop the host call.

- [ ] **Step 1: Opcode 61 (`ClearArea`) at `:1393-1414`**

Replace:

```rust
61 => {
    if let Some(request) = x11::clear_area_request(body) {
        let extents = {
            let s = lock_server(server)?;
            s.resources
                .window(request.window)
                .map(|w| (w.background_pixel, w.width, w.height))
        };
        if let Some((background_pixel, w_width, w_height)) = extents {
            let width = clear_extent(request.width, request.x, w_width);
            let height = clear_extent(request.height, request.y, w_height);
            if width != 0
                && height != 0
                && let Some(host) = host
                && let Ok(mut host) = host.lock()
            {
                let host_id = host.window_id();
                host.fill_rectangle(host_id, background_pixel, request.x, request.y, width, height)?;
            }
        }
    }
    log_void(client_id, sequence, "ClearArea")
}
```

with:

```rust
61 => {
    if let Some(request) = x11::clear_area_request(body) {
        let (extents, target) = {
            let s = lock_server(server)?;
            let extents = s.resources
                .window(request.window)
                .map(|w| (w.background_pixel, w.width, w.height));
            let target = s.resources.top_level_host_target(request.window);
            (extents, target)
        };
        if let Some((background_pixel, w_width, w_height)) = extents
            && let Some(target) = target
            && target.x_offset == 0
            && target.y_offset == 0
        {
            let width = clear_extent(request.width, request.x, w_width);
            let height = clear_extent(request.height, request.y, w_height);
            if width != 0
                && height != 0
                && let Some(host) = host
                && let Ok(mut host) = host.lock()
            {
                host.fill_rectangle(target.host_xid, background_pixel, request.x, request.y, width, height)?;
            }
        }
    }
    log_void(client_id, sequence, "ClearArea")
}
```

- [ ] **Step 2: Opcode 65 (`PolyLine`) at `:1417-1430`**

Replace:

```rust
65 => {
    if let Some((gc_id, points)) = x11::poly_line_data(body) {
        let foreground = {
            let s = lock_server(server)?;
            s.resources.gc_foreground(ResourceId(gc_id))
        };
        if let Some(host) = host
            && let Ok(mut host) = host.lock()
        {
            let host_id = host.window_id();
            host.poly_line(host_id, foreground, header.data, points)?;
        }
    }
    log_void(client_id, sequence, "PolyLine")
}
```

with:

```rust
65 => {
    if let Some((gc_id, points)) = x11::poly_line_data(body)
        && let Some(drawable) = x11::drawable_request_id(body)
    {
        let (foreground, target) = {
            let s = lock_server(server)?;
            (s.resources.gc_foreground(ResourceId(gc_id)),
             s.resources.top_level_host_target(drawable))
        };
        if let Some(target) = target
            && target.x_offset == 0
            && target.y_offset == 0
            && let Some(host) = host
            && let Ok(mut host) = host.lock()
        {
            host.poly_line(target.host_xid, foreground, header.data, points)?;
        }
    }
    log_void(client_id, sequence, "PolyLine")
}
```

- [ ] **Step 3: Opcode 68 (`PolyArc`) at `:1433-1446`**

Apply the identical pattern (drawable extracted via `x11::drawable_request_id(body)`; route through `top_level_host_target`; gate on `x_offset == 0 && y_offset == 0`).

- [ ] **Step 4: Opcode 70 (`PolyFillRectangle`) at `:1448-1461`**

Same pattern.

- [ ] **Step 5: Opcode 71 (`PolyFillArc`) at `:1462-1475`**

Same pattern.

- [ ] **Step 6: Opcode 74 (`PolyText8`) at `:1477-1490`**

The handler already destructures `(_drawable, gc_id, text_body)` — change `_drawable` to `drawable_raw` and use it to look up the target. Same gating.

- [ ] **Step 7: Opcode 76 (`ImageText8`) at `:1491-1507`**

The handler already destructures `(drawable, gc_id, text_body)` — use `ResourceId(drawable)` to look up the target. Same gating.

- [ ] **Step 8: Verify build**

Run: `cargo build -p yserver-core && cargo test -p yserver-core`
Expected: builds cleanly; 58 tests still pass.

### Task 5.5: Verify, format, lint

- [ ] **Step 1: Run the full workspace test suite**

Run: `cargo test`
Expected: yserver-core 58, yserver-protocol 15. All pass.

- [ ] **Step 2: Format and lint**

Run: `cargo fmt && cargo clippy -- -W clippy::pedantic`
Expected: clean exit, no warnings. Some `pedantic` lints commonly fire on the new code:
- `clippy::too_many_lines` on `handle_request` (already `#[allow]`-ed — leave it).
- `clippy::cast_sign_loss`/`clippy::cast_possible_wrap` on `x as i32 as u32` in `configure_subwindow` — add `#[allow(clippy::cast_sign_loss, clippy::cast_possible_wrap)]` on the method if so.
- `clippy::similar_names` on `host_xid`/`host_id` — rename `host_id` away if needed; we removed `host_id` in Task 5.4 anyway.

Fix anything else flagged.

### Task 5.6: Manual smoke checklist

Before committing: run a real `ynest` and verify the four spec-listed behaviors. **This is gating** — the new code path was untouched by automated tests, so manual verification is the only end-to-end confirmation.

- [ ] **Step 1: Start `ynest` on a free display**

```sh
cargo build --bin ynest
DISPLAY=:0 ./target/debug/ynest 9 &
```

(Replace `:0` with your real host display and `9` with a free nested-display number.)

- [ ] **Step 2: Run `xeyes` and `xterm` simultaneously**

```sh
DISPLAY=:9 xeyes &
DISPLAY=:9 xterm &
```

Expected:
- Each app opens in its own host subwindow inside the ynest container.
- `xeyes` follows the cursor in real-time (driven by `MotionNotify`, not the timer-driven `QueryPointer` of pre-Phase-1).
- Drawing in `xeyes` does not bleed into `xterm`'s subwindow (and vice-versa).
- Moving the outer host window does not break either app.

- [ ] **Step 3: Run `xev` and verify all five event kinds fire**

```sh
DISPLAY=:9 xev
```

Move the cursor in/out of the `xev` window, click, and confirm `xev` prints:
- `EnterNotify` and `LeaveNotify` on cursor crossings,
- `MotionNotify` while moving,
- `ButtonPress` and `ButtonRelease` on click.

- [ ] **Step 4: Verify cross-window event routing**

Click inside `xterm`'s region — characters appear in `xterm`, *not* `xeyes`. Click inside `xeyes` — eyes track the click without `xterm` reacting.

- [ ] **Step 5: Sanity-check disconnect cleanup**

Close `xeyes` (e.g., `Ctrl-C` in the terminal that launched it). The host subwindow disappears; `xterm` keeps working.

If any step fails, treat it as a bug — debug + fix before committing.

### Task 5.7: Commit

- [ ] **Step 1: Commit**

```bash
git add crates/yserver-core/src/nested.rs crates/yserver-core/src/resources.rs
git commit -m "feat(events): per-window clipping + pointer events"
```

---

## Final verification

- [ ] **Step 1: Confirm test counts**

Run: `cargo test 2>&1 | grep "test result"`
Expected:
- yserver-core: 58 passed (was 49, +9: 6 unit + 1 proptest in `resources::tests`, 2 unit in `server::tests`).
- yserver-protocol: 15 passed (was 9, +6: 5 unit + 1 proptest in `pointer_event_tests`).

- [ ] **Step 2: Confirm five commits landed cleanly**

Run: `git log --oneline -5`
Expected:
```
<sha> feat(events): per-window clipping + pointer events
<sha> feat(host_x11): HostInputPump + xid_map + pointer_event_fanout
<sha> refactor(host_x11): subwindow lifecycle + host_xid arg on draw
<sha> feat(protocol): add pointer + crossing event encoders
<sha> feat(resources): add Window.host_xid and top_level_host_target
```

- [ ] **Step 3: Update `docs/status.md`**

Tick off the per-window clipping + pointer events item on the Phase 1 punch list. Note any new follow-ups discovered during the manual smoke test (e.g., child-window drawing routing, real `Expose` pumping when host obscures a subwindow).
