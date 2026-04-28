# Per-Window Clipping + Pointer Events Design

**Goal:** Give each nested top-level window its own X11 subwindow on the
host display, route drawing into the right subwindow, and deliver host
pointer events (`ButtonPress`/`ButtonRelease`/`MotionNotify`/
`EnterNotify`/`LeaveNotify`) back to the matching nested clients via
the existing per-(client, window) event-mask fanout.

**Non-goals** (each ends up in `docs/status.md` known follow-ups or
remains a Phase 2 item):

- **Real `Expose` pumping.** Continue synthesizing on `MapWindow` /
  `MapSubwindows`. If a host window obscures a nested subwindow and
  uncovers it, contents are lost until the client redraws on its own
  schedule. Acceptable for `xeyes`, `xclock`, `xterm`.
- **Descendant hit-testing.** Pointer events deliver to the nested
  top-level only. `child` field is always `None`; `mode` for
  `Enter`/`Leave` is `NotifyNormal` and `detail` is `NotifyAncestor`.
- **Pointer grabs** (`GrabPointer` / `UngrabPointer` / passive grabs).
- **`do not propagate` event-mask.**
- **Virtual crossing events.**
- **Pixmap rendering on host.** `CopyArea` / `PutImage` stay stubs.
- **Multi-screen / multi-host-window models.** One host display, one
  container window, N subwindows.
- **Override-redirect honored on host.** All top-levels become host
  subwindows regardless of `override_redirect`. The flag still
  propagates in `MapNotify`.
- **Host-driven geometry.** yserver remains authoritative; outer-host
  WM resizes are not forwarded as nested `ConfigureNotify`.

**Spec reference:** X11 protocol spec §10 (Pointer Events) + §11
(Crossing Events). Recipient semantics match the existing
`UnmapNotify` / `DestroyNotify` work
([`2026-04-28-unmap-notify-design.md`](2026-04-28-unmap-notify-design.md)).

---

## Architecture

Each nested top-level window (parent == `ROOT_WINDOW`) gets its own
X11 subwindow on the host display. The existing single host window
stays as a "screen container"; subwindows are children of it. yserver
remains authoritative on geometry — nested `CreateWindow` /
`ConfigureWindow` / `Map` / `Unmap` / `Destroy` forward to the matching
host subwindow. Drawing into a top-level routes to its host xid;
drawing into a child nested window routes to the top-level ancestor's
host xid with accumulated `(x, y)` offset.

Pointer events are selected on each new host subwindow with mask
`ButtonPress | ButtonRelease | PointerMotion | EnterWindow |
LeaveWindow`. The existing `HostKeyboard` connection grows into a
`HostInputPump` that reads keys from the container plus pointer
events from the subwindows, and fans them out to subscribed nested
clients via the same `subscribers()` machinery used by `UnmapNotify`
and `DestroyNotify`. Pointer events are delivered to the matching
nested top-level only (no descendant hit-testing, no propagation, no
grabs). Coords arrive from the host already in subwindow-local frame,
which equals nested top-level-local frame, so no translation is
needed for delivery.

---

## Components

### `crates/yserver-core/src/resources.rs`

`Window` gains:

```rust
pub host_xid: Option<u32>,
```

Some only for top-levels that have been created on the host. None
for the root, child nested windows, and top-levels whose host
`CreateWindow` failed.

New helper:

```rust
pub struct TopLevelTarget {
    pub top_level: ResourceId,
    pub host_xid: u32,
    pub x_offset: i16,
    pub y_offset: i16,
}

#[must_use]
pub fn top_level_host_target(&self, id: ResourceId) -> Option<TopLevelTarget>;
```

Walks parents accumulating `(x, y)` offsets via `i16::wrapping_add`
until parent == `ROOT_WINDOW`. The window at that point is the
top-level; return its `host_xid` plus the accumulated offset. Returns
`None` if `id` is the root, the parent chain breaks before reaching
root, or the top-level has no `host_xid`.

### `crates/yserver-core/src/host_x11.rs` — drawing

`HostX11` keeps its existing single window as the "container". The
existing `gc_id` is reused across subwindows; a single GC works
against any drawable on the same screen/depth in X11.

`allocate_xid` (currently private at line 57) becomes `pub`.

Five new methods on `HostX11`:

```rust
pub fn create_subwindow(
    &mut self, host_xid: u32, x: i16, y: i16, width: u16, height: u16,
) -> io::Result<()>;

pub fn destroy_subwindow(&mut self, host_xid: u32) -> io::Result<()>;

pub fn configure_subwindow(
    &mut self, host_xid: u32,
    x: Option<i16>, y: Option<i16>, w: Option<u16>, h: Option<u16>,
) -> io::Result<()>;

pub fn map_subwindow(&mut self, host_xid: u32) -> io::Result<()>;
pub fn unmap_subwindow(&mut self, host_xid: u32) -> io::Result<()>;
```

`create_subwindow` writes a host `CreateWindow` request with parent
= `self.window_id` (container), then issues a `GetGeometry(host_xid)`
round-trip and awaits the reply (cross-connection sync — see Error
handling). Sequence numbers maintained as in existing methods.

`configure_subwindow` writes only the fields that are `Some`, using
the bitmask form X11 expects.

All existing drawing methods grow a leading `host_xid: u32`
parameter; internal `self.window_id` references become the passed
xid:

```rust
pub fn poly_fill_arc(&mut self, host_xid: u32, foreground: u32, arcs: &[u8]) -> io::Result<()>;
pub fn poly_arc(&mut self, host_xid: u32, foreground: u32, arcs: &[u8]) -> io::Result<()>;
pub fn poly_fill_rectangle(&mut self, host_xid: u32, foreground: u32, rectangles: &[u8]) -> io::Result<()>;
pub fn fill_rectangle(&mut self, host_xid: u32, foreground: u32, x: i16, y: i16, w: u16, h: u16) -> io::Result<()>;
pub fn poly_line(&mut self, host_xid: u32, foreground: u32, mode: u8, points: &[u8]) -> io::Result<()>;
pub fn image_text8(&mut self, host_xid: u32, foreground: u32, background: u32, text_len: u8, body: &[u8]) -> io::Result<()>;
pub fn poly_text8(&mut self, host_xid: u32, foreground: u32, body: &[u8]) -> io::Result<()>;
```

The container's `host_xid` is never used for client drawing — it's
just a parent for subwindows. `query_pointer` continues to query
against the container.

### `crates/yserver-core/src/host_x11.rs` — input pump

`HostKeyboard` becomes `HostInputPump`. Splits across two ends of one
host connection:

- A read-side handle consumed by the existing event-pump thread (now
  reads keys + pointer events).
- A `HostInputPumpHandle` (cloneable, owned by the main request
  thread). Methods:
  ```rust
  pub fn register_top_level(&self, nested_id: ResourceId, host_xid: u32) -> io::Result<()>;
  pub fn unregister_top_level(&self, host_xid: u32);
  ```
  `register_top_level` locks a write-clone of the stream, sends
  `ChangeWindowAttributes(host_xid, event_mask = pointer mask)`,
  flushes, then inserts `(host_xid → nested_id)` into a shared
  `Arc<Mutex<HashMap<u32, ResourceId>>>`. `unregister_top_level`
  just removes from the map (host server clears the mask
  automatically when the subwindow is destroyed on the main
  connection).

The shared `xid_map` is also handed to the pump-reading thread so
incoming pointer events can resolve `host_xid` → `nested_id` without
a channel round-trip.

`HostEvent` gains a `Pointer` variant:

```rust
pub enum HostEvent {
    Key(HostKeyEvent),
    Pointer(HostPointerEvent),
    Closed,
}

pub enum PointerEventKind {
    ButtonPress, ButtonRelease, MotionNotify, EnterNotify, LeaveNotify,
}

pub struct HostPointerEvent {
    pub kind: PointerEventKind,
    pub host_xid: u32,
    pub detail: u8,    // button number (press/release); 0 (motion); ignored (crossing)
    pub time: u32,
    pub root_x: i16, pub root_y: i16,
    pub event_x: i16, pub event_y: i16,
    pub state: u16,
}
```

Pointer mask bits (from the X11 protocol):
- `ButtonPress` = `0x0000_0004`
- `ButtonRelease` = `0x0000_0008`
- `EnterWindow` = `0x0000_0010`
- `LeaveWindow` = `0x0000_0020`
- `PointerMotion` = `0x0000_0040`

### `crates/yserver-core/src/server.rs`

New helper:

```rust
pub fn pointer_event_fanout(
    state: &Mutex<ServerState>,
    xid_map: &Arc<Mutex<HashMap<u32, ResourceId>>>,
    event: HostPointerEvent,
);
```

1. Resolve `event.host_xid` → `nested_id` via `xid_map`. If absent,
   drop.
2. Pick the matching mask bit from `event.kind`.
3. `subs = state.subscribers(nested_id, mask_bit)`.
4. For each `target`, encode the matching nested event with
   `event_window = nested_id`, `child = None`, `root_x/y` and
   `event_x/y` from the host event, `state` from the host event,
   `time` from the host event. Crossing events use
   `mode = NotifyNormal`, `detail = NotifyAncestor`,
   `same_screen_focus` = `0x01 | 0x02` (focus + same screen).
5. Fan out via the existing `fanout_event(...)` helper.

### `crates/yserver-core/src/nested.rs`

Opcodes that touch top-level windows grow a host call when the
affected window has `host_xid = Some(_)`:

- **Opcode 1 `CreateWindow`** (when `request.parent == ROOT_WINDOW`):
  `resources.create_window(...)` under the server lock as today,
  then drop the lock before any host I/O. Allocate
  `host_xid = host.allocate_xid()`, call `host.create_subwindow(...)`,
  and on success re-acquire the server lock to set
  `windows[id].host_xid = Some(host_xid)` and call
  `input_handle.register_top_level(id, host_xid)`. If
  `create_subwindow` fails, leave `host_xid = None`; the nested
  window exists but draws silently drop and pointer events never
  fire — clean degraded mode.
- **Opcode 4 `DestroyWindow`**: extend the existing `pending`
  snapshot (see `2026-04-28-unmap-notify-design.md`) to also carry
  the `host_xid: Option<u32>` per destroyed window, captured under
  the server lock alongside `parent`/`was_mapped`/subscribers. After
  the lock drops, for each entry with `Some(host_xid)`, call
  `host.destroy_subwindow` and `input_handle.unregister_top_level`
  before the existing `UnmapNotify`/`DestroyNotify` fanouts. Host
  I/O stays out of the lock.
- **Opcode 8 `MapWindow`** (top-level): call `host.map_subwindow`.
- **Opcode 9 `MapSubwindows`** (parent == `ROOT_WINDOW`): for each
  child with `host_xid`, `host.map_subwindow`.
- **Opcode 10 `UnmapWindow`** (top-level): call `host.unmap_subwindow`.
- **Opcode 12 `ConfigureWindow`** (top-level): call
  `host.configure_subwindow` with the request's `Option<i16>`/
  `Option<u16>` fields.
- **Disconnect cleanup**: same as opcode 4, applied to each owned root
  collected by `collect_owned_window_roots`.

Drawing handlers — `PolyArc`, `PolyLine`, `PolyFillArc`,
`PolyFillRectangle`, `ClearArea` (uses `fill_rectangle`),
`ImageText8`, `PolyText8` — all gain the same prelude:

```rust
let target = {
    let s = lock_server(server)?;
    s.resources.top_level_host_target(request.drawable)
};
let Some(target) = target else { return ok };
// translate request coords by (target.x_offset, target.y_offset)
host.poly_<op>(target.host_xid, /* translated args */)?;
```

Handler integration adapter (new): the existing pump thread reads
events via `HostInputPump::read_event()`. The adapter loop dispatches:

- `HostEvent::Key(_)`: same as today (existing keyboard forwarding
  into the focused nested window).
- `HostEvent::Pointer(p)`: call
  `server::pointer_event_fanout(&server, &xid_map, p)`.
- `HostEvent::Closed`: same as today.

### `crates/yserver-protocol/src/x11/mod.rs`

Five new event encoders, each 32 bytes, mirroring the shape of
`encode_key_event` (KeyPress/KeyRelease, events 2 and 3):

```rust
pub fn encode_button_press_event(...)    // event 4
pub fn encode_button_release_event(...)  // event 5
pub fn encode_motion_notify_event(...)   // event 6
pub fn encode_enter_notify_event(...)    // event 7
pub fn encode_leave_notify_event(...)    // event 8
```

Wire format for ButtonPress/ButtonRelease/MotionNotify:

```
1   <event_code>     (4/5/6)
1   detail           (button number; 0 for motion)
2   sequence
4   time
4   root
4   event
4   child            (always 0 — no descendant hit-testing)
2   root_x
2   root_y
2   event_x
2   event_y
2   state
1   same_screen      (1)
1   pad
```

Wire format for EnterNotify/LeaveNotify:

```
1   <event_code>     (7/8)
1   detail           (NotifyAncestor = 0)
2   sequence
4   time
4   root
4   event
4   child            (always 0)
2   root_x
2   root_y
2   event_x
2   event_y
2   state
1   mode             (NotifyNormal = 0)
1   same_screen,focus  (bit 0 = focus, bit 1 = same_screen → 0x03)
```

Total 32 bytes per encoder.

---

## Data flow

### Top-level lifecycle

```
CreateWindow opcode 1 (parent == ROOT_WINDOW):
  {
    lock_server
    resources.create_window(...)            // existing
    unlock_server
  }
  host_xid = host.allocate_xid()
  if host.create_subwindow(host_xid, x, y, w, h).is_ok():
    // GetGeometry round-trip happens inside create_subwindow
    {
      lock_server
      windows[id].host_xid = Some(host_xid)
      unlock_server
    }
    let _ = input_handle.register_top_level(id, host_xid);
  // else: host_xid stays None on the Window; draws + pointer events
  // silently no-op (degraded but stable)

ConfigureWindow opcode 12 (target is top-level):
  resources.configure_window(...)
  host.configure_subwindow(host_xid, x?, y?, w?, h?)

MapWindow opcode 8 (target is top-level):
  resources.map_window(...)
  host.map_subwindow(host_xid)
  // existing MapNotify + Expose synthesis as today

UnmapWindow opcode 10 (target is top-level):
  was_mapped = resources.unmap_window(...)
  host.unmap_subwindow(host_xid)
  // UnmapNotify fanout (already shipped)

DestroyWindow opcode 4:
  pending = {
    lock_server
    collect_destroy_order(root) → order
    for w in order:
      capture (w, parent, was_mapped, host_xid, subs_w, subs_p)
    resources.destroy_window(root)          // existing
    drop_window_subscriptions(&order)
    return pending
    unlock_server
  }
  for entry in pending with host_xid = Some(x):
    host.destroy_subwindow(x)
    input_handle.unregister_top_level(x)
  // existing UnmapNotify (if was_mapped) + DestroyNotify fanouts

Disconnect cleanup: same as DestroyWindow per owned root.
```

### Draw routing (every drawing opcode)

```
target = top_level_host_target(request.drawable)
  if None: drop (no host call)
  else:
    translate: (x, y) → (x + target.x_offset, y + target.y_offset)
    host.poly_<op>(target.host_xid, /* translated coords */, ...)
```

For top-level drawables, `x_offset == y_offset == 0`. For child
windows, parent offsets accumulate (with `i16::wrapping_add`).

### Pointer event delivery

```
HostInputPump thread reads event 4/5/6/7/8 from host stream:
  build HostPointerEvent { kind, host_xid, detail, time,
                           root_x/y, event_x/y, state }
  emit on the existing pump→main channel

Pump adapter (main thread or separate thread):
  pointer_event_fanout(state, xid_map, event)
    nested_id = xid_map.lock().get(event.host_xid)
      if None: drop
    mask_bit = match event.kind {
      ButtonPress    => 0x0000_0004,
      ButtonRelease  => 0x0000_0008,
      MotionNotify   => 0x0000_0040,
      EnterNotify    => 0x0000_0010,
      LeaveNotify    => 0x0000_0020,
    }
    subs = state.subscribers(nested_id, mask_bit)
    for target in subs:
      encode <kind> event with:
        event_window = nested_id
        child        = NONE
        root_x/y, event_x/y, state, time = from host event
      fanout_event helper writes to target.writer
```

Coords don't need translation: the host event's `event_x` / `event_y`
are already in the subwindow's local frame, which equals the nested
top-level's local frame. `child` is always `None`.

---

## Error handling

| Condition | Behavior |
|---|---|
| Host I/O fails on `create/destroy/configure/map/unmap_subwindow` | `io::Error` propagates. Same as today's drawing failures. |
| `top_level_host_target` returns `None` | Drop the host call. Matches today's behavior for non-window drawables. |
| Pointer event arrives with `host_xid` not in `xid_map` | Drop event silently. Recipient already saw `DestroyNotify`. |
| `register_top_level` `ChangeWindowAttributes` fails | Log + leave the entry out of `xid_map`. The subwindow exists but won't deliver pointer events. Subsequent draws still work. |
| Top-level created but never mapped | Host subwindow exists in unmapped state; host drops draws into it (X11 spec); no pointer events fire. |
| Disconnect cleanup runs concurrently with an in-flight pointer event | `xid_map` is `Arc<Mutex<…>>`; cleanup removes the entry; in-flight dispatch sees `None` and drops. No panic. |
| Top-level `CreateWindow` succeeds but `register_top_level` fails | `host_xid` set, no pointer mask. Draws work; pointer events dropped. Acceptable degraded mode. |

**Cross-connection ordering hazard.** `CreateWindow` lands on the
`HostX11` stream (used for drawing). `ChangeWindowAttributes` for
pointer-event selection lands on the `HostInputPump` stream. X11
guarantees order *within* a connection, not across two. If the host
processes the pump's `ChangeWindowAttributes` before the main
connection's `CreateWindow`, the pump gets `BadWindow` and events
never start flowing.

Resolution: `host.create_subwindow(...)` issues a `GetGeometry(host_xid)`
round-trip on the main stream and awaits the reply before returning.
This forces the host to process `CreateWindow` before any subsequent
`ChangeWindowAttributes` from the pump's stream. One extra round-trip
per top-level creation; acceptable for Phase 1 app scale.

---

## Testing

### Unit tests

`crates/yserver-core/src/resources.rs::tests`:

1. `top_level_host_target_for_top_level_returns_self` — top-level
   with `host_xid = Some(X)` returns `(X, 0, 0)`.
2. `top_level_host_target_for_child_accumulates_offset` — child at
   (10, 20) inside top-level returns `(X, 10, 20)`.
3. `top_level_host_target_for_grandchild_sums_offsets` — grandchild
   at (5, 5) inside child at (10, 20) returns `(X, 15, 25)`.
4. `top_level_host_target_returns_none_for_root` — root has no
   top-level.
5. `top_level_host_target_returns_none_when_top_level_has_no_host_xid`
   — top-level with `host_xid = None`.
6. `top_level_host_target_returns_none_for_orphaned_window` — parent
   chain breaks before reaching root.

`crates/yserver-protocol/src/x11/mod.rs::tests` (new
`pointer_event_tests` submodule):

7. `button_press_event::shape` — event 4, 32 bytes, fields at
   correct offsets.
8. `button_release_event::shape` — event 5.
9. `motion_notify_event::shape` — event 6.
10. `enter_notify_event::shape` — event 7, mode=0, same_screen_focus=0x03.
11. `leave_notify_event::shape` — event 8.

`crates/yserver-core/src/server.rs::tests`:

12. `pointer_event_fanout_filters_by_mask` — three clients with
    different masks (ButtonPress, MotionNotify, none) on the same
    nested window; emit a `HostPointerEvent::ButtonPress`; assert
    only the matching subscriber's writer receives 32 bytes with
    byte 0 == 4.
13. `pointer_event_fanout_drops_unknown_host_xid` — `xid_map` empty;
    emit a pointer event; assert no writer receives anything.

### Property tests

`crates/yserver-core/src/resources.rs::tests`:

14. `top_level_host_target_offset_proptest` (proptest) — generate a
    parent chain of `n in 1..=8` windows with random `i16` offsets;
    walk from leaf to top-level; assert the helper returns
    `(top_level_host_xid, sum_x, sum_y)` where sums use
    `i16::wrapping_add`. Catches off-by-one in the recursion and
    verifies the documented overflow behavior.

`crates/yserver-protocol/src/x11/mod.rs::tests::pointer_event_tests`:

15. `pointer_encoder_round_trip` (proptest) — for each of the five
    encoders, with arbitrary `(sequence, detail, time, root, event,
    child, root_x/y, event_x/y, state, byte_order ∈ {LE, BE})`:
    - Buffer length is 32.
    - Byte 0 = event code (4/5/6/7/8).
    - Sequence at bytes 2..4 in chosen order.
    - Time at 4..8, root at 8..12, event at 12..16, child at 16..20
      (all 0 for `child`).
    - root_x/y at 20..24, event_x/y at 24..28, state at 28..30.
    - Padding bytes are zero.

### Test plan limitations

Handler-level integration (opcode 1 with `parent == ROOT_WINDOW`
→ `host.create_subwindow` → `register_top_level`, opcode 4 →
`destroy_subwindow`, etc.) is not covered by automated tests. Same
gap the `UnmapNotify` spec already deferred — would require a mock
host X server. End-to-end correctness is verified by the manual
smoke test below.

### Manual smoke tests (run before declaring complete)

- `xeyes` and `xterm` simultaneously inside ynest. Each renders into
  its own subwindow; no bleed across them. Move/resize the outer
  host window — nested apps continue working.
- `xev` shows `ButtonPress`, `ButtonRelease`, `MotionNotify`,
  `EnterNotify`, `LeaveNotify` when interacting with its window.
- `xeyes` follows the cursor in real-time (today it animates only
  via timer-driven `QueryPointer`; with `MotionNotify` it should
  respond to motion).
- `xeyes` clicked through to `xterm`: clicks in xterm region reach
  xterm only, not xeyes.

### Expected counts

| Crate              | Before | After |
|--------------------|--------|-------|
| `yserver-core`     | 49     | 58    |
| `yserver-protocol` | 9      | 15    |
| **Total**          | **58** | **73** |

(15 new tests total, numbered 1–15 above. Distribution: 9 added in
`yserver-core` — 6 unit + 1 proptest in `resources::tests`, 2 unit
in `server::tests`. 6 added in `yserver-protocol` — 5 unit + 1
proptest in `pointer_event_tests`.)

---

## Implementation staging

Suggest five commits in this order; each compiles, passes its tests,
and ends with `cargo fmt`, `cargo clippy -- -W clippy::pedantic`,
`cargo test`:

1. **Add `Window.host_xid` + `top_level_host_target` helper + tests**
   in `yserver-core::resources`. Pure data + helper. Tests 1–6, 14.

2. **Add five pointer-event encoders + tests** in
   `yserver-protocol`. Pure addition. Tests 7–11, 15.

3. **Refactor `HostX11` drawing methods to take `host_xid`** +
   add the five subwindow-lifecycle methods (with `GetGeometry`
   round-trip in `create_subwindow`). Update every drawing call
   site in `nested.rs` to pass `host.window_id()` (the existing
   `pub` accessor at line 158) as the `host_xid` argument until
   commit 5 wires real routing. No test changes — behavior is
   identical to today. Mostly mechanical signature shuffle.

4. **Add `HostInputPump` + `HostInputPumpHandle` + `xid_map` +
   `pointer_event_fanout`** in `host_x11.rs` and `server.rs`.
   Wire the pump's read loop to dispatch `HostEvent::Pointer`.
   The `xid_map` starts empty; nothing registers yet. Tests 12, 13.

5. **Wire opcodes 1, 4, 8, 9, 10, 12 + disconnect cleanup + every
   drawing handler** to the host subwindow APIs and the
   `register_top_level` / `unregister_top_level` calls. Replace the
   commit-3 placeholder of `host_xid = self.window_id` with real
   `top_level_host_target(...)` resolution per draw. Run the manual
   smoke checklist.

Each commit ships behind `cargo test` green; behavior visibly
changes only at commit 5, when nested top-levels start opening as
real host subwindows and pointer events start flowing.
