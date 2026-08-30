//! OS-agnostic ivshmem link semantics shared by peer endpoints.
//!
//! This module owns the device-side state that peers of one ivshmem link
//! share: the register page, the shared-memory backing, and the peer
//! lifecycle. It deliberately knows nothing about PCI config space, BAR
//! routing, guest address spaces, or TOML options; the AxVM adapter binds
//! this semantics to a [`crate::pci::PciFunction`].
//!
//! Locking: the registry is only taken on the configuration path with the
//! `registry -> link` order. Register and backing locks are independent and
//! are never nested; callbacks outside these locks arrive with later
//! features and must keep that order.

mod backing;
mod error;
mod link;
mod registers;

pub use backing::SharedBarBacking;
pub use error::IvshmemError;
pub use link::{
    IvshmemLink, IvshmemLinkRegistry, LinkId, MAX_PEERS, PeerAttachment, PeerId, PeerReservation,
};
pub use registers::{
    DOORBELL_OFFSET, EVENT_STATUS_OFFSET, ID_OFFSET, INTERRUPT_CONTROL_OFFSET, IvshmemRegisters,
    MAXIMUM_PEERS_OFFSET, REGISTER_PAGE_SIZE, STATE_OFFSET,
};

/// Shared-memory BAR size of the current device profile.
pub const SHARED_MEMORY_SIZE: u64 = 0x1_0000;
