//! Shared BAR2 backing of one ivshmem link.

use alloc::{boxed::Box, vec::Vec};
use core::ops::Range;

use ax_sync::SpinLock;

use super::error::IvshmemError;

/// Byte-copying shared backing behind every peer BAR2 of one link.
///
/// The lock only serializes single accesses; guest-visible ordering is a
/// protocol contract (write data, publish state, then ring the doorbell), not
/// an implementation detail of this lock.
pub struct SharedBarBacking {
    size: u64,
    bytes: SpinLock<Box<[u8]>>,
}

impl SharedBarBacking {
    /// Allocates a zeroed backing of `size` bytes.
    ///
    /// # Errors
    ///
    /// Returns [`IvshmemError::AllocationFailed`] when the size is not
    /// representable or the allocation cannot reserve `size` bytes.
    pub fn try_new(size: u64) -> Result<Self, IvshmemError> {
        const OPERATION: &str = "allocate ivshmem shared memory";
        let byte_count = usize::try_from(size).map_err(|_| IvshmemError::AllocationFailed {
            operation: OPERATION,
        })?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(byte_count)
            .map_err(|_| IvshmemError::AllocationFailed {
                operation: OPERATION,
            })?;
        bytes.resize(byte_count, 0);
        Ok(Self {
            size,
            bytes: SpinLock::new(bytes.into_boxed_slice()),
        })
    }

    /// Returns the backing size in bytes.
    pub const fn size(&self) -> u64 {
        self.size
    }

    /// Reads `width` bytes at `offset` as one little-endian value.
    ///
    /// # Errors
    ///
    /// Returns [`IvshmemError::InvalidSharedMemoryWidth`] for widths outside
    /// `1 | 2 | 4 | 8` and [`IvshmemError::SharedMemoryOutOfRange`] when the
    /// access leaves the region.
    pub fn read(&self, offset: u64, width: usize) -> Result<u64, IvshmemError> {
        let range = self.access_range(offset, width)?;
        let bytes = self.bytes.lock_irqsave();
        let mut value = [0u8; 8];
        value[..range.len()].copy_from_slice(&bytes[range]);
        Ok(u64::from_le_bytes(value))
    }

    /// Writes the low `width` bytes of `value` at `offset` little-endian.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`read`](Self::read).
    pub fn write(&self, offset: u64, width: usize, value: u64) -> Result<(), IvshmemError> {
        let range = self.access_range(offset, width)?;
        let byte_count = range.len();
        let mut bytes = self.bytes.lock_irqsave();
        bytes[range].copy_from_slice(&value.to_le_bytes()[..byte_count]);
        Ok(())
    }

    fn access_range(&self, offset: u64, width: usize) -> Result<Range<usize>, IvshmemError> {
        if !matches!(width, 1 | 2 | 4 | 8) {
            return Err(IvshmemError::InvalidSharedMemoryWidth { width });
        }
        let out_of_range = || IvshmemError::SharedMemoryOutOfRange {
            offset,
            width,
            size: self.size,
        };
        let end = offset.checked_add(width as u64).ok_or_else(out_of_range)?;
        if end > self.size {
            return Err(out_of_range());
        }
        let start = usize::try_from(offset).map_err(|_| out_of_range())?;
        Ok(start..start + width)
    }
}

#[cfg(test)]
mod tests {
    use alloc::format;

    use super::*;

    fn backing() -> SharedBarBacking {
        SharedBarBacking::try_new(0x100).unwrap()
    }

    #[test]
    fn starts_zeroed_and_reports_its_size() {
        let backing = backing();
        assert_eq!(backing.size(), 0x100);
        assert_eq!(backing.read(0, 8).unwrap(), 0);
    }

    #[test]
    fn reads_and_writes_all_access_widths_little_endian() {
        let backing = backing();
        backing.write(0x10, 1, 0xa5).unwrap();
        backing.write(0x12, 2, 0xbeef).unwrap();
        backing.write(0x20, 4, 0xdead_beef).unwrap();
        backing.write(0x40, 8, 0x0102_0304_0506_0708).unwrap();
        assert_eq!(backing.read(0x10, 1).unwrap(), 0xa5);
        assert_eq!(backing.read(0x12, 2).unwrap(), 0xbeef);
        assert_eq!(backing.read(0x20, 4).unwrap(), 0xdead_beef);
        assert_eq!(backing.read(0x40, 8).unwrap(), 0x0102_0304_0506_0708);
        // Wider reads observe the little-endian bytes of narrower writes.
        assert_eq!(backing.read(0x10, 4).unwrap(), 0xbeef_00a5);
    }

    #[test]
    fn rejects_widths_outside_the_allowed_set() {
        let backing = backing();
        for width in [0, 3, 5, 16] {
            assert_eq!(
                backing.read(0, width),
                Err(IvshmemError::InvalidSharedMemoryWidth { width })
            );
            assert_eq!(
                backing.write(0, width, 0),
                Err(IvshmemError::InvalidSharedMemoryWidth { width })
            );
        }
    }

    #[test]
    fn rejects_accesses_beyond_the_shared_region() {
        let backing = backing();
        let expected = |offset, width| IvshmemError::SharedMemoryOutOfRange {
            offset,
            width,
            size: 0x100,
        };
        // The last byte is still accessible; one byte further is not.
        backing.read(0xff, 1).unwrap();
        assert_eq!(backing.read(0xff, 2), Err(expected(0xff, 2)));
        assert_eq!(backing.read(0x100, 1), Err(expected(0x100, 1)));
        assert_eq!(backing.write(0xf9, 8, 0), Err(expected(0xf9, 8)));
        // Offset arithmetic itself must not overflow.
        assert_eq!(
            backing.read(u64::MAX - 1, 8),
            Err(expected(u64::MAX - 1, 8))
        );
        assert_eq!(
            format!("{}", expected(0x100, 1)),
            "ivshmem shared-memory access at offset 0x100 with 1 bytes exceeds the 0x100-byte \
             region"
        );
    }
}
