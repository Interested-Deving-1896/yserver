use std::collections::HashMap;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Instant;

use yserver_protocol::x11::{self, AtomId, ClientByteOrder, ResourceId, SequenceNumber};

use crate::resources::ResourceTable;

pub const FIRST_CLIENT_BASE: u32 = 0x0010_0000;
pub const PER_CLIENT_MASK: u32 = 0x000F_FFFF;

#[derive(Debug)]
pub struct IdAllocator {
    next_base: u32,
}

impl IdAllocator {
    #[must_use]
    pub fn new() -> Self {
        Self {
            next_base: FIRST_CLIENT_BASE,
        }
    }

    /// Returns `(resource_id_base, resource_id_mask)` for a new client.
    /// Returns `None` when the next base would overflow `u32`.
    pub fn allocate(&mut self) -> Option<(u32, u32)> {
        let base = self.next_base;
        let next = base.checked_add(FIRST_CLIENT_BASE)?;
        self.next_base = next;
        Some((base, PER_CLIENT_MASK))
    }

    /// `id` is owned by the holder of `(base, mask)` iff `(id & !mask) == base`.
    #[must_use]
    pub fn validate_owned(id: u32, base: u32, mask: u32) -> bool {
        (id & !mask) == base
    }
}

impl Default for IdAllocator {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Default)]
pub struct AtomTable {
    by_name: HashMap<String, AtomId>,
    names: HashMap<u32, String>,
    next_id: u32,
}

impl AtomTable {
    #[must_use]
    pub fn new() -> Self {
        Self {
            by_name: HashMap::new(),
            names: HashMap::new(),
            next_id: 69, // X11 predefined atoms are 1..=68; custom atoms start here
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

    #[must_use]
    pub fn name(&self, atom: AtomId) -> Option<&str> {
        x11::well_known_atom_name(atom).or_else(|| self.names.get(&atom.0).map(String::as_str))
    }

    #[must_use]
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
    #[must_use]
    pub fn new() -> Self {
        Self {
            atoms: AtomTable::new(),
            resources: ResourceTable::new(),
            clients: HashMap::new(),
            id_allocator: IdAllocator::new(),
            start_instant: Instant::now(),
        }
    }

    #[must_use]
    pub fn timestamp_now(&self) -> u32 {
        // X11 timestamps are 32-bit milliseconds; truncation is intentional.
        let elapsed = self.start_instant.elapsed().as_millis();
        #[allow(clippy::cast_possible_truncation)]
        let ts = elapsed as u32;
        ts
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

/// Snapshot of a client's writer for cross-client event fanout.
#[derive(Clone)]
pub struct EventTarget {
    pub writer: Arc<Mutex<UnixStream>>,
    pub byte_order: ClientByteOrder,
    pub last_sequence: Arc<AtomicU16>,
}

impl ServerState {
    #[must_use]
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

    pub fn drop_window_subscriptions(&mut self, windows: &[ResourceId]) {
        for client in self.clients.values_mut() {
            for w in windows {
                client.event_masks.remove(w);
            }
        }
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
            for _ in 0..seed { a.allocate().unwrap(); }
            let (base, mask) = a.allocate().unwrap();
            let id = base + offset;
            prop_assert!(IdAllocator::validate_owned(id, base, mask));
            let other = base.wrapping_add(0x0010_0000).wrapping_add(offset);
            prop_assert!(!IdAllocator::validate_owned(other, base, mask));
        }
    }

    #[test]
    fn subscribers_returns_clients_with_bit_set() {
        let mut state = ServerState::new();
        state.clients.insert(
            1,
            ClientHandle {
                writer: make_test_writer(),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0010_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::from([(ResourceId(0x100), 0x0040_0000)]),
            },
        );
        state.clients.insert(
            2,
            ClientHandle {
                writer: make_test_writer(),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0020_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::from([(ResourceId(0x100), 0x0000_0001)]),
            },
        );
        // PropertyChange = 0x0040_0000
        let subs = state.subscribers(ResourceId(0x100), 0x0040_0000);
        assert_eq!(subs.len(), 1);
    }

    #[test]
    fn subscribers_omits_other_windows() {
        let mut state = ServerState::new();
        state.clients.insert(
            1,
            ClientHandle {
                writer: make_test_writer(),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0010_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::from([(ResourceId(0x200), 0xFFFF_FFFF)]),
            },
        );
        let subs = state.subscribers(ResourceId(0x100), 0x0040_0000);
        assert!(subs.is_empty());
    }

    #[test]
    fn subscribers_omits_disconnected_client() {
        let mut state = ServerState::new();
        state.clients.insert(
            1,
            ClientHandle {
                writer: make_test_writer(),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0010_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::from([(ResourceId(0x100), 0x0040_0000)]),
            },
        );
        assert_eq!(state.subscribers(ResourceId(0x100), 0x0040_0000).len(), 1);
        state.clients.remove(&1);
        assert!(state.subscribers(ResourceId(0x100), 0x0040_0000).is_empty());
    }

    fn make_test_writer() -> Arc<Mutex<UnixStream>> {
        let (a, _b) = UnixStream::pair().expect("socketpair");
        Arc::new(Mutex::new(a))
    }

    #[test]
    fn drop_window_subscriptions_removes_entries_for_destroyed_windows() {
        let mut state = ServerState::new();
        state.clients.insert(
            1,
            ClientHandle {
                writer: make_test_writer(),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0010_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::from([
                    (ResourceId(0x100), 0x0040_0000),
                    (ResourceId(0x200), 0x0040_0000),
                ]),
            },
        );
        assert_eq!(state.subscribers(ResourceId(0x100), 0x0040_0000).len(), 1);
        state.drop_window_subscriptions(&[ResourceId(0x100)]);
        assert!(state.subscribers(ResourceId(0x100), 0x0040_0000).is_empty());
        // Surviving window's subscription stays.
        assert_eq!(state.subscribers(ResourceId(0x200), 0x0040_0000).len(), 1);
    }
}
