//! Link identity, peer lifecycle, and the process-level registry.
//!
//! The registry is the only cross-VM entry point. It is created once by the
//! AxVisor assembly and injected explicitly into every VM configuration
//! build; links are shared by `Arc` and disappear when the last reservation
//! or attachment drops. Peer slots never return to vacant inside one link:
//! a released reservation retires the slot until the whole link is
//! recreated, which also recreates the zeroed backing.

use alloc::{
    boxed::Box,
    collections::BTreeMap,
    sync::{Arc, Weak},
    vec,
};
use core::{
    fmt,
    sync::atomic::{AtomicU32, Ordering},
};

use ax_sync::SpinLock;

use super::{
    SharedBarBacking,
    backing::SharedBackingAllocator,
    doorbell::{Doorbell, DoorbellEvent, IvshmemEventSink},
    error::IvshmemError,
    layout::IvshmemMemoryLayout,
};

/// Maximum peers of the current device profile.
pub const MAX_PEERS: u16 = 2;

/// The only doorbell vector supported by the current link profile.
const SUPPORTED_DOORBELL_VECTOR: u16 = 0;

/// Interval between repeated doorbell diagnostics: the first ignored write
/// logs immediately and every 1024th repetition of the same reason logs
/// again.
const DOORBELL_DIAGNOSTIC_INTERVAL: u32 = 1024;

/// Per-reason counters for ignored doorbell writes.
///
/// Diagnostics never change guest-visible behaviour, so the counters carry no
/// synchronisation role and use `Relaxed` ordering; they only drive the
/// rate-limited logging decision.
#[derive(Default)]
struct IgnoredDoorbellCounters {
    unsupported_vector: AtomicU32,
    out_of_profile: AtomicU32,
    not_attached: AtomicU32,
    delivery_failed: AtomicU32,
}

impl IgnoredDoorbellCounters {
    /// Increments `counter` and returns whether this occurrence should log.
    fn should_log(counter: &AtomicU32) -> bool {
        let occurrence = counter.fetch_add(1, Ordering::Relaxed) + 1;
        occurrence == 1 || occurrence.is_multiple_of(DOORBELL_DIAGNOSTIC_INTERVAL)
    }
}

/// Stable identity of one shared-memory link inside the AxVisor process.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LinkId(u32);

impl LinkId {
    /// Creates a link identity from its raw configuration value.
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// Returns the raw configuration value.
    pub const fn value(self) -> u32 {
        self.0
    }
}

impl fmt::Display for LinkId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// Identity of one peer slot inside a link.
///
/// The range is checked against the link profile when a reservation is
/// created, not by this newtype.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PeerId(u16);

impl PeerId {
    /// Creates a peer identity from its raw configuration value.
    pub const fn new(value: u16) -> Self {
        Self(value)
    }

    /// Returns the raw configuration value.
    pub const fn value(self) -> u16 {
        self.0
    }
}

impl fmt::Display for PeerId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// Lifecycle state of one peer slot.
#[derive(Clone, Copy, Debug)]
enum PeerSlot {
    /// Never reserved by any configuration.
    Vacant,
    /// Held by one configuration identity; no runtime attached yet.
    Reserved,
    /// Bound to one runtime endpoint build.
    Attached { generation: u64 },
    /// The reservation was released; the slot blocks until the link is
    /// destroyed.
    Retired,
}

/// Peer-slot table protected by one link-local lock.
struct LinkPeers {
    next_attachment_generation: u64,
    slots: Box<[PeerSlot]>,
    /// Per-attachment event sinks; entries follow the slot lifecycle:
    /// attach clears any stale entry, `set_event_sink` fills it, detach
    /// removes it.
    sinks: BTreeMap<PeerId, Arc<dyn IvshmemEventSink>>,
}

/// Shared state of one ivshmem link: profile, backing, and peer slots.
pub struct IvshmemLink {
    id: LinkId,
    max_peers: u16,
    backing: SharedBarBacking,
    layout: IvshmemMemoryLayout,
    peers: SpinLock<LinkPeers>,
    ignored_doorbell: IgnoredDoorbellCounters,
}

impl IvshmemLink {
    fn new(
        id: LinkId,
        max_peers: u16,
        allocator: Arc<dyn SharedBackingAllocator>,
    ) -> Result<Self, IvshmemError> {
        let backing = SharedBarBacking::try_new(super::SHARED_MEMORY_SIZE, allocator)?;
        // The profile freezes bar2_size and peer count, so this derivation
        // cannot fail today; the fallible signature constrains the later
        // configuration-driven layout feature (F8).
        let layout = IvshmemMemoryLayout::derive(super::SHARED_MEMORY_SIZE, max_peers)?;
        Ok(Self {
            id,
            max_peers,
            backing,
            layout,
            peers: SpinLock::new(LinkPeers {
                next_attachment_generation: 0,
                slots: vec![PeerSlot::Vacant; usize::from(max_peers)].into_boxed_slice(),
                sinks: BTreeMap::new(),
            }),
            ignored_doorbell: IgnoredDoorbellCounters::default(),
        })
    }

    /// Returns the link identity.
    pub const fn id(&self) -> LinkId {
        self.id
    }

    /// Returns the peer count of the frozen link profile.
    pub const fn max_peers(&self) -> u16 {
        self.max_peers
    }

    /// Returns the shared BAR2 backing of this link.
    pub fn backing(&self) -> &SharedBarBacking {
        &self.backing
    }

    /// Returns the BAR2 size of the link profile.
    pub const fn bar2_size(&self) -> u64 {
        self.backing.size()
    }

    /// Returns the frozen BAR2 layout of this link.
    pub const fn layout(&self) -> &IvshmemMemoryLayout {
        &self.layout
    }

    /// Publishes the peer's BAR0 State value into its state-table entry.
    ///
    /// Called by the owning endpoint's BAR0 State write path, outside the
    /// endpoint register lock.
    ///
    /// # Errors
    ///
    /// Returns [`IvshmemError::PeerOutOfProfile`] for an unknown peer and
    /// [`IvshmemError::SharedMemoryOutOfRange`] if the entry write would
    /// leave the backing (a layout bug, not a guest condition).
    pub fn publish_state(&self, peer: PeerId, value: u32) -> Result<(), IvshmemError> {
        let offset = self.layout.state_entry_offset(peer, self.max_peers)?;
        self.backing.write_bytes(offset, &value.to_le_bytes())
    }

    /// Clears the peer's state-table entry; called from endpoint reset.
    /// The same errors as `publish_state()` apply.
    pub fn clear_state(&self, peer: PeerId) -> Result<(), IvshmemError> {
        self.publish_state(peer, 0)
    }

    /// Routes one doorbell from the writing endpoint to the target peer.
    ///
    /// Out-of-profile targets, inactive targets, and unsupported vectors are
    /// specification-defined no-ops with rate-limited diagnostics; a guest
    /// doorbell never becomes a device access error or a VM abort. The
    /// target sink is cloned out of the peer table and invoked after the
    /// lock is released, so sink implementations never run under the
    /// peer-table lock.
    pub fn deliver_doorbell(&self, source: PeerId, doorbell: Doorbell) {
        let target = doorbell.target();
        if doorbell.vector() != SUPPORTED_DOORBELL_VECTOR {
            if IgnoredDoorbellCounters::should_log(&self.ignored_doorbell.unsupported_vector) {
                warn!(
                    "ivshmem link {} ignored a doorbell from peer {source}: vector {} is not \
                     supported",
                    self.id,
                    doorbell.vector()
                );
            }
            return;
        }
        if usize::from(target.value()) >= usize::from(self.max_peers) {
            if IgnoredDoorbellCounters::should_log(&self.ignored_doorbell.out_of_profile) {
                warn!(
                    "ivshmem link {} ignored a doorbell from peer {source}: target peer {target} \
                     is outside the profile of {} peers",
                    self.id, self.max_peers
                );
            }
            return;
        }
        let Some(sink) = self.attached_sink(target) else {
            return;
        };
        let event = DoorbellEvent::new(source, doorbell);
        if let Err(error) = sink.deliver(event) {
            // A sink failure is a recoverable internal problem: the write is
            // ignored and no state is rolled back.
            if IgnoredDoorbellCounters::should_log(&self.ignored_doorbell.delivery_failed) {
                warn!(
                    "ivshmem link {} ignored a doorbell from peer {source} to peer {target}: \
                     {error}",
                    self.id
                );
            }
        }
    }

    /// Returns the attached target's sink, or `None` when the doorbell must
    /// be ignored.
    fn attached_sink(&self, target: PeerId) -> Option<Arc<dyn IvshmemEventSink>> {
        let peers = self.peers.lock_irqsave();
        let attached = matches!(
            peers.slots.get(usize::from(target.value())),
            Some(PeerSlot::Attached { .. })
        );
        let sink = if attached {
            peers.sinks.get(&target).cloned()
        } else {
            None
        };
        drop(peers);
        if !attached && IgnoredDoorbellCounters::should_log(&self.ignored_doorbell.not_attached) {
            warn!(
                "ivshmem link {} ignored a doorbell to peer {target}: no endpoint is attached",
                self.id
            );
        }
        sink
    }

    fn reserve_slot(&self, peer: PeerId) -> Result<(), IvshmemError> {
        let mut peers = self.peers.lock_irqsave();
        let Some(slot) = peers.slots.get_mut(usize::from(peer.value())) else {
            return Err(IvshmemError::PeerOutOfProfile {
                peer: peer.value(),
                max_peers: self.max_peers,
            });
        };
        match slot {
            PeerSlot::Vacant => {
                *slot = PeerSlot::Reserved;
                Ok(())
            }
            PeerSlot::Reserved | PeerSlot::Attached { .. } => {
                Err(IvshmemError::PeerAlreadyReserved {
                    link: self.id.value(),
                    peer: peer.value(),
                })
            }
            PeerSlot::Retired => Err(IvshmemError::PeerRetired {
                link: self.id.value(),
                peer: peer.value(),
            }),
        }
    }

    fn attach_slot(&self, peer: PeerId) -> Result<u64, IvshmemError> {
        let mut peers = self.peers.lock_irqsave();
        let index = usize::from(peer.value());
        match peers.slots.get(index) {
            Some(PeerSlot::Reserved) => {}
            Some(PeerSlot::Attached { .. }) => {
                return Err(IvshmemError::PeerAlreadyAttached {
                    link: self.id.value(),
                    peer: peer.value(),
                });
            }
            Some(PeerSlot::Retired) => {
                return Err(IvshmemError::PeerRetired {
                    link: self.id.value(),
                    peer: peer.value(),
                });
            }
            Some(PeerSlot::Vacant) | None => {
                return Err(IvshmemError::PeerNotReserved {
                    link: self.id.value(),
                    peer: peer.value(),
                });
            }
        }
        peers.next_attachment_generation = peers.next_attachment_generation.wrapping_add(1);
        let generation = peers.next_attachment_generation;
        peers.slots[index] = PeerSlot::Attached { generation };
        // A re-attached slot must not keep a sink registered by a previous
        // attachment; the new endpoint registers its own sink.
        peers.sinks.remove(&peer);
        Ok(generation)
    }

    fn detach_slot(&self, peer: PeerId, generation: u64) {
        let mut peers = self.peers.lock_irqsave();
        let index = usize::from(peer.value());
        let is_current = match peers.slots.get(index) {
            Some(PeerSlot::Attached {
                generation: current,
            }) => *current == generation,
            _ => false,
        };
        if is_current {
            // A stale attachment must not clear a newer attachment's state.
            peers.slots[index] = PeerSlot::Reserved;
            // The detached endpoint stops receiving link events immediately.
            peers.sinks.remove(&peer);
        }
    }

    fn release_slot(&self, peer: PeerId) {
        let mut peers = self.peers.lock_irqsave();
        if let Some(slot) = peers.slots.get_mut(usize::from(peer.value())) {
            *slot = PeerSlot::Retired;
        }
    }
}

impl fmt::Debug for IvshmemLink {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IvshmemLink")
            .field("id", &self.id)
            .field("max_peers", &self.max_peers)
            .finish_non_exhaustive()
    }
}

/// Exclusive configuration identity of one `(link, peer)` pair.
///
/// The endpoint model owns the reservation for the VM lifetime; dropping it
/// retires the peer slot until the whole link is destroyed.
pub struct PeerReservation {
    link: Arc<IvshmemLink>,
    peer: PeerId,
}

impl PeerReservation {
    /// Returns the link this reservation belongs to.
    pub fn link(&self) -> &Arc<IvshmemLink> {
        &self.link
    }

    /// Returns the reserved peer identity.
    pub const fn peer_id(&self) -> PeerId {
        self.peer
    }

    /// Attaches one runtime endpoint to the reserved peer.
    ///
    /// Bundle registration may fail and retry, so attaching stays available
    /// after a previous attachment dropped; only one attachment may exist at
    /// a time.
    ///
    /// # Errors
    ///
    /// Returns [`IvshmemError::PeerAlreadyAttached`] while another endpoint is
    /// still attached.
    pub fn attach(&self) -> Result<PeerAttachment, IvshmemError> {
        let generation = self.link.attach_slot(self.peer)?;
        Ok(PeerAttachment {
            link: Arc::clone(&self.link),
            peer: self.peer,
            generation,
        })
    }
}

impl Drop for PeerReservation {
    fn drop(&mut self) {
        self.link.release_slot(self.peer);
    }
}

impl fmt::Debug for PeerReservation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PeerReservation")
            .field("link", &self.link.id())
            .field("peer", &self.peer)
            .finish()
    }
}

/// Runtime binding of one endpoint to its reserved peer slot.
pub struct PeerAttachment {
    link: Arc<IvshmemLink>,
    peer: PeerId,
    generation: u64,
}

impl PeerAttachment {
    /// Returns the attached link.
    pub fn link(&self) -> &Arc<IvshmemLink> {
        &self.link
    }

    /// Returns the attached peer identity.
    pub const fn peer_id(&self) -> PeerId {
        self.peer
    }

    /// Registers this peer's event sink on the attached link.
    ///
    /// The sink receives doorbells addressed to this peer. Re-attaching a
    /// slot drops any sink left by a previous attachment, so endpoints must
    /// register the sink on every build.
    ///
    /// # Errors
    ///
    /// Returns [`IvshmemError::PeerNotReserved`] when this attachment is no
    /// longer the current generation of its peer slot.
    pub fn set_event_sink(&self, sink: Arc<dyn IvshmemEventSink>) -> Result<(), IvshmemError> {
        let mut peers = self.link.peers.lock_irqsave();
        match peers.slots.get(usize::from(self.peer.value())) {
            Some(PeerSlot::Attached { generation }) if *generation == self.generation => {}
            _ => {
                return Err(IvshmemError::PeerNotReserved {
                    link: self.link.id.value(),
                    peer: self.peer.value(),
                });
            }
        }
        peers.sinks.insert(self.peer, sink);
        Ok(())
    }
}

impl Drop for PeerAttachment {
    fn drop(&mut self) {
        self.link.detach_slot(self.peer, self.generation);
    }
}

impl fmt::Debug for PeerAttachment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PeerAttachment")
            .field("link", &self.link.id())
            .field("peer", &self.peer)
            .finish()
    }
}

/// Process-level owner of every ivshmem link.
///
/// The AxVisor assembly creates exactly one registry and injects it into all
/// VM configuration builds. Locking stays on the configuration path with the
/// `registry -> link` order; runtime BAR access never touches the registry.
pub struct IvshmemLinkRegistry {
    links: SpinLock<BTreeMap<LinkId, Weak<IvshmemLink>>>,
    allocator: Arc<dyn SharedBackingAllocator>,
}

impl IvshmemLinkRegistry {
    /// Creates an empty registry over the given backing allocator.
    ///
    /// Every link created through this registry reserves its shared memory
    /// through `allocator`; the allocator must outlive all links, which the
    /// registry guarantees by holding one clone itself.
    pub fn new(allocator: Arc<dyn SharedBackingAllocator>) -> Self {
        Self {
            links: SpinLock::new(BTreeMap::new()),
            allocator,
        }
    }

    /// Reserves one peer of the named link, creating the link on first use.
    ///
    /// # Errors
    ///
    /// Returns [`IvshmemError::PeerOutOfProfile`] for peers outside the link
    /// profile, [`IvshmemError::PeerAlreadyReserved`] when another
    /// configuration holds the slot, [`IvshmemError::PeerRetired`] when the
    /// slot was released earlier, and [`IvshmemError::AllocationFailed`] when
    /// the link backing cannot be allocated.
    pub fn reserve(&self, link_id: u32, peer_id: u16) -> Result<PeerReservation, IvshmemError> {
        let id = LinkId::new(link_id);
        let peer = PeerId::new(peer_id);
        // The registry lock covers link creation so two concurrent
        // configurations of the same new link cannot observe separate links.
        let mut links = self.links.lock_irqsave();
        let link = match links.get(&id).and_then(Weak::upgrade) {
            Some(link) => link,
            None => {
                let link = Arc::new(IvshmemLink::new(
                    id,
                    MAX_PEERS,
                    Arc::clone(&self.allocator),
                )?);
                links.insert(id, Arc::downgrade(&link));
                link
            }
        };
        link.reserve_slot(peer)?;
        Ok(PeerReservation { link, peer })
    }
}

impl fmt::Debug for IvshmemLinkRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IvshmemLinkRegistry")
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use alloc::{sync::Arc, vec, vec::Vec};
    use core::sync::atomic::AtomicBool;
    use std::sync::Mutex;

    use super::*;
    use crate::ivshmem::{IvshmemRegisters, SHARED_MEMORY_SIZE, backing::test_allocator};

    fn registry() -> IvshmemLinkRegistry {
        IvshmemLinkRegistry::new(test_allocator())
    }

    /// Records every delivered event for routing assertions.
    struct EventRecorder(Mutex<Vec<DoorbellEvent>>);

    impl EventRecorder {
        fn new() -> Arc<Self> {
            Arc::new(Self(Mutex::new(Vec::new())))
        }

        fn events(&self) -> Vec<DoorbellEvent> {
            self.0.lock().unwrap().clone()
        }
    }

    impl IvshmemEventSink for EventRecorder {
        fn deliver(&self, event: DoorbellEvent) -> Result<(), IvshmemError> {
            self.0.lock().unwrap().push(event);
            Ok(())
        }
    }

    /// Mirrors the adapter sink: doorbells merge into one event-status bit.
    struct RegisterSink(Mutex<IvshmemRegisters>);

    impl RegisterSink {
        fn pending(&self) -> u32 {
            self.0
                .lock()
                .unwrap()
                .read(
                    crate::ivshmem::EVENT_STATUS_OFFSET,
                    PeerId::new(0),
                    MAX_PEERS,
                )
                .unwrap()
        }
    }

    impl IvshmemEventSink for RegisterSink {
        fn deliver(&self, _event: DoorbellEvent) -> Result<(), IvshmemError> {
            self.0.lock().unwrap().record_event();
            Ok(())
        }
    }

    /// A sink that always fails, for the ignored-delivery path.
    struct FailingSink;

    impl IvshmemEventSink for FailingSink {
        fn deliver(&self, _event: DoorbellEvent) -> Result<(), IvshmemError> {
            Err(IvshmemError::EventDeliveryFailed {
                operation: "record doorbell event",
                detail: "test sink refused the event".into(),
            })
        }
    }

    /// Probes whether the peer-table lock is held while the sink runs.
    struct LockProbeSink {
        link: Arc<IvshmemLink>,
        lock_observed: AtomicBool,
    }

    impl IvshmemEventSink for LockProbeSink {
        fn deliver(&self, _event: DoorbellEvent) -> Result<(), IvshmemError> {
            self.lock_observed
                .store(self.link.peers.is_locked(), Ordering::Relaxed);
            Ok(())
        }
    }

    fn doorbell(target: u16, vector: u16) -> Doorbell {
        Doorbell::from_write(((target as u32) << 16) | vector as u32)
    }

    fn attached_pair() -> (PeerReservation, PeerReservation) {
        let registry = registry();
        let peer0 = registry.reserve(1, 0).unwrap();
        let peer1 = registry.reserve(1, 1).unwrap();
        (peer0, peer1)
    }

    #[test]
    fn reserves_both_peers_of_one_link() {
        let registry = registry();
        let peer0 = registry.reserve(1, 0).unwrap();
        let peer1 = registry.reserve(1, 1).unwrap();
        assert!(Arc::ptr_eq(peer0.link(), peer1.link()));
        assert_eq!(peer0.link().max_peers(), MAX_PEERS);
        assert_eq!(peer0.link().bar2_size(), SHARED_MEMORY_SIZE);
    }

    #[test]
    fn rejects_duplicate_reservations_and_out_of_profile_peers() {
        let registry = registry();
        let _peer0 = registry.reserve(1, 0).unwrap();
        assert_eq!(
            registry.reserve(1, 0).unwrap_err(),
            IvshmemError::PeerAlreadyReserved { link: 1, peer: 0 }
        );
        assert_eq!(
            registry.reserve(1, 2).unwrap_err(),
            IvshmemError::PeerOutOfProfile {
                peer: 2,
                max_peers: MAX_PEERS
            }
        );
    }

    #[test]
    fn shares_backing_across_reservations_and_isolates_links() {
        let registry = registry();
        let peer0 = registry.reserve(1, 0).unwrap();
        let peer1 = registry.reserve(1, 1).unwrap();
        let other = registry.reserve(2, 0).unwrap();
        peer0.link().backing().write(0x40, 8, 0x1234).unwrap();
        assert_eq!(peer1.link().backing().read(0x40, 8).unwrap(), 0x1234);
        assert_eq!(other.link().backing().read(0x40, 8).unwrap(), 0);
        assert!(!Arc::ptr_eq(peer0.link(), other.link()));
    }

    #[test]
    fn recreating_a_dead_link_zeroes_the_backing() {
        let registry = registry();
        let peer = registry.reserve(1, 0).unwrap();
        peer.link().backing().write(0, 4, 0xa5a5_a5a5).unwrap();
        drop(peer);
        let recreated = registry.reserve(1, 0).unwrap();
        assert_eq!(recreated.link().backing().read(0, 4).unwrap(), 0);
    }

    #[test]
    fn retired_peer_blocks_reservation_until_the_link_is_destroyed() {
        let registry = registry();
        let peer0 = registry.reserve(1, 0).unwrap();
        let peer1 = registry.reserve(1, 1).unwrap();
        drop(peer0);
        assert_eq!(
            registry.reserve(1, 0).unwrap_err(),
            IvshmemError::PeerRetired { link: 1, peer: 0 }
        );
        // The surviving peer keeps the link alive.
        assert!(registry.reserve(1, 1).is_err());
        drop(peer1);
        let recreated = registry.reserve(1, 0).unwrap();
        assert_eq!(recreated.peer_id().value(), 0);
    }

    #[test]
    fn attach_toggles_the_peer_slot() {
        let registry = registry();
        let reservation = registry.reserve(1, 0).unwrap();
        let attachment = reservation.attach().unwrap();
        assert_eq!(
            reservation.attach().unwrap_err(),
            IvshmemError::PeerAlreadyAttached { link: 1, peer: 0 }
        );
        assert_eq!(attachment.peer_id().value(), 0);
        assert_eq!(attachment.link().id().value(), 1);
        drop(attachment);
        // Bundle retries reuse the same reservation.
        let retried = reservation.attach().unwrap();
        drop(retried);
    }

    #[test]
    fn reservation_drop_while_attached_stays_consistent() {
        let registry = registry();
        let attachment = {
            let reservation = registry.reserve(1, 0).unwrap();
            reservation.attach().unwrap()
        };
        // The dropped reservation retired the slot; the stale attachment must
        // not resurrect it, and the link stays alive through the attachment.
        assert_eq!(
            registry.reserve(1, 0).unwrap_err(),
            IvshmemError::PeerRetired { link: 1, peer: 0 }
        );
        drop(attachment);
        let recreated = registry.reserve(1, 0).unwrap();
        recreated.attach().unwrap();
    }

    #[test]
    fn many_links_can_coexist_in_one_registry() {
        let registry = registry();
        let mut links = Vec::new();
        for link_id in 0..8u32 {
            links.push(registry.reserve(link_id, 0).unwrap());
        }
        for (index, link) in links.iter().enumerate() {
            assert_eq!(link.link().id().value(), index as u32);
        }
    }

    #[test]
    fn routes_a_doorbell_to_the_target_and_not_to_others() {
        let (peer0, peer1) = attached_pair();
        let sink0 = EventRecorder::new();
        let sink1 = EventRecorder::new();
        let attachment0 = peer0.attach().unwrap();
        let attachment1 = peer1.attach().unwrap();
        attachment0.set_event_sink(sink0.clone()).unwrap();
        attachment1.set_event_sink(sink1.clone()).unwrap();

        let written = doorbell(1, 0);
        attachment0
            .link()
            .deliver_doorbell(attachment0.peer_id(), written);
        assert_eq!(
            sink1.events(),
            vec![DoorbellEvent::new(attachment0.peer_id(), written)]
        );
        assert!(sink0.events().is_empty());
    }

    #[test]
    fn repeated_doorbells_merge_into_one_pending_bit() {
        let (peer0, peer1) = attached_pair();
        let sink1 = Arc::new(RegisterSink(Mutex::new(IvshmemRegisters::new())));
        let attachment0 = peer0.attach().unwrap();
        let attachment1 = peer1.attach().unwrap();
        attachment1.set_event_sink(sink1.clone()).unwrap();

        let link = attachment0.link();
        link.deliver_doorbell(attachment0.peer_id(), doorbell(1, 0));
        link.deliver_doorbell(attachment0.peer_id(), doorbell(1, 0));
        assert_eq!(sink1.pending(), 1);

        // The guest clears via W1C and the next doorbell pends again.
        sink1
            .0
            .lock()
            .unwrap()
            .write(crate::ivshmem::EVENT_STATUS_OFFSET, 1)
            .unwrap();
        assert_eq!(sink1.pending(), 0);
        link.deliver_doorbell(attachment0.peer_id(), doorbell(1, 0));
        assert_eq!(sink1.pending(), 1);
    }

    #[test]
    fn inactive_targets_and_unsupported_vectors_are_ignored() {
        // Peer 0 stays reserved without attaching: it is the inactive target.
        let (_peer0, peer1) = attached_pair();
        let sink1 = EventRecorder::new();
        let attachment1 = peer1.attach().unwrap();
        attachment1.set_event_sink(sink1.clone()).unwrap();

        // Peer 0 is reserved but not attached: the doorbell is a no-op.
        attachment1
            .link()
            .deliver_doorbell(attachment1.peer_id(), doorbell(0, 0));
        // Vector 1 is not supported by the current profile: also a no-op.
        attachment1
            .link()
            .deliver_doorbell(attachment1.peer_id(), doorbell(1, 1));
        // An out-of-profile target is a no-op, not a device error.
        attachment1
            .link()
            .deliver_doorbell(attachment1.peer_id(), doorbell(9, 0));
        assert!(sink1.events().is_empty());
    }

    #[test]
    fn a_failing_sink_is_ignored_without_panicking() {
        let (peer0, _peer1) = attached_pair();
        let attachment0 = peer0.attach().unwrap();
        attachment0.set_event_sink(Arc::new(FailingSink)).unwrap();
        attachment0
            .link()
            .deliver_doorbell(attachment0.peer_id(), doorbell(0, 0));
    }

    #[test]
    fn a_detached_endpoint_stops_receiving_doorbells() {
        let (peer0, peer1) = attached_pair();
        let sink1 = EventRecorder::new();
        let attachment0 = peer0.attach().unwrap();
        let attachment1 = peer1.attach().unwrap();
        attachment1.set_event_sink(sink1.clone()).unwrap();
        drop(attachment1);

        attachment0
            .link()
            .deliver_doorbell(attachment0.peer_id(), doorbell(1, 0));
        assert!(sink1.events().is_empty());
    }

    #[test]
    fn a_stale_sink_does_not_survive_reattachment() {
        let registry = registry();
        let reservation = registry.reserve(1, 0).unwrap();
        let stale = reservation.attach().unwrap();
        let stale_sink = EventRecorder::new();
        stale.set_event_sink(stale_sink.clone()).unwrap();
        drop(stale);
        // Bundle retries attach a new generation and must start without the
        // previous attachment's sink.
        let fresh = reservation.attach().unwrap();
        fresh
            .link()
            .deliver_doorbell(PeerId::new(1), doorbell(0, 0));
        assert!(stale_sink.events().is_empty());
    }

    #[test]
    fn set_event_sink_requires_the_current_attachment_generation() {
        let registry = registry();
        let reservation = registry.reserve(1, 0).unwrap();
        let current = reservation.attach().unwrap();
        // A stale-generation handle cannot arise in safe code: attachments
        // are neither Clone nor re-attachable while held. This white-box
        // handle exercises the generation guard on purpose.
        let stale = PeerAttachment {
            link: Arc::clone(reservation.link()),
            peer: reservation.peer_id(),
            generation: current.generation.wrapping_sub(1),
        };
        assert_eq!(
            stale.set_event_sink(EventRecorder::new()),
            Err(IvshmemError::PeerNotReserved { link: 1, peer: 0 })
        );
        current.set_event_sink(EventRecorder::new()).unwrap();
    }

    #[test]
    fn sinks_run_outside_the_peer_table_lock() {
        let registry = registry();
        let reservation = registry.reserve(1, 0).unwrap();
        let link = Arc::clone(reservation.link());
        let attachment = reservation.attach().unwrap();
        let probe = Arc::new(LockProbeSink {
            link: Arc::clone(&link),
            lock_observed: AtomicBool::new(false),
        });
        attachment.set_event_sink(probe.clone()).unwrap();
        link.deliver_doorbell(PeerId::new(1), doorbell(0, 0));
        assert!(!probe.lock_observed.load(Ordering::Relaxed));
    }

    #[test]
    fn state_writes_publish_into_the_shared_state_table() {
        let (peer0, peer1) = attached_pair();
        let link0 = Arc::clone(peer0.link());
        let link1 = Arc::clone(peer1.link());

        link0.publish_state(PeerId::new(0), 0x0001_0002).unwrap();
        // The owning link sees the entry through its own backing, and the
        // other peer's link is the same object: one shared state table.
        assert_eq!(link0.backing().read(0, 4).unwrap(), 0x0001_0002);
        assert_eq!(link1.backing().read(0, 4).unwrap(), 0x0001_0002);
        // The second peer's entry stays untouched.
        assert_eq!(link1.backing().read(4, 4).unwrap(), 0);

        // Reserved bytes inside the state-table page read zero even after
        // entries are published.
        assert_eq!(link0.backing().read(0x8, 4).unwrap(), 0);
        assert_eq!(link0.backing().read(0xffc, 4).unwrap(), 0);

        // Clearing zeroes the entry without touching the shared region.
        link0.clear_state(PeerId::new(0)).unwrap();
        assert_eq!(link0.backing().read(0, 4).unwrap(), 0);
    }

    #[test]
    fn state_publish_rejects_unknown_peers() {
        let registry = registry();
        let reservation = registry.reserve(1, 0).unwrap();
        let link = Arc::clone(reservation.link());
        assert_eq!(
            link.publish_state(PeerId::new(2), 0),
            Err(IvshmemError::PeerOutOfProfile {
                peer: 2,
                max_peers: MAX_PEERS
            })
        );
    }

    #[test]
    fn doorbell_diagnostics_log_once_per_interval() {
        let counter = AtomicU32::new(0);
        assert!(IgnoredDoorbellCounters::should_log(&counter));
        // Occurrences 2..1023 stay silent; 1024 logs again.
        for _ in 1..DOORBELL_DIAGNOSTIC_INTERVAL - 1 {
            assert!(!IgnoredDoorbellCounters::should_log(&counter));
        }
        assert!(IgnoredDoorbellCounters::should_log(&counter));
    }
}
