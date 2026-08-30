//! Static BAR2 layout of one ivshmem link.
//!
//! The layout object is the single source of truth for how BAR2 is divided:
//! F4's state table, F5's common/output sections, and F6's stage-2 mapping
//! plans all derive from it. It lives in `axdevice::ivshmem` because every
//! peer of one link must observe the same division.

use alloc::{format, string::String};

use super::{error::IvshmemError, link::PeerId};

/// One aligned section of the shared BAR2 region.
///
/// Both bounds are 4 KiB aligned on purpose: F6 enforces per-section stage-2
/// permissions at page granularity, so a section that is not page aligned can
/// never be mapped without sharing a page with a neighbour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SectionDesc {
    offset: u64,
    size: u64,
}

impl SectionDesc {
    /// The alignment every section must keep (stage-2 page granularity).
    const ALIGNMENT: u64 = 0x1000;

    /// Creates one section, rejecting unaligned bounds, a size of zero and
    /// overflow of `offset + size`.
    ///
    /// # Errors
    ///
    /// Returns [`IvshmemError::InvalidLayout`] when `offset` or `size` is not
    /// a 4 KiB multiple, `size` is zero, or the range overflows `u64`.
    pub fn new(offset: u64, size: u64) -> Result<Self, IvshmemError> {
        let reject = |detail: String| IvshmemError::InvalidLayout { detail };
        if !offset.is_multiple_of(Self::ALIGNMENT) {
            return Err(reject(format!(
                "section offset {offset:#x} is not {:#x}-aligned",
                Self::ALIGNMENT
            )));
        }
        if !size.is_multiple_of(Self::ALIGNMENT) {
            return Err(reject(format!(
                "section size {size:#x} is not {:#x}-aligned",
                Self::ALIGNMENT
            )));
        }
        if size == 0 {
            return Err(reject("section size is zero".into()));
        }
        offset.checked_add(size).ok_or_else(|| {
            reject(format!(
                "section offset {offset:#x} + size {size:#x} overflows u64"
            ))
        })?;
        Ok(Self { offset, size })
    }

    /// Returns the section start offset inside BAR2.
    pub const fn offset(self) -> u64 {
        self.offset
    }

    /// Returns the section size in bytes.
    pub const fn size(self) -> u64 {
        self.size
    }

    /// Returns whether `offset` lies inside the section.
    pub const fn contains(self, offset: u64) -> bool {
        offset >= self.offset && offset - self.offset < self.size
    }
}

/// Coarse classification of one BAR2 offset before F5 freezes full sections.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Bar2Region {
    /// Host-owned state table; guests read it, only BAR0 State writes move it.
    StateTable,
    /// Every byte outside the state table; shared read/write across peers.
    Shared,
}

/// Static layout of the shared BAR2 region of one link.
#[derive(Clone, Copy, Debug)]
pub struct IvshmemMemoryLayout {
    state_table: SectionDesc,
    bar2_size: u64,
}

impl IvshmemMemoryLayout {
    /// Size of one peer's state-table entry in bytes.
    const STATE_ENTRY_SIZE: u64 = 4;

    /// Derives the F4 layout: one 4 KiB state-table page at offset 0.
    ///
    /// # Errors
    ///
    /// Returns [`IvshmemError::InvalidLayout`] when `bar2_size` is smaller
    /// than one page or cannot hold `max_peers` 32-bit entries.
    pub fn derive(bar2_size: u64, max_peers: u16) -> Result<Self, IvshmemError> {
        let state_table = SectionDesc::new(0, SectionDesc::ALIGNMENT)?;
        let entries_end = state_table.offset + u64::from(max_peers) * Self::STATE_ENTRY_SIZE;
        if bar2_size < state_table.size() {
            return Err(IvshmemError::InvalidLayout {
                detail: format!(
                    "BAR2 size {bar2_size:#x} cannot host the {:#x} state-table page",
                    state_table.size()
                ),
            });
        }
        if entries_end > state_table.size() {
            return Err(IvshmemError::InvalidLayout {
                detail: format!(
                    "{max_peers} state entries end at {entries_end:#x} beyond the {:#x} \
                     state-table page",
                    state_table.size()
                ),
            });
        }
        Ok(Self {
            state_table,
            bar2_size,
        })
    }

    /// Returns the state-table section.
    pub const fn state_table(&self) -> SectionDesc {
        self.state_table
    }

    /// Returns the total BAR2 size this layout was derived from.
    pub const fn bar2_size(&self) -> u64 {
        self.bar2_size
    }

    /// Returns the offset of one peer's state-table entry.
    ///
    /// The entry arithmetic lives here so links and tests never repeat the
    /// `table + peer * 4` formula.
    ///
    /// # Errors
    ///
    /// Returns [`IvshmemError::PeerOutOfProfile`] when `peer` is at or beyond
    /// the peer count this layout was derived from.
    pub fn state_entry_offset(&self, peer: PeerId, max_peers: u16) -> Result<u64, IvshmemError> {
        if peer.value() >= max_peers {
            return Err(IvshmemError::PeerOutOfProfile {
                peer: peer.value(),
                max_peers,
            });
        }
        Ok(self.state_table.offset() + u64::from(peer.value()) * Self::STATE_ENTRY_SIZE)
    }

    /// Classifies one BAR2 offset for F4 semantics.
    pub const fn region(&self, offset: u64) -> Bar2Region {
        if self.state_table.contains(offset) {
            Bar2Region::StateTable
        } else {
            Bar2Region::Shared
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::{format, string::ToString};

    use super::*;
    use crate::ivshmem::SHARED_MEMORY_SIZE;

    #[test]
    fn derives_the_state_table_page_at_offset_zero() {
        let layout = IvshmemMemoryLayout::derive(SHARED_MEMORY_SIZE, 2).unwrap();
        assert_eq!(layout.bar2_size(), SHARED_MEMORY_SIZE);
        assert_eq!(layout.state_table().offset(), 0);
        assert_eq!(layout.state_table().size(), 0x1000);
        // Both peer entries fit inside the page.
        assert_eq!(layout.state_entry_offset(PeerId::new(0), 2).unwrap(), 0);
        assert_eq!(layout.state_entry_offset(PeerId::new(1), 2).unwrap(), 4);
    }

    #[test]
    fn rejects_bar2_sizes_that_cannot_host_the_state_table() {
        // Smaller than one page.
        let small = IvshmemMemoryLayout::derive(0x800, 2).unwrap_err();
        assert!(small.to_string().contains("cannot host"));
        // Enough pages, but not enough room for the peer entries.
        let crowded = IvshmemMemoryLayout::derive(0x1000, 1025).unwrap_err();
        assert!(crowded.to_string().contains("state entries end at"));
        // Exactly the entries the profile needs still derives.
        IvshmemMemoryLayout::derive(0x1000, 1024).unwrap();
    }

    #[test]
    fn rejects_entries_outside_the_derived_profile() {
        let layout = IvshmemMemoryLayout::derive(SHARED_MEMORY_SIZE, 2).unwrap();
        assert_eq!(
            layout.state_entry_offset(PeerId::new(2), 2),
            Err(IvshmemError::PeerOutOfProfile {
                peer: 2,
                max_peers: 2
            })
        );
    }

    #[test]
    fn classifies_state_table_and_shared_offsets() {
        let layout = IvshmemMemoryLayout::derive(SHARED_MEMORY_SIZE, 2).unwrap();
        for offset in [0, 4, 0x800, 0xfff] {
            assert_eq!(layout.region(offset), Bar2Region::StateTable);
        }
        for offset in [0x1000, 0x1004, SHARED_MEMORY_SIZE - 4] {
            assert_eq!(layout.region(offset), Bar2Region::Shared);
        }
    }

    #[test]
    fn sections_reject_unaligned_or_empty_bounds() {
        let expected_detail = |detail: &str| IvshmemError::InvalidLayout {
            detail: detail.into(),
        };
        assert_eq!(
            SectionDesc::new(0x800, 0x1000),
            Err(expected_detail(
                "section offset 0x800 is not 0x1000-aligned"
            ))
        );
        assert_eq!(
            SectionDesc::new(0, 0x800),
            Err(expected_detail("section size 0x800 is not 0x1000-aligned"))
        );
        assert_eq!(
            SectionDesc::new(0, 0),
            Err(expected_detail("section size is zero"))
        );
        assert!(
            format!("{}", SectionDesc::new(0x800, 0x1000).unwrap_err())
                .contains("not 0x1000-aligned")
        );
    }

    #[test]
    fn section_contains_its_range_exclusively() {
        let section = SectionDesc::new(0x1000, 0x2000).unwrap();
        assert!(section.contains(0x1000));
        assert!(section.contains(0x2fff));
        assert!(!section.contains(0xfff));
        assert!(!section.contains(0x3000));
    }
}
