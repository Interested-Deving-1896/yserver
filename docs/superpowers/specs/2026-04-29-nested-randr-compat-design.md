# Nested RANDR Compatibility Design

## Context

`ynest` is now far enough along to run simple clients and `xterm`, so the
next compatibility target is running a lightweight window manager. `fvwm3`
complains when the RANDR extension is absent, while `openbox` asks for a
broader desktop stack. For nested mode we do not need real monitor
management yet; we need a plausible RANDR surface that lets clients query
the screen layout and continue startup.

This design is intentionally a compatibility stub. It exposes one connected
output backed by the nested host window and avoids dynamic configuration
until host-window resize is modeled cleanly.

## Goals

- Advertise the `RANDR` extension through `QueryExtension`.
- Implement enough RANDR requests for window managers that only probe the
  current screen layout.
- Model exactly one screen, one output, one CRTC, and one mode for `ynest`.
- Report dimensions matching the current nested screen size.
- Keep mutation/configuration paths safe and predictable without promising
  full RANDR semantics.
- Make the implementation easy to replace with real multi-output state later.

## Non-goals

- Full RANDR 1.5 behavior.
- Multiple outputs, providers, leases, transforms, panning, rotations, or
  output properties.
- Real monitor hotplug.
- Standalone DRM/KMS monitor management.
- DPI policy beyond a simple physical-size estimate.
- Dynamic resize events in the first cut. Those can follow once the nested
  host window resize path is reliable.

## Extension Shape

Add an extension registry entry:

- Name: `RANDR`
- Major opcode: allocated from the extension table.
- First event: fixed server-chosen base.
- First error: fixed server-chosen base.

The extension dispatcher should be explicit rather than pretending RANDR is
part of core protocol handling. This keeps future extensions (`XFIXES`,
`RENDER`, `SHAPE`, `XInput2`) from further bloating the core opcode table.

The first implementation should advertise RANDR version `1.5` or lower. A
conservative option is `1.2`, because the planned request set maps naturally
to XRandR 1.2 objects (`screen resources`, `output`, `crtc`, `mode`) while
avoiding provider/lease APIs from later versions. If a client asks for a
lower version, reply with the minimum supported major/minor as RANDR expects.

## Object Model

For nested mode expose stable synthetic ids:

- Output: `ynest-0`
- CRTC: one CRTC bound to `ynest-0`
- Mode: one active mode matching nested screen dimensions

The ids do not need to be client-allocated XIDs. They are RANDR resource ids
owned by the server. Keep them outside normal client resource validation.

Initial values:

- Width/height: current root/nested screen size.
- Refresh: report 60 Hz using RANDR mode timing fields.
- Output connection: connected.
- Output subpixel order: unknown.
- Output physical size: derive from 96 DPI unless a better nested setting is
  added.
- CRTC position: `(0, 0)`.
- Rotation: normal.
- Possible CRTCs/clones: only the single CRTC/output.

Timestamps:

- `timestamp`: server monotonic time at RANDR state creation or latest update.
- `config_timestamp`: same as `timestamp` for the stub.
- Keep them stable during a run unless screen size changes later.

## Request Support

Required first cut:

- `RRQueryVersion`: reply with supported version.
- `RRGetScreenSizeRange`: min/max equal to current nested size, or max equal
  to a large safe bound if clients expect resizability.
- `RRGetScreenResourcesCurrent`: return one CRTC, one output, and one mode.
- `RRGetOutputInfo`: return `ynest-0`, connected, bound to the single CRTC.
- `RRGetCrtcInfo`: return current size, position, mode, rotation, and output.

Useful compatibility no-ops:

- `RRSelectInput`: store or ignore event masks; reply void. Storing is better
  if resize events are added later.
- `RRGetScreenInfo`: legacy RANDR 1.0 clients may still ask for it. Return a
  single size/rate if observed.

Mutation paths:

- `RRSetScreenConfig`, `RRSetCrtcConfig`, output property requests, provider
  requests, and transform/panning requests should initially return a protocol
  error or a benign unsupported reply, depending on exact RANDR semantics.
- Prefer not to mutate root size from client requests in `ynest` yet. The host
  window owns the nested size.

## Wire Encoding Notes

RANDR is extension-protocol data, so request minor opcodes are selected by
the extension major opcode and request `data` byte. Add typed parsers and
reply encoders in `yserver-protocol::x11::randr` or similar rather than
placing all logic in `nested.rs`.

Important reply details:

- All arrays are 32-bit aligned.
- Mode info uses RANDR's fixed `xRRModeInfo` wire layout.
- Output names are length-prefixed and padded.
- Reply sequence numbers are the core request sequence numbers.
- Unknown RANDR resource ids should produce the RANDR extension's `BadRROutput`,
  `BadRRCrtc`, or `BadRRMode` where practical. For the first cut, `BadValue`
  is acceptable if extension-specific errors are not wired yet, but the
  design should leave room for specific errors.

## Integration Points

- Server setup/root dimensions: use the same source of truth as the root
  window and host container.
- `QueryExtension`: return present for `RANDR`.
- Request dispatcher: route the allocated RANDR major opcode to a RANDR
  handler.
- Resource table: keep RANDR objects in a dedicated display-state struct, not
  in the client-owned resource map.
- Host resize follow-up: when `ynest` handles host `ConfigureNotify`, update
  root dimensions, RANDR mode dimensions, timestamps, and eventually emit
  `RRScreenChangeNotify`.

## Validation Plan

- Unit-test reply sizes and alignment for each implemented reply.
- Unit-test stable one-output object ids and timestamp behavior.
- Run `xrandr -q` against `ynest`; expected output should show one connected
  output with the current nested size.
- Run `fvwm3` far enough to verify RANDR absence is no longer the startup
  blocker.
- Re-test `xeyes`, `xclock`, `xterm`, and `xev` to ensure extension
  registration does not disturb core protocol behavior.

## Follow-ups

- Host-window resize propagation.
- `RRScreenChangeNotify` delivery to clients that selected RANDR events.
- Per-monitor DPI property design once there is more than one monitor concept.
- Multi-output model for standalone DRM/KMS mode.
- Xinerama compatibility if a WM or panel probes both Xinerama and RANDR.
