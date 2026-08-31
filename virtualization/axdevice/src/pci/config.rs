//! Conventional Type-0 config image and guest-writable root state.

use alloc::vec::Vec;

use super::{
    PciBdf, PciEndpointIdentity, PciResult,
    address::CONFIG_SPACE_SIZE,
    bar::{BarState, ResolvedBarPlan},
    function::PciConfigByte,
    msix::{MSIX_BAR_INDEX, MSIX_PBA_OFFSET, MSIX_TABLE_OFFSET},
};

/// Fixed config-space location of the MSI-X capability (frozen by design).
pub(crate) const MSIX_CAP_OFFSET: usize = 0x40;
/// Capability ID for MSI-X (PCI Local Bus spec, appendix H).
const MSIIX_CAP_ID: u8 = 0x11;
/// Status register bit 4: capability list present.
const STATUS_CAP_LIST_PRESENT: u8 = 0x10;
/// Writable Message Control bits: MSI-X Enable (15) and Function Mask (14).
const MSIX_CONTROL_WRITABLE_HIGH: u8 = 0xC0;

const COMMAND_MEMORY_SPACE_ENABLE: u8 = 0x02;
const COMMAND_BUS_MASTER_ENABLE: u8 = 0x04;
// PCI Interrupt Disable is command bit 10: bit 2 of the high command byte.
const COMMAND_INTX_DISABLE_HIGH: u8 = 0x04;

/// Standard command state observed after one configuration-space write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PciCommandState {
    pub(crate) memory_space_enabled: bool,
    pub(crate) bus_master_enabled: bool,
    pub(crate) intx_disabled: bool,
}

impl PciCommandState {
    pub(crate) fn from_config(config: &[u8; CONFIG_SPACE_SIZE]) -> Self {
        Self {
            memory_space_enabled: config[4] & COMMAND_MEMORY_SPACE_ENABLE != 0,
            bus_master_enabled: config[4] & COMMAND_BUS_MASTER_ENABLE != 0,
            intx_disabled: config[5] & COMMAND_INTX_DISABLE_HIGH != 0,
        }
    }
}

#[derive(Clone)]
pub(crate) struct PowerOnConfig {
    bytes: [u8; CONFIG_SPACE_SIZE],
    write_mask: [u8; CONFIG_SPACE_SIZE],
}

impl PowerOnConfig {
    pub(crate) fn build(
        identity: PciEndpointIdentity,
        bars: &[ResolvedBarPlan],
        config_bytes: &[PciConfigByte],
        msix: Option<super::msix::PciMsixDeclaration>,
    ) -> PciResult<Self> {
        if identity.vendor_id() == u16::MAX {
            return Err(super::PciError::InvalidEndpointIdentity {
                detail: "vendor ID 0xffff denotes an absent function",
            });
        }
        let mut bytes = [0; CONFIG_SPACE_SIZE];
        let mut write_mask = [0; CONFIG_SPACE_SIZE];
        bytes[0..2].copy_from_slice(&identity.vendor_id().to_le_bytes());
        bytes[2..4].copy_from_slice(&identity.device_id().to_le_bytes());
        write_mask[4] = COMMAND_MEMORY_SPACE_ENABLE | COMMAND_BUS_MASTER_ENABLE;
        write_mask[5] = COMMAND_INTX_DISABLE_HIGH;
        let class = identity.class();
        bytes[8] = identity.revision();
        bytes[9] = class.programming_interface();
        bytes[10] = class.subclass();
        bytes[11] = class.base();
        bytes[14] = 0;
        for patch in config_bytes {
            let offset = usize::from(patch.offset.value());
            bytes[offset] = patch.value;
            write_mask[offset] = patch.write_mask;
        }
        for bar in bars {
            let offset = bar.index.config_offset();
            let attributes = if bar.prefetchable { 0x8 } else { 0 };
            bytes[offset..offset + 4]
                .copy_from_slice(&((bar.address as u32 & 0xffff_fff0) | attributes).to_le_bytes());
        }
        if let Some(msix) = msix {
            // The MSI-X capability sits at the frozen offset; the status
            // register advertises the capability list and the pointer chain
            // terminates after it.
            bytes[6] |= STATUS_CAP_LIST_PRESENT;
            bytes[0x34] = MSIX_CAP_OFFSET as u8;
            bytes[MSIX_CAP_OFFSET] = MSIIX_CAP_ID;
            bytes[MSIX_CAP_OFFSET + 1] = 0; // next pointer: last capability
            // Message Control: table size (vectors - 1) in the low bits is
            // read-only; Enable and Function Mask are guest-writable.
            let table_size = msix.vectors() - 1;
            bytes[MSIX_CAP_OFFSET + 2..MSIX_CAP_OFFSET + 4]
                .copy_from_slice(&table_size.to_le_bytes());
            write_mask[MSIX_CAP_OFFSET + 2] = 0;
            write_mask[MSIX_CAP_OFFSET + 3] = MSIX_CONTROL_WRITABLE_HIGH;
            // Table BIR/offset and PBA BIR/offset: frozen to BAR 1. The
            // offset occupies bits 31:3 verbatim (8-byte alignment) and the
            // low three bits carry the BIR.
            let table_bir = MSIX_TABLE_OFFSET as u32 | u32::from(MSIX_BAR_INDEX);
            bytes[MSIX_CAP_OFFSET + 4..MSIX_CAP_OFFSET + 8]
                .copy_from_slice(&table_bir.to_le_bytes());
            let pba_bir = MSIX_PBA_OFFSET as u32 | u32::from(MSIX_BAR_INDEX);
            bytes[MSIX_CAP_OFFSET + 8..MSIX_CAP_OFFSET + 12]
                .copy_from_slice(&pba_bir.to_le_bytes());
            for mask in write_mask[MSIX_CAP_OFFSET + 4..MSIX_CAP_OFFSET + 12].iter_mut() {
                *mask = 0;
            }
        }
        Ok(Self { bytes, write_mask })
    }
}

pub(crate) struct FunctionState {
    bdf: PciBdf,
    power_on: PowerOnConfig,
    config: [u8; CONFIG_SPACE_SIZE],
    bars: Vec<BarState>,
    msix: Option<super::msix::PciMsixDeclaration>,
}

pub(crate) enum BarWriteAction {
    Probe { bar: usize },
    Relocate { bar: usize, candidate: u64 },
}

impl FunctionState {
    pub(crate) fn new(
        bdf: PciBdf,
        power_on: PowerOnConfig,
        bars: &[ResolvedBarPlan],
        msix: Option<super::msix::PciMsixDeclaration>,
    ) -> Self {
        Self {
            bdf,
            config: power_on.bytes,
            power_on,
            bars: bars.iter().copied().map(BarState::new).collect(),
            msix,
        }
    }

    /// Returns whether this function declares an MSI-X capability.
    pub(crate) const fn has_msix(&self) -> bool {
        self.msix.is_some()
    }

    /// Reads the MSI-X Message Control value from the config image, if the
    /// capability is present.
    pub(crate) fn msix_message_control(&self) -> Option<u16> {
        self.msix.map(|_| {
            u16::from_le_bytes([
                self.config[MSIX_CAP_OFFSET + 2],
                self.config[MSIX_CAP_OFFSET + 3],
            ])
        })
    }

    pub(crate) const fn bdf(&self) -> PciBdf {
        self.bdf
    }

    pub(crate) fn memory_decode_enabled(&self) -> bool {
        self.config[4] & COMMAND_MEMORY_SPACE_ENABLE != 0
    }

    pub(crate) fn command_state(&self) -> PciCommandState {
        PciCommandState::from_config(&self.config)
    }

    pub(crate) fn bars(&self) -> &[BarState] {
        &self.bars
    }

    pub(crate) fn read(&self, offset: usize, size: usize) -> u64 {
        if let Some(bar) = self.bar_dword(offset) {
            let dword = self.bars[bar].raw_dword().to_le_bytes();
            return read_bytes(&dword, offset % 4, size);
        }
        read_bytes(&self.config, offset, size)
    }

    /// Classifies one BAR write after merging the guest lanes into a full
    /// dword. The size probe is recognized only when the merged dword equals
    /// all ones in one access; lane-wise accumulation across multiple writes
    /// is intentionally not tracked, matching the design's four-row contract
    /// rather than hardware register latching.
    pub(crate) fn prepare_bar_write(
        &self,
        offset: usize,
        size: usize,
        value: u64,
    ) -> Option<BarWriteAction> {
        let bar = self.bar_dword(offset)?;
        let mut dword = self.bars[bar].committed_dword().to_le_bytes();
        merge_bytes(&mut dword, offset % 4, size, value, &[u8::MAX; 4]);
        let merged = u32::from_le_bytes(dword);
        if merged == u32::MAX {
            return Some(BarWriteAction::Probe { bar });
        }
        Some(BarWriteAction::Relocate {
            bar,
            candidate: BarState::candidate_address(merged),
        })
    }

    pub(crate) fn write_non_bar(&mut self, offset: usize, size: usize, value: u64) {
        merge_bytes(
            &mut self.config,
            offset,
            size,
            value,
            &self.power_on.write_mask,
        );
    }

    pub(crate) fn apply_probe(&mut self, bar: usize) {
        self.bars[bar].set_probe();
    }

    pub(crate) fn finish_relocation(&mut self, bar: usize, accepted: Option<u64>) {
        self.bars[bar].finish_relocation(accepted);
    }

    pub(crate) fn reset(&mut self) {
        self.config = self.power_on.bytes;
        for bar in &mut self.bars {
            bar.reset();
        }
    }

    fn bar_dword(&self, offset: usize) -> Option<usize> {
        if !(0x10..0x28).contains(&offset) {
            return None;
        }
        let slot = ((offset - 0x10) / 4) as u8;
        self.bars.iter().position(|bar| slot == bar.index().value())
    }
}

pub(crate) fn read_bytes(bytes: &[u8], offset: usize, size: usize) -> u64 {
    bytes[offset..offset + size]
        .iter()
        .enumerate()
        .fold(0, |value, (index, byte)| {
            value | (u64::from(*byte) << (index * 8))
        })
}

fn merge_bytes(bytes: &mut [u8], offset: usize, size: usize, value: u64, masks: &[u8]) {
    for index in 0..size {
        let mask = masks[offset + index];
        let update = (value >> (index * 8)) as u8;
        bytes[offset + index] = (bytes[offset + index] & !mask) | (update & mask);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PciBarIndex, PciClass, PciMemoryBar};

    #[test]
    fn function_state_keeps_unimplemented_header_fields_read_only() {
        let identity = PciEndpointIdentity::new(0x1234, 0x5678, PciClass::new(0x05, 0x00, 0x00));
        let bar = PciMemoryBar::new(PciBarIndex::new(2).unwrap(), 0x1_0000).unwrap();
        let plan = ResolvedBarPlan {
            index: bar.index(),
            size: bar.size(),
            prefetchable: false,
            policy: super::super::PciBarDecodePolicy::RelocatableWithinHostAperture,
            address: 0x2000_0000,
        };
        let power_on = PowerOnConfig::build(identity, &[plan], &[], None).unwrap();
        let mut state = FunctionState::new(PciBdf::bus_zero(1), power_on, &[plan], None);

        state.write_non_bar(0, 4, 0);

        assert_eq!(state.read(0, 4), 0x5678_1234);
    }

    #[test]
    fn msix_capability_is_visible_and_gated_by_the_write_mask() {
        use super::super::{
            PciBarDecodePolicy,
            msix::{MSIX_BAR_SIZE, PciMsixDeclaration},
        };

        let identity = PciEndpointIdentity::new(0x1234, 0x5678, PciClass::new(0x05, 0x00, 0x00));
        let register_bar = PciMemoryBar::new(PciBarIndex::new(0).unwrap(), 0x1000).unwrap();
        let msix_bar = PciMemoryBar::new(PciBarIndex::new(1).unwrap(), MSIX_BAR_SIZE).unwrap();
        let plan = [
            ResolvedBarPlan {
                index: register_bar.index(),
                size: register_bar.size(),
                prefetchable: false,
                policy: PciBarDecodePolicy::RelocatableWithinHostAperture,
                address: 0x2000_0000,
            },
            ResolvedBarPlan {
                index: msix_bar.index(),
                size: msix_bar.size(),
                prefetchable: false,
                policy: PciBarDecodePolicy::RelocatableWithinHostAperture,
                address: 0x2001_0000,
            },
        ];
        let msix = PciMsixDeclaration::new(1).unwrap();
        let power_on = PowerOnConfig::build(identity, &plan, &[], Some(msix)).unwrap();
        let mut state =
            FunctionState::new(PciBdf::bus_zero(1), power_on.clone(), &plan, Some(msix));

        // The capability list is advertised and points at the frozen offset.
        assert_eq!(state.config[6] & 0x10, 0x10);
        assert_eq!(state.config[0x34], 0x40);
        assert_eq!(state.config[0x40], 0x11);
        assert_eq!(state.config[0x41], 0);
        // Message Control starts with the read-only table size (1 vector → 0)
        // and the frozen Table/PBA BIR dwords point at BAR 1.
        assert_eq!(
            u16::from_le_bytes([state.config[0x42], state.config[0x43]]),
            0
        );
        assert_eq!(state.config[0x44], 0x01);
        assert_eq!(
            u32::from_le_bytes([
                state.config[0x48],
                state.config[0x49],
                state.config[0x4a],
                state.config[0x4b]
            ]) & 0x7,
            1
        );
        assert_eq!(
            u32::from_le_bytes([
                state.config[0x48],
                state.config[0x49],
                state.config[0x4a],
                state.config[0x4b]
            ]) >> 3,
            0x100
        );

        // Guest writes flip only the writable Enable/Function Mask bits.
        state.write_non_bar(0x42, 2, 0x8fff);
        assert_eq!(state.msix_message_control().unwrap(), 0x8000);
        state.write_non_bar(0x42, 2, 0x4000);
        assert_eq!(state.msix_message_control().unwrap(), 0x4000);
        // A function without MSI-X reports no control value.
        let plain_power_on = PowerOnConfig::build(identity, &plan, &[], None).unwrap();
        let plain = FunctionState::new(PciBdf::bus_zero(1), plain_power_on, &plan, None);
        assert!(plain.msix_message_control().is_none());
    }
}
