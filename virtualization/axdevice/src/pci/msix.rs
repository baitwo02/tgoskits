//! Emulated MSI-X capability and table state of one PCI endpoint.
//!
//! The device face follows the PCI Local Bus specification's MSI-X model:
//! the capability exposes Enable/Function Mask and the fixed table/PBA
//! locations, the guest programs message address/data/mask through the BAR1
//! table window, and delivery reads the table at the moment of the doorbell.
//! The table is the single source of truth — the device keeps no shadow copy
//! of the message configuration.

use alloc::string::String;

use axdevice_base::DeviceError;

/// Size of the MSI-X BAR: one 4 KiB page keeps table and PBA page-aligned.
pub const MSIX_BAR_SIZE: u64 = 0x1000;
/// BAR slot carrying the table/PBA window (frozen by the current profile).
pub const MSIX_BAR_INDEX: u8 = 1;
/// Offset of the MSI-X table inside the BAR.
pub const MSIX_TABLE_OFFSET: u64 = 0x0000;
/// Offset of the MSI-X PBA inside the BAR.
pub const MSIX_PBA_OFFSET: u64 = 0x0800;
/// Table entry layout: address, upper address, data, vector control.
pub const MSIX_TABLE_ENTRY_SIZE: u64 = 16;

/// Message Control register bits (low 16-bit half of the capability dword).
pub const MSIX_MESSAGE_CONTROL_ENABLE: u16 = 0x8000;
pub const MSIX_MESSAGE_CONTROL_FUNCTION_MASK: u16 = 0x4000;

/// One vector's guest-programmed table content.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MsixTableEntry {
    message_address: u32,
    message_upper_address: u32,
    message_data: u32,
    vector_control: u32,
}

impl MsixTableEntry {
    /// Creates disabled, unmasked, empty entry state.
    pub const fn new() -> Self {
        Self {
            message_address: 0,
            message_upper_address: 0,
            message_data: 0,
            vector_control: 0,
        }
    }

    /// Returns the low 32 bits of the message address.
    pub const fn message_address(self) -> u32 {
        self.message_address
    }

    /// Returns the upper 32 bits of the message address.
    pub const fn message_upper_address(self) -> u32 {
        self.message_upper_address
    }

    /// Returns the message data value.
    pub const fn message_data(self) -> u32 {
        self.message_data
    }

    /// Returns whether the per-vector mask bit (bit 0 of vector control) is
    /// set.
    pub const fn masked(self) -> bool {
        self.vector_control & 1 != 0
    }

    /// Returns the entry with `mask` applied to bit 0 of vector control.
    pub const fn with_mask(self, mask: bool) -> Self {
        let vector_control = if mask {
            self.vector_control | 1
        } else {
            self.vector_control & !1
        };
        Self {
            vector_control,
            ..self
        }
    }

    fn word_mut(&mut self, offset: u64) -> Option<&mut u32> {
        match offset {
            0x0 => Some(&mut self.message_address),
            0x4 => Some(&mut self.message_upper_address),
            0x8 => Some(&mut self.message_data),
            0xc => Some(&mut self.vector_control),
            _ => None,
        }
    }
}

/// MSI-X requirement carried inside `PciFunctionRequirement`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PciMsixDeclaration {
    vectors: u16,
}

impl PciMsixDeclaration {
    /// Declares an MSI-X capability with `vectors` vectors.
    ///
    /// # Errors
    ///
    /// Returns [`crate::PciError::InvalidMsix`] when `vectors` is zero or
    /// beyond 2048. The current AxVisor profile freezes one vector; more
    /// vectors require a new profile revision.
    pub fn new(vectors: u16) -> Result<Self, crate::PciError> {
        const MAX_VECTORS: u16 = 2048;
        if vectors == 0 || vectors > MAX_VECTORS {
            return Err(crate::PciError::InvalidMsix {
                detail: alloc::format!(
                    "MSI-X vector count {vectors} must be within 1..={MAX_VECTORS}"
                ),
            });
        }
        Ok(Self { vectors })
    }

    /// Returns the declared vector count.
    pub const fn vectors(self) -> u16 {
        self.vectors
    }
}

/// Emulated MSI-X capability and BAR state of one endpoint.
///
/// The table is the single source of truth for message configuration: the
/// device reads it at delivery time and keeps no shadow copy.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MsixState {
    message_control: u16,
    table: MsixTableEntry,
    pba: u32,
}

impl MsixState {
    /// Creates disabled, unmasked, empty state.
    pub const fn new() -> Self {
        Self {
            message_control: 0,
            table: MsixTableEntry::new(),
            pba: 0,
        }
    }

    /// Reads the Message Control register (16-bit value in the low half).
    pub const fn read_message_control(self) -> u16 {
        self.message_control
    }

    /// Writes the Message Control register; only Enable and Function Mask
    /// are writable, Table Size and BIR fields are read-only.
    pub fn write_message_control(&mut self, value: u16) {
        let writable = MSIX_MESSAGE_CONTROL_ENABLE | MSIX_MESSAGE_CONTROL_FUNCTION_MASK;
        self.message_control = (self.message_control & !writable) | (value & writable);
    }

    /// Returns whether MSI-X is enabled.
    pub const fn enabled(self) -> bool {
        self.message_control & MSIX_MESSAGE_CONTROL_ENABLE != 0
    }

    /// Returns whether the function mask is set.
    pub const fn function_masked(self) -> bool {
        self.message_control & MSIX_MESSAGE_CONTROL_FUNCTION_MASK != 0
    }

    /// Reads one aligned Dword of the MSI-X BAR (table or PBA window).
    ///
    /// # Errors
    ///
    /// Returns `DeviceError::OutOfRange` for offsets outside the table or
    /// PBA windows and `DeviceError::InvalidInput` for unaligned offsets.
    pub fn read_bar(&self, offset: u64) -> Result<u32, DeviceError> {
        let reject = |detail: String| DeviceError::InvalidInput {
            operation: "read ivshmem MSI-X BAR",
            detail,
        };
        if !offset.is_multiple_of(4) {
            return Err(reject(alloc::format!(
                "MSI-X BAR offset {offset:#x} is not 4-byte aligned"
            )));
        }
        if (MSIX_TABLE_OFFSET..MSIX_TABLE_OFFSET + MSIX_TABLE_ENTRY_SIZE).contains(&offset) {
            let word = (offset - MSIX_TABLE_OFFSET) / 4;
            let value = match word {
                0 => self.table.message_address,
                1 => self.table.message_upper_address,
                2 => self.table.message_data,
                3 => self.table.vector_control,
                _ => 0,
            };
            return Ok(value);
        }
        if offset == MSIX_PBA_OFFSET {
            return Ok(self.pba);
        }
        if offset >= MSIX_BAR_SIZE {
            return Err(DeviceError::OutOfRange { addr: offset });
        }
        // Reserved BAR bytes read zero.
        Ok(0)
    }

    /// Writes one aligned Dword of the MSI-X BAR.
    ///
    /// Table writes update the entry; PBA writes are write-1-to-clear and
    /// report via the return value whether a pending bit was cleared, which
    /// is the caller's trigger to attempt a delivery.
    ///
    /// # Errors
    ///
    /// Same conditions as [`read_bar`](Self::read_bar).
    pub fn write_bar(&mut self, offset: u64, value: u32) -> Result<bool, DeviceError> {
        let reject = |detail: String| DeviceError::InvalidInput {
            operation: "write ivshmem MSI-X BAR",
            detail,
        };
        if !offset.is_multiple_of(4) {
            return Err(reject(alloc::format!(
                "MSI-X BAR offset {offset:#x} is not 4-byte aligned"
            )));
        }
        if (MSIX_TABLE_OFFSET..MSIX_TABLE_OFFSET + MSIX_TABLE_ENTRY_SIZE).contains(&offset) {
            let word = (offset - MSIX_TABLE_OFFSET) / 4;
            match self.table.word_mut(word * 4) {
                Some(field) => {
                    *field = value;
                    return Ok(false);
                }
                None => {
                    return Err(DeviceError::OutOfRange { addr: offset });
                }
            }
        }
        if offset == MSIX_PBA_OFFSET {
            let cleared = self.pba & value != 0;
            self.pba &= !value;
            return Ok(cleared);
        }
        if offset >= MSIX_BAR_SIZE {
            return Err(DeviceError::OutOfRange { addr: offset });
        }
        // Reserved BAR bytes ignore writes.
        Ok(false)
    }

    /// Returns the current table entry of one vector.
    ///
    /// The single-vector profile only knows vector 0; other vectors read as
    /// `None`.
    pub const fn table_entry(&self, vector: u16) -> Option<MsixTableEntry> {
        match vector {
            0 => Some(self.table),
            _ => None,
        }
    }

    /// Returns whether `vector` is blocked from immediate delivery: MSI-X
    /// disabled, function masked, or the per-vector mask set.
    pub const fn vector_blocked(&self, vector: u16) -> bool {
        if !self.enabled() || self.function_masked() {
            return true;
        }
        match self.table_entry(vector) {
            Some(entry) => entry.masked(),
            None => true,
        }
    }

    /// Sets the PBA bit of `vector`; returns true when it was previously
    /// clear.
    pub fn set_pending(&mut self, vector: u16) -> bool {
        if vector != 0 {
            return false;
        }
        let was_clear = self.pba & 1 == 0;
        self.pba |= 1;
        was_clear
    }

    /// Returns the PBA bit of `vector`.
    pub const fn pending(&self, vector: u16) -> bool {
        match vector {
            0 => self.pba & 1 != 0,
            _ => false,
        }
    }

    /// Clears Enable, function mask, table runtime state and PBA.
    pub fn reset(&mut self) {
        *self = Self::new();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declaration_validates_the_vector_count() {
        assert_eq!(PciMsixDeclaration::new(1).unwrap().vectors(), 1);
        assert!(PciMsixDeclaration::new(0).is_err());
        assert!(PciMsixDeclaration::new(2049).is_err());
    }

    #[test]
    fn message_control_exposes_only_writable_bits() {
        let mut state = MsixState::new();
        assert!(!state.enabled());
        assert!(!state.function_masked());
        // Table Size (bits 10:0) and BIR fields ignore writes.
        state.write_message_control(0x0007);
        assert_eq!(state.read_message_control(), 0);
        state.write_message_control(0x8000);
        assert!(state.enabled());
        assert!(!state.function_masked());
        state.write_message_control(0xC000);
        assert!(state.enabled());
        assert!(state.function_masked());
    }

    #[test]
    fn table_window_round_trips_all_four_words() {
        let mut state = MsixState::new();
        state.write_bar(0x0, 0xdead_beef).unwrap();
        state.write_bar(0x4, 0x1).unwrap();
        state.write_bar(0x8, 0x42).unwrap();
        state.write_bar(0xc, 0x1).unwrap();
        let entry = state.table_entry(0).unwrap();
        assert_eq!(entry.message_address(), 0xdead_beef);
        assert_eq!(entry.message_upper_address(), 1);
        assert_eq!(entry.message_data(), 0x42);
        assert!(entry.masked());
        assert_eq!(state.read_bar(0x0).unwrap(), 0xdead_beef);
        assert_eq!(state.read_bar(0xc).unwrap(), 1);
    }

    #[test]
    fn per_vector_mask_unmasks_the_entry() {
        let mut state = MsixState::new();
        state.write_bar(0xc, 0x1).unwrap();
        assert!(state.table_entry(0).unwrap().masked());
        state.write_bar(0xc, 0x0).unwrap();
        assert!(!state.table_entry(0).unwrap().masked());
    }

    #[test]
    fn pba_is_write_one_to_clear() {
        let mut state = MsixState::new();
        assert!(state.set_pending(0));
        assert!(state.pending(0));
        // Writing one clears and reports the pending bit.
        assert!(state.write_bar(MSIX_PBA_OFFSET, 1).unwrap());
        assert!(!state.pending(0));
        // Writing zero changes nothing.
        state.set_pending(0);
        assert!(!state.write_bar(MSIX_PBA_OFFSET, 0).unwrap());
        assert!(state.pending(0));
        // Writing one when clear reports nothing to re-deliver.
        state.write_bar(MSIX_PBA_OFFSET, 1).unwrap();
        assert!(!state.write_bar(MSIX_PBA_OFFSET, 1).unwrap());
    }

    #[test]
    fn rejects_unaligned_and_out_of_range_accesses() {
        let mut state = MsixState::new();
        assert!(state.read_bar(2).is_err());
        assert!(state.write_bar(2, 0).is_err());
        // 0x10 is past the single-entry table window: reserved, reads zero.
        assert_eq!(state.read_bar(0x10).unwrap(), 0);
        assert!(state.read_bar(MSIX_BAR_SIZE).is_err());
        assert!(state.write_bar(MSIX_BAR_SIZE, 0).is_err());
        // Reserved BAR bytes read zero and ignore writes.
        assert_eq!(state.read_bar(0x804).unwrap(), 0);
        state.write_bar(0x804, 0xffff_ffff).unwrap();
        assert_eq!(state.read_bar(0x804).unwrap(), 0);
    }

    #[test]
    fn reset_clears_everything() {
        let mut state = MsixState::new();
        state.write_message_control(0xC000);
        state.write_bar(0x0, 0x1234).unwrap();
        state.set_pending(0);
        state.reset();
        assert_eq!(state, MsixState::new());
    }

    #[test]
    fn vector_blocked_follows_the_gate_chain() {
        let mut state = MsixState::new();
        // Disabled: blocked regardless of masks.
        assert!(state.vector_blocked(0));
        state.write_message_control(0x8000);
        assert!(!state.vector_blocked(0));
        // Function mask blocks.
        state.write_message_control(0xC000);
        assert!(state.vector_blocked(0));
        state.write_message_control(0x8000);
        // Vector mask blocks.
        state.write_bar(0xc, 0x1).unwrap();
        assert!(state.vector_blocked(0));
        state.write_bar(0xc, 0x0).unwrap();
        assert!(!state.vector_blocked(0));
        // Unknown vectors are always blocked.
        assert!(state.vector_blocked(3));
    }
}
