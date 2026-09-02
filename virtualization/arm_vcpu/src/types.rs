// Copyright 2026 The Axvisor Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use core::fmt::{Debug, Formatter, LowerHex, UpperHex};

/// Result type returned by the OS-neutral AArch64 vCPU core.
pub type ArmVcpuResult<T = ()> = Result<T, ArmVcpuError>;

/// Errors produced by the OS-neutral AArch64 vCPU core.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArmVcpuError {
    /// A caller supplied an invalid argument or unsupported hardware encoding.
    InvalidInput,
    /// The requested operation is not supported by this CPU or this vCPU core.
    Unsupported,
    /// Hardware or software state is inconsistent with the requested transition.
    BadState,
}

/// Guest physical address.
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
pub struct ArmGuestPhysAddr(usize);

impl ArmGuestPhysAddr {
    /// Creates a guest physical address from a raw `usize`.
    pub const fn from_usize(addr: usize) -> Self {
        Self(addr)
    }

    /// Returns the raw address value.
    pub const fn as_usize(self) -> usize {
        self.0
    }
}

impl From<usize> for ArmGuestPhysAddr {
    fn from(value: usize) -> Self {
        Self::from_usize(value)
    }
}

impl From<ArmGuestPhysAddr> for usize {
    fn from(value: ArmGuestPhysAddr) -> Self {
        value.as_usize()
    }
}

impl Debug for ArmGuestPhysAddr {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        write!(f, "GPA({:#x})", self.0)
    }
}

impl LowerHex for ArmGuestPhysAddr {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:#x}", self.0)
    }
}

impl UpperHex for ArmGuestPhysAddr {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:#X}", self.0)
    }
}

/// AArch64 system-register address encoding used by trapped MRS/MSR exits.
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
pub struct ArmSysRegAddr(usize);

impl ArmSysRegAddr {
    /// Creates a system-register address from the ISS-derived encoding.
    pub const fn new(addr: usize) -> Self {
        Self(addr)
    }

    /// Returns the raw register address encoding.
    pub const fn addr(self) -> usize {
        self.0
    }
}

impl From<usize> for ArmSysRegAddr {
    fn from(value: usize) -> Self {
        Self::new(value)
    }
}

impl From<ArmSysRegAddr> for usize {
    fn from(value: ArmSysRegAddr) -> Self {
        value.addr()
    }
}

impl Debug for ArmSysRegAddr {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        write!(f, "ArmSysRegAddr({:#x})", self.0)
    }
}

impl LowerHex for ArmSysRegAddr {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:#x}", self.0)
    }
}

impl UpperHex for ArmSysRegAddr {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:#X}", self.0)
    }
}

/// Width of a trapped guest memory access.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum ArmAccessWidth {
    /// 8-bit access.
    Byte,
    /// 16-bit access.
    Word,
    /// 32-bit access.
    Dword,
    /// 64-bit access.
    Qword,
}

impl ArmAccessWidth {
    /// Returns this access width in bytes.
    pub const fn size(self) -> usize {
        match self {
            Self::Byte => 1,
            Self::Word => 2,
            Self::Dword => 4,
            Self::Qword => 8,
        }
    }
}

impl TryFrom<usize> for ArmAccessWidth {
    type Error = ArmVcpuError;

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Byte),
            2 => Ok(Self::Word),
            4 => Ok(Self::Dword),
            8 => Ok(Self::Qword),
            _ => Err(ArmVcpuError::InvalidInput),
        }
    }
}

impl From<ArmAccessWidth> for usize {
    fn from(value: ArmAccessWidth) -> Self {
        value.size()
    }
}

/// Reconstructed register state for injecting a synchronous data abort into
/// a guest after the VMM refuses to service a stage-2 permission fault.
///
/// The frame redirects the next guest entry to the guest's own synchronous
/// exception vector with `ELR_EL1`/`SPSR_EL1` describing the faulting
/// context, so the guest handles the access exactly like a hardware-reported
/// external abort. `FAR_EL1` is zero because the guest virtual address
/// cannot be recovered from `HPFAR_EL2`; a zero FAR guarantees the guest
/// kernel takes its bad-area path and never matches a real VMA.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArmGuestAbortFrame {
    /// `ELR_EL1`: faulting guest PC reported to the guest handler.
    pub guest_pc: u64,
    /// `SPSR_EL1`: guest PSTATE reported to the guest handler.
    pub guest_spsr: u64,
    /// `ESR_EL1`: reconstructed syndrome (data abort, external-abort FSC).
    pub esr_el1: u32,
    /// `FAR_EL1`: always zero; see the type-level documentation.
    pub far_el1: u64,
    /// `ELR_EL2`: guest synchronous exception vector for the next entry.
    pub entry_pc: u64,
    /// `SPSR_EL2`: EL1h with all exceptions masked for the vector entry.
    pub entry_spsr: u64,
}

impl ArmGuestAbortFrame {
    /// ESR exception class for a data abort from a lower exception level.
    const ESR_EC_DATA_ABORT_LOWER: u64 = 0x24;
    /// ESR exception class for a data abort from the current exception level.
    const ESR_EC_DATA_ABORT_CURRENT: u64 = 0x25;
    /// ESR instruction-length bit (always set for AArch64).
    const ESR_IL: u64 = 1 << 25;
    /// ESR write-not-read bit.
    const ESR_WNR: u64 = 1 << 6;
    /// ESR fault status code for a synchronous external abort.
    const ESR_FSC_EXTERNAL_ABORT: u64 = 0x10;

    /// PSTATE mode field mask (`M[3:0]`).
    const SPSR_MODE_MASK: u64 = 0xf;
    /// PSTATE mode value for EL0t.
    const SPSR_MODE_EL0T: u64 = 0x0;
    /// PSTATE mode value for EL1t.
    const SPSR_MODE_EL1T: u64 = 0x4;

    /// Vector offset for exceptions from a lower AArch64 exception level.
    const VECTOR_OFFSET_LOWER_AARCH64: u64 = 0x400;
    /// Vector offset for exceptions from the current level using SP_ELx.
    const VECTOR_OFFSET_CURRENT_SPX: u64 = 0x200;
    /// Vector offset for exceptions from the current level using SP_EL0.
    const VECTOR_OFFSET_CURRENT_SP0: u64 = 0x000;

    /// PSTATE for the injected vector entry: EL1h with D/A/I/F masked.
    const PSTATE_EL1H_MASKED: u64 = 0x3c5;

    /// Builds the injection frame for one denied guest access.
    ///
    /// `fault_pc` and `source_spsr` describe the faulting guest context and
    /// become `ELR_EL1`/`SPSR_EL1`; `vbar_el1` is the guest vector base.
    pub fn data_abort(fault_pc: u64, source_spsr: u64, vbar_el1: u64, is_write: bool) -> Self {
        let (exception_class, vector_offset) = match source_spsr & Self::SPSR_MODE_MASK {
            Self::SPSR_MODE_EL0T => (
                Self::ESR_EC_DATA_ABORT_LOWER,
                Self::VECTOR_OFFSET_LOWER_AARCH64,
            ),
            Self::SPSR_MODE_EL1T => (
                Self::ESR_EC_DATA_ABORT_CURRENT,
                Self::VECTOR_OFFSET_CURRENT_SP0,
            ),
            _ => (
                Self::ESR_EC_DATA_ABORT_CURRENT,
                Self::VECTOR_OFFSET_CURRENT_SPX,
            ),
        };
        let mut esr = (exception_class << 26) | Self::ESR_IL | Self::ESR_FSC_EXTERNAL_ABORT;
        if is_write {
            esr |= Self::ESR_WNR;
        }
        Self {
            guest_pc: fault_pc,
            guest_spsr: source_spsr,
            esr_el1: esr as u32,
            far_el1: 0,
            entry_pc: vbar_el1 + vector_offset,
            entry_spsr: Self::PSTATE_EL1H_MASKED,
        }
    }
}

/// Stage-2 page table configuration selected by the embedding VMM.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArmNestedPagingConfig {
    /// Root physical address of the stage-2 page table.
    pub root_paddr: usize,
    /// Number of stage-2 page-table levels.
    pub levels: usize,
    /// Guest physical address width in bits.
    pub gpa_bits: usize,
    /// Hardware-specific mode value. For AArch64 this carries host PA bits when non-zero.
    pub mode: usize,
}

impl ArmNestedPagingConfig {
    /// Creates a nested paging configuration.
    pub const fn new(root_paddr: usize, levels: usize, gpa_bits: usize, mode: usize) -> Self {
        Self {
            root_paddr,
            levels,
            gpa_bits,
            mode,
        }
    }
}

/// Common GICv3 CPU-interface register trapped by the vCPU core.
///
/// Keeping the architectural register identity typed prevents raw system
/// register encodings from escaping into the embedding VMM.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArmGicCpuInterfaceRegister {
    /// `ICC_CTLR_EL1`, the common CPU-interface control register.
    Control,
    /// `ICC_PMR_EL1`, the virtual priority-mask register.
    PriorityMask,
    /// `ICC_RPR_EL1`, the virtual running-priority register.
    RunningPriority,
}

/// VM-exit reason returned by the AArch64 vCPU core.
#[non_exhaustive]
#[derive(Debug)]
pub enum ArmVmExit {
    /// A guest instruction triggered a hypercall.
    Hypercall {
        /// Hypercall number.
        nr: u64,
        /// Hypercall arguments.
        args: [u64; 6],
    },
    /// The guest performed an MMIO read.
    MmioRead {
        /// Guest physical address being read.
        addr: ArmGuestPhysAddr,
        /// Access width.
        width: ArmAccessWidth,
        /// Destination guest register.
        reg: usize,
        /// Destination register width.
        reg_width: ArmAccessWidth,
        /// Whether the value should be sign-extended.
        signed_ext: bool,
    },
    /// The guest performed an MMIO write.
    MmioWrite {
        /// Guest physical address being written.
        addr: ArmGuestPhysAddr,
        /// Access width.
        width: ArmAccessWidth,
        /// Value written by the guest.
        data: u64,
    },
    /// The guest performed a system-register read.
    SysRegRead {
        /// System-register address.
        addr: ArmSysRegAddr,
        /// Destination guest register.
        reg: usize,
    },
    /// The guest performed a system-register write.
    SysRegWrite {
        /// System-register address.
        addr: ArmSysRegAddr,
        /// Value written by the guest.
        value: u64,
    },
    /// The guest read a trapped GICv3 common CPU-interface register.
    GicCpuInterfaceRead {
        /// Register selected by the trapped MRS instruction.
        register: ArmGicCpuInterfaceRegister,
        /// Destination guest general-purpose register.
        destination: usize,
    },
    /// The guest wrote a trapped GICv3 common CPU-interface register.
    GicCpuInterfaceWrite {
        /// Register selected by the trapped MSR instruction.
        register: ArmGicCpuInterfaceRegister,
        /// Value written by the guest.
        value: u64,
    },
    /// The guest faulted on stage-2 permissions for a mapped GPA.
    ///
    /// The vCPU leaves `ELR_EL2` on the faulting instruction so the VMM can
    /// either install a mapping and retry, or inject a guest-visible abort
    /// for the denied access.
    NestedPageFault {
        /// Guest physical address being accessed.
        addr: ArmGuestPhysAddr,
        /// Whether the faulting access was a write.
        is_write: bool,
    },
    /// A physical host interrupt should be handled by the embedding VMM.
    ExternalInterrupt {
        /// Opaque acknowledgement token, or `None` for a spurious interrupt.
        ///
        /// The token is returned unchanged so a split-EOI host controller can
        /// retain source information until the guest deactivates the interrupt.
        token: Option<usize>,
    },
    /// A guest WFI or WFE instruction was trapped.
    WaitForInterrupt,
    /// A guest PSCI CPU_OFF call was trapped.
    CpuDown {
        /// Guest-provided target state.
        state: u64,
    },
    /// A guest PSCI CPU_ON call was trapped.
    CpuUp {
        /// Target CPU affinity.
        target_cpu: u64,
        /// Guest entry point for the target CPU.
        entry_point: ArmGuestPhysAddr,
        /// Guest argument for the target CPU.
        arg: u64,
    },
    /// The guest requested system power-off.
    SystemDown,
    /// The guest wrote a GIC SGI system register.
    SendIPI {
        /// Complete `ICC_SGI1R_EL1` value, including affinity and range selector.
        value: u64,
    },
    /// The guest wrote `ICC_DIR_EL1` while deactivation trapping was enabled.
    DeactivateInterrupt {
        /// Guest-visible INTID carried by `ICC_DIR_EL1`.
        intid: u32,
    },
    /// The vCPU handled the event internally.
    Nothing,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_abort_frame_for_el0_write_targets_lower_el_vector() {
        let frame = ArmGuestAbortFrame::data_abort(0x1000, 0x0, 0xffff_0000_0800_0000, true);

        assert_eq!(frame.guest_pc, 0x1000);
        assert_eq!(frame.guest_spsr, 0x0);
        assert_eq!(frame.entry_pc, 0xffff_0000_0800_0400);
        assert_eq!(frame.entry_spsr, 0x3c5);
        assert_eq!(frame.far_el1, 0);
        assert_eq!(frame.esr_el1 >> 26, 0x24);
        assert!(frame.esr_el1 & (1 << 25) != 0);
        assert!(frame.esr_el1 & (1 << 6) != 0);
        assert_eq!(frame.esr_el1 & 0x3f, 0x10);
    }

    #[test]
    fn data_abort_frame_for_el1h_read_targets_current_el_vector() {
        let frame = ArmGuestAbortFrame::data_abort(0x2000, 0x5, 0xffff_0000_0800_0000, false);

        assert_eq!(frame.entry_pc, 0xffff_0000_0800_0200);
        assert_eq!(frame.esr_el1 >> 26, 0x25);
        assert!(frame.esr_el1 & (1 << 6) == 0);
    }

    #[test]
    fn data_abort_frame_for_el1t_uses_sp0_vector() {
        let frame = ArmGuestAbortFrame::data_abort(0x2000, 0x4, 0xffff_0000_0800_0000, false);

        assert_eq!(frame.entry_pc, 0xffff_0000_0800_0000);
        assert_eq!(frame.esr_el1 >> 26, 0x25);
    }
}
