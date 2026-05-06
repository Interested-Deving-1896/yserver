# Single-threaded core Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Eliminate the `ServerState`↔`Backend` lock-inversion deadlock class by collapsing all core mutation onto one thread, reducing every other thread to an I/O-only `mpsc` producer.

**Architecture:** A single core thread owns a plain `ServerState` and `Box<dyn Backend>` (no `Arc`/`Mutex`). The core's mio poller owns the listener fd, all client read/write fds, libinput fd, drm fd, signalfd, host-X11 fd, and a `Waker` for the message channel. Per-client *reader threads* are the only producers that turn raw bytes into `Message::Request`s; everything else (libinput, DRM page-flip, signalfd, host-X11 pump) sends straight into the channel. Replies/events go out via per-client non-blocking write halves with bounded outbound buffers; on `EAGAIN` we register `WRITABLE` interest, on overflow we disconnect.

**Tech Stack:** Rust 2024, `mio` 1.x for the unified poller (listener, client read+write fds, channel `Waker`, libinput fd, drm fd, signalfd, host-X11 fd). Channel: `crossbeam-channel` (already a dep) — both `bounded` for input/host-X11 and `unbounded` for `Request` are used; see Phase B for the policy.

**Spec:** `docs/superpowers/specs/2026-05-05-single-threaded-core-design.md`

**Branch:** Already on `single-threaded-core`. Single landing — `master` merge happens only when smoke matrix is green.

**Workspace warning:** Phase D (the request lift) leaves the workspace **non-compiling** for an extended stretch. Don't run `cargo test` during that window — only `cargo check`. The plan re-greens at D5.

---

## Conventions

- Run `cargo +nightly fmt && cargo clippy -- -D warnings && cargo test -p yserver-core` after each task that ends compile-clean. Per CLAUDE.md, drop pedantic; regular clippy must stay clean.
- Commit messages: `refactor(core): <step> — <spec migration step>`. One commit per task unless noted.
- `codex` for review at the listed checkpoints (per CLAUDE.md, codex handles review of work YOU did to save tokens).
- File-path references with `:N` are accurate at time of writing — re-grep before editing.

---

## Inbound channel policy (decided up front)

The spec already pre-empts unbounded outbound by bounding per-client write buffers. The inbound side needs an explicit policy too — one slow handler can otherwise let `Message::HostInput` accumulate without limit during a motion storm.

- `Message::Request`: **unbounded**. Reader threads are 1:1 with clients; if the core is slow, the reader naturally stops reading the socket once `crossbeam_channel::send` blocks — but we don't want readers to block. So the channel is unbounded; the safety valve is `process_request` returning fast, plus the outbound disconnect cap from §I.
- `Message::HostInput(Pointer)`: **coalesced at producer**. The libinput thread coalesces consecutive motion events into the latest before sending. Buttons/keys are not coalesced.
- `Message::SetupAllocate`: setup threads send through the unbounded message channel; the core's response goes back on a per-call `bounded(1)` rendezvous channel. Setup threads block on the response — fine, they're already off the core.
- Host X11 bytes are **not** in the channel at all — the host fd is owned by the core's mio poller and read directly (see F2). This eliminates the unbounded-channel-vs-kernel-buffer-pressure failure mode codex flagged.
- `Message::HostInput(Key)`, `Message::PageFlipReady`, `Message::ClientSetup`, `Message::ClientDisconnected`, `Message::Shutdown`: **unbounded** but inherently rate-limited by their fds.

Decision recorded so reviewers can challenge it; flagged in Phase B's commit body.

---

## Phase A — `ClientState` rename + add new fields (spec migration step 1)

### Task A1: Rename `ClientHandle` → `ClientState`, add core-loop fields

**Critical scope decision (codex finding #2):** A1 does **not** demote `writer: Arc<Mutex<UnixStream>>` or `last_sequence: Arc<AtomicU16>` to plain types. Today's fanout helpers correctly snapshot writers and write outside `server.lock()`; rewriting that snapshot pattern is exactly D2's job. A1 is purely a struct rename plus new fields used by later phases. No `legacy_send_to_target` shim — the existing snapshot+write pattern is preserved verbatim.

**Files:**
- Modify: `crates/yserver-core/src/server.rs:166` (the `clients` field type), `:508-522` (`ClientHandle`).

**Step 1 — Add new fields to the existing `ClientHandle` and rename in place**

```rust
#[derive(Debug)]
pub struct ClientState {
    // unchanged from ClientHandle:
    pub writer: Arc<Mutex<UnixStream>>,
    pub byte_order: ClientByteOrder,
    pub last_sequence: Arc<AtomicU16>,
    pub resource_id_base: u32,
    pub resource_id_mask: u32,
    pub event_masks: HashMap<ResourceId, u32>,
    pub save_set: HashSet<ResourceId>,
    pub big_requests_enabled: bool,
    pub xi2_masks: HashMap<(ResourceId, u16), u32>,
    // new in A1, used by later phases:
    pub outbound: std::collections::VecDeque<u8>,        // populated in D2
    pub watching_writable: bool,                          // poller registration state, used in I2
    pub focused_window: ResourceId,                       // moved off Arc<Mutex<ResourceId>> in D3
    pub reader_control: Option<crossbeam_channel::Sender<ReaderControl>>, // see C3 / D3
}

pub enum ReaderControl {
    EnableBigRequests,         // see "BigRequests sync" below
    Shutdown,
}
```

`ClientHandle` keeps existing as a `pub type ClientHandle = ClientState;` alias (deleted in H4).

**Step 2 — Update the `clients` map type** at `:166` to `HashMap<u32, ClientState>`. All construction sites (15+ in tests) get the four new fields with defaults: empty `outbound`, `watching_writable: false`, `focused_window: ROOT_WINDOW`, `reader_control: None`.

**Step 3 — Build + tests pass**

`cargo build --workspace && cargo test -p yserver-core` — green.

**Step 4 — Commit**

```
refactor(core): rename ClientHandle to ClientState, add core-loop fields — step 1
```

`EventTarget` is **not touched in A1**. It still carries `Arc<Mutex<UnixStream>>` snapshots until D2.

---

## Phase B — Message + poller scaffolding + backend trait reshape (spec migration step 2)

### Task B1: Add `Message` enum (with two-stage client lifecycle)

**Files:**
- Create: `crates/yserver-core/src/core_loop/{mod.rs,message.rs}`
- Modify: `crates/yserver-core/src/lib.rs`

The client lifecycle is **two-stage**. The setup thread does the entire handshake (read SetupRequest, sync `SetupAllocate` round-trip with the core, write setup_success), then sends `ClientSetupComplete` carrying the still-blocking stream. The core never does I/O on a not-yet-registered client. Host-X11 events do not flow through the channel at all — the host fd is owned by the core's poller (see F2).

```rust
pub enum Message {
    /// Sent by a setup thread mid-handshake. Core allocates resource IDs
    /// + snapshots geometry, replies via response_tx. Never blocks.
    SetupAllocate {
        id: ClientId,
        response_tx: crossbeam_channel::Sender<SetupAllocateResponse>,
    },
    /// Setup thread finished writing setup_success; hands the stream
    /// over to the core for split/register/reader-spawn (see D4).
    ClientSetupComplete {
        id: ClientId,
        stream: UnixStream,
        resource_id_base: u32,
        resource_id_mask: u32,
        byte_order: ClientByteOrder,
    },
    Request {
        id: ClientId,
        sequence: u16,
        header: RequestHeader,
        body: Vec<u8>,
        attached_fd: Option<std::os::fd::OwnedFd>,
    },
    ClientDisconnected { id: ClientId, reason: std::io::Error },
    HostInput(HostInputEvent),
    PageFlipReady,
    Shutdown,
}

pub enum HostInputEvent {
    PointerMotion { x: i32, y: i32, time: u32 },
    PointerButton { button: u8, pressed: bool, time: u32 },
    Key(crate::host_x11::HostKeyEvent),
}

pub struct SetupAllocateResponse {
    pub resource_id_base: u32,                  // 0 ⇒ allocator exhausted
    pub resource_id_mask: u32,
    pub screen_width_px: u16,
    pub screen_height_px: u16,
    pub current_input_masks: u32,
}
```

Test: `assert!(matches!(Message::Shutdown, Message::Shutdown))`. Commit:

```
refactor(core): add Message enum carrying sequence + attached_fd — step 2
```

### Task B2: `CoreSender` / `CoreReceiver` over a `crossbeam-channel` + mio `Waker`

**Files:** `crates/yserver-core/src/core_loop/sender.rs`

```rust
pub const NOTIFY_TOKEN: mio::Token = mio::Token(0);

pub struct CoreSender {
    waker: std::sync::Arc<mio::Waker>,
    tx: crossbeam_channel::Sender<Message>,
}

pub struct CoreReceiver { rx: crossbeam_channel::Receiver<Message> }

pub fn channel() -> std::io::Result<(mio::Poll, CoreSender, CoreReceiver)> {
    let poll = mio::Poll::new()?;
    let waker = std::sync::Arc::new(mio::Waker::new(poll.registry(), NOTIFY_TOKEN)?);
    let (tx, rx) = crossbeam_channel::unbounded();
    Ok((poll, CoreSender { waker, tx }, CoreReceiver { rx }))
}

impl CoreSender {
    pub fn send(&self, m: Message) -> std::io::Result<()> {
        self.tx.send(m).map_err(|_| std::io::Error::other("core dropped"))?;
        self.waker.wake()
    }
    pub fn clone_handle(&self) -> Self { Self { waker: self.waker.clone(), tx: self.tx.clone() } }
}
impl CoreReceiver {
    pub fn try_recv_all(&self) -> impl Iterator<Item = Message> + '_ {
        std::iter::from_fn(|| self.rx.try_recv().ok())
    }
}
```

**Test (fix the inconsistency codex flagged in the draft):**

```rust
#[test]
fn sender_wakes_poll() {
    let (mut poll, sender, _rx) = channel().unwrap();
    sender.clone_handle().send(Message::Shutdown).unwrap();
    let mut events = mio::Events::with_capacity(4);
    poll.poll(&mut events, Some(std::time::Duration::from_millis(50))).unwrap();
    assert!(events.iter().any(|e| e.token() == NOTIFY_TOKEN));
}
```

Commit.

### Task B3: Reshape the `Backend` trait for core-driven dispatch

Pulled forward from E2/E3 — everything else depends on the trait shape. (codex finding #5: no `panic!` defaults; methods are required, with concrete no-op-but-compiling impls on each backend that E2/F2 then fill in.)

**Files:** `crates/yserver-core/src/backend/trait_def.rs`, `crates/yserver/src/kms/backend.rs`, `crates/yserver-core/src/host_x11/mod.rs`.

Add to `pub trait Backend` (no defaults):

```rust
/// Dispatch a host input event the core received over the channel.
/// Backend produces zero or more X11 wire events via `state` fanout
/// helpers.  KmsBackend implements this in E2; HostX11 in F2.
fn on_host_input(&mut self, state: &mut ServerState, ev: HostInputEvent);

/// Drain DRM completion events and submit the next composite/flip if needed.
fn on_page_flip_ready(&mut self, state: &mut ServerState);

/// Raw fds the core's poller should watch on this backend's behalf.
/// Returns (fd, kind) tuples; core registers with the matching token.
fn poll_fds(&self) -> Vec<(std::os::fd::RawFd, BackendFdKind)>;
```

```rust
pub enum BackendFdKind { Libinput, Drm }
```

`set_event_sink` is **kept on the trait** through Phase F (H2 deletes it). It is the legacy wiring; the new methods are additive.

**B3 stub impls (compile-only, behaviorally inert):**

`KmsBackend::on_host_input`:
```rust
fn on_host_input(&mut self, _state: &mut ServerState, _ev: HostInputEvent) {
    // E2 fills this in. Until then no producer sends Message::HostInput,
    // so this is unreachable; we keep it as an empty body (not panic!)
    // so a regression that accidentally wires a producer early degrades
    // to a missed event rather than a runtime crash.
}
```

`KmsBackend::on_page_flip_ready`: delegate to existing `drain_page_flips_and_composite` immediately — the method has the right shape and no migration risk.

`KmsBackend::poll_fds`: returns `[(libinput_fd, Libinput), (drm_fd, Drm)]` already.

`HostX11Backend::on_host_input`, `on_page_flip_ready`: empty bodies (host backend never page-flips, host input arrives via F2's redesign).

`HostX11Backend::poll_fds`: empty vec (host-X11 has its own dispatcher thread; F1/F2 redesign).

Commit:

```
refactor(backend): add on_host_input / on_page_flip_ready / poll_fds with concrete impls — step 2
```

### Task B4: Stub `run_core` skeleton

`crates/yserver-core/src/core_loop/run.rs`. Loops `poll.poll()`, dispatches `NOTIFY_TOKEN` → drain channel, and stubs all other arms with `unimplemented!()`. A unit test feeds `Shutdown` and asserts return.

Commit.

---

## Phase C — Per-client reader threads + non-blocking write helper

### Task C1: Per-client outbound write helper + tests

**Files:** `crates/yserver-core/src/core_loop/client_io.rs`

```rust
pub const OUTBOUND_CAP: usize = 64 * 1024;

pub enum WriteOutcome { Done, WouldBlock, Disconnect }

pub fn write_or_buffer(c: &mut ClientState, bytes: &[u8]) -> std::io::Result<WriteOutcome> { … }
pub fn drain_outbound(c: &mut ClientState) -> std::io::Result<()> { … }
```

**Tests (TDD, fail first):**

- partial-write under filled kernel buffer → outbound non-empty, returns `WouldBlock`
- buffer exceeds `OUTBOUND_CAP` → returns `Disconnect`
- drain after reader empties kernel buffer → outbound emptied
- `disconnect_with_pending_outbound`: caller drops the client; verify no panic / no double-close (covers codex's missing-test bullet for §I)

Commit.

### Task C2: Setup thread does the *entire* handshake (incl. setup_success write)

Codex finding #4: `write_setup_success` on the core thread can stall the whole core if the peer stops reading. Fix: the setup thread does both the setup read **and** the setup write. The core's only role is allocating resource IDs synchronously via a rendezvous channel.

Codex finding #5: detached setup threads can leak on slow/malicious clients. Fix: per-thread teardown registry + read timeout.

**Files:** `crates/yserver-core/src/core_loop/setup_thread.rs`

**Setup-thread protocol:**

1. Set `SO_RCVTIMEO` and `SO_SNDTIMEO` on the freshly-accepted `UnixStream` (e.g. 5 s).
2. `read_setup_request(&mut stream)` (blocking, timeout-bounded).
3. Validate `byte_order`. On mismatch, write `setup_failed` synchronously, drop the stream, exit (`Drop` guard removes the registry entry — see teardown model below).
4. **Sync allocate from the core.** Send `Message::SetupAllocate { id, response_tx }` (one-shot `crossbeam_channel::bounded(1)`). Block on `response_rx.recv()`. Core never blocks.
5. Core's `SetupAllocate` arm runs `state.id_allocator.allocate()` + snapshots geometry, replies via `response_tx`.
6. Setup thread writes `setup_success` to the stream (blocking, timeout-bounded).
7. Setup thread sends `Message::ClientSetupComplete { id, stream, resource_id_base, resource_id_mask, byte_order }` to the core, then exits (Drop guard removes the registry entry).

**Teardown model (single source of truth — supersedes any other description in this doc):**

```rust
type SetupRegistry = Arc<Mutex<HashMap<ClientId, UnixStream>>>;
```

- The setup thread owns the **original** `UnixStream` returned by `accept`.
- Before the setup thread starts blocking I/O, `setup_thread::spawn` does `let cloned = stream.try_clone()?;` and inserts `cloned` into the registry under `id`. Both fds reference the same socket.
- A `Drop` guard inside the setup thread removes the entry from the registry on any exit path (success, error, panic).
- On `Message::Shutdown` the core iterates the registry, calls `cloned.shutdown(Shutdown::Both)?` on each (and `drop`s the clone). On Linux Unix sockets, `shutdown(Both)` causes any blocked `read`/`write`/`recvmsg`/`sendmsg` on *any* fd referencing that socket — including the setup thread's original — to return immediately (`EOF` for reads, `EPIPE` for writes). The setup thread propagates that as an `io::Error`, drops the stream, removes its registry entry via the Drop guard, and exits.
- No `JoinHandle` is kept and no explicit join happens. Setup threads exit on their own once `shutdown` lands; the binary's `main` can return after `run_core` does.

This is the only teardown mechanism — the JoinHandle/flag scheme mentioned in earlier drafts is dropped.

**Add to the `Message` enum (replaces the earlier `ClientSetup`):**

```rust
SetupAllocate {
    id: ClientId,
    response_tx: crossbeam_channel::Sender<SetupAllocateResponse>,
},
ClientSetupComplete {
    id: ClientId,
    stream: UnixStream,                 // blocking, intact
    resource_id_base: u32,
    resource_id_mask: u32,
    byte_order: ClientByteOrder,
},

pub struct SetupAllocateResponse {
    pub resource_id_base: u32,
    pub resource_id_mask: u32,
    pub screen_width_px: u16,
    pub screen_height_px: u16,
    pub current_input_masks: u32,
}
```

(There is no `Message::ClientSetup` variant any more.)

```rust
pub fn spawn(
    id: ClientId,
    stream: UnixStream,
    sender: CoreSender,
    registry: SetupRegistry,
) {
    // Insert the clone synchronously, before the thread starts.
    {
        let cloned = stream.try_clone().expect("clone setup stream");
        registry.lock().unwrap().insert(id, cloned);
    }
    std::thread::Builder::new()
        .name(format!("yserver-setup-{}", id.0))
        .spawn(move || {
            let _guard = SetupGuard { id, registry: registry.clone() }; // RAII removal
            run_setup(id, stream, sender);
        })
        .expect("spawn setup thread");
}
```

**Tests:**
1. Synthesise a SetupRequest on a `socketpair`; assert sender receives `SetupAllocate`, then on response, peer reads valid `setup_success` bytes, then sender receives `ClientSetupComplete`. After completion, registry no longer contains `id`.
2. Slow-client teardown: connect a peer that never sends bytes; trigger core shutdown; setup thread exits within timeout. Registry is empty.
3. Byte-order mismatch: assert peer reads `setup_failed`, no `ClientSetupComplete` is sent, registry no longer contains `id`.
4. Core-shutdown-during-write: peer reads SetupRequest reply but stops draining mid-`setup_success`; trigger shutdown; assert setup thread's blocked write returns `EPIPE` and the thread exits.

Commit:

```
feat(core): setup thread handles full handshake; sync resource-id allocate via SetupAllocate — step 3
```

### Task C3: Per-client reader thread (with BigRequests sync via control channel)

Reader is spawned by the core in D4's `ClientSetup` handler, **after** setup_success has been written. The reader owns the original (blocking) stream's reader role; the writer half is a `try_clone` kept by the core (and made non-blocking on the core side).

**Why both fds work despite shared OFD flags:** `O_NONBLOCK` on the writer clone *is* observed on the reader's fd. The reader compensates by treating `EAGAIN` as "block on `poll(2)` and retry":

```rust
fn blocking_read_request(reader: &mut FdReader, big: bool) -> io::Result<Option<(RequestHeader, Vec<u8>)>> {
    loop {
        match yserver_protocol::x11::read_request(reader, big) {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                wait_readable(reader.fd())?; // poll(2) on reader.fd
                continue;
            }
            other => return other,
        }
    }
}
```

**BigRequests sync — protocol-complete reader barrier.** yserver *is* the X server, so the BigRequests extension's major opcode is known at our compile time (or at server startup; the value is whatever `ServerState::extensions` assigns it during init — `BIG_REQUESTS_MAJOR`). The reader is given this constant when spawned. There is no `SetBigRequestsMajor` control message and no window during which the reader can fail to recognise Enable.

For every reader-detected Enable, the core replies on the control channel with **either** `ApplyBigRequests` or `IgnoreBigRequests` — no path leaves the reader parked. Three explicit cases:

- **Success**: Enable processed, big-mode toggled in core, `ApplyBigRequests` sent, reader resumes with `big = true`.
- **Duplicate / already-enabled**: Enable processed (returns reply with same `maximum_request_length`), `ApplyBigRequests` sent (idempotent on the reader side — `big` already `true`).
- **Malformed Enable** (wrong minor opcode for the BigRequests major, body length wrong): core sends `IgnoreBigRequests`, reader resumes with `big` unchanged.

```rust
pub enum ReaderControl {
    ApplyBigRequests,
    IgnoreBigRequests,
    Shutdown,
}

// Reader thread:
let big_requests_major: u8 = /* passed at spawn time */;
let mut big = false;
loop {
    while let Ok(ctrl) = control_rx.try_recv() {
        if matches!(ctrl, ReaderControl::Shutdown) { return Ok(()); }
        // Apply/Ignore are only meaningful right after we sent an Enable;
        // a stray one outside that window is a bug — log + drop.
    }
    let (header, body, attached_fd) = blocking_read_request(&mut reader, big)?;
    let is_enable = header.opcode == big_requests_major
                  && header.data == BIG_REQUESTS_ENABLE_MINOR;
    sender.send(Message::Request { id, sequence, header, body, attached_fd })?;
    if is_enable {
        match control_rx.recv()? {
            ReaderControl::ApplyBigRequests => big = true,
            ReaderControl::IgnoreBigRequests => { /* keep current big */ }
            ReaderControl::Shutdown => return Ok(()),
        }
    }
}
```

**Disconnect path also unblocks the reader.** When the core processes a client disconnect (or shuts down server-wide), it sends `ReaderControl::Shutdown` to every reader before dropping `client.reader_control`. A reader parked on `recv()` after an Enable observes `Shutdown` and exits. There is no path to a permanently parked reader.

**Core-side handler for Enable** (in `process_request`):

```rust
match handle_bigrequests_enable(state, id, &header, &body) {
    Ok(reply_bytes) => {
        write_or_buffer(client, &reply_bytes)?;
        client.reader_control.send(ReaderControl::ApplyBigRequests).ok();
    }
    Err(_) => {
        // Send X11 error to client, then unpark reader.
        write_or_buffer(client, &error_bytes)?;
        client.reader_control.send(ReaderControl::IgnoreBigRequests).ok();
    }
}
```

**Tests (write before impl):**

1. `enable_bigreq_then_large_request` — round-trip Enable, then a big request; assert big-framing.
2. `pipelined_enable_plus_big_request_in_one_write` — client writes Enable + big request without waiting; reader parks, big request frames correctly.
3. `enable_before_query_extension` — client sends Enable as the first request (no prior QueryExtension). Reader still recognises it (we know the major opcode), parks, core processes, sends `ApplyBigRequests`. No mis-framing.
4. `duplicate_enable_does_not_wedge` — two consecutive Enables; second receives `ApplyBigRequests`, reader resumes.
5. `malformed_enable_does_not_wedge` — Enable with wrong body length; reader parked → `IgnoreBigRequests` → reader resumes with `big` unchanged.
6. `disconnect_during_enable_park` — kill the client while reader is parked; assert reader sees `ReaderControl::Shutdown` and exits.
7. MIT-SHM AttachFd cmsg passthrough.

Commit:

```
feat(core): per-client reader thread + BigRequests sync handshake — step 3
```

---

## Phase D — `process_request` lift (spec migration step 4) — bulk of diff

Workspace stays non-compiling through D1–D5. Re-greens at D6.

### Task D1: Audit + freeze the lift target list

```bash
grep -n "lock_server\|server\.lock\|host\.lock\|writer\.lock\|last_sequence\.\(load\|store\|fetch\)\|focused_window\.lock\|spawn_keyboard_forwarder\|Arc::clone(&server\|Arc::clone(&host" crates/yserver-core/src/nested.rs crates/yserver/src/kms/backend.rs > /tmp/lift-sites.txt
wc -l /tmp/lift-sites.txt
```

Audit note in the commit body. No code change. Commit.

### Task D2: Lift fanout helpers

Codex flagged this is **not** mechanical — `pointer_event_fanout` does propagation+XI2 dedup, damage notifies snapshot mid-mutation, etc. So the new shape is explicit:

For each fanout helper (expose, pointer, key, focus, damage, present, xi2):

1. Take `&mut ServerState` (and `&mut dyn Backend` where the helper currently calls into the backend).
2. Compute the target list **first** (immutable iteration over `state.clients`, building a `Vec<EventTarget>` with `client_id`s, deduped via `client_id` equality — this replaces the `Arc::ptr_eq` dedup).
3. For each target, encode bytes against `state.clients[&id].byte_order` and `last_sequence`, then call `client_io::write_or_buffer(&mut state.clients[&id], &bytes)`.
4. Damage helpers: do the state mutation (mark `pending_notify_fired`, push rects) interleaved with target collection in a single `&mut` borrow scope; the writer pass comes after.

Sub-checkpoint per helper. **Each commit must `cargo check -p yserver-core` cleanly inside that helper's region** — track via region markers if needed. The crate as a whole still doesn't compile until D5.

Commit per helper (5–7 commits expected):

```
refactor(core): lift <name>_fanout to &mut ServerState
```

### Task D3: Lift opcode dispatch — including focus + keyboard forwarder removal

Codex finding #4: the per-client keyboard-forwarder thread reads `focused_window` and per-client `last_sequence`. It can't survive the focus migration in A1. So **delete `spawn_keyboard_forwarder` (`nested.rs:611`) here**, not in H3.

**Files:**
- Create: `crates/yserver-core/src/core_loop/process_request.rs`
- Modify: `crates/yserver-core/src/nested.rs` — `handle_request` becomes a shim deleted in H1.

```rust
pub fn process_request(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    id: ClientId,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
    attached_fd: Option<OwnedFd>,
) -> io::Result<()> { … }
```

Mechanical edits across the lifted opcode `match`:
- `lock_server(&server)?` → `state`
- `host.as_ref().and_then(|h| h.lock().ok())` → `backend`
- `client.writer.lock()` → `client_io::write_or_buffer(client, …)`
- `last_sequence.store(seq, _)` → `client.last_sequence = seq;`
- `focused_window.lock()…` → `client.focused_window`

Key dispatch (formerly `spawn_keyboard_forwarder`'s body) moves into the host-input handler that the core invokes (added in B3 as `Backend::on_host_input`); after the backend produces wire events the core fans out to clients using each client's own `focused_window` — no separate forwarder thread.

Sub-checkpoints by opcode group (Window, GContext, Drawables, Properties, Events, Extensions, Render, Damage, XFixes, Composite, Present, RandR, XI2, MIT-SHM, Sync, Shape).

### Task D4: Wire `run_core` arms

In `run.rs` fill:

- `Message::SetupAllocate { id, response_tx }`: allocate resource IDs and snapshot screen geometry. **Never blocks.**
  ```rust
  let resp = match state.id_allocator.allocate() {
      Some((base, mask)) => SetupAllocateResponse {
          resource_id_base: base,
          resource_id_mask: mask,
          screen_width_px: state.randr.screen_width,
          screen_height_px: state.randr.screen_height,
          current_input_masks: state.clients.values()
              .filter_map(|c| c.event_masks.get(&ROOT_WINDOW).copied())
              .fold(0u32, |a, b| a | b),
      },
      None => /* sentinel response with mask=0; setup thread will write setup_failed */
  };
  let _ = response_tx.send(resp);
  ```
- `Message::ClientSetupComplete { id, stream, resource_id_base, resource_id_mask, byte_order }`: setup_success is already on the wire. Now do the split + register + spawn:
  1. `let writer = stream.try_clone()?; writer.set_nonblocking(true)?;` (Reader keeps `stream` and handles the resulting OFD-flag share via the EAGAIN→`wait_readable` loop in C3.)
  2. Build `(reader_control_tx, reader_control_rx)`.
  3. Insert `ClientState { writer, byte_order, last_sequence: Arc::new(AtomicU16::new(0)), reader_control: reader_control_tx, resource_id_base, resource_id_mask, focused_window: ROOT_WINDOW, outbound: VecDeque::new(), watching_writable: false, … }` into `state.clients`. (`writer` and `last_sequence` are still `Arc<Mutex>`/`Arc<Atomic>` until D2 demotes them.)
  4. Remove `id` from the setup teardown registry (setup thread is exiting).
  5. Register the writer fd with mio:
     ```rust
     let raw = client.writer.as_raw_fd();   // binding, not temporary
     poll.registry().register(&mut SourceFd(&raw), client_token(id), Interest::empty())?;
     ```
  6. Spawn the reader thread (`client_reader::spawn(id, stream, reader_control_rx, sender.clone_handle())`).
- `Message::Request`: look up the client, call `process_request`, on `Disconnect` from `write_or_buffer` post a `ClientDisconnected`.
- `Message::HostInput(ev)`: `backend.on_host_input(&mut state, ev)` — body is empty until E2 (codex finding #5).
- `Message::PageFlipReady`: `backend.on_page_flip_ready(&mut state)` — wired from B3.
- *(no `HostX11Decoded` / `HostX11Bytes` arm — host I/O happens via the `HOST_X11_TOKEN` poll arm, not the message channel; see F2.)*
- `Message::ClientDisconnected`: call `process_disconnect(&mut state, &mut backend, id)` — extracted free function, originally `nested.rs:660-790`. Drain ack/control channels, deregister from poll, drop the client.
- `Message::Shutdown`: `return Ok(())`.

No ack-drain needed — the BigRequests barrier is implemented in the reader thread (see C3), not via a deferred-reply queue on the core.

### Task D5: Listener owned by core poll (drop the listener thread)

Codex YAGNI bullet: don't spawn a listener thread.

In `run_core` setup, register the `UnixListener` fd with `LISTENER_TOKEN`. On readiness:

```rust
loop {
    match listener.accept() {
        Ok((stream, _)) => {
            let id = next_client_id();
            setup_thread::spawn(id, stream, sender.clone_handle(), setup_teardown.clone());
            // setup_thread does the FULL handshake (read SetupRequest, sync
            // SetupAllocate round-trip with the core, write setup_success),
            // then sends ClientSetupComplete and exits.
            // The long-running reader thread is spawned by the
            // ClientSetupComplete arm in D4.
        }
        Err(e) if e.kind() == ErrorKind::WouldBlock => break,
        Err(e) => log::warn!("accept: {e}"),
    }
}
```

`setup_teardown: SetupRegistry` is the registry defined in C2 (an `Arc<Mutex<HashMap<ClientId, UnixStream>>>`). The listener fd is set non-blocking before registration.

On `Message::Shutdown`: take the registry, iterate, `shutdown(Shutdown::Both)` each entry, drop them. Setup threads' blocked syscalls return errors and exit; their Drop guards have nothing to remove (the entries are already taken). No JoinHandles, no explicit joins.

### Task D6: Restore green build

`cargo build --workspace && cargo test -p yserver-core` — all 249 tests pass. **Do not commit a red build.**

```
refactor(core): process_request lift complete, build+tests green — step 4
```

**Codex review checkpoint.**

---

## Phase E — KMS input / DRM / signal threads → senders (spec migration step 5)

### Task E1: Strip `Arc<Mutex>` from `KmsBackend` private state

`crates/yserver/src/kms/backend.rs`. `key_subscribers: Arc<Mutex<Vec<...>>>` becomes `Vec<Sender<...>>` — *and* is replaced entirely (no per-client subscribers) since key events go through `Message::HostInput::Key` now. Same audit for any other internal mutex. Commit.

### Task E2: libinput thread is sender-only + motion coalescing

**Files:** `crates/yserver/src/lib.rs:114-183` and `crates/yserver/src/kms/backend.rs:1433` (`process_one_input_event`).

The libinput thread owns `input_ctx` only. It maps each libinput event to `HostInputEvent` and sends. **Motion coalescing** policy: across a single `dispatch()` batch (and across consecutive batches if no non-motion event intervenes), at most one `PointerMotion` is in flight to the core; the latest position wins. Buttons/keys are never coalesced and flush any pending motion immediately before being sent.

Corrected state machine (codex finding #4 — the prior pseudocode never stored the first motion):

```rust
let mut pending_motion: Option<HostInputEvent> = None;
loop {
    wait_readable(input_fd)?;
    for raw in input_ctx.dispatch()? {
        let mapped = map(raw);
        match mapped {
            HostInputEvent::PointerMotion { .. } => {
                // Always replace — latest position wins.  This covers
                // both "no prior pending" and "coalesce" cases.
                pending_motion = Some(mapped);
            }
            non_motion => {
                if let Some(m) = pending_motion.take() {
                    sender.send(Message::HostInput(m))?;
                }
                sender.send(Message::HostInput(non_motion))?;
            }
        }
    }
    // End of batch: flush any pending motion so the core sees movement
    // before going idle.
    if let Some(m) = pending_motion.take() {
        sender.send(Message::HostInput(m))?;
    }
}
```

Test against this: feed `[Motion(1,1), Motion(2,2), Motion(3,3), Button(press), Motion(4,4), Motion(5,5), Motion(6,6)]` in a single batch. Sender should receive `[Motion(3,3), Button(press), Motion(6,6)]` — three events, not seven.

Implement `KmsBackend::on_host_input` as the new home of the keymap translation + fanout that used to live in `process_one_input_event`.

**Delete** `pending_pointer_events`, `drain_pending_pointer_events`, `process_input_events`, and the `BackendEventSink` impl on `KmsBackend` (spec §3 obligation).

Tests: motion-coalescing unit test — feed 5 motions + 1 button + 3 motions, assert sender received `(motion, button, motion)` (3 events, not 9). Commit.

### Task E3: DRM page-flip via core poller

Register `drm_fd` with `DRM_TOKEN` in `run_core`. On readiness send `Message::PageFlipReady`. `KmsBackend::on_page_flip_ready` calls the existing `drain_page_flips_and_composite()`. **Liveness test (codex missing-test bullet):** unit-style test that feeds back-to-back `PageFlipReady`s and asserts the backend submits a fresh composite each time. Commit.

### Task E4: signalfd via core poller

Register signalfd, on readiness send `Message::Shutdown`. Drop the binary's bespoke epoll loop entirely; `crates/yserver/src/lib.rs::run` becomes:

```rust
let (poll, sender, rx) = channel()?;
register_listener(&poll, &listener)?;
register_signalfd(&poll, &signal_fd)?;
register_backend_fds(&poll, &backend)?;            // libinput + drm
spawn_libinput_thread(input_ctx, sender.clone_handle());
core_loop::run::run_core(state, backend, poll, rx, sender, listener, signal_fd)
```

Commit.

---

## Phase F — Host-X11 redesign (spec migration step 6)

Codex finding #6: `pending_origins` is touched on every issued host request and on every reply/error consumption; `xid_map` is mutated on register/unregister and read in hot event-routing paths. "Documented mutex exception" is unsafe — these are exactly the surfaces where lock-order discipline rotted before. The redesign **moves both onto the core thread** and reduces the dispatcher to a raw-bytes forwarder.

### Task F1: Inventory + boundary decision

Inventory existing host_x11 threads/state:
- **Dispatcher thread** (decodes events, looks up xid_map, consults pending_origins, dispatches via sink) — **deleted entirely** in F2. The core does host I/O directly.
- **Sink consumer thread** — deleted (events fan out directly from `decode_and_route` into `&mut ServerState` helpers).
- **Per-client key forwarders** — already deleted in D3.
- `xid_map: Arc<Mutex<HostXidMap>>` (host_x11/mod.rs:294) — **moves onto `HostX11Backend` as plain `xid_map: HostXidMap`**. Touched only on the core thread (both mutation in `process_request` and lookup in `decode_and_route`).
- `pending_origins: Arc<Mutex<...>>` (host_x11/mod.rs:329) — moves onto `HostX11Backend` as plain `pending_origins: HashMap<u16, OriginContext>`. Written on every host-side request issued by `process_request` (core); read on every reply/error decode in `decode_and_route` (core).
- `pending_replies: HashMap<u16, HostReply>` — moves onto `HostX11Backend` as a plain field. Filled by `decode_and_route`, consumed by `wait_for_reply`. All on the core.
- `key_subscribers` — deleted (no per-client subscribers anymore; key events flow as `HostInputEvent::Key` once F2's `decode_and_route` recognises a `KeyPress`/`KeyRelease`, then through the same fanout machinery as KMS keys).

The host fd is set non-blocking and registered with the core's mio poller as `HOST_X11_TOKEN`. F2 implements the rest.

Commit:

```
refactor(host_x11): move xid_map + pending_origins + pending_replies onto HostX11Backend (core-owned) — step 6
```

### Task F2: Core-driven host-X11 I/O (no decoder thread)

Codex finding (third pass) #2: a separate dispatcher decoding into core would deadlock against synchronous `wait_for_reply` calls in current host-backend methods (`host_x11/request.rs:49`, `host_x11/mod.rs:901`). Two codex-proposed options either bring back shared state (option a) or require an async rewrite of every host call site (option b). We pick a **third option**: the core itself does host-X11 I/O — both for replies during sync waits and for spontaneous events when idle. The dispatcher thread is gone entirely.

**Why this is not measurably slower than the current dispatcher-thread design:**
- Sync waits already block the core; channel-hop vs direct `recvmsg` is identical wall-clock latency on a local socket.
- Once the core is single-threaded, the dispatcher's parallelism gain over the core has nothing to overlap with.
- Spontaneous events arrive via the mio poller's `HOST_X11_TOKEN`; in the worst case they're delayed by one client-request's processing time (sub-ms typically).

**Architecture (with reentrancy invariant):**

The host X11 socket fd is registered with the core's mio poller as `HOST_X11_TOKEN` with `Interest::READABLE`.

`HostX11Backend::drain_host_socket(&mut self)` reads frames non-blockingly until `EAGAIN`. It is **decode-only** — it does not call into `ServerState` and does not fan out events. It populates two on-backend queues:

- `pending_replies: HashMap<u16, HostReply>` — keyed by sequence.
- `pending_events: VecDeque<HostEvent>` — FIFO of decoded events to be fanned out later.

```rust
pub fn drain_host_socket(&mut self) -> io::Result<HostSocketStatus> {
    loop {
        match read_one_frame_nonblocking(self.host_fd) {
            Ok(Some(bytes)) => match decode(&bytes) {
                DecodedFrame::Reply { seq, reply } => { self.pending_replies.insert(seq, reply); }
                DecodedFrame::Event(ev) => { self.pending_events.push_back(ev); }
                DecodedFrame::Error { seq, err } => { self.pending_replies.insert(seq, HostReply::Error(err)); }
            },
            Ok(None) => return Ok(HostSocketStatus::WouldBlock),
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(HostSocketStatus::Eof),
            Err(e) => return Err(e),
        }
    }
}
```

**`wait_for_reply`** loops on `pending_replies` and drains the socket (replies + events) — but does **not** dispatch events:

```rust
pub fn wait_for_reply(&mut self, seq: u16) -> io::Result<HostReply> {
    loop {
        if let Some(reply) = self.pending_replies.remove(&seq) { return Ok(reply); }
        wait_readable(self.host_fd)?;
        match self.drain_host_socket()? {
            HostSocketStatus::WouldBlock => continue,
            HostSocketStatus::Eof => return Err(io::Error::from(ErrorKind::UnexpectedEof)),
        }
    }
}
```

Note: `wait_for_reply` takes `&mut self`, **not** `&mut ServerState`. It cannot touch `state`. This is the invariant that makes reentrancy structurally impossible: a host method called by `process_request` cannot fan out an event mid-call, cannot recurse into another fanout helper that issues a host request, cannot deadlock on its own queue.

**Event drain happens at outer-loop boundaries**, after `process_request` returns and after the `HOST_X11_TOKEN` arm. Add this at the bottom of each main-loop iteration:

```rust
// After dispatching one Message or one poll arm: drain any host events
// that decode_and_route enqueued.  We are at the outermost stack frame,
// no host request is in flight, so fanout can issue host requests
// freely.
while let Some(ev) = host_backend(&mut backend).map(|h| h.pending_events.pop_front()).flatten() {
    fanout_host_event(&mut state, &mut backend, ev)?;
}
```

`fanout_host_event` is the lifted body of the existing dispatcher's event-handling match — but now it runs on the core, with `&mut ServerState` and `&mut dyn Backend` in scope, and is allowed to issue further host requests because no `wait_for_reply` is on the stack.

**Reentrancy invariant (documented in code as a doc comment on `HostX11Backend`):**

> Host event dispatch (`fanout_host_event`) only runs at the outermost
> core-loop boundary. `decode_and_route` and `wait_for_reply` never
> dispatch events — they only enqueue. This makes recursive
> `wait_for_reply` impossible: a fanout helper that issues a host
> request from inside fanout returns to the same outer loop, the new
> events get queued behind the in-flight ones, and FIFO drain order
> preserves wire ordering.

**Test for the invariant:** synthesise an Expose event mid-`wait_for_reply`. Assert the Expose is **not** fanned out before `wait_for_reply` returns; assert it *is* fanned out on the next outer-loop iteration.

**Files:** delete `crates/yserver-core/src/host_x11/pump.rs` (the entire file goes away); modify `crates/yserver-core/src/host_x11/mod.rs` to add `drain_host_socket` + `decode_and_route` (mostly lifted from the existing dispatcher's match body); modify `crates/yserver-core/src/nested.rs::run` to register the host fd with the core poll.

**Defined shutdown semantics (replacing `process::exit(0)`):**

`drain_host_socket` returns `HostSocketStatus::Eof` when the host server closes the connection. The core's `HOST_X11_TOKEN` arm handles this by issuing `Message::Shutdown` to itself. `wait_for_reply` does the same on EOF (returns `Err(BrokenPipe)`, which the calling request handler propagates). The binary's `main` does socket-unlink and backend cleanup before exiting.

**Tests:**

1. `host_eof_during_idle_returns_cleanly` — drop the host side of a `socketpair`; trigger the `HOST_X11_TOKEN` arm; assert `run_core` returns `Ok(())` without panic / `process::exit`.
2. `host_event_arrives_during_wait_for_reply_is_dispatched` — synthesise: send a host request from inside `process_request`; before the reply arrives, push an `Expose` event onto the host socket. Assert the Expose is fanned out to nested clients before `wait_for_reply` returns.
3. `host_eof_during_wait_for_reply_propagates_error` — same setup but close the host socket before the reply; assert `wait_for_reply` returns `Err(BrokenPipe)`, request handler returns an error to the client.

Commit:

```
refactor(host_x11): core-driven host I/O; delete dispatcher thread; defined shutdown — step 6
```

---

## Phase G — Implicit-grab crossings (spec §"Pre-existing bugs" #2)

(unchanged from prior draft — this phase was not in codex's hit list)

### Task G1: `implicit_grab_crossings` helper in `server.rs`

Three cases: equal (empty), ancestor/descendant (path-walk through common ancestor), disjoint (Leave up + Enter down). `detail` codes per X11 spec; cross-check with Xephyr at `/home/jos/Projects/xserver/hw/kdrive/ephyr/`.

### Task G2: Tests first (TDD)

```rust
#[test] fn implicit_grab_target_eq_grab_window_emits_no_crossings() { … }
#[test] fn implicit_grab_disjoint_subtrees_walks_through_root() { … }
#[test] fn implicit_grab_ancestor_descendant_walks_through_common_ancestor() { … }
```

### Task G3: Replace unconditional emission

`crates/yserver/src/kms/backend.rs:1656` (`process_pointer_button`) and the host-X11 mirror, if any. Run all tests. Commit.

---

## Phase H — Delete dead types (spec migration steps 7-9)

### Task H1: Delete `lock_server`, `nested::handle_client` shim, `nested::handle_request`

### Task H2: Delete `BackendEventSink` trait + `host_pump_event_sink` factory + `HostPumpEventSink`

### Task H3: Delete remaining stragglers

`spawn_keyboard_forwarder` was deleted in D3. `KmsBackend::add_key_subscriber` was deleted in E1. Verify nothing references them:

```bash
grep -rn "spawn_keyboard_forwarder\|add_key_subscriber\|host_pump_event_sink\|BackendEventSink" crates/
```

### Task H4: Drop `pub type ClientHandle = ClientState;`

### Task H5: Final grep for `Arc<Mutex<ServerState>>` / `Arc<Mutex<dyn Backend>>` / `lock_server`

```bash
grep -rn "Arc<Mutex<ServerState>>\|Arc<Mutex<dyn Backend>>\|lock_server" crates/
```

Zero hits. Adjust any tests that build `ServerState` via `Arc::new(Mutex::new(...))` to construct it directly.

Commit:

```
refactor(core): delete deprecated mutex/sink types — steps 7-9
```

---

## Phase I — Backpressure poller integration

### Task I1: Token scheme

```rust
const NOTIFY_TOKEN: Token = Token(0);
const LISTENER_TOKEN: Token = Token(1);
const DRM_TOKEN: Token = Token(2);
const SIGNAL_TOKEN: Token = Token(3);
const LIBINPUT_TOKEN: Token = Token(4);
const HOST_X11_TOKEN: Token = Token(5);   // owned by core; see F2

fn client_token(id: ClientId) -> Token { Token(0x1000 + id.0 as usize) }
fn token_to_client(t: Token) -> Option<ClientId> { … }
```

To prevent stale-token reuse across disconnect/connect (codex missing-test bullet), `next_client_id` is monotonic (no reuse).

### Task I2: Register `WRITABLE` only when buffered

When `write_or_buffer` returns `WouldBlock` and `!client.watching_writable`:

```rust
let raw = client.writer.as_raw_fd();
poll.registry().reregister(&mut SourceFd(&raw), client_token(id), Interest::WRITABLE)?;
client.watching_writable = true;
```

When buffer drains to empty:

```rust
let raw = client.writer.as_raw_fd();
poll.registry().reregister(&mut SourceFd(&raw), client_token(id), Interest::empty())?;
client.watching_writable = false;
```

### Task I3: EPOLLOUT drain handler in dispatch loop

```rust
tok if let Some(id) = token_to_client(tok) => {
    if !ev.is_writable() { continue; }
    let Some(client) = state.clients.get_mut(&id.0) else { continue };
    drain_outbound(client)?;
    if client.outbound.is_empty() {
        let raw = client.writer.as_raw_fd();
        poll.registry().reregister(&mut SourceFd(&raw), tok, Interest::empty())?;
        client.watching_writable = false;
    }
}
```

### Task I4: Disconnect path

On `WriteOutcome::Disconnect`, deregister the fd, drop `state.clients[&id]`, run `process_disconnect`, *do not* let any later EPOLLOUT for that token re-enter. Test: synthetic slow client — fill outbound, force `Disconnect`, then poll for spurious WRITABLE on the dead token; must be a no-op.

### Task I5: Backpressure unit tests

Per-test behaviors (each gets its own `#[test]`):

- `slow_client_disconnected_when_buffer_overflows`
- `disconnect_with_pending_outbound_no_panic`
- `epollout_drains_partial_buffer`
- `disconnect_then_reconnect_uses_fresh_token` (next_client_id monotonicity)

Commits per task. Final:

```
feat(core): slow-client backpressure with bounded outbound buffer
```

---

## Phase J — Smoke matrix + merge

### Task J1: ynest smoke

`RUST_LOG=debug` ynest under `bwrap`+`Xephyr`:
- xterm: connect/draw/resize.
- fvwm3: menus, drag.
- wmaker: dock manipulation.
- e16: **rapid clicks across multiple windows for ≥5 minutes** — original freeze repro. Must not lock up.
- gnome-calculator (GTK3).
- **Host window close test**: close the `Xephyr` window; ynest must shut down cleanly (no `process::exit`, no leaked socket).

### Task J2: yserver (KMS) smoke

Under `vng` (memory `reference_virtme_ng_drm_harness.md`): fvwm3, wmaker, e16. Same click-stress.

### Task J3: Stress regressions for spec bug #1 (expose-side inversion)

e16 menu spam (destroy/map churn): no freeze, no missing repaint.

### Task J4: Update `docs/status.md` and memory

Per AGENTS.md. Memory entry for "single-threaded core landed".

### Task J5: Final review + merge

- `codex` review on the full diff.
- Address feedback, push, ask user before squash-merge to `master` (per AGENTS.md).

---

## Roll-up checklist

- [x] A1: ClientState rename + new fields (Arc<Mutex> writer kept until D2; compile-clean)
- [x] B1-B4: Message (SetupAllocate + ClientSetupComplete + HostInput; no host bytes in channel) + sender + backend trait (no panics) + run_core stub
- [x] C1-C3: write helper + setup thread (full handshake + teardown registry) + reader thread (reader-side BigRequests barrier; AttachFd)
- [ ] D1-D6: process_request lift incl. focus + keyboard forwarder removal; listener owned by poll; build green
  - [x] D1: lift target list audit (340 sites)
  - [x] D2: lift fanout helpers — `emit_window_event_to_state`,
    `expose_event_fanout_to_state`, `pointer_event_fanout_to_state`,
    `accumulate_damage_to_state`, `emit_xi2_focus_event_to_state`
    added in `core_loop::fanout` / `core_loop::pointer_fanout` /
    `core_loop::damage_fanout`; old callers still on the
    `Mutex<ServerState>` helpers
  - [x] D3: opcode dispatch lift — **complete**. Every X11 core opcode
    (1-127) plus every extension dispatcher (128 RANDR, 130 MIT-SHM,
    133 RENDER, 135 BIG-REQUESTS, 136 XKB, 137 XI2, 138 GE, 140 XFIXES,
    141 SHAPE, 142 SYNC, 143 DAMAGE, 144 COMPOSITE, 145 PRESENT) has a
    state-borrowing implementation in `core_loop::process_request`.
    Helper toolkit:
      - `emit_x11_error` (state-borrowing protocol error encoder)
      - `set_focused_window_to_state` (per-client focus + emit
        FocusOut/FocusIn + XI2 focus)
      - `destroy_window_subtree` (recursive teardown + UnmapNotify
        + DestroyNotify + host xid cleanup)
      - `invalidate_composite_named_pixmaps_to_state` (alias drop)
      - `mirror_shape_to_host_state` (SHAPE → host mirror)
      - `send_reply_with_fd` (MIT-SHM CreateSegment SCM_RIGHTS path)
      - drawing skeleton: `apply_clip_state → apply_draw_state →
        backend.<op> → accumulate_damage_to_state`.
    `RequestOutcome::LiftPending` was dropped along with the
    transitional `nested::handle_request` callback path; the default
    match arm now logs unsupported opcodes and returns `Handled`.
    `nested::handle_client` / `handle_request` are dead code on the
    production path — H1 deletes them.
  - [x] D4: run_core wiring — every Message arm has a real handler.
    SetupAllocate/ClientSetupComplete/ClientDisconnected/HostInput/
    PageFlipReady plus the existing Request + Shutdown arms.
    Adds `core_loop::process_disconnect` for the per-client cleanup
    path lifted from `nested::handle_client`'s closing block.
  - [x] D5: listener fd owned by poll; setup-thread teardown registry
    wired (`SetupRegistry::shutdown_all` on Shutdown);
    ClientSetupComplete now spawns the reader thread + registers the
    writer fd against `client_token(id)`.
  - [x] D6: build is green throughout. Each commit landed without
    breaking compile/test/clippy — the original "Workspace stays
    non-compiling through D1–D5" warning didn't apply because the
    lift was structured as additive `core_loop::process_request`
    arms with `RequestOutcome::LiftPending` shimming the legacy
    path until the last opcode was migrated, after which LiftPending
    was dropped and the default arm took over for unsupported
    opcodes.
- [x] E1-E4: KMS senders + motion coalescing; delete pending_pointer_events
  - [x] E1: KmsBackend.key_subscribers demoted to plain Vec
  - [x] E2: libinput-thread sender (`yserver::input_thread`) with motion
    coalescing — five tests covering relative→absolute mapping,
    absolute scaling, button/key cursor invariance, batch coalescing
    of `[Motion×5, Button, Motion×3] → 3 sender messages`, and
    cross-batch motion carry-over. `KmsBackend::on_host_input` drives
    `process_pointer_absolute` / `process_pointer_button` and drains
    the local pointer-event scratch through
    `pointer_event_fanout_to_state`. Key events run through a new
    `cook_host_key` (xkb update + cursor coords + modifier mask) and
    fan out via `core_loop::key_fanout::key_event_fanout_to_state`.
    `yserver_protocol::x11::encode_key_event` adds the buffer-encoding
    variant of `write_key_event`.
  - [x] E3: `run_core` registers `backend.poll_fds()` against
    `DRM_TOKEN`/`LIBINPUT_TOKEN`; DRM_TOKEN arm calls
    `backend.on_page_flip_ready(state)` directly. Liveness test
    asserts back-to-back `Message::PageFlipReady` each dispatch
    (counter on RecordingBackend reaches 3).
  - [x] E4: `yserver::run` rewritten on top of `run_core`. `ServerState`
    and `KmsBackend` are owned by-value on the main thread (no more
    Arc<Mutex>). Libinput thread is sender-only; signalfd watcher
    thread reads the SignalFd and posts `Message::Shutdown`. Per-
    client setup goes through the run_core setup-thread + reader-
    thread chain. Legacy KMS helpers
    (`process_one_input_event`, `process_input_events`,
    `drain_pending_pointer_events`, `process_key_event`,
    `process_pointer_motion`) deleted — no callers after the swap.
    `event_sink` field + `set_event_sink` impl + `synthesize_expose`
    stay (unreachable on standalone yserver but the trait surface
    survives until H1/H2; a follow-up moves synthesize_expose onto
    state-borrowing fanout).
- [x] F1-F2: host-X11 core-driven I/O (no dispatcher thread); defined shutdown
  - dispatcher thread + sink consumer thread deleted; `pending_replies` /
    `pending_errors` / `pending_origins` / `xid_map` demoted from
    `Arc<Mutex<…>>` to plain fields on `HostX11Backend`
  - `drain_host_socket` reads via `recv(MSG_DONTWAIT)` so the stream
    stays blocking for `write_all` (avoids per-call EAGAIN handling at
    every host send site); `try_extract_frame` parses complete X11
    frames out of the read buffer (reply/GenericEvent extra-payload
    aware)
  - `wait_for_reply` drives the socket directly: `take_buffered_reply`
    → `drain_host_socket` → `wait_readable(host_fd)` loop. Reentrancy
    invariant: drain only enqueues; `dispatch_pending_host_events`
    (run at the outer-loop boundary) is the single fanout point —
    nested `wait_for_reply` cannot recurse into event dispatch.
  - run_core gains the `HOST_X11_TOKEN` arm + a post-iteration
    `dispatch_pending_host_events` pass; `BackendFdKind::HostX11`
    threads the host fd through `poll_fds()` like Drm/Libinput.
  - `nested::run` rewritten on top of `run_core` (parallel to E4 for
    yserver). `handle_client` / `handle_request` /
    `spawn_keyboard_forwarder` survive as dead code until H1.
  - **F2 follow-ups landed in the same commit**:
    - `process_request` now stores `client.last_sequence` at entry
      (legacy `nested::handle_client` did this; the run_core path
      had a regression that emitted PropertyNotify with `seq=0`).
    - `handle_big_requests_request` sends `ReaderControl::ApplyBigRequests`
      / `IgnoreBigRequests` to unblock the parked reader thread.
    - `client_reader::run` posts `Message::ClientDisconnected` on
      Ok(None) (peer EOF), not just on `Err`, so `process_disconnect`
      always runs.
- [x] G1-G3: spec-correct implicit-grab crossings
  - [x] G1+G2: `crossings::implicit_grab_crossings` helper + 6 TDD tests
  - [x] G3: wired into `kms::process_pointer_button`. The helper drives
    the path-walk between focus (prev_pointer_window) and grab (press
    window); each event is emitted via `emit_crossing` with the
    crossing-mode set to NotifyGrab on press / NotifyUngrab on release.
    Falls back to the legacy single-event approximation when either
    focus or grab isn't a known nested window. The host-X11 path
    inherits the host server's own crossings via the new
    `dispatch_pending_host_events` so no separate mirror is needed.
- [x] H1-H5: delete dead types
  - [x] **H1**: deleted `nested::handle_client`, `handle_request`,
    `lock_server`, plus all their support types (`KeyTarget`,
    `WriterTag`, `route_key_event`, `spawn_keyboard_forwarder`,
    `emit_x11_error`, `resolve_host_subwindow_visual`,
    `collect_destroy_order`, `fanout_destroy_sequence`,
    `set_focused_window`, `emit_xi2_focus_event`,
    `emit_expose_subtree`, `clear_extent`, `OwnedGetPropertyReply`,
    every `handle_*_request` extension dispatcher,
    `accumulate_damage`, `invalidate_composite_named_pixmaps`,
    `accumulate_damage_full`, `drawable_full_rect`,
    `rewrite_reply_sequence`, `supported_pixmap_depth`,
    `zpixmap_expected_len`, `log_void`/`log_reply`,
    `window_attributes`, `window_geometry`, `pixmap_geometry`).
    ~9.7k LOC removed. 50 dead in-file tests removed
    alongside (key_routing, xfixes_requests, shape_requests,
    atom_name, damage, most of composite, backend_trait_integration,
    phase3_2_*, zpixmap_*, subtract_*); 247 yserver-core tests
    still pass.
  - [x] **H2**: deleted `BackendEventSink` trait + `BackendEvent`
    enum, `Backend::set_event_sink` method (and its three impls),
    `HostPumpEventSink` struct + `host_pump_event_sink` factory +
    its sink impl in `server.rs`, the `KmsBackend.event_sink`
    field + `synthesize_expose` (and its 4 callers in destroy /
    map / unmap / configure subwindow paths — all no-ops since
    E4), `nested::expose_event_fanout` (last caller was
    `HostPumpEventSink`), and `HostX11Backend::set_event_sink`.
  - [x] **H3**: dropped `KmsBackend.key_subscribers` field +
    its `Vec::new()` initializers in both constructors and the
    test-only one. Cleaned up stale comment refs to
    `spawn_keyboard_forwarder`, `add_key_subscriber`,
    `BackendEventSink`, `process_one_input_event`,
    `pending_pointer_events` in `core_loop/key_fanout.rs`,
    `kms/backend.rs`, and `nested.rs::run`.
  - [x] **H4**: dropped `pub type ClientHandle = ClientState`
    alias and renamed every remaining `ClientHandle` reference
    (mostly server.rs test fixtures) to `ClientState`.
  - [x] **H5**: lifted `server::handle_host_container_resize`
    (previously `&Arc<Mutex<ServerState>>`-shaped, kept under
    `#[allow(dead_code)]`) into
    `core_loop::run::handle_host_container_resize`, which now
    owns the full RANDR fanout (ScreenChangeNotify +
    CrtcChangeNotify + OutputChangeNotify) on top of the existing
    ConfigureNotify-on-root path. RANDR events now flow through
    `client_io::write_or_buffer` instead of the legacy
    `target.writer.lock()` path, so they participate in the I-phase
    backpressure mechanism. The `mod root_resize` tests in
    nested.rs are converted to drive the state-borrowing helper
    with a plain `&mut ServerState`. Final grep confirms zero
    `Arc<Mutex<ServerState>>` production references and zero
    `lock_server` references.
- [x] I1-I5: backpressure + monotonic client ids
  - [x] I1: token scheme + monotonic ClientIdAllocator
  - [x] I2: `reconcile_client_writable_interest` runs per outer poll
    iteration; toggles `client.watching_writable` in lock-step with
    `client.outbound` emptiness. Reregister errors of kind NotFound
    (fd deregistered by a concurrent disconnect) are swallowed.
  - [x] I3: WRITABLE-readiness on a per-client token dispatches to
    `client_io::drain_outbound`; Disconnect/Err results feed
    `process_disconnect` directly.
  - [x] I4: `process_disconnect` drops the ClientState (closes the
    writer fd, which the kernel auto-removes from epoll); the
    reconcile NotFound guard + the `state.clients.get_mut`
    None-check in the WRITABLE arm cover the dead-token race.
  - [x] I5: `reconcile_writable_interest_tracks_outbound_state`
    unit test against a real `mio::Registry`. Other I5 bullets
    are covered by pre-existing tests in `client_io` (overflow,
    drop-with-pending) and `poll_tokens` (monotonic allocator).
- [~] J1-J5: smoke + merge — **J1–J4 done, J5 pending**
  - [x] **J1 (ynest smoke)**: xterm + fvwm3 / wmaker / e16 boot
    cleanly; gtk3-demo runs under fvwm3 and e16 with content
    interaction; host-window-close cleanly shuts ynest down (no
    `process::exit`, no leaked socket).
  - [x] **J2 (yserver / KMS smoke)** under `vng`: e16 and Window
    Maker work the same as pre-refactor; fvwm3 boots but its core-
    font menus render with no text (`compute_font_metrics` left
    `properties` empty; H5-followup commit synthesises the XLFD
    properties — confirmed running, but fvwm3's text-drawing gate
    has a second condition we haven't pinpointed yet, filed in
    `known-issues.md`).
  - [x] **J3 (e16 click-stress for the spec bug-#1 expose-side
    inversion)**: no freeze, no missing repaint. The original
    `ServerState ↔ Backend` lock-inversion deadlock class is
    gone; the multi-WM smoke completed without ever hitting the
    repro pattern.
  - [x] **J4 (docs)**: `docs/status.md` Phase 6.8 section,
    README, and `docs/known-issues.md` updated with the smoke
    matrix, every Phase J fix, and the deferred bugs.
  - [ ] **J5 (final review + merge)**: codex review on the full
    diff + squash to `master`. Pending user sign-off.

  Phase J fixes that landed inline (none of them blocking
  Phase J — each fixed a real protocol bug the lift surfaced
  once real WMs and apps drove the new path):

  - `process_disconnect` not idempotent (writer-EPIPE racing
    reader-EOF could fire it twice). Fix: bail if
    `state.clients` no longer contains the id.
  - `OUTBOUND_CAP` 64 KB → 4 MB. ISO10646 host fonts produce
    ~786 KB QueryFont replies that fit in Linux's default
    208 KB unix-socket send buffer with ~578 KB tail; the old
    cap forced a spurious slow-client disconnect on a fast
    client.
  - Edge-triggered EPOLLOUT race in `reconcile`: the kernel
    could transition the writer fd writable before we
    re-registered for WRITABLE, so we'd miss the edge.
    Fix: proactive `drain_outbound` per iteration, regardless
    of registration state.
  - `PointerEvent.child` always 0 → fvwm3's
    `Mouse 1 R A Menu MainMenu` binding fired on every click
    anywhere, since every event looked like a bare-root click.
    `pointer_propagation_target_by_id` now returns the
    immediate descendant of the propagation target along the
    path to the source window, and the wire encoder uses it.
  - XI2 events stolen by core grabs. Active-grab path
    redirected ALL events (Release, Motion) to the grab
    client via core only. Per X11 spec, XI2 grabs are
    independent of core grabs. Fix: split fanout so the XI2
    arm always runs.
  - XI2 `buttons` mask wrong on Release (was always
    `1 << (button-1)`; per spec the post-event button state).
  - `SetInputFocus` not mirrored across clients. The
    `current_focus` doc says `ClientState::focused_window`
    is mirrored across every client; the implementation only
    updated the caller. Symptom: keyboard input didn't reach
    xterm under wmaker because both wmaker and xterm called
    SetInputFocus on their own internal windows and
    `current_focus` (HashMap-iteration first non-ROOT) picked
    whichever got visited first. Fix: write the new focus
    into every `state.clients[*].focused_window`.
  - `HostInputEvent::PointerButton.button` was `u8` but Linux
    button codes are u32 with `BTN_LEFT = 0x110` —
    truncated to `0x10` and dropped by the KMS backend's
    `0x110 => 1` mapping. Symptom on yserver: every click
    logged `unmapped libinput button code 0x10, dropping`.
    Widened the channel field to `u16`.
  - `compute_font_metrics` on KMS returned empty properties.
    Xt/Athena and fvwm3 menu code interpret an empty
    properties list as "font unusable" and silently skip text
    drawing. `handle_open_font` now synthesizes the standard
    XLFD property set (FONT, FOUNDRY, FAMILY_NAME, …,
    CHARSET_ENCODING — 15 properties) when the backend
    returns an empty list. (Necessary but apparently not
    sufficient for fvwm3 menus on yserver — see known-issues.)
