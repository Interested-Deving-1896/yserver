# Phase 2 wrap-up — opcodes and extensions

Date: 2026-04-30
Status: draft (autonomous; pending user/codex review)

## Purpose

Phase 2's stated goal in `docs/high-level-design.md` is to support
ICCCM/EWMH desktop semantics and run a simple WM (Openbox, i3, awesome,
fluxbox). Today fvwm3 comes up under `ynest`. The remaining gap is the
set of opcodes and extension behavior that the simpler reparenting WMs
exercise that fvwm3 happens not to.

This spec scopes the residual core opcode work and the Phase 2 RANDR /
RENDER / event follow-ups already noted in `docs/status.md` so we can
declare Phase 2 done with at least Openbox or Fluxbox running. i3 and
awesome require XKB and are deliberately deferred to Phase 3.

## Out of scope

- Extensions named Phase 3+ in `high-level-design.md`: BIG-REQUESTS,
  MIT-SHM, XKB, XFIXES, DAMAGE, COMPOSITE, SYNC, PRESENT, SHAPE,
  XInput2, GLX. `QueryExtension` keeps returning "absent" for all of
  them; clients that hard-require any of these are not Phase 2
  validation targets.
- Big-endian client support.
- Full ICCCM/EWMH text-property semantics. The server provides storage
  and event delivery; interpretation lives in the WM.

## Validation targets

- **Primary:** Openbox runs end to end — manages a top-level (xterm or
  xclock), key shortcuts (Alt+F4 close, Alt+drag move, Alt+right-drag
  resize) work, the root menu opens.
- **Secondary:** Fluxbox runs through its splash and shows its menu.
- Existing fvwm3 + xclock/xterm flow still works (regression gate).

## Scope: opcodes and behavior

Grouped by subsystem. Each item lists the wire-level work plus the
state it must touch in `ResourceTable` / `ServerState`.

### A. Keyboard for WMs

The keyboard forwarder (`spawn_keyboard_forwarder` in `nested.rs`)
currently routes every key event to the focused client. WMs need
*passive key grabs* that pre-empt the focus path: when a grabbed
keycode+modifier combination fires, the event must go to the grab
owner instead.

1. **`GrabKey` (33)** — store
   `(window, keycode, modifiers, owner_event, pointer_mode, keyboard_mode)`
   in a per-server `KeyGrab` table. Modifiers may include `AnyModifier`
   (0x8000) and keycode may be `AnyKey` (0); both wildcards must match
   in the lookup.
2. **`UngrabKey` (34)** — remove matching entries.
3. **`GrabKeyboard` (31)** — replace the stub `GrabSuccess` with real
   active-grab tracking; on success, every key event routes to the grab
   owner until `UngrabKeyboard`.
4. **`UngrabKeyboard` (32)** — clear the active keyboard grab.
5. **Routing in the keyboard forwarder** — before falling through to
   the focus path, look up `(keycode, state)` in the `KeyGrab` table
   anchored at the focused window's ancestor chain (X11 semantics:
   any grab on an ancestor of the focused window qualifies). If a
   grab matches, deliver `KeyPress`/`KeyRelease` to the grab owner with
   `event` set to the grab window. If an active keyboard grab exists,
   it overrides everything.
6. **`GetKeyboardMapping` (101)** — replace the hard-coded
   `keysyms.rs` table with a host proxy: `XGetKeyboardMapping` from the
   host server, cache the result, return the requested slice. Falls
   back to the existing table if the host call fails.
7. **`GetModifierMapping` (119)** — proxy to host
   `XGetModifierMapping`, cache, return real reply.
8. **`ChangeKeyboardMapping` (100)** — accept silently and emit a
   `MappingNotify` event (request 0=Modifier, 1=Keyboard, 2=Pointer)
   to all clients. The actual mapping is host-controlled via the
   nested user's session, so this is effectively a refresh signal.

State: new `pub struct KeyGrab` array on `ServerState`; lookup helper
`grab_owner_for_key(focus, keycode, state) -> Option<(ClientId,
ResourceId)>` that walks the focus → ancestor chain.

### B. Window operations for reparenting WMs

Reparenting WMs (Openbox, Fluxbox) call `ChangeSaveSet` on every
managed client window. The semantics: if the WM dies while holding
reparented children, the server reparents those children back to the
root before destroying them.

9. **`ChangeSaveSet` (6)** — store, per client, a
   `HashSet<ResourceId>` of foreign windows the client wants to keep
   alive. `mode=Insert/Delete`. On client disconnect, before resource
   cleanup, reparent each save-set window back to the root and then
   remap it. Honor save-set on `DestroyWindow` of a parent: do not
   destroy save-set children, reparent them to root instead.
10. **`CirculateWindow` (13)** — if the parent has a substructure
    redirect subscriber, emit `CirculateRequest` (event 27) to it and
    do not change stacking. Otherwise reorder children (Top/Bottom)
    and emit `CirculateNotify` (event 26) to subscribers of
    StructureNotify on the window and SubstructureNotify on the
    parent.
11. **`CirculateNotify` / `CirculateRequest` events** — encoders in
    `protocol/x11`; emit hooks in `nested.rs`.
12. **`DestroySubwindows` (5)** — recurse the existing destroy path
    over each child of the target window. Reuse `destroy_window`
    internals.

State: `Client.save_set: HashSet<ResourceId>` on the per-client
state; `ResourceTable::reparent_save_set_to_root(client_id)` helper
called from the disconnect path.

### C. Drawing

13. **`CopyPlane` (63)** — forward to host `XCopyPlane`. Same drawable
    matrix as `CopyArea` (window↔window, pixmap↔window, etc.). Only
    plane=1 is exercised in practice; pass through whatever the client
    sends.

### D. Pointer/grab follow-ups

14. **`ChangeActivePointerGrab` (30)** — update the active pointer
    grab's `event_mask` / `cursor` / `time` if there is one. No-op
    otherwise (no error, X11 spec allows the silent no-op when the
    grab id matches no active grab).
15. **`GrabButton` sync replay (known follow-up)** — replace the
    deferred-replay TODO. Move the replay path to a small command
    queue (`mpsc::Sender<ReplayCmd>`) consumed by the
    `pointer_event_fanout` thread, which already holds `xid_map`.
    `AllowEvents(ReplayPointer)` enqueues the frozen event; the pump
    thread re-routes it through the normal owner-lookup path.

### E. RANDR follow-ups

16. **Host window resize propagation** — the host watcher thread
    already monitors `Closed`; extend it to surface `ConfigureNotify`
    on the host container window. On size change update
    `RandrState { width, height }` and emit `RRScreenChangeNotify`
    (event 0 of RANDR's `first_event`) to clients that have selected
    `RRScreenChangeNotifyMask` via `RRSelectInput`.
17. **`RRSelectInput` mask storage** — accept the mask, store
    `(client_id, window) -> mask` in `RandrState`. Used by item 16.
18. **`RRGetScreenInfo` (RANDR 1.0)** — fluxbox probes legacy RANDR
    1.0 first. Implement as a stub that returns the single mode
    matching the current screen size.

### F. Cross-cutting bugs and known follow-ups

19. **`DestroyWindow` releases bg-pixmap host XIDs** — call
    `XFreePixmap` on `Window.background_pixmap_host_xid` during
    destroy if set. Listed as a known follow-up in `status.md`.
20. **`SendEvent` propagation** — when `event_mask == 0` and the
    destination subscribers don't carry the event, walk up the
    parent chain emitting to each ancestor whose mask covers the
    event type, until an ancestor has the
    "do-not-propagate" bit for that type. Today the impl delivers to
    direct subscribers only.
21. **`UnmapNotify.from_configure = true`** — on parent
    `ConfigureWindow` that shrinks a child out of view, emit the
    implicit unmap with `from_configure=true`. Encoder already
    accepts the byte.

## Architecture

No new modules. The work touches:

- `crates/yserver-protocol/src/x11/mod.rs` — encoders for
  `MappingNotify`, `CirculateNotify`, `CirculateRequest`,
  `RRScreenChangeNotify`; decoders for `ChangeSaveSet`, `GrabKey`,
  `UngrabKey`, `CirculateWindow`, `CopyPlane`, `ChangeActivePointerGrab`,
  `GetKeyboardMapping` proxy result, `GetModifierMapping` proxy result.
- `crates/yserver-core/src/resources.rs` — `Client.save_set`,
  `KeyGrab` table, `ChangeSaveSet` helpers, save-set restore on
  disconnect, bg-pixmap free on destroy.
- `crates/yserver-core/src/host_x11.rs` — host calls for
  `XGetKeyboardMapping`, `XGetModifierMapping`, `XCopyPlane`, host
  `ConfigureNotify` watcher.
- `crates/yserver-core/src/randr.rs` — `RandrState.subscribers`,
  `RRGetScreenInfo`, screen-change emission.
- `crates/yserver-core/src/nested.rs` — dispatch arms for the new
  opcodes, keyboard-forwarder grab lookup, save-set restore in the
  disconnect path, host-resize hook.

## Data flow

**Key grab path (illustrative, item A.5):**

```
HostInputPump::Key event
  → spawn_keyboard_forwarder thread
    → ServerState::lookup_key_grab(focus, keycode, state)
      ├── matches passive GrabKey       → deliver to grab owner+window
      ├── matches active GrabKeyboard   → deliver to grab owner+grab_window
      └── no match                      → existing focus delivery
```

**Save-set on disconnect (item B.9):**

```
client_disconnect(client_id)
  → for w in client.save_set:
      reparent w to root
      remap w if it was mapped under the dying parent
  → resource cleanup (existing path)
```

**Host resize (item E.16):**

```
Host watcher thread sees ConfigureNotify on container
  → RandrState::set_size(w, h)
  → for (cid, win) in subscribers: emit RRScreenChangeNotify
```

## Error handling

- Key grab table lookup is read-only on the hot key path; lock
  contention is bounded by the existing single `ServerState` mutex.
- Save-set restore tolerates already-destroyed windows (race with
  client disconnect): each restore wraps a `resources.lookup` and
  skips on `None`.
- Host proxy calls (`XGetKeyboardMapping` etc.) fall back to the
  existing stub on error so a flaky host doesn't fail the reply.
- New events use the existing `subscribers()` snapshot pattern; no
  new fanout machinery.

## Testing

Per-item: encoder/decoder unit tests in `protocol/x11` (this is the
established pattern — see `write_unmap_notify` tests, etc.). The
`ResourceTable` save-set state machine and `KeyGrab` lookup get
focused unit tests in `resources.rs`.

End-to-end the validation gates are the WM runs listed under
*Validation targets*. We don't have a harness for running a WM in
CI; manual validation under an existing `ynest` session is the bar,
same as previous Phase 2 items.

## Open issues (deliberate)

- We are not implementing XKB. If Openbox has a hard XKB dependency
  on this distro's Xlib build (it shouldn't — Openbox uses Xlib's
  core keymap helpers by default), this gets bumped to Phase 3.
- We are not implementing BIG-REQUESTS. Some Xlib clients call
  `XQueryExtension("BIG-REQUESTS")` early; absent is a valid answer
  and Xlib falls back to 256 KB max request size. If a Phase 2 target
  hits the cap (unlikely for menus and decorations), revisit then.
- Save-set is a per-client `HashSet`. The X11 spec also allows
  save-set entries to outlive a `DestroyWindow` of the *parent* —
  i.e. when the WM destroys a frame, its child should reparent to
  root. Our destroy path will need to call the save-set restore
  before the recursive destroy walk, in addition to the disconnect
  path.
