# Property Storage Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement real per-window property storage with `ChangeProperty` / `DeleteProperty` / `GetProperty` and cross-client `PropertyNotify`, by introducing shared `ServerState` and per-(client, window) event masks.

**Architecture:** Move all resource tables out of `ClientState` into a single `Arc<Mutex<ServerState>>` shared across client threads. Replace `Window.event_mask` with per-`ClientHandle` `event_masks: HashMap<ResourceId, u32>`. Route every window-targeted event through one helper (`subscribers()` + `emit_window_event`). Properties live on `Window` and use a small pure module for compute (`apply_change`, `slice_for_get`).

**Tech Stack:** Rust 2024, std `Mutex`/`Arc`, `proptest` (new dev-dependency in `yserver-core` + `yserver-protocol`).

**Spec:** [`docs/superpowers/specs/2026-04-27-property-storage-design.md`](../specs/2026-04-27-property-storage-design.md).

**Project conventions** (run on every commit, see `~/.claude/CLAUDE.md`):

```sh
cargo fmt
cargo clippy -- -W clippy::pedantic
cargo test
```

Fix all warnings before committing. Use `cargo +nightly fmt` only if the project later requires it (it does not today).

---

## File Structure

| Path | Status | Purpose |
|---|---|---|
| `crates/yserver-core/src/server.rs` | **new** | `ServerState`, `ClientHandle`, `IdAllocator`, `EventTarget`, `subscribers()`, `emit_window_event()`, `timestamp_now()` |
| `crates/yserver-core/src/properties.rs` | **new** | `PropertyValue`, `PropertyFormat`, `ChangeMode`, `apply_change`, `slice_for_get`, `MAX_PROPERTY_BYTES` |
| `crates/yserver-core/src/resources.rs` | modify | Add `owner: ClientId` to every resource; add `properties: HashMap<AtomId, PropertyValue>` to `Window`; remove `event_mask: u32` from `Window`; expose mutable accessors needed by handlers |
| `crates/yserver-core/src/nested.rs` | modify | Refactor writer locking; switch handlers to `Arc<Mutex<ServerState>>`; wire up `BadIDChoice`, property handlers, `DestroyNotify`, per-client event masks |
| `crates/yserver-core/src/lib.rs` | modify | `pub mod server; pub mod properties;` |
| `crates/yserver-core/Cargo.toml` | modify | Add `[dev-dependencies] proptest = "1"` |
| `crates/yserver-protocol/src/x11.rs` | modify | Add request parsers (ChangeProperty, DeleteProperty, GetProperty), `GetPropertyReply`, `write_get_property_reply` (replace stub), `write_property_notify_event`, `write_destroy_notify_event`, `error::*` constants |
| `crates/yserver-protocol/Cargo.toml` | modify | Add `[dev-dependencies] proptest = "1"` |

The implementation is staged (`Stage 0`–`Stage 5`) per the spec. Each stage compiles, passes its tests, and ends with a commit. Whether they ship as one PR or several is a separate call — push that decision until after Stage 5 lands.

---

## Stage 0 — Writer-lock refactor (precondition)

Today `nested.rs:167-179` locks the per-client `Arc<Mutex<UnixStream>>` once at request entry and holds it for the whole handler. Stage 2 will route events to the issuing client itself, and `std::sync::Mutex` is non-reentrant — so the outer hold must go away first.

Pure refactor: no observable behavior change. Single-client correctness is preserved because each client still has its own writer mutex and only its own thread reads requests.

### Task 0.1: Drop the outer writer lock in `handle_client`

**Files:**
- Modify: `crates/yserver-core/src/nested.rs:142-181`
- Modify: `crates/yserver-core/src/nested.rs:271-313` (`set_focused_window`, `focus_if_window_wants_keys`)
- Modify: `crates/yserver-core/src/nested.rs:369-952` (`handle_request` signature + every internal write call)

- [ ] **Step 1: Change `handle_request` to take a writer handle instead of a borrow**

Replace the signature:

```rust
#[allow(clippy::too_many_arguments)]
fn handle_request(
    client_id: ClientId,
    state: &mut ClientState,
    host: Option<&Arc<Mutex<HostX11>>>,
    writer: &Arc<Mutex<UnixStream>>,
    focused_window: &Arc<Mutex<ResourceId>>,
    sequence: SequenceNumber,
    header: RequestHeader,
    body: &[u8],
) -> io::Result<()> {
    // ... body changes per Step 2
}
```

- [ ] **Step 2: Introduce a `lock_writer` local helper inside `handle_request`**

At the top of `handle_request`, introduce:

```rust
let lock_writer = || -> io::Result<std::sync::MutexGuard<'_, UnixStream>> {
    writer
        .lock()
        .map_err(|_| io::Error::new(ErrorKind::BrokenPipe, "client writer mutex poisoned"))
};
```

Then replace every `stream` reference inside `handle_request` with a freshly acquired guard. Pattern: change `x11::write_foo(stream, ...)?;` to `x11::write_foo(&mut *lock_writer()?, ...)?;`. For the `ListFonts` / `ListFontsWithInfo` paths that call `stream.write_all(&reply)?`, do the same: lock, write, drop guard before the next iteration.

- [ ] **Step 3: Update `set_focused_window` and `focus_if_window_wants_keys` to take `&Arc<Mutex<UnixStream>>`**

```rust
fn set_focused_window(
    focused_window: &Arc<Mutex<ResourceId>>,
    writer: &Arc<Mutex<UnixStream>>,
    sequence: SequenceNumber,
    window: ResourceId,
) -> io::Result<()> {
    if window == ResourceId(0) {
        return Ok(());
    }
    let Ok(mut focused_window) = focused_window.lock() else {
        return Ok(());
    };
    if *focused_window == window {
        return Ok(());
    }

    if *focused_window != ROOT_WINDOW {
        let mut w = writer
            .lock()
            .map_err(|_| io::Error::new(ErrorKind::BrokenPipe, "client writer mutex poisoned"))?;
        x11::write_focus_event(&mut *w, sequence, false, *focused_window)?;
    }
    *focused_window = window;
    let mut w = writer
        .lock()
        .map_err(|_| io::Error::new(ErrorKind::BrokenPipe, "client writer mutex poisoned"))?;
    x11::write_focus_event(&mut *w, sequence, true, window)
}

fn focus_if_window_wants_keys(
    focused_window: &Arc<Mutex<ResourceId>>,
    state: &ClientState,
    writer: &Arc<Mutex<UnixStream>>,
    sequence: SequenceNumber,
    window: ResourceId,
) -> io::Result<()> {
    const KEY_PRESS_MASK: u32 = 1 << 0;
    const KEY_RELEASE_MASK: u32 = 1 << 1;

    if state.resources.window(window).is_some_and(|window| {
        window.map_state == MapState::Viewable
            && window.event_mask & (KEY_PRESS_MASK | KEY_RELEASE_MASK) != 0
    }) {
        debug!("focus key window 0x{:x}", window.0);
        set_focused_window(focused_window, writer, sequence, window)?;
    }
    Ok(())
}
```

(Stage 2 will replace `window.event_mask` here with the per-client lookup; for now keep the existing field check.)

- [ ] **Step 4: Update the request loop to pass `&writer` instead of locking**

Replace `nested.rs:167-179`:

```rust
loop {
    let Some((header, body)) = x11::read_request(&mut reader)? else {
        return Ok(());
    };
    sequence = sequence.next();
    last_sequence.store(sequence.0, Ordering::Relaxed);
    handle_request(
        client_id,
        &mut state,
        host.as_ref(),
        &writer,
        &focused_window,
        sequence,
        header,
        &body,
    )?;
}
```

- [ ] **Step 5: Verify build, lint, and smoke test**

```sh
cargo fmt
cargo clippy -- -W clippy::pedantic
cargo test
```

Expected: clean build, no warnings, no test regressions (no tests exist yet — `cargo test` should report `0 passed`).

Manual smoke (only if X11 host available):

```sh
DISPLAY=:0 cargo run --bin ynest -- 42 &
DISPLAY=:42 xterm
```

Expected: xterm comes up and accepts input (same as today).

- [ ] **Step 6: Commit**

```sh
git add crates/yserver-core/src/nested.rs
git commit -m "refactor(nested): drop outer writer lock per request

Each call site that writes to the client stream now acquires the
writer mutex locally. No observable behavior change; precondition
for cross-client event delivery, where re-entrance would deadlock
on a non-reentrant std Mutex."
```

---

## Stage 1 — `ServerState` skeleton, `IdAllocator`, per-resource `owner`

Move `ResourceTable` and `AtomTable` (the latter currently lives as `ClientState.atoms_by_name` / `atom_names` / `next_atom_id`) into a shared `Arc<Mutex<ServerState>>`. Hand out non-overlapping resource-ID ranges per client. Add `BadIDChoice` validation to every `Create*` handler. Add disconnect cleanup that walks all five resource tables.

### Task 1.1: Add `proptest` to dev-dependencies

**Files:**
- Modify: `crates/yserver-core/Cargo.toml`
- Modify: `crates/yserver-protocol/Cargo.toml`

- [ ] **Step 1: Append dev-dep to `yserver-core/Cargo.toml`**

```toml
[dev-dependencies]
proptest = "1"
```

- [ ] **Step 2: Append dev-dep to `yserver-protocol/Cargo.toml`**

```toml
[dev-dependencies]
proptest = "1"
```

- [ ] **Step 3: Verify build and commit**

```sh
cargo test
```

Expected: clean build, both crates compile; `0 passed`.

```sh
git add crates/yserver-core/Cargo.toml crates/yserver-protocol/Cargo.toml
git commit -m "build: add proptest dev-dependency to core and protocol crates"
```

### Task 1.2: `IdAllocator` (TDD)

**Files:**
- Create: `crates/yserver-core/src/server.rs`
- Modify: `crates/yserver-core/src/lib.rs`

- [ ] **Step 1: Add `pub mod server;` to lib.rs**

`crates/yserver-core/src/lib.rs`:

```rust
pub mod host_x11;
pub mod nested;
pub mod resources;
pub mod server;
```

- [ ] **Step 2: Create `server.rs` with the `IdAllocator` skeleton and failing tests**

```rust
use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU16;

use std::os::unix::net::UnixStream;
use std::time::Instant;

use yserver_protocol::x11::{ClientByteOrder, ClientId, ResourceId, SequenceNumber};

use crate::resources::ResourceTable;

pub const FIRST_CLIENT_BASE: u32 = 0x0010_0000;
pub const PER_CLIENT_MASK: u32 = 0x000F_FFFF;

#[derive(Debug)]
pub struct IdAllocator {
    next_base: u32,
}

impl IdAllocator {
    pub fn new() -> Self {
        Self { next_base: FIRST_CLIENT_BASE }
    }

    /// Returns `(resource_id_base, resource_id_mask)` for a new client.
    /// Returns `None` when the next base would overflow `u32`.
    pub fn allocate(&mut self) -> Option<(u32, u32)> {
        let base = self.next_base;
        // Each client owns the FIRST_CLIENT_BASE-sized window above `base`.
        let next = base.checked_add(FIRST_CLIENT_BASE)?;
        self.next_base = next;
        Some((base, PER_CLIENT_MASK))
    }

    /// `id` is owned by the holder of `(base, mask)` iff `(id & !mask) == base`.
    pub fn validate_owned(id: u32, base: u32, mask: u32) -> bool {
        (id & !mask) == base
    }
}

impl Default for IdAllocator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn first_client_base_is_above_root_resources() {
        let mut a = IdAllocator::new();
        let (base, mask) = a.allocate().expect("first allocate");
        assert_eq!(base, 0x0010_0000);
        assert_eq!(mask, 0x000F_FFFF);
    }

    #[test]
    fn allocate_increments_by_first_client_base() {
        let mut a = IdAllocator::new();
        let (b1, _) = a.allocate().unwrap();
        let (b2, _) = a.allocate().unwrap();
        assert_eq!(b2 - b1, FIRST_CLIENT_BASE);
    }

    #[test]
    fn validate_owned_accepts_ids_in_range() {
        let (base, mask) = (0x0020_0000, 0x000F_FFFF);
        assert!(IdAllocator::validate_owned(base, base, mask));
        assert!(IdAllocator::validate_owned(base | mask, base, mask));
        assert!(IdAllocator::validate_owned(base + 0x42, base, mask));
    }

    #[test]
    fn validate_owned_rejects_ids_outside_range() {
        let (base, mask) = (0x0020_0000, 0x000F_FFFF);
        assert!(!IdAllocator::validate_owned(0x0010_0000, base, mask));
        assert!(!IdAllocator::validate_owned(0x0030_0000, base, mask));
        assert!(!IdAllocator::validate_owned(0x0000_0100, base, mask));
    }

    proptest! {
        #[test]
        fn pairwise_non_overlap(n in 1usize..256) {
            let mut a = IdAllocator::new();
            let mut ranges = Vec::with_capacity(n);
            for _ in 0..n {
                ranges.push(a.allocate().expect("range"));
            }
            for (i, (b1, m1)) in ranges.iter().enumerate() {
                for (b2, m2) in ranges.iter().skip(i + 1) {
                    let lo1 = *b1;
                    let hi1 = b1 | m1;
                    let lo2 = *b2;
                    let hi2 = b2 | m2;
                    prop_assert!(hi1 < lo2 || hi2 < lo1, "overlap {:x}..={:x} vs {:x}..={:x}", lo1, hi1, lo2, hi2);
                }
            }
        }

        #[test]
        fn mask_covers_assigned_bits(n in 1usize..64) {
            let mut a = IdAllocator::new();
            for _ in 0..n {
                let (base, mask) = a.allocate().unwrap();
                prop_assert_eq!(base & mask, 0);
            }
        }

        #[test]
        fn allocated_bases_above_root_range(n in 1usize..64) {
            let mut a = IdAllocator::new();
            for _ in 0..n {
                let (base, _) = a.allocate().unwrap();
                prop_assert!(base >= 0x0010_0000);
            }
        }

        #[test]
        fn validate_round_trip(seed in 0u32..256, offset in 0u32..=PER_CLIENT_MASK) {
            let mut a = IdAllocator::new();
            // skip `seed` ranges, then probe.
            for _ in 0..seed { a.allocate().unwrap(); }
            let (base, mask) = a.allocate().unwrap();
            let id = base + offset;
            prop_assert!(IdAllocator::validate_owned(id, base, mask));
            // Any id outside the allocated window must be rejected.
            let other = base.wrapping_add(0x0010_0000).wrapping_add(offset);
            prop_assert!(!IdAllocator::validate_owned(other, base, mask));
        }
    }
}
```

- [ ] **Step 3: Run the tests; verify they pass**

```sh
cargo test -p yserver-core --lib server::tests
```

Expected: 4 unit tests + 4 property tests pass.

- [ ] **Step 4: Commit**

```sh
git add crates/yserver-core/src/lib.rs crates/yserver-core/src/server.rs
git commit -m "feat(server): add IdAllocator with per-client ID ranges

Hands out (base, mask) pairs starting at 0x0010_0000 so server-owned
IDs stay reserved below the first client range. Includes proptest
coverage for pairwise non-overlap and validate_owned round-trip."
```

### Task 1.3: Add `owner: ClientId` to every resource type

**Files:**
- Modify: `crates/yserver-core/src/resources.rs:14-394`

- [ ] **Step 1: Add `owner` field to `Window`, `Pixmap`, `Gc`, `Font`, `Cursor`**

Add `pub owner: ClientId,` to each struct. Add `use yserver_protocol::x11::ClientId;` at the top.

The root window is server-owned. We model that with a sentinel `SERVER_OWNER: ClientId = ClientId(0)` (no real client gets ID 0 — `NEXT_CLIENT_ID` starts at 1 in `nested.rs:27`).

Add at the top of `resources.rs`:

```rust
pub const SERVER_OWNER: ClientId = ClientId(0);
```

Set `owner: SERVER_OWNER` on the root window in `ResourceTable::new()`.

- [ ] **Step 2: Update every constructor / `entry().or_insert_with` site to thread an `owner`**

`ResourceTable` methods change signatures:

```rust
pub fn create_window(&mut self, owner: ClientId, request: CreateWindowRequest) { /* ... */ }
pub fn create_pixmap(&mut self, owner: ClientId, request: CreatePixmapRequest) { /* ... */ }
pub fn create_gc(&mut self, owner: ClientId, request: CreateGcRequest) { /* ... */ }
pub fn install_font(&mut self, owner: ClientId, id: ResourceId, name: String, host_xid: u32, metrics: FontMetrics) { /* ... */ }
pub fn create_glyph_cursor(&mut self, owner: ClientId, id: ResourceId) { /* ... */ }
```

In `create_window`, the `or_insert_with(|| Window::placeholder(request.parent))` site uses the existing `SERVER_OWNER` (the placeholder represents an as-yet-unseen parent — root or some not-yet-created window). Change `Window::placeholder` to accept and use `SERVER_OWNER`:

```rust
fn placeholder(id: ResourceId) -> Self {
    Self {
        id,
        parent: ROOT_WINDOW,
        // ...
        owner: SERVER_OWNER,
        cursor: None,
    }
}
```

In `change_gc`, the entry path also takes `or_insert(Gc { ... })`. That path requires an `owner` too — pass `SERVER_OWNER`. (This path only fires when a client changes a GC it never created, which shouldn't happen for well-behaved clients; we keep parity with existing tolerance.)

- [ ] **Step 3: Update `nested.rs` call sites to pass `client_id`**

Each `state.resources.create_*` call site in `nested.rs:382-892` passes `client_id` as the new first argument.

- [ ] **Step 4: Verify build**

```sh
cargo fmt
cargo clippy -- -W clippy::pedantic
cargo test
```

Expected: clean.

- [ ] **Step 5: Commit**

```sh
git add crates/yserver-core/src/resources.rs crates/yserver-core/src/nested.rs
git commit -m "feat(resources): track owner ClientId on every resource

Root resources are owned by SERVER_OWNER (ClientId(0), unreachable
for real clients). Lays groundwork for per-client cleanup on
disconnect under shared ServerState."
```

### Task 1.4: `ServerState` struct + migrate `ClientState` resource tables

`ClientState` becomes thin: it holds only the `client_id`, `byte_order`, and `last_sequence` (plus anything purely local). All resource state — windows, pixmaps, gcs, fonts, cursors, and atoms — moves into `ServerState`. Atoms are shared across clients now (X11 atoms are server-global by spec; the per-client atom namespace today is a known divergence — we fix it here).

**Files:**
- Modify: `crates/yserver-core/src/server.rs`
- Modify: `crates/yserver-core/src/nested.rs`

- [ ] **Step 1: Define `ServerState`, `AtomTable`, `ClientHandle`, `EventTarget` in `server.rs`**

Append to `server.rs`:

```rust
use std::sync::atomic::Ordering;

use yserver_protocol::x11::{self, AtomId};

use crate::resources::SERVER_OWNER;

#[derive(Debug, Default)]
pub struct AtomTable {
    by_name: HashMap<String, AtomId>,
    names: HashMap<u32, String>,
    next_id: u32,
}

impl AtomTable {
    pub fn new() -> Self {
        Self {
            by_name: HashMap::new(),
            names: HashMap::new(),
            next_id: 69, // matches the existing ClientState::new() default
        }
    }

    pub fn intern(&mut self, name: &str, only_if_exists: bool) -> AtomId {
        if let Some(atom) = x11::well_known_atom(name) {
            return atom;
        }
        if let Some(atom) = self.by_name.get(name).copied() {
            return atom;
        }
        if only_if_exists {
            return AtomId(0);
        }
        let atom = AtomId(self.next_id);
        self.next_id += 1;
        self.by_name.insert(name.to_owned(), atom);
        self.names.insert(atom.0, name.to_owned());
        atom
    }

    pub fn name(&self, atom: AtomId) -> Option<&str> {
        x11::well_known_atom_name(atom).or_else(|| self.names.get(&atom.0).map(String::as_str))
    }

    pub fn exists(&self, atom: AtomId) -> bool {
        atom.0 != 0
            && (x11::well_known_atom_name(atom).is_some() || self.names.contains_key(&atom.0))
    }
}

#[derive(Debug)]
pub struct ServerState {
    pub atoms: AtomTable,
    pub resources: ResourceTable,
    pub clients: HashMap<u32, ClientHandle>,
    pub id_allocator: IdAllocator,
    pub start_instant: Instant,
}

impl ServerState {
    pub fn new() -> Self {
        Self {
            atoms: AtomTable::new(),
            resources: ResourceTable::new(),
            clients: HashMap::new(),
            id_allocator: IdAllocator::new(),
            start_instant: Instant::now(),
        }
    }

    pub fn timestamp_now(&self) -> u32 {
        // X11 timestamps are 32-bit milliseconds; truncation is intentional.
        let elapsed = self.start_instant.elapsed().as_millis();
        (elapsed as u32) & 0xFFFF_FFFF
    }
}

impl Default for ServerState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub struct ClientHandle {
    pub writer: Arc<Mutex<UnixStream>>,
    pub byte_order: ClientByteOrder,
    pub last_sequence: Arc<AtomicU16>,
    pub resource_id_base: u32,
    pub resource_id_mask: u32,
    pub event_masks: HashMap<ResourceId, u32>,
}

#[derive(Clone)]
pub struct EventTarget {
    pub writer: Arc<Mutex<UnixStream>>,
    pub byte_order: ClientByteOrder,
    pub last_sequence: Arc<AtomicU16>,
}

#[allow(unused_imports)]
use crate::resources::SERVER_OWNER as _; // marker — referenced by callers
```

- [ ] **Step 2: Migrate `nested.rs` to construct and share `ServerState`**

In `run()` (`nested.rs:29`), build the shared state once:

```rust
let server = Arc::new(Mutex::new(crate::server::ServerState::new()));
```

Pass `server.clone()` into each spawned thread. Drop the static `NEXT_CLIENT_ID` — `IdAllocator` becomes the source of truth for `(base, mask)`. Client IDs themselves still need a separate counter; keep `NEXT_CLIENT_ID` for the `ClientId` value and replace `RESOURCE_ID_BASE` / `RESOURCE_ID_MASK` constants with values pulled from `id_allocator.allocate()` at connect time.

Update `handle_client`:

```rust
fn handle_client(
    client_id: ClientId,
    mut stream: UnixStream,
    server: Arc<Mutex<crate::server::ServerState>>,
    host: Option<Arc<Mutex<HostX11>>>,
    host_window_id: Option<u32>,
) -> io::Result<()> {
    let setup = x11::read_setup_request(&mut stream)?;
    if setup.byte_order != ClientByteOrder::LittleEndian {
        x11::write_setup_failed(
            &mut stream,
            setup.byte_order,
            "ynest currently supports only little-endian clients",
        )?;
        return Ok(());
    }

    let (resource_id_base, resource_id_mask) = {
        let mut guard = server
            .lock()
            .map_err(|_| io::Error::new(ErrorKind::BrokenPipe, "server state poisoned"))?;
        match guard.id_allocator.allocate() {
            Some(pair) => pair,
            None => {
                x11::write_setup_failed(
                    &mut stream,
                    setup.byte_order,
                    "ynest exhausted resource ID space",
                )?;
                return Ok(());
            }
        }
    };

    info!(
        "client {} setup: protocol {}.{}, base=0x{:x}",
        client_id.0, setup.protocol_major, setup.protocol_minor, resource_id_base
    );

    x11::write_setup_success(
        &mut stream,
        x11::SetupSuccess {
            // ... existing fields, with:
            resource_id_base,
            resource_id_mask,
            // ...
        },
    )?;

    // Wrap the writer and register the ClientHandle BEFORE the request loop.
    // `BadIDChoice` validation in Task 1.5 reads `clients[client_id]` to get
    // (resource_id_base, resource_id_mask), so the entry must exist as soon
    // as the first Create* request arrives. The `event_masks` field is
    // populated lazily in Stage 2; it stays an empty HashMap until then.
    let mut reader = stream.try_clone()?;
    let writer = Arc::new(Mutex::new(stream));
    let last_sequence = Arc::new(AtomicU16::new(0));
    {
        let mut s = lock_server(&server)?;
        s.clients.insert(
            client_id.0,
            crate::server::ClientHandle {
                writer: writer.clone(),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: last_sequence.clone(),
                resource_id_base,
                resource_id_mask,
                event_masks: HashMap::new(),
            },
        );
    }
    // ... rest of function (focused_window, keyboard forwarder, request loop)
}
```

`ClientState` shrinks. Remove `atoms_by_name`, `atom_names`, `resources`, `next_atom_id`. Replace every `state.resources.*` and `state.intern_atom` / `state.atom_name` call with the locked-server equivalent. Establish a helper:

```rust
fn lock_server<'a>(
    server: &'a Mutex<crate::server::ServerState>,
) -> io::Result<std::sync::MutexGuard<'a, crate::server::ServerState>> {
    server
        .lock()
        .map_err(|_| io::Error::new(ErrorKind::BrokenPipe, "server state poisoned"))
}
```

Each handler in `handle_request` locks `server` for the duration of its update, performs the mutation, drops the lock, then performs any I/O. Example for opcode 1 (CreateWindow):

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
        let mut s = lock_server(server)?;
        s.resources.create_window(client_id, request);
    }
    log_void(client_id, sequence, "CreateWindow")
}
```

`focus_if_window_wants_keys` reads `state.resources` today; it needs a `&ServerState` borrow now. Pass it the locked guard (or refactor it into a function that takes the values it needs after locking).

Until Stage 2, leave `Window.event_mask` in place; it gets removed in Task 2.2.

- [ ] **Step 3: Verify build, lint, smoke**

```sh
cargo fmt
cargo clippy -- -W clippy::pedantic
cargo test
```

Expected: clean.

```sh
DISPLAY=:0 cargo run --bin ynest -- 42 &
DISPLAY=:42 xterm
```

Expected: same as Stage 0.

- [ ] **Step 4: Commit**

```sh
git add crates/yserver-core/src/server.rs crates/yserver-core/src/nested.rs
git commit -m "feat(server): introduce ServerState with shared resources and atoms

Resource tables and atoms move from per-thread ClientState into a
single Arc<Mutex<ServerState>> shared across client threads. Each
client's resource ID range now comes from the IdAllocator. No
behavior change visible to single-client scenarios."
```

### Task 1.5: `BadIDChoice` validation on every `Create*`

**Files:**
- Modify: `crates/yserver-protocol/src/x11.rs` — add `pub mod error { ... }` constants
- Modify: `crates/yserver-core/src/nested.rs` — add validator + emit errors

- [ ] **Step 1: Add error code constants**

Append to `crates/yserver-protocol/src/x11.rs`:

```rust
pub mod error {
    pub const BAD_REQUEST:    u8 = 1;
    pub const BAD_VALUE:      u8 = 2;
    pub const BAD_WINDOW:     u8 = 3;
    pub const BAD_ATOM:       u8 = 5;
    pub const BAD_MATCH:      u8 = 8;
    pub const BAD_ALLOC:      u8 = 11;
    pub const BAD_ID_CHOICE:  u8 = 14;
    pub const BAD_LENGTH:     u8 = 16;
}
```

- [ ] **Step 2: Add an `emit_x11_error` helper in nested.rs**

```rust
fn emit_x11_error(
    writer: &Arc<Mutex<UnixStream>>,
    sequence: SequenceNumber,
    code: u8,
    bad_value: u32,
    major_opcode: u8,
) -> io::Result<()> {
    let mut w = writer
        .lock()
        .map_err(|_| io::Error::new(ErrorKind::BrokenPipe, "client writer mutex poisoned"))?;
    x11::write_error(&mut *w, sequence, code, bad_value, 0, major_opcode)
}
```

- [ ] **Step 3: Validate in each `Create*` handler before mutation**

Pattern (CreateWindow shown; CreatePixmap/CreateGC/OpenFont/CreateGlyphCursor follow the same shape with their respective ID fields and resource tables):

```rust
1 => {
    if let Some(request) = x11::create_window_request(header.data, body) {
        let new_id = request.window.0;
        let mut s = lock_server(server)?;
        let owned = crate::server::IdAllocator::validate_owned(
            new_id,
            s.clients[&client_id.0].resource_id_base,
            s.clients[&client_id.0].resource_id_mask,
        );
        let in_use = s.resources.any_resource_exists(request.window);
        if !owned || in_use {
            drop(s);
            return emit_x11_error(writer, sequence, x11::error::BAD_ID_CHOICE, new_id, 1);
        }
        s.resources.create_window(client_id, request);
    }
    log_void(client_id, sequence, "CreateWindow")
}
```

Add `any_resource_exists(ResourceId) -> bool` to `ResourceTable` that checks all five maps.

- [ ] **Step 4: Verify and commit**

```sh
cargo fmt && cargo clippy -- -W clippy::pedantic && cargo test
```

```sh
git add crates/yserver-protocol/src/x11.rs crates/yserver-core/src/resources.rs crates/yserver-core/src/nested.rs
git commit -m "feat(nested): emit BadIDChoice for out-of-range or duplicate IDs

Every Create* (CreateWindow, CreatePixmap, CreateGC, OpenFont,
CreateGlyphCursor) now validates the proposed resource ID against
the caller's allocated range and rejects duplicates across all
five tables."
```

### Task 1.6: Disconnect cleanup walks all resource tables

**Files:**
- Modify: `crates/yserver-core/src/resources.rs` — add `collect_owned_window_roots` + `remove_non_window_resources_owned_by`
- Modify: `crates/yserver-core/src/nested.rs` — call cleanup at end of `handle_client`

The full-fledged disconnect path with `DestroyNotify` emission lands in Stage 5; this task installs the cleanup *hook* itself, including a proper subtree destroy so parent/child links never go stale (a flat `windows.retain(|_, w| w.owner != client)` would leave orphaned `Window.children` entries in surviving parents owned by other clients).

- [ ] **Step 1: Add `ResourceTable` accessors needed by cleanup**

```rust
impl ResourceTable {
    /// Top-level windows owned by `client`: windows whose parent is *not*
    /// owned by the same client. Reachable descendants (regardless of
    /// owner) get destroyed transitively when each root is destroyed.
    pub fn collect_owned_window_roots(
        &self,
        client: ClientId,
        out: &mut Vec<ResourceId>,
    ) {
        for w in self.windows.values() {
            if w.owner != client {
                continue;
            }
            let parent_owner = self.windows.get(&w.parent.0).map(|p| p.owner);
            if parent_owner != Some(client) {
                out.push(w.id);
            }
        }
    }

    /// Remove every non-window resource owned by `client`. Returns the
    /// host_xid of every removed font so the caller can issue host-side
    /// `CloseFont` after dropping the ServerState lock.
    pub fn remove_non_window_resources_owned_by(&mut self, client: ClientId) -> Vec<u32> {
        self.pixmaps.retain(|_, p| p.owner != client);
        self.gcs.retain(|_, g| g.owner != client);
        self.cursors.retain(|_, c| c.owner != client);
        let mut closed_fonts = Vec::new();
        self.fonts.retain(|_, f| {
            if f.owner == client {
                closed_fonts.push(f.host_xid);
                false
            } else {
                true
            }
        });
        closed_fonts
    }
}
```

(`destroy_window` already exists on `ResourceTable` and recursively unlinks children from parents — Task 2.5 changes its signature to return the destroyed-window list, but for Stage 1 the existing recursive behavior is enough.)

- [ ] **Step 2: Drive cleanup at end of `handle_client`**

Wrap the request loop so cleanup always runs, even on `?`-propagated errors:

```rust
let result: io::Result<()> = (|| {
    loop {
        let Some((header, body)) = x11::read_request(&mut reader)? else {
            return Ok(());
        };
        sequence = sequence.next();
        last_sequence.store(sequence.0, Ordering::Relaxed);
        handle_request(
            client_id,
            &mut state,
            host.as_ref(),
            &server,
            &writer,
            &focused_window,
            sequence,
            header,
            &body,
        )?;
    }
})();

let closed_fonts = {
    let mut s = lock_server(&server)?;
    let mut roots = Vec::new();
    s.resources.collect_owned_window_roots(client_id, &mut roots);
    for root in roots {
        s.resources.destroy_window(root);
    }
    let fonts = s.resources.remove_non_window_resources_owned_by(client_id);
    s.clients.remove(&client_id.0);
    fonts
};
if let Some(host) = host.as_ref() {
    if let Ok(mut h) = host.lock() {
        for xid in closed_fonts {
            let _ = h.close_font(xid);
        }
    }
}
result
```

`s.clients.remove(&client_id.0)` is safe here because `ClientHandle` registration lands in Task 1.4. `DestroyNotify` emission for the destroyed subtrees is added on top of this scaffolding in Task 5.3 — Stage 1 still leaves ResourceTable internally consistent.

- [ ] **Step 3: Verify build and commit**

```sh
cargo fmt && cargo clippy -- -W clippy::pedantic && cargo test
```

```sh
git add crates/yserver-core/src/resources.rs crates/yserver-core/src/server.rs crates/yserver-core/src/nested.rs
git commit -m "feat(server): drop a client's resources on disconnect

Walks every resource table on disconnect and removes entries owned
by the departing client. Fonts also queue host-side CloseFont
calls, executed after the ServerState lock is released."
```

---

## Stage 2 — Per-(client, window) event masks, `subscribers()`

Switch every event-emission site to the new `subscribers()` + `emit_window_event()` primitive. Replace `Window.event_mask` with per-`ClientHandle` `event_masks`. (`ClientHandle` registration on connect and removal on disconnect already landed in Stage 1, since Task 1.5's `BadIDChoice` validation depends on the `clients` map being populated.)

### Task 2.1: (absorbed into Stage 1)

Client registration on connect and removal on disconnect ship in Task 1.4 and Task 1.6 respectively. This task is intentionally empty and exists only as an anchor so later task numbers stay stable across review iterations. Skip to Task 2.2.

### Task 2.2: Replace `Window.event_mask` with per-client `event_masks`

**Files:**
- Modify: `crates/yserver-core/src/resources.rs` — drop `event_mask` field on `Window`
- Modify: `crates/yserver-core/src/nested.rs` — update `ChangeWindowAttributes`, `CreateWindow`, `GetWindowAttributes`, `focus_if_window_wants_keys`

- [ ] **Step 1: Drop `event_mask: u32` from `Window`**

Remove the field from the struct, the constructor in `create_window`, the `placeholder` builder, and the root-window builder. Remove the assignment branch in `change_window_attributes` (the `event_mask` path now lives in `nested.rs`).

`change_window_attributes` keeps the `background_pixel` and `cursor` paths.

- [ ] **Step 2: Update `CreateWindow` handler to seed event_mask in `ClientHandle`**

```rust
1 => {
    if let Some(request) = x11::create_window_request(header.data, body) {
        let new_id = request.window.0;
        let mask = request.event_mask.unwrap_or(0);
        let mut s = lock_server(server)?;
        let handle = s.clients.get(&client_id.0).expect("client registered");
        let owned = crate::server::IdAllocator::validate_owned(
            new_id, handle.resource_id_base, handle.resource_id_mask,
        );
        let in_use = s.resources.any_resource_exists(request.window);
        if !owned || in_use {
            drop(s);
            return emit_x11_error(writer, sequence, x11::error::BAD_ID_CHOICE, new_id, 1);
        }
        s.resources.create_window(client_id, request);
        if mask != 0 {
            s.clients.get_mut(&client_id.0).unwrap().event_masks.insert(request.window, mask);
        }
    }
    log_void(client_id, sequence, "CreateWindow")
}
```

- [ ] **Step 3: Update `ChangeWindowAttributes` handler**

```rust
2 => {
    if let Some(request) = x11::change_window_attributes_request(body) {
        let mut s = lock_server(server)?;
        if let Some(event_mask) = request.event_mask {
            let entry = s.clients.get_mut(&client_id.0).expect("client registered");
            if event_mask == 0 {
                entry.event_masks.remove(&request.window);
            } else {
                entry.event_masks.insert(request.window, event_mask);
            }
        }
        s.resources.change_window_attributes(request);
        // focus refresh below uses the per-client mask, see Step 4
        let want_focus = s.clients[&client_id.0]
            .event_masks
            .get(&request.window)
            .copied()
            .unwrap_or(0);
        let viewable = s.resources.window(request.window).is_some_and(|w| w.map_state == MapState::Viewable);
        drop(s);
        if viewable && want_focus & 0x3 != 0 {
            set_focused_window(focused_window, writer, sequence, request.window)?;
        }
    }
    log_void(client_id, sequence, "ChangeWindowAttributes")
}
```

- [ ] **Step 4: Drop `focus_if_window_wants_keys` — inline its logic in callers using per-client mask**

The two remaining callers in `nested.rs` (opcode 1 CreateWindow and opcode 8 MapWindow / opcode 9 MapSubwindows) get an inline check that mirrors the snippet above. Pattern: lock server → look up `clients[client_id].event_masks.get(window)` → if `KeyPress|KeyRelease` set and viewable, focus.

- [ ] **Step 5: `GetWindowAttributes` derives masks**

```rust
3 => {
    log_reply(client_id, sequence, "GetWindowAttributes");
    let s = lock_server(server)?;
    let id = x11::drawable_request_id(body).unwrap_or(ROOT_WINDOW);
    let target = if s.resources.window(id).is_some() { id } else { ROOT_WINDOW };
    let your_event_mask = s
        .clients
        .get(&client_id.0)
        .and_then(|c| c.event_masks.get(&target).copied())
        .unwrap_or(0);
    let all_event_masks: u32 = s
        .clients
        .values()
        .filter_map(|c| c.event_masks.get(&target).copied())
        .fold(0u32, |a, b| a | b);
    let attrs = window_attributes(s.resources.window(target), all_event_masks, your_event_mask);
    drop(s);
    let mut w = writer.lock().map_err(|_| io::Error::new(ErrorKind::BrokenPipe, "writer poisoned"))?;
    x11::write_get_window_attributes_reply(&mut *w, sequence, attrs)
}
```

Update `window_attributes()` (`nested.rs:971`) to accept the two derived masks instead of reading `window.event_mask`.

- [ ] **Step 6: `current_input_masks` at setup-reply time**

In `handle_client`, before writing setup_success, compute:

```rust
let current_input_masks: u32 = {
    let s = lock_server(&server)?;
    s.clients
        .values()
        .filter_map(|c| c.event_masks.get(&ROOT_WINDOW).copied())
        .fold(0u32, |a, b| a | b)
};
```

Pass it into the `Screen` literal. (At first-client-connect time this is always 0; when later clients connect, prior root-window selections appear in the OR.)

- [ ] **Step 7: Verify and commit**

```sh
cargo fmt && cargo clippy -- -W clippy::pedantic && cargo test
git add crates/yserver-core/src/resources.rs crates/yserver-core/src/nested.rs
git commit -m "feat(nested): move event_mask from Window to per-client ClientHandle

Each client now tracks its own selection on every window. Replies
that need a mask (GetWindowAttributes.your_event_mask /
.all_event_masks, current_input_masks) compute the value on
demand. Fixes a latent last-writer-wins bug between clients."
```

### Task 2.3: `subscribers()` and `emit_window_event` (TDD)

**Files:**
- Modify: `crates/yserver-core/src/server.rs`

- [ ] **Step 1: Failing tests first**

Add to the `tests` module of `server.rs`:

```rust
#[test]
fn subscribers_returns_clients_with_bit_set() {
    let mut state = ServerState::new();
    let writer_a = make_test_writer();
    let writer_b = make_test_writer();
    let seq_a = Arc::new(AtomicU16::new(0));
    let seq_b = Arc::new(AtomicU16::new(0));
    state.clients.insert(1, ClientHandle {
        writer: writer_a, byte_order: ClientByteOrder::LittleEndian,
        last_sequence: seq_a, resource_id_base: 0x0010_0000, resource_id_mask: 0x000F_FFFF,
        event_masks: HashMap::from([(ResourceId(0x100), 0x0040_0000)]),
    });
    state.clients.insert(2, ClientHandle {
        writer: writer_b, byte_order: ClientByteOrder::LittleEndian,
        last_sequence: seq_b, resource_id_base: 0x0020_0000, resource_id_mask: 0x000F_FFFF,
        event_masks: HashMap::from([(ResourceId(0x100), 0x0000_0001)]),
    });
    // PropertyChange = 0x0040_0000
    let subs = state.subscribers(ResourceId(0x100), 0x0040_0000);
    assert_eq!(subs.len(), 1);
}

#[test]
fn subscribers_omits_other_windows() {
    let mut state = ServerState::new();
    state.clients.insert(1, ClientHandle {
        writer: make_test_writer(), byte_order: ClientByteOrder::LittleEndian,
        last_sequence: Arc::new(AtomicU16::new(0)),
        resource_id_base: 0x0010_0000, resource_id_mask: 0x000F_FFFF,
        event_masks: HashMap::from([(ResourceId(0x200), 0xFFFF_FFFF)]),
    });
    let subs = state.subscribers(ResourceId(0x100), 0x0040_0000);
    assert!(subs.is_empty());
}

#[test]
fn subscribers_omits_disconnected_client() {
    let mut state = ServerState::new();
    state.clients.insert(1, ClientHandle {
        writer: make_test_writer(), byte_order: ClientByteOrder::LittleEndian,
        last_sequence: Arc::new(AtomicU16::new(0)),
        resource_id_base: 0x0010_0000, resource_id_mask: 0x000F_FFFF,
        event_masks: HashMap::from([(ResourceId(0x100), 0x0040_0000)]),
    });
    assert_eq!(state.subscribers(ResourceId(0x100), 0x0040_0000).len(), 1);
    // Simulate disconnect — the cleanup path in `handle_client` removes
    // the ClientHandle (which drops the client's event_masks with it).
    state.clients.remove(&1);
    assert!(state.subscribers(ResourceId(0x100), 0x0040_0000).is_empty());
}

fn make_test_writer() -> Arc<Mutex<UnixStream>> {
    // socketpair lets us construct a real UnixStream without a listener.
    let (a, _b) = UnixStream::pair().expect("socketpair");
    Arc::new(Mutex::new(a))
}
```

(The companion test for `drop_window_subscriptions` lands in Task 2.5, which is where the function itself is added.)

Run `cargo test -p yserver-core --lib server::tests::subscribers_returns_clients_with_bit_set`. Expected: FAIL (function doesn't exist).

- [ ] **Step 2: Implement `subscribers()` and `emit_window_event()`**

```rust
impl ServerState {
    pub fn subscribers(&self, window: ResourceId, mask_bit: u32) -> Vec<EventTarget> {
        self.clients
            .values()
            .filter_map(|c| {
                let mask = c.event_masks.get(&window).copied().unwrap_or(0);
                if mask & mask_bit != 0 {
                    Some(EventTarget {
                        writer: c.writer.clone(),
                        byte_order: c.byte_order,
                        last_sequence: c.last_sequence.clone(),
                    })
                } else {
                    None
                }
            })
            .collect()
    }
}

pub fn emit_window_event(
    state: &Mutex<ServerState>,
    window: ResourceId,
    mask_bit: u32,
    encode: impl Fn(&mut Vec<u8>, SequenceNumber, ClientByteOrder),
) {
    let targets = match state.lock() {
        Ok(g) => g.subscribers(window, mask_bit),
        Err(_) => return,
    };
    for target in targets {
        let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
        let mut buf = Vec::with_capacity(32);
        encode(&mut buf, seq, target.byte_order);
        if let Ok(mut w) = target.writer.lock() {
            let _ = w.write_all(&buf);
        }
    }
}
```

Add `use std::io::Write;` to `server.rs`.

- [ ] **Step 3: Re-run tests; expect green**

```sh
cargo test -p yserver-core --lib server::tests
```

- [ ] **Step 4: Commit**

```sh
git add crates/yserver-core/src/server.rs
git commit -m "feat(server): add subscribers() and emit_window_event router

emit_window_event snapshots subscribers under the ServerState
lock, releases the lock, then writes to each subscriber's writer
serially. Per-target writer locks are non-reentrant and brief; the
request loop never holds the writer lock at this point."
```

### Task 2.4: Migrate existing event sites to `emit_window_event`

`nested.rs` currently writes events directly: `Expose` (opcodes 8, 9), `MapNotify` (opcode 8), `ConfigureNotify` (opcode 12), `FocusIn`/`FocusOut` (`set_focused_window`).

**Files:**
- Modify: `crates/yserver-core/src/nested.rs`

For each site, replace direct `x11::write_*_event(writer, sequence, ...)` with a call through `emit_window_event` keyed on the appropriate event mask bit. Bit values per X11 spec:

| Event | Mask bit |
|---|---|
| Expose | 0x0000_8000 |
| MapNotify | 0x0002_0000 (StructureNotify) |
| ConfigureNotify | 0x0002_0000 (StructureNotify) |
| FocusIn / FocusOut | 0x0020_0000 (FocusChange) |
| PropertyNotify | 0x0040_0000 (PropertyChange) |
| DestroyNotify on the window | 0x0002_0000 (StructureNotify) |
| DestroyNotify on the parent | 0x0008_0000 (SubstructureNotify) |

- [ ] **Step 1: `Expose` migration**

For opcode 8 (MapWindow) and 9 (MapSubwindows), replace `x11::write_expose_event(stream, sequence, ...)` with:

```rust
crate::server::emit_window_event(server, window, 0x0000_8000, |buf, seq, order| {
    let _ = x11::encode_expose_event(buf, seq, order, window, width, height);
});
```

This requires renaming/refactoring the existing writer functions to "encode" variants that take `&mut Vec<u8>` and a `ClientByteOrder` instead of a `&mut impl Write`. Add new `encode_*` thin wrappers that share the body:

```rust
pub fn encode_expose_event(
    out: &mut Vec<u8>,
    sequence: SequenceNumber,
    order: ClientByteOrder,
    window: ResourceId,
    width: u16,
    height: u16,
) {
    out.push(12); // Expose
    out.push(0);
    write_u16(order, out, sequence.0);
    write_u32(order, out, window.0);
    write_u16(order, out, 0);
    write_u16(order, out, 0);
    write_u16(order, out, width);
    write_u16(order, out, height);
    write_u16(order, out, 0); // count
    out.extend_from_slice(&[0; 14]);
}
```

Keep `write_expose_event` as a thin wrapper for places that still need a streaming write — for now there are none after this refactor, so feel free to delete it.

- [ ] **Step 2: `MapNotify` migration**

Same pattern: introduce `encode_map_notify_event` and route through `emit_window_event(server, window, 0x0002_0000, ...)`.

- [ ] **Step 3: `ConfigureNotify` migration**

Introduce `encode_configure_notify_event`. Route through `emit_window_event(server, window, 0x0002_0000, ...)`.

- [ ] **Step 4: `FocusIn` / `FocusOut` migration**

`set_focused_window` today writes directly to the issuing client's writer. With per-client masks, the focus events should fan out to every subscriber of the focused/unfocused window with `FocusChange` selected — typically just the client that selected it, but cross-client is now possible.

Introduce `encode_focus_event(out, sequence, order, focus_in, window)`. Replace the two `x11::write_focus_event` calls in `set_focused_window` with `emit_window_event(server, focus_target, 0x0020_0000, ...)`.

This means `set_focused_window` now needs `&Mutex<ServerState>` instead of `&Arc<Mutex<UnixStream>>`. Update its signature and every caller.

- [ ] **Step 5: Verify and commit**

```sh
cargo fmt && cargo clippy -- -W clippy::pedantic && cargo test
git add crates/yserver-core/src/nested.rs crates/yserver-protocol/src/x11.rs
git commit -m "refactor(nested): route window events through emit_window_event

Expose, MapNotify, ConfigureNotify, FocusIn/Out now go through the
shared subscribers() router. Per-client event_masks govern
delivery; behavior is unchanged for single-client use."
```

### Task 2.5: `event_masks` cleanup on `DestroyWindow`

**Files:**
- Modify: `crates/yserver-core/src/server.rs` — add helper
- Modify: `crates/yserver-core/src/resources.rs` — return destroyed IDs from `destroy_window`
- Modify: `crates/yserver-core/src/nested.rs` — wire it up

- [ ] **Step 1: `ResourceTable::destroy_window` returns destroyed IDs**

Change the signature (currently `pub fn destroy_window(&mut self, id: ResourceId)`):

```rust
pub fn destroy_window(&mut self, id: ResourceId) -> Vec<ResourceId> {
    let mut destroyed = Vec::new();
    self.destroy_window_inner(id, &mut destroyed);
    destroyed
}

fn destroy_window_inner(&mut self, id: ResourceId, destroyed: &mut Vec<ResourceId>) {
    let Some(window) = self.windows.remove(&id.0) else { return; };
    if let Some(parent) = self.windows.get_mut(&window.parent.0) {
        parent.children.retain(|child| *child != id);
    }
    destroyed.push(id);
    for child in window.children {
        self.destroy_window_inner(child, destroyed);
    }
}
```

- [ ] **Step 2: `ServerState::drop_window_subscriptions` helper + test**

```rust
impl ServerState {
    pub fn drop_window_subscriptions(&mut self, windows: &[ResourceId]) {
        for client in self.clients.values_mut() {
            for w in windows {
                client.event_masks.remove(w);
            }
        }
    }
}
```

Add a unit test in `server.rs::tests` that exercises the helper directly:

```rust
#[test]
fn drop_window_subscriptions_removes_entries_for_destroyed_windows() {
    let mut state = ServerState::new();
    state.clients.insert(1, ClientHandle {
        writer: make_test_writer(), byte_order: ClientByteOrder::LittleEndian,
        last_sequence: Arc::new(AtomicU16::new(0)),
        resource_id_base: 0x0010_0000, resource_id_mask: 0x000F_FFFF,
        event_masks: HashMap::from([
            (ResourceId(0x100), 0x0040_0000),
            (ResourceId(0x200), 0x0040_0000),
        ]),
    });
    assert_eq!(state.subscribers(ResourceId(0x100), 0x0040_0000).len(), 1);
    state.drop_window_subscriptions(&[ResourceId(0x100)]);
    assert!(state.subscribers(ResourceId(0x100), 0x0040_0000).is_empty());
    // Surviving window's subscription stays.
    assert_eq!(state.subscribers(ResourceId(0x200), 0x0040_0000).len(), 1);
}
```

- [ ] **Step 3: Update opcode 4 (DestroyWindow) handler**

```rust
4 => {
    if let Some(window) = x11::free_resource_id(body) {
        let mut s = lock_server(server)?;
        let destroyed = s.resources.destroy_window(window);
        s.drop_window_subscriptions(&destroyed);
        // DestroyNotify emission lands in Stage 5; for now just drop
        // the subscriptions so future events don't mis-route.
    }
    log_void(client_id, sequence, "DestroyWindow")
}
```

- [ ] **Step 4: Verify and commit**

```sh
cargo fmt && cargo clippy -- -W clippy::pedantic && cargo test
git add crates/yserver-core/src/server.rs crates/yserver-core/src/resources.rs crates/yserver-core/src/nested.rs
git commit -m "feat(nested): drop event_masks entries on DestroyWindow

When a window subtree is destroyed, every client's event_masks
entry keyed on a destroyed window is removed so future events
can't mis-route into reused IDs."
```

---

## Stage 3 — `properties.rs` (pure)

Add the pure types and helpers. No locking, no I/O, no wiring. Full unit + proptest coverage.

### Task 3.1: `PropertyValue`, `PropertyFormat`, `ChangeMode`, `ChangePropertyError`

**Files:**
- Create: `crates/yserver-core/src/properties.rs`
- Modify: `crates/yserver-core/src/lib.rs`

- [ ] **Step 1: Add module declaration to `lib.rs`**

```rust
pub mod properties;
```

- [ ] **Step 2: Create `properties.rs` with types and constants**

```rust
use yserver_protocol::x11::AtomId;

pub const MAX_PROPERTY_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PropertyValue {
    pub r#type: AtomId,
    pub format: PropertyFormat,
    pub data: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PropertyFormat { F8, F16, F32 }

impl PropertyFormat {
    pub fn from_protocol(v: u8) -> Option<Self> {
        match v {
            8 => Some(Self::F8),
            16 => Some(Self::F16),
            32 => Some(Self::F32),
            _ => None,
        }
    }
    pub fn bytes(self) -> usize {
        match self { Self::F8 => 1, Self::F16 => 2, Self::F32 => 4 }
    }
    pub fn protocol_value(self) -> u8 {
        match self { Self::F8 => 8, Self::F16 => 16, Self::F32 => 32 }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChangeMode { Replace, Prepend, Append }

impl ChangeMode {
    pub fn from_protocol(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Replace),
            1 => Some(Self::Prepend),
            2 => Some(Self::Append),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChangePropertyError { BadValue, BadMatch, BadAlloc }
```

- [ ] **Step 3: Verify and commit**

```sh
cargo build && cargo clippy -- -W clippy::pedantic
git add crates/yserver-core/src/lib.rs crates/yserver-core/src/properties.rs
git commit -m "feat(properties): add PropertyValue, PropertyFormat, ChangeMode types"
```

### Task 3.2: `apply_change` (TDD)

**Files:**
- Modify: `crates/yserver-core/src/properties.rs`

- [ ] **Step 1: Add failing unit tests**

Append to `properties.rs`:

```rust
#[cfg(test)]
mod apply_change_tests {
    use super::*;
    use yserver_protocol::x11::AtomId;

    fn val(t: u32, f: PropertyFormat, data: Vec<u8>) -> PropertyValue {
        PropertyValue { r#type: AtomId(t), format: f, data }
    }

    #[test]
    fn replace_on_empty() {
        let result = apply_change(None, ChangeMode::Replace, AtomId(31), PropertyFormat::F8, b"hello").unwrap();
        assert_eq!(result, val(31, PropertyFormat::F8, b"hello".to_vec()));
    }

    #[test]
    fn replace_ignores_existing_type_and_format() {
        let existing = val(31, PropertyFormat::F8, b"old".to_vec());
        let result = apply_change(Some(&existing), ChangeMode::Replace, AtomId(4), PropertyFormat::F32, &[1,2,3,4]).unwrap();
        assert_eq!(result, val(4, PropertyFormat::F32, vec![1,2,3,4]));
    }

    #[test]
    fn append_on_empty_acts_like_replace() {
        let result = apply_change(None, ChangeMode::Append, AtomId(31), PropertyFormat::F8, b"hi").unwrap();
        assert_eq!(result.data, b"hi".to_vec());
    }

    #[test]
    fn prepend_on_empty_acts_like_replace() {
        let result = apply_change(None, ChangeMode::Prepend, AtomId(31), PropertyFormat::F8, b"hi").unwrap();
        assert_eq!(result.data, b"hi".to_vec());
    }

    #[test]
    fn append_concatenates() {
        let existing = val(31, PropertyFormat::F8, b"hello ".to_vec());
        let result = apply_change(Some(&existing), ChangeMode::Append, AtomId(31), PropertyFormat::F8, b"world").unwrap();
        assert_eq!(result.data, b"hello world".to_vec());
    }

    #[test]
    fn prepend_concatenates() {
        let existing = val(31, PropertyFormat::F8, b"world".to_vec());
        let result = apply_change(Some(&existing), ChangeMode::Prepend, AtomId(31), PropertyFormat::F8, b"hello ").unwrap();
        assert_eq!(result.data, b"hello world".to_vec());
    }

    #[test]
    fn append_type_mismatch_is_bad_match() {
        let existing = val(31, PropertyFormat::F8, b"hi".to_vec());
        let err = apply_change(Some(&existing), ChangeMode::Append, AtomId(4), PropertyFormat::F8, b"yo").unwrap_err();
        assert_eq!(err, ChangePropertyError::BadMatch);
    }

    #[test]
    fn append_format_mismatch_is_bad_match() {
        let existing = val(31, PropertyFormat::F8, b"hi".to_vec());
        let err = apply_change(Some(&existing), ChangeMode::Append, AtomId(31), PropertyFormat::F32, &[1,2,3,4]).unwrap_err();
        assert_eq!(err, ChangePropertyError::BadMatch);
    }

    #[test]
    fn replace_at_max_succeeds() {
        let data = vec![0u8; MAX_PROPERTY_BYTES];
        let result = apply_change(None, ChangeMode::Replace, AtomId(31), PropertyFormat::F8, &data).unwrap();
        assert_eq!(result.data.len(), MAX_PROPERTY_BYTES);
    }

    #[test]
    fn replace_above_max_is_bad_alloc() {
        let data = vec![0u8; MAX_PROPERTY_BYTES + 1];
        let err = apply_change(None, ChangeMode::Replace, AtomId(31), PropertyFormat::F8, &data).unwrap_err();
        assert_eq!(err, ChangePropertyError::BadAlloc);
    }

    // Spec: "MAX boundary: existing.len() + new.len() ≤ MAX ⇒ Ok; > MAX ⇒
    // Err(BadAlloc). Probe at the edge." Each test allocates ~64 MiB once,
    // which is the only practical way to probe the real constant.
    #[test]
    fn append_at_cumulative_max_succeeds() {
        let existing = val(31, PropertyFormat::F8, vec![0u8; MAX_PROPERTY_BYTES - 1]);
        let result = apply_change(Some(&existing), ChangeMode::Append, AtomId(31), PropertyFormat::F8, &[0]).unwrap();
        assert_eq!(result.data.len(), MAX_PROPERTY_BYTES);
    }

    #[test]
    fn append_above_cumulative_max_is_bad_alloc() {
        let existing = val(31, PropertyFormat::F8, vec![0u8; MAX_PROPERTY_BYTES]);
        let err = apply_change(Some(&existing), ChangeMode::Append, AtomId(31), PropertyFormat::F8, &[0]).unwrap_err();
        assert_eq!(err, ChangePropertyError::BadAlloc);
    }

    #[test]
    fn prepend_at_cumulative_max_succeeds() {
        let existing = val(31, PropertyFormat::F8, vec![0u8; MAX_PROPERTY_BYTES - 1]);
        let result = apply_change(Some(&existing), ChangeMode::Prepend, AtomId(31), PropertyFormat::F8, &[0]).unwrap();
        assert_eq!(result.data.len(), MAX_PROPERTY_BYTES);
    }
}
```

Run: `cargo test -p yserver-core --lib properties::apply_change_tests`. Expected: FAIL (`apply_change` not defined).

- [ ] **Step 2: Implement `apply_change`**

```rust
pub fn apply_change(
    existing: Option<&PropertyValue>,
    mode: ChangeMode,
    new_type: AtomId,
    format: PropertyFormat,
    data: &[u8],
) -> Result<PropertyValue, ChangePropertyError> {
    let combined: Vec<u8> = match (mode, existing) {
        (ChangeMode::Replace, _) | (_, None) => data.to_vec(),
        (ChangeMode::Prepend, Some(v)) | (ChangeMode::Append, Some(v)) => {
            if v.r#type != new_type || v.format != format {
                return Err(ChangePropertyError::BadMatch);
            }
            let mut combined = Vec::with_capacity(v.data.len() + data.len());
            match mode {
                ChangeMode::Prepend => {
                    combined.extend_from_slice(data);
                    combined.extend_from_slice(&v.data);
                }
                ChangeMode::Append => {
                    combined.extend_from_slice(&v.data);
                    combined.extend_from_slice(data);
                }
                ChangeMode::Replace => unreachable!(),
            }
            combined
        }
    };
    if combined.len() > MAX_PROPERTY_BYTES {
        return Err(ChangePropertyError::BadAlloc);
    }
    Ok(PropertyValue { r#type: new_type, format, data: combined })
}
```

Run tests; expect green.

- [ ] **Step 3: Add proptest coverage**

```rust
#[cfg(test)]
mod apply_change_props {
    use super::*;
    use proptest::prelude::*;

    fn arb_format() -> impl Strategy<Value = PropertyFormat> {
        prop_oneof![Just(PropertyFormat::F8), Just(PropertyFormat::F16), Just(PropertyFormat::F32)]
    }

    fn arb_aligned_data(format: PropertyFormat, max_units: usize) -> impl Strategy<Value = Vec<u8>> {
        let bytes = format.bytes();
        prop::collection::vec(any::<u8>(), 0..max_units).prop_map(move |v| {
            let len = v.len() - (v.len() % bytes);
            v.into_iter().take(len).collect()
        })
    }

    proptest! {
        #[test]
        fn replace_round_trip(t in 1u32..1000, f in arb_format(), data in any::<Vec<u8>>().prop_filter("len bound", |v| v.len() <= 4096)) {
            let aligned: Vec<u8> = data.iter().take(data.len() - (data.len() % f.bytes())).copied().collect();
            let v = apply_change(None, ChangeMode::Replace, AtomId(t), f, &aligned).unwrap();
            prop_assert_eq!(v.data, aligned);
            prop_assert_eq!(v.r#type, AtomId(t));
            prop_assert_eq!(v.format, f);
        }

        #[test]
        fn append_additivity(t in 1u32..1000, f in arb_format(), a in arb_aligned_data(PropertyFormat::F8, 1024), b in arb_aligned_data(PropertyFormat::F8, 1024)) {
            let trimmed_a: Vec<u8> = a.iter().take(a.len() - (a.len() % f.bytes())).copied().collect();
            let trimmed_b: Vec<u8> = b.iter().take(b.len() - (b.len() % f.bytes())).copied().collect();
            let existing = PropertyValue { r#type: AtomId(t), format: f, data: trimmed_a.clone() };
            let result = apply_change(Some(&existing), ChangeMode::Append, AtomId(t), f, &trimmed_b).unwrap();
            prop_assert_eq!(result.data.len(), trimmed_a.len() + trimmed_b.len());
            prop_assert_eq!(&result.data[..trimmed_a.len()], &trimmed_a[..]);
            prop_assert_eq!(&result.data[trimmed_a.len()..], &trimmed_b[..]);
        }

        #[test]
        fn prepend_concat_order(t in 1u32..1000, f in arb_format(), a in arb_aligned_data(PropertyFormat::F8, 1024), b in arb_aligned_data(PropertyFormat::F8, 1024)) {
            let trimmed_a: Vec<u8> = a.iter().take(a.len() - (a.len() % f.bytes())).copied().collect();
            let trimmed_b: Vec<u8> = b.iter().take(b.len() - (b.len() % f.bytes())).copied().collect();
            let existing = PropertyValue { r#type: AtomId(t), format: f, data: trimmed_a.clone() };
            let result = apply_change(Some(&existing), ChangeMode::Prepend, AtomId(t), f, &trimmed_b).unwrap();
            prop_assert_eq!(&result.data[..trimmed_b.len()], &trimmed_b[..]);
            prop_assert_eq!(&result.data[trimmed_b.len()..], &trimmed_a[..]);
        }

        #[test]
        fn append_type_mismatch_always_bad_match(t1 in 1u32..1000, t2 in 1u32..1000, f in arb_format()) {
            prop_assume!(t1 != t2);
            let existing = PropertyValue { r#type: AtomId(t1), format: f, data: vec![] };
            let err = apply_change(Some(&existing), ChangeMode::Append, AtomId(t2), f, &[]).unwrap_err();
            prop_assert_eq!(err, ChangePropertyError::BadMatch);
        }
    }
}
```

Run: `cargo test -p yserver-core --lib properties`. Expect green.

- [ ] **Step 4: Commit**

```sh
git add crates/yserver-core/src/properties.rs
git commit -m "feat(properties): add apply_change with proptest coverage

Replace ignores existing; Prepend/Append require type+format match
and concatenate. Fails BadAlloc above MAX_PROPERTY_BYTES (64 MiB)."
```

### Task 3.3: `slice_for_get` (TDD)

**Files:**
- Modify: `crates/yserver-core/src/properties.rs`

- [ ] **Step 1: Failing unit tests**

```rust
#[cfg(test)]
mod slice_for_get_tests {
    use super::*;
    use yserver_protocol::x11::AtomId;

    #[test]
    fn absent_property_returns_none_metadata() {
        let s = slice_for_get(None, AtomId(0), 0, 1024).unwrap();
        assert_eq!(s.r#type, AtomId(0));
        assert_eq!(s.format, 0);
        assert!(s.value.is_empty());
        assert_eq!(s.bytes_after, 0);
    }

    #[test]
    fn type_mismatch_returns_metadata_no_data() {
        let p = PropertyValue { r#type: AtomId(31), format: PropertyFormat::F8, data: b"hello".to_vec() };
        let s = slice_for_get(Some(&p), AtomId(4), 0, 1024).unwrap();
        assert_eq!(s.r#type, AtomId(31));
        assert_eq!(s.format, 8);
        assert!(s.value.is_empty());
        assert_eq!(s.bytes_after, 5);
    }

    #[test]
    fn read_format32_long_length_one_returns_4_bytes() {
        let p = PropertyValue { r#type: AtomId(31), format: PropertyFormat::F32, data: vec![1,2,3,4,5,6,7,8] };
        let s = slice_for_get(Some(&p), AtomId(31), 0, 1).unwrap();
        assert_eq!(s.value, [1,2,3,4]);
        assert_eq!(s.bytes_after, 4);
    }

    #[test]
    fn read_format8_long_length_one_returns_4_bytes() {
        let p = PropertyValue { r#type: AtomId(31), format: PropertyFormat::F8, data: b"hello world!".to_vec() };
        let s = slice_for_get(Some(&p), AtomId(31), 0, 1).unwrap();
        assert_eq!(s.value, b"hell");
    }

    #[test]
    fn offset_past_end_is_bad_value() {
        let p = PropertyValue { r#type: AtomId(31), format: PropertyFormat::F8, data: b"hi".to_vec() };
        let err = slice_for_get(Some(&p), AtomId(31), 1, 1).unwrap_err();
        assert_eq!(err, ChangePropertyError::BadValue);
    }

    #[test]
    fn offset_at_end_is_valid_empty_slice() {
        let p = PropertyValue { r#type: AtomId(31), format: PropertyFormat::F8, data: b"abcd".to_vec() };
        let s = slice_for_get(Some(&p), AtomId(31), 1, 0).unwrap();
        assert!(s.value.is_empty());
        assert_eq!(s.bytes_after, 0);
    }
}
```

Run: `cargo test -p yserver-core --lib properties::slice_for_get_tests`. Expected: FAIL.

- [ ] **Step 2: Implement `slice_for_get`**

```rust
pub struct GetPropertySlice<'a> {
    pub r#type: AtomId,
    pub format: u8,
    pub bytes_after: u32,
    pub value: &'a [u8],
}

pub fn slice_for_get<'a>(
    property: Option<&'a PropertyValue>,
    requested_type: AtomId,
    long_offset: u32,
    long_length: u32,
) -> Result<GetPropertySlice<'a>, ChangePropertyError> {
    let Some(p) = property else {
        return Ok(GetPropertySlice { r#type: AtomId(0), format: 0, bytes_after: 0, value: &[] });
    };
    let total = p.data.len() as u64;

    let any = requested_type.0 == 0;
    let matches = any || requested_type == p.r#type;
    if !matches {
        return Ok(GetPropertySlice {
            r#type: p.r#type,
            format: p.format.protocol_value(),
            bytes_after: total as u32,
            value: &[],
        });
    }

    let offset_bytes = (long_offset as u64).checked_mul(4).ok_or(ChangePropertyError::BadValue)?;
    if offset_bytes > total {
        return Err(ChangePropertyError::BadValue);
    }
    let remaining = total - offset_bytes;
    let want_bytes = (long_length as u64).checked_mul(4).ok_or(ChangePropertyError::BadValue)?;
    let mut len_to_return = remaining.min(want_bytes);

    let unit = p.format.bytes() as u64;
    len_to_return -= len_to_return % unit;

    let start = offset_bytes as usize;
    let end = start + len_to_return as usize;
    let bytes_after = (remaining - len_to_return) as u32;
    Ok(GetPropertySlice {
        r#type: p.r#type,
        format: p.format.protocol_value(),
        bytes_after,
        value: &p.data[start..end],
    })
}
```

Run tests; expect green.

- [ ] **Step 3: Add proptest coverage**

```rust
#[cfg(test)]
mod slice_for_get_props {
    use super::*;
    use proptest::prelude::*;

    fn arb_property() -> impl Strategy<Value = PropertyValue> {
        let format_strat = prop_oneof![Just(PropertyFormat::F8), Just(PropertyFormat::F16), Just(PropertyFormat::F32)];
        (1u32..1000, format_strat, 0usize..512).prop_flat_map(|(t, f, len_units)| {
            let bytes = f.bytes();
            let total = len_units * bytes;
            prop::collection::vec(any::<u8>(), total..=total)
                .prop_map(move |data| PropertyValue { r#type: AtomId(t), format: f, data })
        })
    }

    proptest! {
        #[test]
        fn read_all_recovers_data(p in arb_property()) {
            let s = slice_for_get(Some(&p), p.r#type, 0, u32::MAX / 4).unwrap();
            prop_assert_eq!(s.value, &p.data[..]);
            prop_assert_eq!(s.bytes_after, 0);
        }

        #[test]
        fn value_len_in_format_units(p in arb_property(), off_units in 0u32..32) {
            let off_bytes = (off_units as u64) * 4;
            prop_assume!(off_bytes <= p.data.len() as u64);
            let s = slice_for_get(Some(&p), p.r#type, off_units, 8).unwrap();
            let unit = p.format.bytes();
            prop_assert_eq!(s.value.len() % unit, 0);
        }

        #[test]
        fn bytes_after_invariant(p in arb_property(), off_units in 0u32..32, len_units in 0u32..32) {
            let off_bytes = (off_units as u64) * 4;
            prop_assume!(off_bytes <= p.data.len() as u64);
            let s = slice_for_get(Some(&p), p.r#type, off_units, len_units).unwrap();
            let remaining = (p.data.len() as u64) - off_bytes;
            prop_assert_eq!(s.value.len() as u64 + s.bytes_after as u64, remaining);
        }

        #[test]
        fn any_type_matches(p in arb_property()) {
            let s = slice_for_get(Some(&p), AtomId(0), 0, u32::MAX / 4).unwrap();
            prop_assert_eq!(s.r#type, p.r#type);
            prop_assert_eq!(s.value, &p.data[..]);
        }

        #[test]
        fn type_mismatch_metadata(p in arb_property(), other_t in 1u32..1000) {
            prop_assume!(other_t != p.r#type.0);
            let s = slice_for_get(Some(&p), AtomId(other_t), 0, u32::MAX / 4).unwrap();
            prop_assert!(s.value.is_empty());
            prop_assert_eq!(s.bytes_after as usize, p.data.len());
            prop_assert_eq!(s.r#type, p.r#type);
        }

        #[test]
        fn offset_past_end_is_bad_value(p in arb_property()) {
            prop_assume!(!p.data.is_empty());
            let off_units = ((p.data.len() / 4) as u32) + 1;
            let err = slice_for_get(Some(&p), p.r#type, off_units, 1).unwrap_err();
            prop_assert_eq!(err, ChangePropertyError::BadValue);
        }
    }

    // Spec calls for 1000 cases on chunked reassembly specifically (the most
    // important and most expensive invariant). Other props stay at the
    // default (256).
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(1000))]
        #[test]
        fn chunked_reassembles(p in arb_property()) {
            let mut acc: Vec<u8> = Vec::new();
            let mut offset = 0u32;
            loop {
                let s = slice_for_get(Some(&p), p.r#type, offset, 8).unwrap();
                acc.extend_from_slice(s.value);
                if s.bytes_after == 0 { break; }
                offset += (s.value.len() / 4) as u32;
            }
            prop_assert_eq!(acc, p.data.clone());
        }
    }
}
```

Run: `cargo test -p yserver-core --lib properties`. Expect green.

- [ ] **Step 4: Commit**

```sh
git add crates/yserver-core/src/properties.rs
git commit -m "feat(properties): add slice_for_get with proptest coverage

Handles AnyPropertyType, type mismatch metadata, partial reads
in 4-byte units, and end-of-property as a valid empty result."
```

---

## Stage 4 — Wire-format and handler implementations

### Task 4.1: `ChangePropertyRequest` parser

**Files:**
- Modify: `crates/yserver-protocol/src/x11.rs`

- [ ] **Step 1: Add type and parser**

```rust
#[derive(Clone, Debug, PartialEq)]
pub struct ChangePropertyRequest {
    pub mode: u8,
    pub window: ResourceId,
    pub property: AtomId,
    pub r#type: AtomId,
    pub format: u8,
    pub data: Vec<u8>,
    pub length: u32,
}

pub fn change_property_request(header_data: u8, body: &[u8]) -> Option<ChangePropertyRequest> {
    let window = ResourceId(read_u32_le(body.get(0..4)?));
    let property = AtomId(read_u32_le(body.get(4..8)?));
    let r#type = AtomId(read_u32_le(body.get(8..12)?));
    let format = *body.get(12)?;
    let length = read_u32_le(body.get(16..20)?);
    let unit = match format { 8 => 1, 16 => 2, 32 => 4, _ => return None };
    let data_bytes = (length as usize).checked_mul(unit)?;
    let data = body.get(20..20 + data_bytes)?.to_vec();
    Some(ChangePropertyRequest { mode: header_data, window, property, r#type, format, data, length })
}
```

(`header_data` carries `mode`, since the X11 `ChangeProperty` request encodes mode in the request-header `data` byte.)

- [ ] **Step 2: Add round-trip proptest**

Append to `x11.rs`:

```rust
#[cfg(test)]
mod change_property_tests {
    use super::*;
    use proptest::prelude::*;

    fn encode(req: &ChangePropertyRequest) -> (u8, Vec<u8>) {
        let mut body = Vec::new();
        write_u32(ClientByteOrder::LittleEndian, &mut body, req.window.0);
        write_u32(ClientByteOrder::LittleEndian, &mut body, req.property.0);
        write_u32(ClientByteOrder::LittleEndian, &mut body, req.r#type.0);
        body.push(req.format);
        body.extend_from_slice(&[0; 3]);
        write_u32(ClientByteOrder::LittleEndian, &mut body, req.length);
        body.extend_from_slice(&req.data);
        pad_vec4(&mut body);
        (req.mode, body)
    }

    proptest! {
        #[test]
        fn round_trip(
            mode in 0u8..=2,
            window in any::<u32>(),
            property in 1u32..0xFFFF,
            r#type in 1u32..0xFFFF,
            format_choice in 0u8..3,
            length in 0u32..256,
        ) {
            let format = [8u8, 16, 32][format_choice as usize];
            let unit = match format { 8 => 1, 16 => 2, _ => 4 };
            let data = vec![0xAB; (length as usize) * unit];
            let req = ChangePropertyRequest {
                mode,
                window: ResourceId(window),
                property: AtomId(property),
                r#type: AtomId(r#type),
                format,
                data: data.clone(),
                length,
            };
            let (header_data, body) = encode(&req);
            let parsed = change_property_request(header_data, &body).unwrap();
            prop_assert_eq!(parsed, req);
        }
    }
}
```

Run: `cargo test -p yserver-protocol --lib change_property_tests`. Expect green.

- [ ] **Step 3: Commit**

```sh
git add crates/yserver-protocol/src/x11.rs
git commit -m "feat(protocol): parse ChangePropertyRequest with round-trip prop"
```

### Task 4.2: `DeletePropertyRequest` parser

**Files:**
- Modify: `crates/yserver-protocol/src/x11.rs`

- [ ] **Step 1: Add parser and tests**

```rust
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DeletePropertyRequest {
    pub window: ResourceId,
    pub property: AtomId,
}

pub fn delete_property_request(body: &[u8]) -> Option<DeletePropertyRequest> {
    Some(DeletePropertyRequest {
        window: ResourceId(read_u32_le(body.get(0..4)?)),
        property: AtomId(read_u32_le(body.get(4..8)?)),
    })
}

#[cfg(test)]
mod delete_property_tests {
    use super::*;
    use proptest::prelude::*;
    proptest! {
        #[test]
        fn round_trip(window in any::<u32>(), property in any::<u32>()) {
            let mut body = Vec::new();
            write_u32(ClientByteOrder::LittleEndian, &mut body, window);
            write_u32(ClientByteOrder::LittleEndian, &mut body, property);
            let req = delete_property_request(&body).unwrap();
            prop_assert_eq!(req, DeletePropertyRequest {
                window: ResourceId(window), property: AtomId(property),
            });
        }
    }
}
```

- [ ] **Step 2: Verify and commit**

```sh
cargo test -p yserver-protocol --lib delete_property_tests
git add crates/yserver-protocol/src/x11.rs
git commit -m "feat(protocol): parse DeletePropertyRequest"
```

### Task 4.3: `GetPropertyRequest` parser

**Files:**
- Modify: `crates/yserver-protocol/src/x11.rs`

- [ ] **Step 1: Add parser and round-trip prop**

```rust
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GetPropertyRequest {
    pub delete: bool,
    pub window: ResourceId,
    pub property: AtomId,
    pub r#type: AtomId,
    pub long_offset: u32,
    pub long_length: u32,
}

pub fn get_property_request(header_data: u8, body: &[u8]) -> Option<GetPropertyRequest> {
    Some(GetPropertyRequest {
        delete: header_data != 0,
        window: ResourceId(read_u32_le(body.get(0..4)?)),
        property: AtomId(read_u32_le(body.get(4..8)?)),
        r#type: AtomId(read_u32_le(body.get(8..12)?)),
        long_offset: read_u32_le(body.get(12..16)?),
        long_length: read_u32_le(body.get(16..20)?),
    })
}

#[cfg(test)]
mod get_property_tests {
    use super::*;
    use proptest::prelude::*;
    proptest! {
        #[test]
        fn round_trip(
            delete: bool,
            window in any::<u32>(), property in any::<u32>(),
            r#type in any::<u32>(), long_offset in any::<u32>(), long_length in any::<u32>(),
        ) {
            let mut body = Vec::new();
            write_u32(ClientByteOrder::LittleEndian, &mut body, window);
            write_u32(ClientByteOrder::LittleEndian, &mut body, property);
            write_u32(ClientByteOrder::LittleEndian, &mut body, r#type);
            write_u32(ClientByteOrder::LittleEndian, &mut body, long_offset);
            write_u32(ClientByteOrder::LittleEndian, &mut body, long_length);
            let req = get_property_request(if delete { 1 } else { 0 }, &body).unwrap();
            prop_assert_eq!(req, GetPropertyRequest {
                delete, window: ResourceId(window), property: AtomId(property),
                r#type: AtomId(r#type), long_offset, long_length,
            });
        }
    }
}
```

- [ ] **Step 2: Verify and commit**

```sh
cargo test -p yserver-protocol --lib get_property_tests
git add crates/yserver-protocol/src/x11.rs
git commit -m "feat(protocol): parse GetPropertyRequest"
```

### Task 4.4: Replace `write_get_property_reply` with a real one

**Files:**
- Modify: `crates/yserver-protocol/src/x11.rs:1294-1304`

- [ ] **Step 1: Replace the stub with the real reply writer**

```rust
#[derive(Clone, Debug)]
pub struct GetPropertyReply<'a> {
    pub format: u8,
    pub r#type: AtomId,
    pub bytes_after: u32,
    pub value_len: u32,   // in format units
    pub value: &'a [u8],  // padded externally? no — we pad here.
}

pub fn write_get_property_reply(
    writer: &mut impl Write,
    sequence: SequenceNumber,
    reply: GetPropertyReply<'_>,
) -> io::Result<()> {
    let mut padded = reply.value.to_vec();
    pad_vec4(&mut padded);
    let length_units = checked_units(padded.len())? as u32;
    let mut out = fixed_reply(sequence, reply.format, length_units);
    write_u32(ClientByteOrder::LittleEndian, &mut out, reply.r#type.0);
    write_u32(ClientByteOrder::LittleEndian, &mut out, reply.bytes_after);
    write_u32(ClientByteOrder::LittleEndian, &mut out, reply.value_len);
    out.extend_from_slice(&[0; 12]);
    out.extend_from_slice(&padded);
    writer.write_all(&out)
}
```

- [ ] **Step 2: Update the existing call site**

`nested.rs:546-548` currently calls `x11::write_get_property_reply(stream, sequence)` with no payload. The full GetProperty handler in Task 4.8 will replace this with a real reply; for now leave the empty reply path emitting `GetPropertyReply { format: 0, r#type: AtomId(0), bytes_after: 0, value_len: 0, value: &[] }`:

```rust
20 => {
    log_reply(client_id, sequence, "GetProperty");
    let mut w = writer.lock().map_err(|_| io::Error::new(ErrorKind::BrokenPipe, "writer poisoned"))?;
    x11::write_get_property_reply(&mut *w, sequence, x11::GetPropertyReply {
        format: 0, r#type: AtomId(0), bytes_after: 0, value_len: 0, value: &[],
    })
}
```

- [ ] **Step 3: Add round-trip / shape tests**

```rust
#[cfg(test)]
mod get_property_reply_tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn shape(
            format_choice in 0u8..3,
            r#type in any::<u32>(),
            bytes_after in any::<u32>(),
            len_units in 0u32..256,
        ) {
            let format = [8u8, 16, 32][format_choice as usize];
            let unit = match format { 8 => 1, 16 => 2, _ => 4 };
            let value: Vec<u8> = (0..len_units as usize * unit).map(|i| (i & 0xff) as u8).collect();
            let value_len = len_units;

            let mut buf = Vec::new();
            write_get_property_reply(&mut buf, SequenceNumber(0xdead), GetPropertyReply {
                format, r#type: AtomId(r#type), bytes_after, value_len, value: &value,
            }).unwrap();

            let pad = (4 - value.len() % 4) % 4;
            let payload = value.len() + pad;
            prop_assert_eq!(buf.len(), 32 + payload);
            // wire length field (4..8) equals payload/4
            let wire_len = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
            prop_assert_eq!(wire_len as usize * 4, payload);
            // value_len field (16..20) is in format units
            let wire_value_len = u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]);
            prop_assert_eq!(wire_value_len, value_len);
        }
    }
}
```

- [ ] **Step 4: Verify and commit**

```sh
cargo test -p yserver-protocol --lib get_property_reply_tests
git add crates/yserver-protocol/src/x11.rs crates/yserver-core/src/nested.rs
git commit -m "feat(protocol): replace stub write_get_property_reply with real writer

Reply length field is the padded payload in 4-byte units; value_len
is in format units (8/16/32-bit elements). Property handler still
returns an empty reply; real wiring lands in a follow-up task."
```

### Task 4.5: `write_property_notify_event`

**Files:**
- Modify: `crates/yserver-protocol/src/x11.rs`

- [ ] **Step 1: Add streaming + encode variants and a shape test**

```rust
pub fn encode_property_notify_event(
    out: &mut Vec<u8>,
    sequence: SequenceNumber,
    order: ClientByteOrder,
    window: ResourceId,
    atom: AtomId,
    timestamp: u32,
    deleted: bool,
) {
    out.push(28); // PropertyNotify
    out.push(0);
    write_u16(order, out, sequence.0);
    write_u32(order, out, window.0);
    write_u32(order, out, atom.0);
    write_u32(order, out, timestamp);
    out.push(if deleted { 1 } else { 0 });
    out.extend_from_slice(&[0; 15]);
}

pub fn write_property_notify_event(
    writer: &mut impl Write,
    sequence: SequenceNumber,
    window: ResourceId,
    atom: AtomId,
    timestamp: u32,
    deleted: bool,
) -> io::Result<()> {
    let mut out = Vec::with_capacity(32);
    encode_property_notify_event(&mut out, sequence, ClientByteOrder::LittleEndian, window, atom, timestamp, deleted);
    writer.write_all(&out)
}

#[cfg(test)]
mod property_notify_tests {
    use super::*;
    #[test]
    fn shape() {
        let mut buf = Vec::new();
        encode_property_notify_event(
            &mut buf,
            SequenceNumber(0x1234),
            ClientByteOrder::LittleEndian,
            ResourceId(0x100002),
            AtomId(0x42),
            0xdead_beef,
            true,
        );
        assert_eq!(buf.len(), 32);
        assert_eq!(buf[0], 28);
        assert_eq!(&buf[2..4], &[0x34, 0x12]);
        assert_eq!(&buf[4..8], &0x100002u32.to_le_bytes());
        assert_eq!(&buf[8..12], &0x42u32.to_le_bytes());
        assert_eq!(&buf[12..16], &0xdead_beefu32.to_le_bytes());
        assert_eq!(buf[16], 1);
    }
}
```

- [ ] **Step 2: Verify and commit**

```sh
cargo test -p yserver-protocol --lib property_notify_tests
git add crates/yserver-protocol/src/x11.rs
git commit -m "feat(protocol): encode PropertyNotify event"
```

### Task 4.6: `ChangeProperty` handler

**Files:**
- Modify: `crates/yserver-core/src/nested.rs`

- [ ] **Step 1: Replace opcode 18 stub**

```rust
18 => {
    let Some(req) = x11::change_property_request(header.data, body) else {
        return emit_x11_error(writer, sequence, x11::error::BAD_LENGTH, 0, 18);
    };

    let mode = match crate::properties::ChangeMode::from_protocol(req.mode) {
        Some(m) => m,
        None => return emit_x11_error(writer, sequence, x11::error::BAD_VALUE, u32::from(req.mode), 18),
    };
    let format = match crate::properties::PropertyFormat::from_protocol(req.format) {
        Some(f) => f,
        None => return emit_x11_error(writer, sequence, x11::error::BAD_VALUE, u32::from(req.format), 18),
    };
    let expected_bytes = (req.length as usize).checked_mul(format.bytes());
    if expected_bytes != Some(req.data.len()) {
        return emit_x11_error(writer, sequence, x11::error::BAD_LENGTH, 0, 18);
    }

    let (timestamp, subscribers) = {
        let mut s = lock_server(server)?;
        if s.resources.window(req.window).is_none() {
            drop(s);
            return emit_x11_error(writer, sequence, x11::error::BAD_WINDOW, req.window.0, 18);
        }
        if !s.atoms.exists(req.property) {
            drop(s);
            return emit_x11_error(writer, sequence, x11::error::BAD_ATOM, req.property.0, 18);
        }
        if !s.atoms.exists(req.r#type) {
            drop(s);
            return emit_x11_error(writer, sequence, x11::error::BAD_ATOM, req.r#type.0, 18);
        }
        let existing = s.resources.window_property(req.window, req.property).cloned();
        let new_value = match crate::properties::apply_change(
            existing.as_ref(), mode, req.r#type, format, &req.data,
        ) {
            Ok(v) => v,
            Err(crate::properties::ChangePropertyError::BadMatch) =>
                { drop(s); return emit_x11_error(writer, sequence, x11::error::BAD_MATCH, req.window.0, 18); }
            Err(crate::properties::ChangePropertyError::BadAlloc) =>
                { drop(s); return emit_x11_error(writer, sequence, x11::error::BAD_ALLOC, 0, 18); }
            Err(crate::properties::ChangePropertyError::BadValue) =>
                { drop(s); return emit_x11_error(writer, sequence, x11::error::BAD_VALUE, 0, 18); }
        };
        s.resources.set_window_property(req.window, req.property, new_value);
        let timestamp = s.timestamp_now();
        let subs = s.subscribers(req.window, 0x0040_0000);
        (timestamp, subs)
    };

    for target in subscribers {
        let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
        let mut buf = Vec::with_capacity(32);
        x11::encode_property_notify_event(&mut buf, seq, target.byte_order, req.window, req.property, timestamp, false);
        if let Ok(mut w) = target.writer.lock() {
            let _ = w.write_all(&buf);
        }
    }
    Ok(())
}
```

Add the missing accessors on `ResourceTable`:

```rust
pub fn window_property(&self, w: ResourceId, atom: AtomId) -> Option<&PropertyValue> {
    self.windows.get(&w.0)?.properties.get(&atom)
}

pub fn set_window_property(&mut self, w: ResourceId, atom: AtomId, value: PropertyValue) {
    if let Some(window) = self.windows.get_mut(&w.0) {
        window.properties.insert(atom, value);
    }
}

pub fn delete_window_property(&mut self, w: ResourceId, atom: AtomId) -> Option<PropertyValue> {
    self.windows.get_mut(&w.0)?.properties.remove(&atom)
}
```

`Window` gets `pub properties: HashMap<AtomId, PropertyValue>,` — initialize empty in `new()`, `placeholder`, and `create_window`. Add `use crate::properties::PropertyValue; use yserver_protocol::x11::AtomId;` to `resources.rs`.

- [ ] **Step 2: Smoke test**

```sh
cargo fmt && cargo clippy -- -W clippy::pedantic && cargo test
DISPLAY=:0 cargo run --bin ynest -- 42 &
DISPLAY=:42 xterm &
xdotool search --name xterm  # → e.g. 0x100002
xprop -display :42 -id 0x100002 -f FOO 8s -set FOO "hello"
```

Expected: no error logged.

- [ ] **Step 3: Commit**

```sh
git add crates/yserver-core/src/resources.rs crates/yserver-core/src/nested.rs
git commit -m "feat(nested): implement ChangeProperty with PropertyNotify fanout

Validates mode, format, length, window, atoms; runs apply_change
under the ServerState lock; releases the lock before writing
PropertyNotify(NewValue) to every subscriber of the window."
```

### Task 4.7: `DeleteProperty` handler

**Files:**
- Modify: `crates/yserver-core/src/nested.rs`

- [ ] **Step 1: Replace opcode 19 stub**

```rust
19 => {
    let Some(req) = x11::delete_property_request(body) else {
        return emit_x11_error(writer, sequence, x11::error::BAD_LENGTH, 0, 19);
    };
    let (existed, timestamp, subscribers) = {
        let mut s = lock_server(server)?;
        if s.resources.window(req.window).is_none() {
            drop(s);
            return emit_x11_error(writer, sequence, x11::error::BAD_WINDOW, req.window.0, 19);
        }
        if !s.atoms.exists(req.property) {
            drop(s);
            return emit_x11_error(writer, sequence, x11::error::BAD_ATOM, req.property.0, 19);
        }
        let existed = s.resources.delete_window_property(req.window, req.property).is_some();
        let timestamp = s.timestamp_now();
        let subs = if existed { s.subscribers(req.window, 0x0040_0000) } else { Vec::new() };
        (existed, timestamp, subs)
    };
    if existed {
        for target in subscribers {
            let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
            let mut buf = Vec::with_capacity(32);
            x11::encode_property_notify_event(&mut buf, seq, target.byte_order, req.window, req.property, timestamp, true);
            if let Ok(mut w) = target.writer.lock() {
                let _ = w.write_all(&buf);
            }
        }
    }
    Ok(())
}
```

- [ ] **Step 2: Verify and commit**

```sh
cargo fmt && cargo clippy -- -W clippy::pedantic && cargo test
git add crates/yserver-core/src/nested.rs
git commit -m "feat(nested): implement DeleteProperty with conditional PropertyNotify"
```

### Task 4.8: `GetProperty` handler

**Files:**
- Modify: `crates/yserver-core/src/nested.rs`

- [ ] **Step 1: Replace opcode 20 stub**

```rust
20 => {
    let Some(req) = x11::get_property_request(header.data, body) else {
        return emit_x11_error(writer, sequence, x11::error::BAD_LENGTH, 0, 20);
    };
    let (reply_owned, delete_subscribers, timestamp) = {
        let mut s = lock_server(server)?;
        if s.resources.window(req.window).is_none() {
            drop(s);
            return emit_x11_error(writer, sequence, x11::error::BAD_WINDOW, req.window.0, 20);
        }
        if !s.atoms.exists(req.property) {
            drop(s);
            return emit_x11_error(writer, sequence, x11::error::BAD_ATOM, req.property.0, 20);
        }
        if req.r#type.0 != 0 && !s.atoms.exists(req.r#type) {
            drop(s);
            return emit_x11_error(writer, sequence, x11::error::BAD_ATOM, req.r#type.0, 20);
        }
        let existing = s.resources.window_property(req.window, req.property).cloned();
        let slice = match crate::properties::slice_for_get(
            existing.as_ref(), req.r#type, req.long_offset, req.long_length,
        ) {
            Ok(s) => s,
            Err(crate::properties::ChangePropertyError::BadValue) => {
                drop(s);
                return emit_x11_error(writer, sequence, x11::error::BAD_VALUE, req.long_offset, 20);
            }
            Err(_) => unreachable!("slice_for_get only returns BadValue on error"),
        };
        let value_len_units = if slice.format == 0 {
            0
        } else {
            slice.value.len() as u32 / u32::from(slice.format / 8)
        };
        let owned = OwnedGetPropertyReply {
            format: slice.format,
            r#type: slice.r#type,
            bytes_after: slice.bytes_after,
            value_len: value_len_units,
            value: slice.value.to_vec(),
        };

        // Decide whether `delete=1` actually fires.
        let type_matched = existing.as_ref().is_some_and(|p| req.r#type.0 == 0 || req.r#type == p.r#type);
        let mut subs = Vec::new();
        let mut timestamp = 0u32;
        if req.delete && type_matched && slice.bytes_after == 0 && existing.is_some() {
            s.resources.delete_window_property(req.window, req.property);
            timestamp = s.timestamp_now();
            subs = s.subscribers(req.window, 0x0040_0000);
        }
        (owned, subs, timestamp)
    };

    {
        let mut w = writer.lock().map_err(|_| io::Error::new(ErrorKind::BrokenPipe, "writer poisoned"))?;
        x11::write_get_property_reply(&mut *w, sequence, x11::GetPropertyReply {
            format: reply_owned.format,
            r#type: reply_owned.r#type,
            bytes_after: reply_owned.bytes_after,
            value_len: reply_owned.value_len,
            value: &reply_owned.value,
        })?;
    }
    for target in delete_subscribers {
        let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
        let mut buf = Vec::with_capacity(32);
        x11::encode_property_notify_event(&mut buf, seq, target.byte_order, req.window, req.property, timestamp, true);
        if let Ok(mut w) = target.writer.lock() {
            let _ = w.write_all(&buf);
        }
    }
    Ok(())
}
```

Add at module scope:

```rust
struct OwnedGetPropertyReply {
    format: u8,
    r#type: AtomId,
    bytes_after: u32,
    value_len: u32,
    value: Vec<u8>,
}
```

- [ ] **Step 2: Smoke test**

```sh
xprop -display :42 -id 0x100002 FOO
# → FOO(STRING) = "hello"
```

- [ ] **Step 3: Cross-client `xev` smoke test (the headline test for Stage 4)**

```sh
xev -display :42 -id 0x100002 &        # selects PropertyChange
xprop -display :42 -id 0x100002 -set FOO "world"
# xev should print:
#   PropertyNotify event ... atom = FOO ... state PropertyNewValue
```

- [ ] **Step 4: Commit**

```sh
cargo fmt && cargo clippy -- -W clippy::pedantic && cargo test
git add crates/yserver-core/src/nested.rs
git commit -m "feat(nested): implement GetProperty with delete-on-empty semantics

Handles AnyPropertyType, type mismatch metadata, partial reads in
4-byte units, and the delete=1 path (only fires when type matched
and bytes_after==0). PropertyNotify(Deleted) goes out after the
reply on the issuing client's writer."
```

---

## Stage 5 — `DestroyNotify` on `DestroyWindow` and disconnect

This stage uses the routing primitive from Stage 2.

### Task 5.1: `encode_destroy_notify_event`

**Files:**
- Modify: `crates/yserver-protocol/src/x11.rs`

- [ ] **Step 1: Add encoder + shape test**

```rust
pub fn encode_destroy_notify_event(
    out: &mut Vec<u8>,
    sequence: SequenceNumber,
    order: ClientByteOrder,
    event_window: ResourceId,
    window: ResourceId,
) {
    out.push(17); // DestroyNotify
    out.push(0);
    write_u16(order, out, sequence.0);
    write_u32(order, out, event_window.0);
    write_u32(order, out, window.0);
    out.extend_from_slice(&[0; 20]);
}

#[cfg(test)]
mod destroy_notify_tests {
    use super::*;
    #[test]
    fn shape() {
        let mut buf = Vec::new();
        encode_destroy_notify_event(&mut buf, SequenceNumber(0x1234), ClientByteOrder::LittleEndian,
            ResourceId(0x100), ResourceId(0x100002));
        assert_eq!(buf.len(), 32);
        assert_eq!(buf[0], 17);
        assert_eq!(&buf[4..8], &0x100u32.to_le_bytes());
        assert_eq!(&buf[8..12], &0x100002u32.to_le_bytes());
    }
}
```

- [ ] **Step 2: Verify and commit**

```sh
cargo test -p yserver-protocol --lib destroy_notify_tests
git add crates/yserver-protocol/src/x11.rs
git commit -m "feat(protocol): encode DestroyNotify event"
```

### Task 5.2: `DestroyWindow` emits `DestroyNotify`

**Files:**
- Modify: `crates/yserver-core/src/nested.rs`

- [ ] **Step 1: Build the pending-emit list under the lock**

For each destroyed window, snapshot:
- the window ID;
- its parent ID;
- subscribers on the window itself (StructureNotify, mask 0x0002_0000);
- subscribers on the parent (SubstructureNotify, mask 0x0008_0000).

Then drop subscriptions and finally drop the lock.

Per X11 spec, the recipient's `event_window` (the first window field in the event) is the window the recipient *selected on*. So `StructureNotify` subscribers (selected on `w`) see `event_window = w`, and `SubstructureNotify` subscribers (selected on `parent`) see `event_window = parent`. The destroyed window itself is the second field. This is why we must keep `parent` in the pending tuple and emit per subscriber set:

```rust
4 => {
    if let Some(window) = x11::free_resource_id(body) {
        let pending = {
            let mut s = lock_server(server)?;
            // Snapshot parents BEFORE destroy, since destroy strips them.
            let mut order = Vec::new();
            collect_destroy_order(&s.resources, window, &mut order);
            let mut pending: Vec<(ResourceId, ResourceId, Vec<crate::server::EventTarget>, Vec<crate::server::EventTarget>)> = Vec::new();
            for w in &order {
                let parent = s.resources.window(*w).map(|win| win.parent).unwrap_or(ROOT_WINDOW);
                let on_window = s.subscribers(*w, 0x0002_0000);
                let on_parent = s.subscribers(parent, 0x0008_0000);
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
                x11::encode_destroy_notify_event(&mut buf, seq, target.byte_order, parent, w);
                if let Ok(mut wr) = target.writer.lock() {
                    let _ = wr.write_all(&buf);
                }
            }
        }
    }
    log_void(client_id, sequence, "DestroyWindow")
}
```

Helper (used by Task 5.3 too):

```rust
fn collect_destroy_order(table: &crate::resources::ResourceTable, root: ResourceId, out: &mut Vec<ResourceId>) {
    let Some(w) = table.window(root) else { return; };
    for child in w.children.clone() {
        collect_destroy_order(table, child, out);
    }
    out.push(root);
}
```

- [ ] **Step 2: Verify and commit**

```sh
cargo fmt && cargo clippy -- -W clippy::pedantic && cargo test
git add crates/yserver-core/src/nested.rs
git commit -m "feat(nested): emit DestroyNotify on DestroyWindow

Walks the subtree depth-first, snapshots StructureNotify and
SubstructureNotify subscribers, destroys, then fans out events
after releasing the ServerState lock."
```

### Task 5.3: `DestroyNotify` on client disconnect

**Files:**
- Modify: `crates/yserver-core/src/nested.rs`

`collect_owned_window_roots` already exists from Task 1.6; this task adds DestroyNotify emission *on top of* the Stage-1 cleanup. The pending tuple keeps `parent` so SubstructureNotify recipients can see `event_window = parent`, matching Task 5.2.

- [ ] **Step 1: Replace the disconnect cleanup body from Task 1.6 with the full DestroyNotify-emitting version**

```rust
let (closed_fonts, pending_destroys) = {
    let mut s = lock_server(&server)?;
    // Collect the windows owned by this client, in destroy order.
    let mut owned_roots: Vec<ResourceId> = Vec::new();
    s.resources.collect_owned_window_roots(client_id, &mut owned_roots);

    let mut pending: Vec<(ResourceId, ResourceId, Vec<crate::server::EventTarget>, Vec<crate::server::EventTarget>)> = Vec::new();
    let mut all_destroyed: Vec<ResourceId> = Vec::new();
    for root in owned_roots {
        let mut order = Vec::new();
        collect_destroy_order(&s.resources, root, &mut order);
        for w in &order {
            let parent = s.resources.window(*w).map(|win| win.parent).unwrap_or(ROOT_WINDOW);
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
if let Some(host) = host.as_ref() {
    if let Ok(mut h) = host.lock() {
        for xid in closed_fonts {
            let _ = h.close_font(xid);
        }
    }
}
result
```

- [ ] **Step 2: Smoke test cross-client disconnect**

```sh
DISPLAY=:0 cargo run --bin ynest -- 42 &
DISPLAY=:42 xev &        # client A — selects on root
DISPLAY=:42 xterm        # client B — creates windows
# Close xterm; xev should print DestroyNotify lines.
```

- [ ] **Step 3: Verify and commit**

```sh
cargo fmt && cargo clippy -- -W clippy::pedantic && cargo test
git add crates/yserver-core/src/nested.rs
git commit -m "feat(nested): emit DestroyNotify when a client disconnects

Walks each top-level window owned by the departing client, snapshots
StructureNotify / SubstructureNotify subscribers, destroys, then
fans out DestroyNotify after the ServerState lock is released.
SubstructureNotify recipients see event_window=parent."
```

---

## Final verification

After Stage 5 lands, run the full pre-commit check one more time on a clean tree:

```sh
cargo fmt
cargo clippy -- -W clippy::pedantic
cargo test
```

Then update `docs/status.md`:

- Move "Property storage" from "Pending" to "Working" (`ChangeProperty`,
  `DeleteProperty`, `GetProperty`, `PropertyNotify` cross-client).
- Move `DestroyNotify` out of "Lifecycle / WM events" — it ships with
  this work; the remaining items (`UnmapNotify`, `ReparentNotify`,
  `ClientMessage`) stay pending.

Optional (only if a host X11 is available):

```sh
DISPLAY=:0 cargo run --bin ynest -- 42 &
DISPLAY=:42 xterm &
xdotool search --name xterm    # → window id, e.g. 0x100002
xprop -display :42 -id 0x100002 -f FOO 8s -set FOO "hello"
xprop -display :42 -id 0x100002 FOO
# → FOO(STRING) = "hello"

xev -display :42 -id 0x100002 &
xprop -display :42 -id 0x100002 -set FOO "world"
# xev should print PropertyNotify ... atom = FOO ... state PropertyNewValue
```
