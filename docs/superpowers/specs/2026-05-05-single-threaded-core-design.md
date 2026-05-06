# Single-threaded core — design

## Motivation

Two pieces of state — `ServerState` and the `Backend` (`KmsBackend` /
`HostX11Backend`) — both sit behind `Arc<Mutex<…>>` and are touched by
several threads (per-client request handlers, libinput input thread,
DRM page-flip handler, host-X11 pump). Different paths take the locks
in different orders:

- **Input / DRM / host-X11 pump threads** want backend state first
  (cursor / window geometry / coord translation), then need to deliver
  through `pointer_event_fanout`, which acquires `server.lock()`.
- **Per-client request handlers** want server state first (resource
  lookup, grab tables, atom tables), then need to call into the backend
  for rendering / window ops.

Two locks, taken in opposite directions. Phase 6.6 fixed one inversion
(commit `6f7754b`). Phase 6.7 added crossing-event emission, which
tripled per-motion fanout calls and surfaced a second latent inversion
as a routinely-reproducible e16 freeze. The current state — buffering
pointer events in `pending_pointer_events` and dispatching after the
backend mutex is dropped — papers over the symptom but leaves the
deadlock class intact for any future code path that takes
server-then-backend.

This design eliminates the deadlock class entirely by making the core
single-threaded. All other threads are I/O-only and communicate via a
single `mpsc` channel.

## Architecture

One **core thread** owns:

- `state: ServerState` (no `Arc`, no `Mutex`). Per-client bookkeeping
  continues to live under `state.clients` — it is X11 server state,
  not core-orchestration state. `ClientState` collapses
  `ServerState::clients` and `ClientHandle` into one struct holding
  the `UnixStream` write-half, byte order, sequence counter as plain
  `u16` (no `AtomicU16`), and event masks.
- `backend: Box<dyn Backend>` — `KmsBackend` or `HostX11Backend`,
  with all internal `Mutex`/`Arc<Mutex>` over its own fields stripped.

Other threads, all I/O-only:

- **Listener thread**: accepts new connections on the Unix socket,
  splits each `UnixStream` (via `try_clone`), spawns a reader thread
  for it, sends `Message::ClientConnected{ id, writer }` carrying the
  write-half.
- **N per-client reader threads** (one per connected client): blocking
  reads request frames off the read-half, sends
  `Message::Request{ id, header, body }`.
- **Input thread** (KMS only): polls libinput fd, sends
  `Message::HostInput(InputEvent)`.
- **DRM thread** (KMS only): polls drm fd, sends
  `Message::PageFlipReady`.
- **Signal thread**: polls signalfd, sends `Message::Shutdown` on
  SIGINT/SIGTERM.
- **Host-X11 pump thread** (ynest only): replaces `host_x11/pump.rs`'s
  thread; sends `Message::HostX11(HostEvent)`.

All I/O threads share one `mpsc::Sender<Message>`. The core holds the
single `Receiver<Message>` and runs:

```rust
loop {
    match rx.recv() {
        Ok(Message::ClientConnected{id, writer}) => …,
        Ok(Message::Request{id, header, body}) => process_request(…),
        Ok(Message::HostInput(ev))             => process_host_input(…),
        Ok(Message::PageFlipReady)             => backend.on_page_flip_ready(),
        Ok(Message::ClientDisconnected{id, …}) => state.clients.remove(&id),
        Ok(Message::Shutdown) | Err(_)         => break,
    }
}
```

Replies and events flow back via the per-client write-half stored on
the core. Single-threaded ⇒ no mutex on the writer. The X11 wire is
inherently asynchronous (clients demux replies/events by sequence
number and event type), so reader threads never block on the core —
they parse the next request immediately after sending the previous
one.

### Slow-client backpressure

The cost of a single core thread is that one slow reader can stall
*everything* — request processing, input dispatch, page-flip
acknowledgements, disconnect handling. The chosen mitigation:

- **Per-client write side is non-blocking** (`set_nonblocking(true)`
  on each `UnixStream` write-half).
- The core attempts a direct write. If it returns `EAGAIN`, the bytes
  are appended to a per-client outbound `VecDeque<u8>` (a small
  buffer, e.g. 64 KiB cap).
- The core's main poller (we add the listener via `epoll`/`mio` and
  fold the message channel into it through an
  `EventFd`/`UnixDatagram` wakeup, OR just use `mio` end-to-end)
  watches each client's write fd for `EPOLLOUT` while its outbound
  buffer is non-empty. On `EPOLLOUT`, drain as much as possible.
- If the buffer would exceed the cap, **disconnect the client**
  (server-initiated drop with `KillClient`-equivalent state cleanup).
  Better than wedging the whole server on a misbehaving peer.

This means the main loop must poll fds, not just `rx.recv()`. The
`Message` enum is delivered into the same poller via an internal
notify fd. Core loop sketch becomes a poll-then-dispatch instead of
a pure `rx.recv()` blocking call (see Migration step 1).

## Message enum

```rust
pub enum Message {
    ClientConnected { id: ClientId, writer: UnixStream },
    Request         { id: ClientId, header: RequestHeader, body: Vec<u8> },
    ClientDisconnected { id: ClientId, reason: io::Error },
    HostInput(HostInputEvent),
    HostX11Other(HostEvent),
    PageFlipReady,
    Shutdown,
}
```

Backends post events to the same channel rather than going through a
`BackendEventSink` callback. Removes one layer of indirection.

## What goes away

- `Arc<Mutex<ServerState>>` everywhere
- `Arc<Mutex<dyn Backend>>` everywhere
- `lock_server()` helper
- All `host.lock()?` calls in `nested.rs`
- `ClientHandle::writer: Arc<Mutex<UnixStream>>` mutex (just a `UnixStream`)
- `ClientHandle::last_sequence: Arc<AtomicU16>` (just `u16`)
- The per-client keyboard-forwarder thread in `nested.rs` — key events
  flow through `Message::HostInput` like everything else; the core
  fans them out to clients
- `BackendEventSink` trait (replaced by direct `Sender<Message>`)
- `KmsBackend::pending_pointer_events` and
  `drain_pending_pointer_events` — the deadlock-workaround we just
  added becomes unnecessary

## Migration order (single landing)

Compile breakage spans the whole `yserver-core` crate during the
refactor. Accepted as the cost of doing this in one PR.

1. **Settle `ClientState`.** Collapse `ServerState::clients` and
   `ClientHandle` into a single `state.clients: HashMap<ClientId,
   ClientState>` with non-`Arc`/`Mutex` fields. This must precede the
   `process_request` lift so handler code knows which struct to read
   from.
2. Add `Message` enum + `Sender<Message>` plumbing; stub
   `run_core(state, backend, poller, rx)` looping on the poller doing
   nothing yet. Wire the poller (mio or epoll) for the listener fd,
   client write-readiness, the message channel's notify fd, signalfd,
   and any backend fds (libinput, drm, host-X11).
3. Replace per-client `handle_client` with
   `client_reader_thread(stream, id, tx)`. Reader parses one request
   frame at a time, sends `Message::Request`. `ClientConnected`
   carries the write-half (set non-blocking).
4. Lift `handle_client`'s opcode `match` into a free
   `process_request(&mut ServerState, &mut dyn Backend, id, header, body)`.
   Mechanically rewrite every `lock_server(server)?` → `state` and
   every `host.lock()?` → `backend`. Fanout helpers
   (`pointer_event_fanout`, `expose_event_fanout`, …) flip to
   `&mut ServerState` in the same step — they have to, because their
   call sites are in `process_request`. Steps 4 and 6 in the previous
   ordering are coupled and land together. **Bulk of the diff** —
   hundreds of edit sites in `nested.rs`.
5. Convert KMS input / DRM / signal threads to senders. Strip
   `Arc<Mutex>` from `KmsBackend` field accesses. Delete
   `pending_pointer_events`, `process_input_events`,
   `BackendEventSink` impl on `KmsBackend`, and the synthesize_expose
   sink-dispatch (replaced by direct fanout on the core).
6. Convert `host_x11::pump` to a sender for ynest. Strip `Arc<Mutex>`
   from `HostX11Backend`. Same `BackendEventSink` deletion.
7. Replace the implicit-grab crossing emission with the spec-correct
   gate + path-walk. Delete the unconditional emission in
   `process_pointer_button`.
8. Delete the per-client keyboard-forwarder thread.
9. Delete `lock_server`, all `Arc<Mutex<ServerState>>` /
   `Arc<Mutex<dyn Backend>>` typedefs, and the `BackendEventSink`
   trait. Adjust tests to build `ServerState` directly.

## Testing

- The 249 `yserver-core` unit tests must keep passing throughout.
  Most just construct a `ServerState` and call into handlers, so the
  signature change is mechanical for them.
- Smoke matrix after the refactor:
  - ynest: fvwm3, wmaker, e16, GTK3 apps
  - yserver: fvwm3, wmaker, e16
  Same WMs that work today must still work.
- Stress test for the deadlock: extended e16 session with rapid clicks
  across multiple windows, the exact pattern that triggered the
  recent freeze. Must run for ≥5 minutes without locking up.
- New unit tests for implicit-grab crossings: pre-grab target
  `==` grab-window (no crossings), pre-grab target in disjoint
  subtree (Leave path from pre-grab up to common ancestor + Enter
  path from common ancestor down to grab-window), and the
  ancestor/descendant case where one is a parent of the other (path
  walk through the common ancestor between them).
- New unit test for slow-client backpressure: simulate a client whose
  socket is full; verify `core` doesn't block, the outbound buffer
  fills, and the client is disconnected when the buffer cap is
  exceeded.

## Risk

Step 3 — the `process_request` lift — is the bulk of the work and
will leave the workspace in a non-compiling state for some time. Once
it compiles end-to-end, the unit-test suite either tells us we got it
right or there's a long debug tail. Estimate 2-3 days of focused work
for the lift, plus 1-2 days for the surrounding migration steps and
smoke validation.

## Pre-existing bugs the refactor must not regress

A codex review of the buffered-dispatch workaround surfaced three
issues that we explicitly chose **not** to patch separately, on the
basis that the single-threaded core obsoletes all of them. The
refactor must demonstrably handle each.

1. **Expose-side lock inversion still alive.**
   `KmsBackend::synthesize_expose` (`crates/yserver/src/kms/backend.rs:1371`)
   calls `sink.handle_backend_event(...)` while the backend mutex is
   held. It is reached from `destroy_subwindow` / `map_subwindow` /
   `unmap_subwindow` / `configure_subwindow` (lines 2377, 2388, 2419,
   2481), all of which run with the backend mutex held by request-
   handler threads. The sink immediately re-enters
   `expose_event_fanout`, which takes `server.lock()`. This is the
   same backend↔server inversion the pointer buffer fixed, just on a
   different code path.
   **Refactor obligation:** in the single-threaded core, expose
   synthesis runs on the core thread with `&mut ServerState` directly
   in scope. No mutex involved. The class of bug disappears. The
   refactor's smoke matrix must include WM scenarios that stress
   destroy/map/unmap/configure (e16 menus, fvwm popups,
   wmaker dock manipulation).

2. **Implicit-grab crossings emit unconditionally on KMS.**
   `process_pointer_button` (`crates/yserver/src/kms/backend.rs:1656`)
   emits `EnterNotify(NotifyGrab)` on press and
   `LeaveNotify(NotifyUngrab)` on release for the press window
   regardless of where the pre-grab event target was. Per X11 spec
   these crossings only fire when the pre-grab pointer-window
   (the deepest window the pointer is in, after `pointer_target_at`
   + propagation — `crates/yserver-core/src/server.rs:566`,
   `:1236-1247`) differs from the grab-window. For the common case of
   a click inside one window — pre-grab target == grab-window — no
   crossings should fire. The current behaviour produces a spurious
   enter/leave with no pointer movement, and the release-side
   synthetic Leave can make clients think the pointer left the window
   until the next motion event.
   **Refactor obligation:** the new core has direct access to the
   pre-grab pointer target (already computed by `pointer_target_at`
   + `pointer_propagation_target` for the press's normal delivery
   path), so it can gate correctly: suppress NotifyGrab crossings
   when `pre-grab pointer-window == grab_window`, and analogous
   suppression for NotifyUngrab on release. The interesting case is
   ancestor↔descendant: pointer-window in the grab-window's subtree
   (or vice versa) — that is exactly where the spec-correct path-walk
   is required (Leave on each window from pre-grab target up to the
   common ancestor, Enter on each window from common ancestor down
   to the grab-window). The over-eager emission was a deliberate
   shortcut to unblock e16; the refactor must replace it with the
   correct algorithm. Tests must cover three cases: equal
   (suppressed), unrelated subtrees (full crossings on the path), and
   ancestor/descendant (path-walk through the common ancestor).

3. **`process_input_events` is dead-and-wrong code.**
   `crates/yserver/src/kms/backend.rs:1455` still routes pointer
   events through `emit_pointer`, which only buffers. The dedicated
   input thread is the only drain caller; if anyone reactivates this
   path KMS pointer events will silently stop reaching clients.
   **Refactor obligation:** `process_input_events` and the polling-
   based input dispatch it implements both go away. Input events flow
   exclusively through the input thread → channel → core path. Delete
   the stale function as part of step 4 of the migration.

## Why the alternatives were rejected

- **Strict lock-ordering discipline (backend-before-server, audited)**
  was rejected because the rule has to be hand-enforced — clippy
  can't catch a violation. Every new piece of input or request
  handling is a deadlock-in-waiting until the audit, and the audit
  isn't a permanent fix.
- **Merging the two mutexes** was rejected because it throws away the
  parallelism the input thread was added for (commit `877a399`),
  without making the type story significantly cleaner.
- **Staged refactor** was rejected because stages 2-4 each contain
  their own internal "big-bang" — once you start lifting
  `lock_server(server)?` → `server` across `nested.rs`, you can't
  stop halfway. The intermediate states are no easier to validate
  than the final one. Single landing avoids carrying vestigial
  `Arc<Mutex>` types in the codebase indefinitely.
