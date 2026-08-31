//! VM-side stage-2 direct-mapping service.
//!
//! [`AxStage2Remap`] implements the device-facing [`Stage2Remap`] port for
//! one VM: it tracks which graph node owns which committed mapping, turns
//! mapping requests into nested-page-table updates through the VM's shared
//! address-space handle, and answers fault-path diagnosis queries.
//!
//! Locking: the address-space lock is the innermost lock this service takes.
//! Callers on the device path hold no VM locks; callers on VM paths may hold
//! the machine lock, which establishes the documented `machine → stage-2`
//! order. The stage-2 lock never acquires anything else.

use std::{collections::BTreeMap, ops::Range, string::ToString, sync::Arc, vec::Vec};

use ax_std::os::arceos::sync::IrqSafeMutex;
use axaddrspace::AddrSpace;
use axdevice::{DeviceNodeId, DirectMapping, DirectMappingFault, GpaRange, Stage2Remap};
use axdevice_base::DeviceError;
use axvm_types::{GuestPhysAddr, HostPhysAddr, MappingFlags};

use crate::{AxVmError, AxVmResult};

/// One owner's committed direct mappings.
struct CommittedMappings {
    mappings: Vec<DirectMapping>,
}

impl CommittedMappings {
    fn covers(&self, range: Range<u64>) -> bool {
        self.mappings.iter().any(|mapping| {
            let start = mapping.gpa_base();
            let end = start + mapping.size();
            range.start >= start && range.end <= end
        })
    }
}

/// Stage-2 update service for one VM.
pub struct AxStage2Remap {
    address_space: Arc<IrqSafeMutex<AddrSpace<crate::arch::current::ArchNestedPageTable>>>,
    committed: IrqSafeMutex<BTreeMap<DeviceNodeId, CommittedMappings>>,
}

impl AxStage2Remap {
    /// Creates the service over one VM's shared address-space handle.
    pub(crate) fn new(
        address_space: Arc<IrqSafeMutex<AddrSpace<crate::arch::current::ArchNestedPageTable>>>,
    ) -> Self {
        Self {
            address_space,
            committed: IrqSafeMutex::new(BTreeMap::new()),
        }
    }

    /// Drops every committed mapping record after a VM address-space reset.
    ///
    /// The stage-2 mappings themselves are gone with `clear()`; only the
    /// diagnosis registry needs an explicit reset.
    pub(crate) fn reset_registry(&self) {
        self.committed.lock().clear();
    }

    fn stage2_flags(writable: bool) -> MappingFlags {
        // Stage-2 has no privilege distinction; guests reach these mappings
        // from their own stage-1 translations.
        let flags = MappingFlags::READ | MappingFlags::USER;
        if writable {
            flags | MappingFlags::WRITE
        } else {
            flags
        }
    }

    fn map_one(&self, mapping: &DirectMapping) -> AxVmResult {
        self.address_space
            .lock()
            .map_linear(
                GuestPhysAddr::from(mapping.gpa_base() as usize),
                HostPhysAddr::from(mapping.hpa_base() as usize),
                mapping.size() as usize,
                Self::stage2_flags(mapping.writable()),
            )
            .map_err(|error| AxVmError::from_addrspace("map direct device mapping", error))?;
        Ok(())
    }

    fn unmap_one(&self, range: &GpaRange) -> AxVmResult {
        self.address_space
            .lock()
            .unmap(
                GuestPhysAddr::from(range.base() as usize),
                range.size() as usize,
            )
            .map_err(|error| AxVmError::from_addrspace("unmap direct device mapping", error))?;
        Ok(())
    }
}

impl Stage2Remap for AxStage2Remap {
    fn update(
        &self,
        owner: &DeviceNodeId,
        revoke: &[GpaRange],
        commit: &[DirectMapping],
    ) -> Result<(), DeviceError> {
        // Conflict validation runs before any page-table change so a rejected
        // update leaves the current mappings untouched.
        {
            let committed = self.committed.lock();
            for mapping in commit {
                let start = mapping.gpa_base();
                let end = start + mapping.size();
                for (other_owner, other) in committed.iter() {
                    if other_owner == owner {
                        continue;
                    }
                    if other.covers(start..end)
                        || other.mappings.iter().any(|m| {
                            let other_start = m.gpa_base();
                            let other_end = other_start + m.size();
                            start < other_end && other_start < end
                        })
                    {
                        return Err(DeviceError::ResourceBusy {
                            resource: format!(
                                "direct mapping {start:#x}..{end:#x} owned by {other_owner}"
                            ),
                            operation: "update direct mappings",
                        });
                    }
                }
            }
        }

        // Install every commit entry before revoking, so a same-address
        // rewrite never exposes a gap.
        for mapping in commit {
            self.map_one(mapping)
                .map_err(|error| DeviceError::Backend {
                    operation: "install direct mapping",
                    detail: error.to_string(),
                })?;
        }
        for range in revoke {
            self.unmap_one(range)
                .map_err(|error| DeviceError::Backend {
                    operation: "revoke direct mapping",
                    detail: error.to_string(),
                })?;
        }

        let mut committed = self.committed.lock();
        committed.insert(
            owner.clone(),
            CommittedMappings {
                mappings: commit.to_vec(),
            },
        );
        Ok(())
    }

    fn diagnose(&self, gpa: u64) -> Option<DirectMappingFault> {
        let committed = self.committed.lock();
        for (owner, mappings) in committed.iter() {
            for mapping in &mappings.mappings {
                let start = mapping.gpa_base();
                if gpa >= start && gpa < start + mapping.size() {
                    return Some(DirectMappingFault::new(
                        owner.clone(),
                        mapping.label(),
                        mapping.writable(),
                    ));
                }
            }
        }
        None
    }
}
