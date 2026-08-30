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
use core::fmt;

use ax_sync::SpinLock;

use super::{SharedBarBacking, error::IvshmemError};

/// Maximum peers of the current device profile.
pub const MAX_PEERS: u16 = 2;

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
}

/// Shared state of one ivshmem link: profile, backing, and peer slots.
pub struct IvshmemLink {
    id: LinkId,
    max_peers: u16,
    backing: SharedBarBacking,
    peers: SpinLock<LinkPeers>,
}

impl IvshmemLink {
    fn new(id: LinkId, max_peers: u16) -> Result<Self, IvshmemError> {
        let backing = SharedBarBacking::try_new(super::SHARED_MEMORY_SIZE)?;
        Ok(Self {
            id,
            max_peers,
            backing,
            peers: SpinLock::new(LinkPeers {
                next_attachment_generation: 0,
                slots: vec![PeerSlot::Vacant; usize::from(max_peers)].into_boxed_slice(),
            }),
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
#[derive(Default)]
pub struct IvshmemLinkRegistry {
    links: SpinLock<BTreeMap<LinkId, Weak<IvshmemLink>>>,
}

impl IvshmemLinkRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
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
                let link = Arc::new(IvshmemLink::new(id, MAX_PEERS)?);
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
    use alloc::{sync::Arc, vec::Vec};

    use super::*;
    use crate::ivshmem::SHARED_MEMORY_SIZE;

    fn registry() -> IvshmemLinkRegistry {
        IvshmemLinkRegistry::new()
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
}
