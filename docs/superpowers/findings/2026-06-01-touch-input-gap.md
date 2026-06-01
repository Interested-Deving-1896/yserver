# Touch input gap on yoga

Date: 2026-06-01

## Summary

On yoga, the current input stack has two distinct gaps:

- `touchscreen` input is not implemented end-to-end.
- `touchpad` motion can reach the server, but the device is still
  exposed as a generic pointer with no touchpad-specific identity or
  properties, so desktops have no reason to treat it as a real
  touchpad.

That matches the observed behaviour:

- touch does nothing.
- tap does not work.
- the desktop does not discover touchpad semantics.

## What exists today

The libinput wrapper already translates only this subset of events:

- keyboard press/release
- relative pointer motion
- absolute pointer motion
- pointer button
- wheel / finger / continuous scroll

See [`crates/yserver/src/input/context.rs`](/home/jos/Projects/yserver/crates/yserver/src/input/context.rs#L272).

The local input event enum only has those same pointer/keyboard forms;
there is no touch-specific variant in [`crates/yserver/src/input/event.rs`](/home/jos/Projects/yserver/crates/yserver/src/input/event.rs#L7).

The KMS backend consumes only:

- `HostInputEvent::PointerMotion`
- `HostInputEvent::PointerButton`
- `HostInputEvent::Key`

See [`crates/yserver/src/kms/v2/backend.rs`](/home/jos/Projects/yserver/crates/yserver/src/kms/v2/backend.rs#L6585).

On the X11 side, the device model is hardcoded to a virtual master pointer,
master keyboard, and one generic slave pointer / keyboard pair. The
XI2 device reply advertises button, valuator, and scroll classes only:
see [`crates/yserver-core/src/core_loop/process_request.rs`](/home/jos/Projects/yserver/crates/yserver-core/src/core_loop/process_request.rs#L8392).

## Why touch fails

Touch is missing at three layers:

1. libinput touch events are not translated into local input events.
2. The backend has no touch event handler.
3. XI2 touch requests and touch delivery are not implemented.

There are comments in the XI2 request path that explicitly call touch
out as unsupported, including `XIAllowEvents` touch modes and
`XIPassiveGrabDevice` `TouchBegin` handling.

## Why touchpad looks like a mouse

The server currently presents the pointer as a generic XI2 device with
mouse-like button/valuator/scroll classes. It does not expose the
device properties desktops usually use to distinguish touchpads from
mice.

The main missing piece here is property discovery:

- `XIGetProperty` returns "not found" unconditionally.
- There is no real `XIListProperties`/property inventory backed by
  device state.
- The server does not retain libinput device classification or expose
  any touchpad-specific capability flags.

As a result, even if libinput knows the device is a touchpad, the X11
clients above it cannot see that fact.

## What it would take

Smallest useful fix for the touchpad side:

- keep libinput device classification when devices are added,
- expose XI2 properties for the master/slave pointer devices,
- return the touchpad-relevant properties desktops inspect,
- emit `XI_DeviceChanged` when those properties become available.

Full touch support is a larger follow-up:

- add touch variants to the local input model,
- translate libinput touch begin/update/end/cancel,
- route those events through the backend,
- implement XI2 touch delivery and the related grab/allow-events
  paths.

## Practical takeaway

If the goal is "make the Yoga laptop usable as expected", the
touchpad metadata/property path is the nearer-term fix. Touchscreen
support is a separate, larger feature.
