# Phase 3.1 — Toolkit Compatibility Bootstrap Design

## Goal

Get a simple GTK3 application (e.g. `gtk3-demo`) running fully interactively
under ynest: windows render, keyboard input is correct, mouse clicks and basic
scroll work.

## Validation target

`gtk3-demo` (ships with GTK3; exercises widgets, menus, dialogs, text entry)
running under ynest with no WM. Success = app opens, user can type in text
fields, click buttons and open menus, close the window cleanly.

## Scope

Phase 3.1 adds three extensions: **BIG-REQUESTS**, **XKB**, **XInput2 (XI2)**.

XFIXES, DAMAGE, COMPOSITE, MIT-SHM, SHAPE, SYNC, and PRESENT are deferred to
Phase 3.2+. GDK3 degrades gracefully when they are absent; they are not
blocking for the validation target.

---

## Extension 1: BIG-REQUESTS

### Purpose

Xlib queries BIG-REQUESTS at connection open and uses extended-length request
encoding when it is present. Without it, large Cairo image transfers (PutImage
for big windows) silently fail because the standard 16-bit length field
overflows.

### Registration

- Extension name: `"BIG-REQUESTS"`
- Major opcode: 135
- first_event: 0
- first_error: 0

`ListExtensions` (opcode 99) must be updated to include all three Phase 3.1
extensions.

### Per-client state

Add `big_requests_enabled: bool` to `ClientHandle` (default false).

### New request — BigReqEnable (major=135, minor=0, body empty)

1. Set `big_requests_enabled = true` on the requesting client.
2. Reply (32 bytes): standard reply header; `maximum_request_length` at bytes
   8–11 as CARD32 = `0x0003_FFFF` (256 MB).

### Extended request parsing

When `big_requests_enabled` is true, a request whose standard 2-byte length
field equals zero uses the extended format:

```
byte 0:     major opcode
byte 1:     minor / data
bytes 2–3:  0x0000  (signals extended length)
bytes 4–7:  extended_length (CARD32, units = 4-byte words including this header)
bytes 8..:  body = (extended_length − 2) × 4 bytes
```

The request reader in the per-client loop must check: if `big_requests_enabled
&& length == 0`, read 4 more bytes as `extended_length` and set
`body_len = (extended_length − 2) * 4`.

---

## Extension 2: XKB

### Purpose

Xlib auto-calls `XkbQueryExtension` inside `XOpenDisplay`. Without XKB, Xlib
falls back to core keyboard mapping, which is degraded (no multi-level keys,
no dead-key composition, imprecise modifier tracking). GDK3 also calls several
XKB queries directly for keyboard state.

### Strategy: host proxy

Forward XKB requests verbatim to the host X server and relay raw reply bytes
back to the nested client. This is the same pattern already used for
`GetKeyboardMapping` (opcode 101) and `GetModifierMapping` (opcode 119). No
local keymap is maintained.

XKB requests reference the keyboard via `deviceSpec` (core keyboard =
`0x0100`), not via window or pixmap XIDs, so no XID translation is needed.

### Registration

- Extension name: `"XKEYBOARD"`
- Major opcode: 136
- `first_event` and `first_error`: taken from the host's own
  `QueryExtension("XKEYBOARD")` response, so event/error codes stay consistent
  with the host. If the host does not have XKB, ynest does not advertise it.

### Host initialisation (`HostX11`)

At `open_from_env`, send `QueryExtension("XKEYBOARD")` to the host and store
the result:

```rust
struct HostXkbInfo {
    opcode: u8,
    first_event: u8,
    first_error: u8,
}
// Field on HostX11: xkb: Option<HostXkbInfo>
```

### Host proxy function

```rust
fn send_xkb_request(&mut self, minor: u8, body: &[u8]) -> io::Result<Vec<u8>>
```

Builds a request with `opcode = host_xkb_opcode`, `minor`, correct length,
sends to the host stream, drains to the matching sequence number, returns the
raw reply bytes (starting at byte 0 of the host reply, i.e. including the
reply type byte).

### Nested XKB dispatcher

When a client sends a request with major opcode 136, extract `minor` and call
`send_xkb_request(minor, body)`, then write the raw reply to the client.

### Minors proxied

| Minor | Name            | Notes |
|-------|-----------------|-------|
| 0     | UseExtension    | Version negotiation |
| 1     | SelectEvents    | Store subscription mask per client; no XKB events forwarded in Phase 3.1 |
| 4     | GetState        | Current modifier state |
| 8     | GetMap          | Key types + sym map — primary payload Xlib needs |
| 10    | GetCompatMap    | Compat actions |
| 14    | GetIndicatorState | Current LED state |
| 17    | GetNames        | Key/group name atoms |
| 20    | GetControls     | Keyboard control bits |

All minors not in this table return an empty unsupported-minor reply rather
than a hard error, so future additions do not break clients.

### Known limitation — atoms in GetNames

`GetNames` replies contain atom IDs from the host's atom namespace. GDK3/Xlib
uses these internally for keysym-to-name lookups and does not round-trip them
back to ynest as `InternAtom` requests, so in practice this causes no
problems. Noted as a deferred correctness item.

### XKB events (deferred)

`SelectEvents` masks are stored per client but not acted on. XKB state-change
events (e.g. `XkbStateNotify`) are not forwarded in Phase 3.1. This is
sufficient for `gtk3-demo`; key-layout switching and indicator updates are
Phase 3.2.

---

## Extension 3: XInput2 (XI2)

### Purpose

GDK3 replaces `XSelectInput` with `XISelectEvents` once XI2 is present. This
means GDK3 only listens for `GenericEvent` (type=35) packets and ignores core
ButtonPress/KeyPress events. If we advertise XI2 without delivering GenericEvent-
wrapped events, GDK3 receives no input at all. We must both advertise XI2 and
wrap our existing pointer/keyboard events in the XI2 wire format.

### Registration

- Extension name: `"XInputExtension"`
- Major opcode: 137
- first_event: 90 (one past RANDR's 89)
- first_error: allocated next available

### Requests handled

| Minor | Name             | Behaviour |
|-------|------------------|-----------|
| 44    | GetClientPointer | Return master pointer id=2 |
| 45    | SetClientPointer | Accept, reply success, no-op |
| 46    | SelectEvents     | Store per-(window, deviceid) event mask in client state |
| 47    | QueryVersion     | Return major=2, minor=2 |
| 48    | QueryDevice      | Return 2 synthetic master devices |

All other minors return a generic empty reply so unknown XI2 requests do not
block clients.

### XIQueryDevice reply

Two master devices:

| Field         | Device 1               | Device 2                 |
|---------------|------------------------|--------------------------|
| id            | 2                      | 3                        |
| use           | XIMasterPointer (1)    | XIMasterKeyboard (2)     |
| attachment    | 3                      | 2                        |
| name          | "Virtual core pointer" | "Virtual core keyboard"  |
| enabled       | true                   | true                     |
| num_classes   | 0                      | 0                        |

Device IDs 0 (AllDevices) and 1 (AllMasterDevices) are reserved; real devices
start at 2, matching what real X servers return.

### Per-client state

Add to `ClientHandle`:

```rust
xi2_masks: HashMap<(ResourceId, u16), u32>
// key = (window_id, device_id); device_id 0/1 = wildcard (AllDevices / AllMasterDevices)
// value = XI2 event mask bits (XI_KeyPressMask = 1<<2, XI_ButtonPressMask = 1<<4, etc.)
```

`SelectEvents` (minor 46) parses the per-window device mask list and populates
this map. Mask=0 removes the entry.

### XI2 event mask bits

```
XI_DeviceChangedMask  = 1 << 1
XI_KeyPressMask       = 1 << 2
XI_KeyReleaseMask     = 1 << 3
XI_ButtonPressMask    = 1 << 4
XI_ButtonReleaseMask  = 1 << 5
XI_MotionMask         = 1 << 6
XI_EnterMask          = 1 << 7
XI_LeaveMask          = 1 << 8
XI_FocusInMask        = 1 << 9
XI_FocusOutMask       = 1 << 10
```

### GenericEvent wire format (XI_DeviceEvent)

Used for ButtonPress (4), ButtonRelease (5), Motion (6), KeyPress (2),
KeyRelease (3):

```
byte  0:     35  (GenericEvent type)
byte  1:     137 (XI2 major opcode)
bytes 2–3:   sequence number
bytes 4–7:   length = 12  (additional CARD32s beyond the 32-byte base)
bytes 8–9:   evtype
bytes 10–11: deviceid
bytes 12–15: time
bytes 16–19: detail  (keycode or button number)
bytes 20–23: root window id
bytes 24–27: event window id
bytes 28–31: child window id (0 if none)
bytes 32–35: root_x  FP1616  (i16 coord << 16)
bytes 36–39: root_y  FP1616
bytes 40–43: event_x FP1616
bytes 44–47: event_y FP1616
bytes 48–49: buttons_len = 1  (one CARD32 for 32 button bits)
bytes 50–51: valuators_len = 0
bytes 52–53: sourceid  (same as deviceid)
bytes 54–55: flags = 0
bytes 56–59: mods_base
bytes 60–63: mods_latched
bytes 64–67: mods_locked
bytes 68–71: mods_effective  (current tracked modifier state)
bytes 72–75: group (base/latched/locked/effective, all 0 in Phase 3.1)
bytes 76–79: button mask CARD32 = 0
```

Total: 80 bytes. The `length` field = (80 − 32) / 4 = 12.

New encoder: `encode_xi2_device_event` in `yserver-protocol/src/x11/mod.rs`.

### Enter/Leave GenericEvent (XI_Enter=7, XI_Leave=8)

Separate structure. Reuses the device-event header up to `child`, then:

```
bytes 32–35: root_x FP1616
bytes 36–39: root_y FP1616
bytes 40–43: event_x FP1616
bytes 44–47: event_y FP1616
byte  48:    mode   (0=Normal)
byte  49:    detail (0=Ancestor)
byte  50:    same_screen (1)
byte  51:    focus (0)
bytes 52–53: buttons_len = 1
bytes 54–55: padding
bytes 56–75: mods + group (same layout as DeviceEvent)
bytes 76–79: button mask = 0
```

Total: 80 bytes, length=12.

New encoder: `encode_xi2_enter_leave_event`.

### FocusIn/Out GenericEvent (XI_FocusIn=9, XI_FocusOut=10)

Same layout as Enter/Leave but without coordinates (set to zero). Delivered
when `SetInputFocus` changes the focused window.

### Dual-delivery strategy

When an event is ready for window W:

1. **Core path (existing):** clients whose `event_masks[W]` matches the
   relevant core mask receive the 32-byte core event unchanged.
2. **XI2 path (new):** clients whose `xi2_masks[(W, 2)]`, `xi2_masks[(W, 3)]`,
   `xi2_masks[(W, 0)]`, or `xi2_masks[(W, 1)]` match the relevant XI2 mask
   receive the 80-byte GenericEvent.

GDK3 calls `XISelectEvents` but not `XSelectInput`, so it has no entry in
`event_masks` and receives events only via path 2. Core-only clients have no
`xi2_masks` entries and receive events only via path 1. Duplication does not
occur in practice.

### What is deferred to Phase 3.2+

- Scroll valuators (smooth scrolling via XI_Motion with valuator state)
- XI2 raw events (XI_RawMotion, XI_RawButtonPress etc.)
- Device hierarchy events (XI_HierarchyChanged)
- Per-device focus (XI_GetFocus / XI_SetFocus)

---

## File changes summary

| File | Change |
|------|--------|
| `server.rs` | `ClientHandle`: add `big_requests_enabled`, `xi2_masks` |
| `host_x11.rs` | `HostXkbInfo` struct; `xkb: Option<HostXkbInfo>` on `HostX11`; `send_xkb_request`; init XKB at startup |
| `nested.rs` | BIG-REQUESTS extended-length reader; `QueryExtension` + `ListExtensions` updated; XKB dispatch (8 minors, all proxy); XI2 dispatch (5 minors); XI2 dual-delivery in pointer + keyboard fanout paths |
| `yserver-protocol/src/x11/mod.rs` | `encode_xi2_device_event`; `encode_xi2_enter_leave_event` |

---

## Testing

- `gtk3-demo` under ynest: window renders, menus open, text entry works, close button works.
- `xclock`, `xterm`, `xeyes` must continue to work (regression check).
- Manual: type a sentence in a GTK3 text field; verify correct characters including shifted and accented keys.
- Manual: click a menu, navigate with arrow keys, press Enter to activate an item.
