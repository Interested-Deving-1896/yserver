# Phase 6.5 Step 0: fvwm3 trace diagnosis

## Trace formats

`Xephyr-fvwm3.log` is a full x11trace / xtrace capture (13 807 lines). Every
protocol message appears as `CCC:D:NNNN:SZ: Content`, where `CCC` is the
connection number (000 = fvwm itself, 002–005 = its modules), `D` is `<` for
client-to-server requests and `>` for server-to-client replies/events. Events
carry their symbolic name and numeric type, e.g. `Event ConfigureNotify(22)`.
This log is directly comparable to the X11 spec and contains ground-truth event
delivery counts.

`yserver.log` is yserver's own structured Rust `tracing` log (6 065 lines).
It records requests received from clients (`client N #SEQ OpcodeName ...`) and
backend operations (`MapWindow`, `ReparentWindow`, etc.), but does **not** emit
a line each time an event is synthesised and written into a client's output
buffer. The two logs are therefore **not** directly comparable: Xephyr counts
what was received by clients; yserver counts what was requested by clients. The
closest possible comparison is inferring yserver's event delivery by (a)
counting the request-side operations that trigger synthesis and (b) tracing
through the code to confirm the synthesis path executes.

---

## Event counts

The Xephyr column counts lines of the form `Event X(N)` delivered to any
client.  The yserver column is an **inferred lower bound** from counting
triggering requests in the log (MapWindow, ReparentWindow, etc.) and confirming
in nested.rs that the corresponding `encode_*` path fires.  Exact delivered
counts for yserver are unavailable without adding per-delivery trace points.

| Event class | Xephyr (host) | yserver (KMS) | Notes |
| --- | --- | --- | --- |
| ConfigureNotify (22) | 25 | ≥ 9 (SendEvent) + many server-synthesised | server synthesises at nested.rs:5932,5952 |
| MapNotify (19) | 24 | ≥ 17 (MapWindow ops logged) | synthesised at nested.rs:5615,5635,5699,5717 |
| UnmapNotify (18) | 12 | ≥ 5 (UnmapWindow ops) | synthesised at nested.rs:470,473,5769,5772,5820,5825 |
| ReparentNotify (21) | 21 | ≥ 52 (ReparentWindow ops) | synthesised at nested.rs:5472,5485,5498 |
| CreateNotify (16) | 16 | ≥ 54 (CreateWindow ops, SubstructureNotify filter) | synthesised at nested.rs:4998 |
| DestroyNotify (17) | 3 | ≥ 1 | synthesised at nested.rs:477,480 |
| GravityNotify (24) | 0 | 0 | not implemented; not needed here |
| CirculateNotify (26) | 0 | 0 | implemented via write_circulate_notify_event at nested.rs:6039 |
| FocusIn (9) | 1 | present | synthesised at nested.rs:1080 |
| FocusOut (10) | 0 | 0 | synthesised at nested.rs:1075 |
| PropertyNotify (28) | 113 | many | synthesised at nested.rs:6209,6266,6385 |
| MapRequest (20) | 4 | present | synthesised at nested.rs:5546 |
| ConfigureRequest (23) | 3 | present | synthesised at nested.rs:5859 |

---

## Busy-loop signature

The loop IS present in yserver.log, on **client 4** (FvwmPager module,
base=0x500000). The repeating unit is approximately:

```
client 4 #222 GetInputFocus
client 4 #223 GetInputFocus
client 4 #224 GetGeometry
client 4 #225 ConfigureWindow 0x500026 parent=Some("0x500024") mask=0xf x=Some(-1) y=Some(-1) w=Some(110) h=Some(80) ...
client 4 #226 ConfigureWindow 0x500027 parent=Some("0x500026") mask=0xf x=Some(-1) y=Some(-1) w=Some(110) h=Some(80) ...
client 4 #227 ChangeWindowAttributes
... (draw operations) ...
-- repeats at #256,#257,#258,#259,#260 and again at #275,#276,#277,#278,#279 --
```

The loop appears from request #160 (first `GetInputFocus` pair) and is still
running at the end of the log capture (request #341 at ~12:05:04Z), well after
fvwm has reparented, configured, and mapped 0x500024. The density is roughly
one loop iteration per 28–34 requests. fvwm's `SendEvent type=22 dest=0x500024`
at fvwm requests #4955 and #4983 do not break the loop.

---

## nested.rs synthesis coverage

All event classes delivered by Xephyr during this session have corresponding
`encode_*` call sites in `nested.rs`. No class is completely unimplemented:

| Notify class | Call sites in nested.rs | Protocol-level encoder |
| --- | --- | --- |
| ConfigureNotify | 5932, 5952 | `encode_configure_notify_event` |
| MapNotify | 5615, 5635, 5699, 5717 | `encode_map_notify_event` |
| UnmapNotify | 470, 473, 5769, 5772, 5820, 5825 | `encode_unmap_notify_event` |
| ReparentNotify | 5472, 5485, 5498 | `encode_reparent_notify_event` |
| CreateNotify | 4998 | `encode_create_notify_event` |
| DestroyNotify | 477, 480 | `encode_destroy_notify_event` |
| FocusIn/Out | 1075, 1080 | `encode_focus_event` |
| PropertyNotify | 6209, 6266, 6385 | `encode_property_notify_event` |
| CirculateNotify | 6039 | `write_circulate_notify_event` |
| GravityNotify | — | **not implemented** — but not triggered in this trace |

---

## Diagnosis

### What the loop waits for

The FvwmPager module initialises its widget hierarchy by placing internal
subwindows off-screen at `(-1, -1)`. Its event loop runs a "tick" function
that:
1. Calls `GetInputFocus` (twice, likely a synchronisation barrier).
2. Calls `GetGeometry` on its top-level window (0x500024 / 0x00a00023).
3. If `GetGeometry` returns `x == 0 && y == 0` (i.e., the window has not yet
   been placed at a non-trivial position), it re-runs the widget init: places
   subwindows at `(-1, -1)` and repaints.
4. When `GetGeometry` returns `x != 0 || y != 0` the module considers itself
   "finally placed" and exits the init loop.

### How Xephyr terminates the loop

In the Xephyr trace, fvwm:
1. First reparents `0x00a00023` into a temporary frame `0x00200252` (position
   `(0,0)` — GetGeometry still 0).
2. Then reparents again into the real frame `0x00600001` (120×480).
3. Issues `ConfigureWindow 0x00a00023 x=5 y=75 width=110 height=80` and
   `MapWindow` on the same window.
4. The server synthesises `ConfigureNotify(x=5, y=75)` and `MapNotify` to
   client 004 (which has `StructureNotifyMask` on 0x00a00023).

At request `0x0185`, `GetGeometry` returns `x=5 y=75` — non-zero — and the
module does one final `ConfigureWindow(-1,-1)` pair then stops permanently.

Key: fvwm places the client **directly inside the outer frame** at `(5, 75)`,
meaning the parent-relative position is non-zero.

### Why yserver does not terminate the loop

In yserver, fvwm creates a **two-level frame hierarchy** (because RENDER is
absent; `QueryExtension "RENDER" -> absent` in the yserver log):

```
0x10010d (outer frame, 120×108, root child)
  └─ 0x10010e (client-area inner frame, 110×80, at offset (5,23) in 0x10010d)
       └─ 0x500024 (FvwmPager window, 110×80, at offset (0,0) in 0x10010e)
```

fvwm places `0x500024` at `(0, 0)` within `0x10010e`. `GetGeometry` on
`0x500024` always returns `x=0 y=0` (parent-relative). The module's termination
condition `x != 0 || y != 0` is never true → the loop runs indefinitely.

The ConfigureNotify delivered by nested.rs (lines 5932/5952) when fvwm issues
`ConfigureWindow 0x500024 x=0 y=0` **does reach client 4** — subscriber lookup
finds client 4 with `StructureNotifyMask (0x20000)` on 0x500024. But the event
carries `x=0, y=0`, which the module treats as "still not placed." fvwm's
`SendEvent type=22 dest=0x500024` (#4955, #4983) also carries `x=0, y=0` for
the same reason.

### The known-issues.md hypothesis is inaccurate

The prior entry states: "Likely fix: have `KmsBackend::configure_subwindow`
synthesise a ConfigureNotify." This is incorrect in two ways:

1. `nested.rs` already synthesises ConfigureNotify unconditionally in both
   backends (lines 5926–5962) — it does not rely on the backend for this.
2. Adding more ConfigureNotify events would not help because the problem is the
   *position value* (`x=0, y=0`), not the absence of the event.

The root cause is that without RENDER, fvwm uses a different (deeper) frame
hierarchy, resulting in the client always appearing at `(0,0)` within its direct
parent. The FvwmPager module's "have I been placed?" heuristic then never fires.

---

## Fix shape

**Decision: INSUFFICIENT DATA — lean toward (B) or structural/config**

The analysis cannot determine A/B/C with confidence because:

1. No ynest (`HostX11Backend`) log is available to confirm whether the same
   loop occurs there. If ynest also loops → shape (A) (event never synthesised
   for any backend). If only KMS loops → shape (B) (gate fails on KMS path).
   But from code inspection both backends take the same nested.rs path.

2. The loop termination in Xephyr depends on `GetGeometry` returning a non-zero
   position, which itself depends on **fvwm's frame-creation strategy** — not on
   which events yserver sends. Sending more/different ConfigureNotify events
   will not help unless the position value is changed.

The most likely actual fix directions are:

- **(B-adjacent) Implement RENDER (at least stubs)**: With RENDER present,
  fvwm uses a single-level frame and places the client directly at `(5, y)` in
  it, giving GetGeometry a non-zero result and breaking the loop. This is the
  cleanest behavioural fix.

- **(Workaround) Understand what protocol reply causes fvwm to choose the
  two-level frame hierarchy**: Run `yserver -v` or add more protocol logging to
  identify if some QueryExtension / GetWindowAttributes / GetGeometry reply from
  yserver triggers the deeper frame style.

- **Not recommended**: Synthesising a ConfigureNotify with a fake non-zero
  position — this would be incorrect protocol behaviour and could break other
  clients.

---

## Specific implementation hint for Step 1

Before writing any code, collect a **ynest trace** to determine if the loop
also occurs in the HostX11Backend:

```
DISPLAY=:0 ynest &   # start ynest nested inside real X
DISPLAY=:N fvwm3 &   # start fvwm inside ynest
```

Then capture with xtrace or x11trace and count ConfigureNotify deliveries to
FvwmPager. If the loop terminates in ynest → the fix is in KmsBackend (shape B
or C). If it also loops in ynest → the fix is in nested.rs or RENDER stubs
(shape A or structural).

If ynest also loops, add a TRACE-level log line immediately after
`emit_window_event` at `nested.rs:5927` and `5947` to confirm delivery, then
inspect what `GetGeometry` replies nested.rs returns for 0x500024 and whether
the position could be made non-zero by adjusting the frame placement logic or
by reporting the absolute (root-relative) position in the GetGeometry reply
(which would be protocol-incorrect but worth verifying as a diagnostic).

The smoking-gun for a nested.rs fix would be a ynest log where client 4's
`GetGeometry` reply contains `x=0 y=0` even though the window is visibly
placed at `(5, 23)` on screen — that would confirm the root cause is the
parent-relative coordinate reporting, not a missing event.
