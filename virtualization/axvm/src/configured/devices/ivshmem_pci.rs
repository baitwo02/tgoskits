//! AxVM adapter for the ivshmem PCI endpoint.
//!
//! This module converts guest configuration into a reservation-backed PCI
//! function. Link identity, register semantics, and the shared BAR2 backing
//! live in `axdevice::ivshmem`; this file only parses options, declares PCI
//! resources, and routes BAR accesses to the reservation's link.

use std::{
    format,
    string::ToString,
    sync::{Arc, Mutex, MutexGuard},
};

use ax_std::os::arceos::sync::IrqSafeMutex;
use axdevice::*;
use axdevice_base::{
    AccessWidth, Device, DeviceAccess, DeviceContext, DeviceError, DeviceResult, Resource,
};
use axvmconfig::VirtualDeviceRequest;

use crate::{ConfiguredDeviceError, ConfiguredModelRegistration, DeviceInstantiationContext};

const MODEL: &str = "ivshmem-pci";
const HOST_KEY: &str = "aarch64-ecam";
const IVSHMEM_VENDOR_ID: u16 = 0x1af4;
const IVSHMEM_DEVICE_ID: u16 = 0x1110;
/// Device revision of the ivshmem BAR/register model this surface is building
/// toward (`.notes/ivshmem/01-pci-enumeration.md` §3.4). The value is frozen
/// here with a unit-test assertion; re-verify against the pinned QEMU and
/// Jailhouse revisions before claiming interop with real ivshmem drivers.
const IVSHMEM_REVISION: u8 = 1;
/// BAR0 is the ivshmem register page. BAR1 stays an inventory placeholder
/// until the MSI-X feature (F7) defines its table/PBA semantics, and BAR2 is
/// the shared memory of one link.
const REGISTER_BAR_INDEX: u8 = 0;
const MSIX_BAR_INDEX: u8 = 1;
const MSIX_BAR_SIZE: u64 = 0x100;
const SHARED_MEMORY_BAR_INDEX: u8 = 2;
const REGISTER_ACCESS_WIDTH_DETAIL: &str =
    "ivshmem BAR0 registers only accept aligned 32-bit accesses";

fn host_key() -> PciHostKey {
    // This module is architecture-neutral while the provider is AArch64-only.
    // A mismatch fails during typed graph declaration rather than silently
    // attaching the endpoint to another host.
    PciHostKey::new(HOST_KEY).expect("static AArch64 PCI host key is valid")
}

const REGISTRATION: ConfiguredModelRegistration = ConfiguredModelRegistration {
    model: MODEL,
    create: create_device_node,
};

pub(super) fn register(
    catalog: &mut crate::ConfiguredDeviceCatalog,
) -> Result<(), ConfiguredDeviceError> {
    catalog.register(module_path!(), REGISTRATION)
}

#[derive(Clone, Copy, Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct IvshmemPciOptions {
    /// Shared-link identity; peers of one link must use the same value.
    link_id: u32,
    /// Peer slot inside the link; the current profile allows 0 and 1.
    peer_id: u16,
}

fn create_device_node(
    id: DeviceNodeId,
    request: &VirtualDeviceRequest,
    context: &DeviceInstantiationContext,
) -> Result<DeviceNodeSpec, ConfiguredDeviceError> {
    let options = request
        .deserialize_options::<IvshmemPciOptions>()
        .map_err(|error| ConfiguredDeviceError::InvalidOptions {
            device: request.id.clone(),
            model: request.model.clone(),
            detail: error.to_string(),
        })?;
    let registry =
        context
            .ivshmem_registry()
            .ok_or_else(|| ConfiguredDeviceError::Instantiation {
                device: request.id.clone(),
                model: request.model.clone(),
                detail: "no ivshmem link registry was injected into this VM".into(),
            })?;
    let reservation = registry
        .reserve(options.link_id, options.peer_id)
        .map_err(|error| ConfiguredDeviceError::Instantiation {
            device: request.id.clone(),
            model: request.model.clone(),
            detail: match context.vm_id() {
                Some(vm_id) => format!("vm {vm_id}: {error}"),
                None => error.to_string(),
            },
        })?;
    Ok(DeviceNodeSpec::virtual_device(
        id,
        Arc::new(IvshmemPciModel { reservation }),
    ))
}

struct IvshmemPciModel {
    reservation: PeerReservation,
}

impl DeviceModel for IvshmemPciModel {
    fn requirements(&self) -> DeviceManagerResult<DeviceRequirements> {
        let register_bar =
            PciMemoryBar::new(PciBarIndex::new(REGISTER_BAR_INDEX)?, REGISTER_PAGE_SIZE)?;
        let msix_bar = PciMemoryBar::new(PciBarIndex::new(MSIX_BAR_INDEX)?, MSIX_BAR_SIZE)?;
        let shared_memory_bar = PciMemoryBar::new(
            PciBarIndex::new(SHARED_MEMORY_BAR_INDEX)?,
            self.reservation.link().bar2_size(),
        )?;
        let function = PciFunctionRequirement::new(
            host_key(),
            PciEndpointIdentity::new(
                IVSHMEM_VENDOR_ID,
                IVSHMEM_DEVICE_ID,
                PciClass::new(0x05, 0x00, 0x00),
            )
            .with_revision(IVSHMEM_REVISION),
        )
        .with_bar(register_bar)?
        .with_bar(msix_bar)?
        .with_bar(shared_memory_bar)?;
        DeviceRequirements::new().with_pci_function(function)
    }

    fn firmware(&self) -> DeviceFirmwareSpec {
        DeviceFirmwareSpec::None
    }

    fn build(&self, context: &mut DeviceBuildContext<'_>) -> DeviceManagerResult<DeviceBundle> {
        // The reservation survives build retries; only one endpoint may be
        // attached at a time, and a failed bundle drops its attachment.
        let attachment =
            self.reservation
                .attach()
                .map_err(|error| DeviceManagerError::InvalidState {
                    operation: "build ivshmem PCI endpoint",
                    detail: error.to_string(),
                })?;
        // BAR2 maps directly into the guest stage-2, so this endpoint needs
        // the VM's update port.
        let stage2_remap =
            context
                .stage2_remap()
                .ok_or_else(|| DeviceManagerError::InvalidConfig {
                    operation: "build ivshmem PCI endpoint",
                    detail: "no stage-2 update port was injected into this VM".into(),
                })?;
        let registers = Arc::new(Mutex::new(IvshmemRegisters::new()));
        let function = IvshmemPciFunction {
            attachment,
            registers: Arc::clone(&registers),
            stage2_remap,
            owner: context.node_id(),
            direct_plan: IrqSafeMutex::new(None),
        };
        // The sink shares the endpoint's register lock, so events routed by
        // the link land in exactly the registers BAR0 exposes.
        function
            .attachment
            .set_event_sink(Arc::new(RegisterEventSink::new(registers)))
            .map_err(|error| DeviceManagerError::InvalidState {
                operation: "build ivshmem PCI endpoint",
                detail: error.to_string(),
            })?;
        let function = Arc::new(function);
        let mut bundle = DeviceBundle::new();
        bundle.add_pci_function(function)?;
        Ok(bundle)
    }
}

struct IvshmemPciFunction {
    attachment: PeerAttachment,
    registers: Arc<Mutex<IvshmemRegisters>>,
    /// Stage-2 update port; BAR2 maps directly into the guest.
    stage2_remap: Arc<dyn Stage2Remap>,
    /// Graph node identity of this endpoint; direct mappings register per
    /// owner.
    owner: DeviceNodeId,
    /// Currently committed BAR2 plan, replaced on every BAR assignment or
    /// relocation.
    direct_plan: IrqSafeMutex<Option<IvshmemDirectPlan>>,
}

/// The endpoint's event sink: doorbells routed by the link record the event
/// in this endpoint's register page.
///
/// The sink shares the endpoint's register `Mutex` instead of owning state,
/// so the Event Status observed through BAR0 and the events recorded by the
/// link cannot diverge.
struct RegisterEventSink {
    registers: Arc<Mutex<IvshmemRegisters>>,
}

impl RegisterEventSink {
    fn new(registers: Arc<Mutex<IvshmemRegisters>>) -> Self {
        Self { registers }
    }
}

impl IvshmemEventSink for RegisterEventSink {
    fn deliver(&self, event: DoorbellEvent) -> Result<(), IvshmemError> {
        let Ok(mut registers) = self.registers.lock() else {
            return Err(IvshmemError::EventDeliveryFailed {
                operation: "record doorbell event",
                detail: format!(
                    "peer {} register lock is poisoned while recording a doorbell from peer {}",
                    event.target().value(),
                    event.source().value()
                ),
            });
        };
        registers.record_event();
        Ok(())
    }
}

impl IvshmemPciFunction {
    fn lock_registers(
        &self,
        operation: &'static str,
    ) -> DeviceResult<MutexGuard<'_, IvshmemRegisters>> {
        self.registers
            .lock()
            .map_err(|_| DeviceError::InvalidState {
                operation,
                detail: "ivshmem register lock is poisoned".into(),
            })
    }

    /// Decodes one doorbell write and hands it to the link router.
    ///
    /// Inactive targets and unsupported vectors are specification no-ops
    /// inside the link, so this write always succeeds from the guest's point
    /// of view.
    fn deliver_doorbell(&self, value: u32) {
        let doorbell = Doorbell::from_write(value);
        self.attachment
            .link()
            .deliver_doorbell(self.attachment.peer_id(), doorbell);
    }

    /// Derives this peer's BAR2 direct-mapping plan from the resolved BAR2
    /// GPA.
    ///
    /// The plan splits BAR2 by F5 sections: the state table maps read-only,
    /// the peer's own output read-write, other peers' outputs read-only, and
    /// the reserved tail stays unmapped so stray accesses fault.
    fn derive_plan(&self, bar2_gpa: u64) -> DeviceResult<IvshmemDirectPlan> {
        IvshmemDirectPlan::derive(
            self.attachment.link().layout(),
            bar2_gpa,
            self.attachment.link().backing().allocation(),
            self.attachment.peer_id(),
        )
        .map_err(|error| DeviceError::InvalidState {
            operation: "derive ivshmem direct-mapping plan",
            detail: error.to_string(),
        })
    }
}

impl IvshmemPciFunction {
    /// Submits one BAR2 plan through the stage-2 port and records it.
    ///
    /// A relocation to a different BAR2 GPA revokes the old whole-BAR2
    /// range; re-writing the same GPA is a no-op because the committed
    /// mappings are identical.
    fn replace_direct_plan(&self, new_plan: IvshmemDirectPlan) -> DeviceResult {
        let previous = self.direct_plan.lock().take();
        let same_base = previous
            .as_ref()
            .is_some_and(|plan| plan.bar2_gpa() == new_plan.bar2_gpa());
        if !same_base {
            let revoke = previous
                .iter()
                .map(|plan| plan.revocation_range())
                .collect::<Vec<_>>();
            self.stage2_remap
                .update(&self.owner, &revoke, new_plan.mappings())
                .map_err(|error| DeviceError::Backend {
                    operation: "commit ivshmem direct mappings",
                    detail: error.to_string(),
                })?;
        }
        *self.direct_plan.lock() = Some(new_plan);
        Ok(())
    }
}

/// Builds the denial error for one rejected BAR2 write.
///
/// The address is valid and the width legal; the peer simply may not write
/// the owning section. The root layer records this as a guest protocol
/// violation instead of a VM abort.
fn bar2_denial_error(offset: u64, peer: PeerId, section: Bar2Section) -> DeviceError {
    DeviceError::AccessDenied {
        operation: "write ivshmem BAR2",
        detail: format!(
            "peer {} may not write {} at offset {offset:#x}",
            peer.value(),
            section.name()
        ),
    }
}

fn bar_access_error(error: IvshmemError) -> DeviceError {
    match error {
        IvshmemError::SharedMemoryOutOfRange { offset, .. } => {
            DeviceError::OutOfRange { addr: offset }
        }
        IvshmemError::InvalidRegisterAccess { .. }
        | IvshmemError::InvalidSharedMemoryWidth { .. } => DeviceError::InvalidInput {
            operation: "access ivshmem BARs",
            detail: error.to_string(),
        },
        other => DeviceError::InvalidState {
            operation: "access ivshmem endpoint",
            detail: other.to_string(),
        },
    }
}

impl Device for IvshmemPciFunction {
    fn name(&self) -> &str {
        MODEL
    }

    fn resources(&self) -> &[Resource] {
        &[]
    }

    fn read(&self, _access: &DeviceAccess, _context: &mut dyn DeviceContext) -> DeviceResult<u64> {
        Err(DeviceError::Unsupported {
            operation: "access ivshmem PCI endpoint",
            detail: "direct access is routed through the BARs".into(),
        })
    }

    fn write(
        &self,
        _access: &DeviceAccess,
        _value: u64,
        _context: &mut dyn DeviceContext,
    ) -> DeviceResult {
        Err(DeviceError::Unsupported {
            operation: "access ivshmem PCI endpoint",
            detail: "direct access is routed through the BARs".into(),
        })
    }
}

impl PciFunction for IvshmemPciFunction {
    fn notify_bar_assignment(&self, bars: &[BarAssignment]) -> DeviceResult {
        let Some(assignment) = bars
            .iter()
            .find(|bar| bar.bar().value() == SHARED_MEMORY_BAR_INDEX)
        else {
            return Err(DeviceError::InvalidState {
                operation: "bind ivshmem PCI endpoint",
                detail: "the shared-memory BAR was not resolved".into(),
            });
        };
        let plan = self.derive_plan(assignment.gpa())?;
        self.replace_direct_plan(plan)
    }

    fn notify_bar_relocated(&self, bar: PciBarIndex, new_gpa: u64) -> DeviceResult {
        if bar.value() != SHARED_MEMORY_BAR_INDEX {
            // Only BAR2 leaves the emulated path; other relocations do not
            // affect this endpoint's mappings.
            return Ok(());
        }
        let plan = self.derive_plan(new_gpa)?;
        self.replace_direct_plan(plan)
    }

    fn direct_mappings(&self) -> Vec<DirectMapping> {
        self.direct_plan
            .lock()
            .as_ref()
            .map(|plan| plan.mappings().to_vec())
            .unwrap_or_default()
    }

    fn read_bar(
        &self,
        access: PciBarAccess,
        _context: &mut dyn DeviceContext,
    ) -> DeviceResult<u64> {
        match access.bar().value() {
            REGISTER_BAR_INDEX => {
                if access.width() != AccessWidth::Dword {
                    return Err(DeviceError::InvalidInput {
                        operation: "read ivshmem BAR0 registers",
                        detail: REGISTER_ACCESS_WIDTH_DETAIL.into(),
                    });
                }
                let link = self.attachment.link();
                let registers = self.lock_registers("read ivshmem BAR0 registers")?;
                registers
                    .read(access.offset(), self.attachment.peer_id(), link.max_peers())
                    .map(u64::from)
                    .map_err(bar_access_error)
            }
            // BAR1 is an inventory placeholder until MSI-X (F7) defines its
            // semantics; reads stay zero so guest probes observe a defined
            // value instead of a routing error.
            MSIX_BAR_INDEX => Ok(0),
            SHARED_MEMORY_BAR_INDEX => self
                .attachment
                .link()
                .backing()
                .read(access.offset(), access.width().size())
                .map_err(bar_access_error),
            _ => Err(DeviceError::OutOfRange {
                addr: access.offset(),
            }),
        }
    }

    fn write_bar(
        &self,
        access: PciBarAccess,
        value: u64,
        _context: &mut dyn DeviceContext,
    ) -> DeviceResult {
        match access.bar().value() {
            REGISTER_BAR_INDEX => {
                if access.width() != AccessWidth::Dword {
                    return Err(DeviceError::InvalidInput {
                        operation: "write ivshmem BAR0 registers",
                        detail: REGISTER_ACCESS_WIDTH_DETAIL.into(),
                    });
                }
                let value = u32::try_from(value).map_err(|_| DeviceError::InvalidInput {
                    operation: "write ivshmem BAR0 registers",
                    detail: REGISTER_ACCESS_WIDTH_DETAIL.into(),
                })?;
                // The doorbell routes through the link before any register
                // lock is taken: the writing endpoint must not hold its own
                // register lock while the target sink locks its registers.
                if access.offset() == DOORBELL_OFFSET {
                    self.deliver_doorbell(value);
                    return Ok(());
                }
                {
                    let mut registers = self.lock_registers("write ivshmem BAR0 registers")?;
                    registers
                        .write(access.offset(), value)
                        .map_err(bar_access_error)?;
                }
                // The local register is the first state transition; the
                // state-table publish follows outside the register lock, so
                // the register and backing locks never nest. Between the two
                // steps another peer may observe a briefly stale table entry;
                // the state table remains the source of truth.
                if access.offset() == STATE_OFFSET {
                    self.attachment
                        .link()
                        .publish_state(self.attachment.peer_id(), value)
                        .map_err(|error| DeviceError::InvalidState {
                            operation: "publish ivshmem state",
                            detail: error.to_string(),
                        })?;
                }
                Ok(())
            }
            MSIX_BAR_INDEX => Ok(()),
            SHARED_MEMORY_BAR_INDEX => {
                // One unified permission decision for every BAR2 write: the
                // state table denies, other peers' outputs deny, the own
                // output and a common section allow, and the reserved tail
                // is silently ignored without touching the backing.
                let section = self.attachment.link().layout().classify(access.offset());
                if section == Bar2Section::Reserved {
                    return Ok(());
                }
                if !section.allows_write(self.attachment.peer_id()) {
                    return Err(bar2_denial_error(
                        access.offset(),
                        self.attachment.peer_id(),
                        section,
                    ));
                }
                self.attachment
                    .link()
                    .backing()
                    .write(access.offset(), access.width().size(), value)
                    .map_err(bar_access_error)
            }
            _ => Err(DeviceError::OutOfRange {
                addr: access.offset(),
            }),
        }
    }

    fn reset(&self) -> DeviceResult {
        // Endpoint reset clears the local registers and zeroes this peer's
        // state-table entry; the rest of the shared backing stays intact for
        // the other peer.
        self.lock_registers("reset ivshmem endpoint")?.reset();
        self.attachment
            .link()
            .clear_state(self.attachment.peer_id())
            .map_err(|error| DeviceError::InvalidState {
                operation: "reset ivshmem endpoint",
                detail: error.to_string(),
            })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{alloc::Layout, collections::BTreeMap};

    use axdevice::{BackingAllocation, SharedBackingAllocator};
    use axvmconfig::VirtualDeviceRequest;

    use super::*;

    /// Page-aligned heap allocator for adapter tests: the returned HPA is
    /// the virtual address, which pure derivation paths never dereference.
    #[derive(Default)]
    struct TestBackingAllocator;

    impl SharedBackingAllocator for TestBackingAllocator {
        fn allocate(&self, size: u64) -> Result<BackingAllocation, IvshmemError> {
            let layout = Layout::from_size_align(size as usize, 0x1000).map_err(|_| {
                IvshmemError::AllocationFailed {
                    operation: "test backing layout",
                }
            })?;
            let pointer = unsafe { std::alloc::alloc_zeroed(layout) };
            if pointer.is_null() {
                return Err(IvshmemError::AllocationFailed {
                    operation: "test backing alloc",
                });
            }
            Ok(BackingAllocation::from_parts(
                pointer as usize as u64,
                size,
                pointer,
            ))
        }

        fn release(&self, allocation: BackingAllocation) {
            let layout = Layout::from_size_align(allocation.size() as usize, 0x1000).unwrap();
            // SAFETY: the pointer came from alloc_zeroed with the same
            // layout and every allocation is released exactly once.
            unsafe { std::alloc::dealloc(allocation.virtual_base(), layout) };
        }
    }

    fn test_registry() -> Arc<IvshmemLinkRegistry> {
        Arc::new(IvshmemLinkRegistry::new(Arc::new(TestBackingAllocator)))
    }

    /// In-memory stage-2 port for adapter tests: mappings are recorded, and
    /// per-owner commits replace each other without touching page tables.
    #[derive(Default)]
    struct TestStage2Remap {
        committed: Mutex<BTreeMap<DeviceNodeId, Vec<DirectMapping>>>,
    }

    impl Stage2Remap for TestStage2Remap {
        fn update(
            &self,
            owner: &DeviceNodeId,
            _revoke: &[GpaRange],
            commit: &[DirectMapping],
        ) -> Result<(), DeviceError> {
            self.committed
                .lock()
                .unwrap()
                .insert(owner.clone(), commit.to_vec());
            Ok(())
        }

        fn diagnose(&self, gpa: u64) -> Option<DirectMappingFault> {
            let committed = self.committed.lock().unwrap();
            for (owner, mappings) in committed.iter() {
                for mapping in mappings {
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

    const APERTURE_BASE: u64 = 0x0c00_0000;
    const APERTURE_SIZE: u64 = 0x0400_0000;
    // Shared-region scratch offset. F4 reserves the first BAR2 page for the
    // host-maintained state table, so test payloads live past 0x1000.
    const TEST_OFFSET: u64 = 0x1100;
    const TEST_VALUE: u64 = 0x4956_5348_4d45_4d31;

    fn id(value: &str) -> DeviceNodeId {
        DeviceNodeId::new(value).unwrap()
    }

    fn slot(value: &str) -> ResourceSlot {
        ResourceSlot::new(value).unwrap()
    }

    fn request(id: &str, link_id: u32, peer_id: u16) -> VirtualDeviceRequest {
        let mut options = toml::Table::new();
        options.insert("link_id".into(), toml::Value::Integer(link_id.into()));
        options.insert("peer_id".into(), toml::Value::Integer(peer_id.into()));
        VirtualDeviceRequest {
            id: id.into(),
            model: MODEL.into(),
            options,
        }
    }

    fn registered_catalog() -> crate::ConfiguredDeviceCatalog {
        let mut catalog = crate::ConfiguredDeviceCatalog::new();
        register(&mut catalog).unwrap();
        catalog
    }

    fn model_for(
        registry: &Arc<IvshmemLinkRegistry>,
        link_id: u32,
        peer_id: u16,
    ) -> IvshmemPciModel {
        let reservation = registry.reserve(link_id, peer_id).unwrap();
        IvshmemPciModel { reservation }
    }

    #[test]
    fn requirements_declare_the_full_bar_inventory() {
        let registry = test_registry();
        let requirements = model_for(&registry, 7, 1).requirements().unwrap();
        let function = requirements.pci_function().unwrap();
        let expected = PciFunctionRequirement::new(
            host_key(),
            PciEndpointIdentity::new(
                IVSHMEM_VENDOR_ID,
                IVSHMEM_DEVICE_ID,
                PciClass::new(0x05, 0, 0),
            )
            .with_revision(IVSHMEM_REVISION),
        )
        .with_bar(
            PciMemoryBar::new(
                PciBarIndex::new(REGISTER_BAR_INDEX).unwrap(),
                REGISTER_PAGE_SIZE,
            )
            .unwrap(),
        )
        .unwrap()
        .with_bar(
            PciMemoryBar::new(PciBarIndex::new(MSIX_BAR_INDEX).unwrap(), MSIX_BAR_SIZE).unwrap(),
        )
        .unwrap()
        .with_bar(
            PciMemoryBar::new(
                PciBarIndex::new(SHARED_MEMORY_BAR_INDEX).unwrap(),
                SHARED_MEMORY_SIZE,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(function, &expected);
    }

    #[test]
    fn options_require_link_and_peer_identity() {
        let registry = test_registry();
        let catalog = registered_catalog();
        let context =
            DeviceInstantiationContext::new().with_ivshmem_registry(Some(registry.clone()));

        let Err(missing) = catalog.instantiate_node(
            &VirtualDeviceRequest {
                id: "ivshmem0".into(),
                model: MODEL.into(),
                options: Default::default(),
            },
            &context,
        ) else {
            panic!("empty options must be rejected");
        };
        assert!(matches!(
            missing,
            ConfiguredDeviceError::InvalidOptions { .. }
        ));

        let mut unknown_options = request("ignored", 1, 0).options;
        unknown_options.insert("peer_count".into(), toml::Value::Integer(2));
        let Err(unknown) = catalog.instantiate_node(
            &VirtualDeviceRequest {
                id: "ivshmem0".into(),
                model: MODEL.into(),
                options: unknown_options,
            },
            &context,
        ) else {
            panic!("unknown option keys must be rejected");
        };
        assert!(matches!(
            unknown,
            ConfiguredDeviceError::InvalidOptions { .. }
        ));

        let Err(out_of_profile) = catalog.instantiate_node(&request("ivshmem0", 1, 2), &context)
        else {
            panic!("peer 2 must be rejected by the profile");
        };
        let ConfiguredDeviceError::Instantiation { detail, .. } = out_of_profile else {
            panic!("expected an instantiation error");
        };
        assert!(detail.contains("outside the current profile"));
    }

    #[test]
    fn duplicate_peer_reservations_fail_the_second_vm() {
        let registry = test_registry();
        let catalog = registered_catalog();
        let context =
            DeviceInstantiationContext::new().with_ivshmem_registry(Some(registry.clone()));
        // The first node spec keeps its reservation alive; dropping it would
        // retire the peer and change the second VM's error.
        let _first = catalog
            .instantiate_node(&request("ivshmem0", 1, 0), &context)
            .unwrap();
        let Err(error) = catalog.instantiate_node(&request("ivshmem1", 1, 0), &context) else {
            panic!("a second reservation of the same peer must fail");
        };
        let ConfiguredDeviceError::Instantiation { detail, .. } = error else {
            panic!("expected an instantiation error");
        };
        assert!(detail.contains("already reserved"));
    }

    #[test]
    fn instantiation_requires_an_injected_registry() {
        let catalog = registered_catalog();
        let context = DeviceInstantiationContext::new();
        let Err(error) = catalog.instantiate_node(&request("ivshmem0", 1, 0), &context) else {
            panic!("a missing registry must fail instantiation");
        };
        let ConfiguredDeviceError::Instantiation { detail, .. } = error else {
            panic!("expected an instantiation error");
        };
        assert!(detail.contains("ivshmem link registry"));
    }

    struct HostModel {
        root: Arc<Mutex<Option<Arc<PciRootState>>>>,
    }

    impl DeviceModel for HostModel {
        fn requirements(&self) -> DeviceManagerResult<DeviceRequirements> {
            DeviceRequirements::new().with_mmio(
                slot("pci-memory"),
                APERTURE_SIZE,
                APERTURE_SIZE,
                ResourceRequest::Auto,
            )
        }

        fn firmware(&self) -> DeviceFirmwareSpec {
            DeviceFirmwareSpec::None
        }

        fn build(&self, context: &mut DeviceBuildContext<'_>) -> DeviceManagerResult<DeviceBundle> {
            let _ = context.mmio("pci-memory")?;
            let topology =
                context
                    .pci_host_topology()
                    .cloned()
                    .ok_or(DeviceManagerError::InvalidState {
                        operation: "build ivshmem test host",
                        detail: "test host topology was not resolved".into(),
                    })?;
            let root = Arc::new(PciRootState::new(topology));
            *self.root.lock().unwrap() = Some(root.clone());
            let binding = Arc::new(PciRootBinding::new(id("pci-host"), root));
            DeviceBundle::new().with_service::<PciRootBindingKey>(binding)
        }
    }

    struct TestEndpoint {
        // A live VM keeps its planned device graph (and with it the peer
        // reservations inside the endpoint models) for its whole lifetime;
        // holding the graph here mirrors that ownership.
        _graph: ResolvedDeviceGraph,
        _runtime: DeviceRuntime,
        stage2: Arc<TestStage2Remap>,
        binding: Arc<PciRootBinding>,
        root: Arc<PciRootState>,
        bdf: PciBdf,
        register_bar: u64,
        msix_bar: u64,
        shared_bar: u64,
    }

    impl TestEndpoint {
        fn enable_memory(&self) {
            // Command register bit 1 is Memory Space Enable.
            self.root
                .write_config(
                    self.bdf,
                    ConfigOffset::new(4).unwrap(),
                    AccessWidth::Word,
                    2,
                )
                .unwrap();
        }

        fn read_register(&self, offset: u64, width: AccessWidth) -> DeviceResult<u64> {
            self.binding.read_bar(self.register_bar + offset, width)
        }

        fn write_register(&self, offset: u64, value: u32) -> DeviceResult {
            self.binding.write_bar(
                self.register_bar + offset,
                AccessWidth::Dword,
                u64::from(value),
            )
        }
    }

    fn build_endpoint(
        node_id: &str,
        model: IvshmemPciModel,
        stage2: Arc<TestStage2Remap>,
    ) -> TestEndpoint {
        let root_slot = Arc::new(Mutex::new(None));
        let provider = PciHostProvider::new(
            host_key(),
            DeviceNodeSpec::virtual_device(
                id("pci-host"),
                Arc::new(HostModel {
                    root: root_slot.clone(),
                }),
            ),
            slot("pci-memory"),
        );
        let mut builder = DeviceGraphBuilder::new();
        builder.register_pci_host(provider).unwrap();
        builder
            .add(DeviceNodeSpec::virtual_device(id(node_id), Arc::new(model)))
            .unwrap();
        let mut pools = ResourcePools::new();
        pools
            .add_auto_mmio(APERTURE_BASE..APERTURE_BASE + APERTURE_SIZE)
            .unwrap();
        let graph = builder.declare().unwrap().resolve(pools).unwrap();
        let stage2_trait: Arc<dyn Stage2Remap> = Arc::clone(&stage2) as _;
        let mut runtime_builder =
            DeviceRuntimeBuilder::new(RuntimeAccessPorts::new()).with_stage2_remap(stage2_trait);
        for node in graph.nodes() {
            runtime_builder
                .build_graph_node(node, graph.resource_plan())
                .unwrap();
        }
        let runtime = runtime_builder.finish(graph.resource_plan()).unwrap();
        let binding = runtime
            .services()
            .all::<PciRootBindingKey>()
            .into_iter()
            .next()
            .unwrap();
        let root = root_slot.lock().unwrap().clone().unwrap();
        let resolved = graph.pci_topology(&host_key()).unwrap();
        let resolved_function = resolved.function(&id(node_id)).unwrap();
        let bdf = resolved_function.bdf();
        let bar = |index: u8| {
            resolved_function
                .bar(PciBarIndex::new(index).unwrap())
                .unwrap()
                .address()
        };
        let (register_bar, msix_bar, shared_bar) = (
            bar(REGISTER_BAR_INDEX),
            bar(MSIX_BAR_INDEX),
            bar(SHARED_MEMORY_BAR_INDEX),
        );
        TestEndpoint {
            _graph: graph,
            _runtime: runtime,
            binding,
            root,
            bdf,
            register_bar,
            msix_bar,
            shared_bar,
            stage2,
        }
    }

    #[test]
    fn graph_routes_shared_backing_across_two_roots() {
        let registry = test_registry();
        let stage2_peer0 = Arc::new(TestStage2Remap::default());
        let peer0 = build_endpoint(
            "ivshmem0",
            model_for(&registry, 1, 0),
            Arc::clone(&stage2_peer0),
        );
        let stage2_peer1 = Arc::new(TestStage2Remap::default());
        let peer1 = build_endpoint(
            "ivshmem1",
            model_for(&registry, 1, 1),
            Arc::clone(&stage2_peer1),
        );
        peer0.enable_memory();
        peer1.enable_memory();

        // Shared BAR2 bytes cross the two roots.
        peer0
            .binding
            .write_bar(
                peer0.shared_bar + TEST_OFFSET,
                AccessWidth::Qword,
                TEST_VALUE,
            )
            .unwrap();
        assert_eq!(
            peer1
                .binding
                .read_bar(peer1.shared_bar + TEST_OFFSET, AccessWidth::Qword)
                .unwrap(),
            TEST_VALUE
        );

        // The register page exposes the link identity, not per-VM values.
        assert_eq!(
            peer0.read_register(ID_OFFSET, AccessWidth::Dword).unwrap(),
            0
        );
        assert_eq!(
            peer1.read_register(ID_OFFSET, AccessWidth::Dword).unwrap(),
            1
        );
        assert_eq!(
            peer0
                .read_register(MAXIMUM_PEERS_OFFSET, AccessWidth::Dword)
                .unwrap(),
            2
        );

        // BAR1 stays an inventory placeholder.
        assert_eq!(
            peer0
                .binding
                .read_bar(peer0.msix_bar, AccessWidth::Dword)
                .unwrap(),
            0
        );

        // State registers are endpoint-local.
        peer0.write_register(STATE_OFFSET, 0x1234).unwrap();
        assert_eq!(
            peer0
                .read_register(STATE_OFFSET, AccessWidth::Dword)
                .unwrap(),
            0x1234
        );
        assert_eq!(
            peer1
                .read_register(STATE_OFFSET, AccessWidth::Dword)
                .unwrap(),
            0
        );

        // BAR0 rejects widths other than Dword and unaligned offsets.
        assert!(matches!(
            peer0.read_register(ID_OFFSET, AccessWidth::Byte),
            Err(DeviceError::InvalidInput { .. })
        ));
        assert!(matches!(
            peer0.read_register(2, AccessWidth::Dword),
            Err(DeviceError::InvalidInput { .. })
        ));

        // A different link stays isolated from this one.
        let other = registry.reserve(2, 0).unwrap();
        other.link().backing().write(TEST_OFFSET, 8, 0xee).unwrap();
        assert_eq!(
            peer0
                .binding
                .read_bar(peer0.shared_bar + TEST_OFFSET, AccessWidth::Qword)
                .unwrap(),
            TEST_VALUE
        );

        // Relocating BAR2 keeps the shared backing reachable through the new
        // route, while the peer keeps its own route to the same bytes.
        const RELOCATED_BAR2: u64 = 0x0ffe_0000;
        peer0
            .root
            .write_config(
                peer0.bdf,
                ConfigOffset::new(0x18).unwrap(),
                AccessWidth::Dword,
                RELOCATED_BAR2,
            )
            .unwrap();
        assert_eq!(
            peer0
                .root
                .read_config(
                    peer0.bdf,
                    ConfigOffset::new(0x18).unwrap(),
                    AccessWidth::Dword
                )
                .unwrap(),
            RELOCATED_BAR2
        );
        peer0
            .binding
            .write_bar(RELOCATED_BAR2 + TEST_OFFSET, AccessWidth::Qword, TEST_VALUE)
            .unwrap();
        assert_eq!(
            peer1
                .binding
                .read_bar(peer1.shared_bar + TEST_OFFSET, AccessWidth::Qword)
                .unwrap(),
            TEST_VALUE
        );
        assert!(matches!(
            peer0
                .binding
                .read_bar(peer0.shared_bar + TEST_OFFSET, AccessWidth::Qword),
            Err(DeviceError::NotFound)
        ));
    }

    #[test]
    fn endpoint_reset_clears_registers_but_not_shared_backing() {
        let registry = test_registry();
        let stage2_peer0 = Arc::new(TestStage2Remap::default());
        let peer0 = build_endpoint(
            "ivshmem0",
            model_for(&registry, 1, 0),
            Arc::clone(&stage2_peer0),
        );
        let stage2_peer1 = Arc::new(TestStage2Remap::default());
        let peer1 = build_endpoint(
            "ivshmem1",
            model_for(&registry, 1, 1),
            Arc::clone(&stage2_peer1),
        );
        peer0.enable_memory();
        peer1.enable_memory();

        peer0
            .binding
            .write_bar(
                peer0.shared_bar + TEST_OFFSET,
                AccessWidth::Qword,
                TEST_VALUE,
            )
            .unwrap();
        peer0.write_register(STATE_OFFSET, 0x99).unwrap();

        peer0.binding.reset().unwrap();
        peer0.enable_memory();
        assert_eq!(
            peer0
                .read_register(STATE_OFFSET, AccessWidth::Dword)
                .unwrap(),
            0
        );
        // The peer keeps observing the shared backing after the reset.
        assert_eq!(
            peer1
                .binding
                .read_bar(peer1.shared_bar + TEST_OFFSET, AccessWidth::Qword)
                .unwrap(),
            TEST_VALUE
        );
    }

    const fn doorbell(target: u32, vector: u32) -> u32 {
        (target << 16) | vector
    }

    fn event_status(endpoint: &TestEndpoint) -> u64 {
        endpoint
            .read_register(EVENT_STATUS_OFFSET, AccessWidth::Dword)
            .unwrap()
    }

    #[test]
    fn doorbell_sets_only_the_target_event_status() {
        let registry = test_registry();
        let stage2_peer0 = Arc::new(TestStage2Remap::default());
        let peer0 = build_endpoint(
            "ivshmem0",
            model_for(&registry, 1, 0),
            Arc::clone(&stage2_peer0),
        );
        let stage2_peer1 = Arc::new(TestStage2Remap::default());
        let peer1 = build_endpoint(
            "ivshmem1",
            model_for(&registry, 1, 1),
            Arc::clone(&stage2_peer1),
        );
        peer0.enable_memory();
        peer1.enable_memory();

        // Peer 0 writes `target << 16 | vector`; peer 1 observes the event.
        peer0
            .write_register(DOORBELL_OFFSET, doorbell(1, 0))
            .unwrap();
        assert_eq!(event_status(&peer1), 1);
        assert_eq!(event_status(&peer0), 0);

        // Repeated doorbells merge into the single pending bit.
        peer0
            .write_register(DOORBELL_OFFSET, doorbell(1, 0))
            .unwrap();
        assert_eq!(event_status(&peer1), 1);

        // W1C clear, then the next doorbell pends again.
        peer1.write_register(EVENT_STATUS_OFFSET, 1).unwrap();
        assert_eq!(event_status(&peer1), 0);
        peer0
            .write_register(DOORBELL_OFFSET, doorbell(1, 0))
            .unwrap();
        assert_eq!(event_status(&peer1), 1);
    }

    #[test]
    fn doorbell_ignores_inactive_targets_and_unsupported_vectors() {
        let registry = test_registry();
        let stage2_peer0 = Arc::new(TestStage2Remap::default());
        let peer0 = build_endpoint(
            "ivshmem0",
            model_for(&registry, 1, 0),
            Arc::clone(&stage2_peer0),
        );
        let stage2_peer1 = Arc::new(TestStage2Remap::default());
        let peer1 = build_endpoint(
            "ivshmem1",
            model_for(&registry, 1, 1),
            Arc::clone(&stage2_peer1),
        );
        peer0.enable_memory();
        peer1.enable_memory();

        // Vector 1 is outside the current profile.
        peer0
            .write_register(DOORBELL_OFFSET, doorbell(1, 1))
            .unwrap();
        // Target 9 is outside the two-peer profile.
        peer0
            .write_register(DOORBELL_OFFSET, doorbell(9, 0))
            .unwrap();
        assert_eq!(event_status(&peer1), 0);

        // A self-addressed doorbell must not deadlock: it proves the writing
        // endpoint does not hold its register lock while the sink re-locks
        // the very same registers.
        peer0
            .write_register(DOORBELL_OFFSET, doorbell(0, 0))
            .unwrap();
        assert_eq!(event_status(&peer0), 1);
    }

    #[test]
    fn doorbell_leaves_other_registers_and_shared_bytes_untouched() {
        let registry = test_registry();
        let stage2_peer0 = Arc::new(TestStage2Remap::default());
        let peer0 = build_endpoint(
            "ivshmem0",
            model_for(&registry, 1, 0),
            Arc::clone(&stage2_peer0),
        );
        let stage2_peer1 = Arc::new(TestStage2Remap::default());
        let peer1 = build_endpoint(
            "ivshmem1",
            model_for(&registry, 1, 1),
            Arc::clone(&stage2_peer1),
        );
        peer0.enable_memory();
        peer1.enable_memory();

        peer0.write_register(STATE_OFFSET, 0x1234).unwrap();
        peer0
            .binding
            .write_bar(
                peer0.shared_bar + TEST_OFFSET,
                AccessWidth::Qword,
                TEST_VALUE,
            )
            .unwrap();

        peer0
            .write_register(DOORBELL_OFFSET, doorbell(1, 0))
            .unwrap();

        assert_eq!(event_status(&peer1), 1);
        // The doorbell write value never leaks into the writer's registers.
        assert_eq!(
            peer0
                .read_register(STATE_OFFSET, AccessWidth::Dword)
                .unwrap(),
            0x1234
        );
        // The receiver's local registers and the shared bytes stay intact.
        assert_eq!(
            peer1
                .read_register(STATE_OFFSET, AccessWidth::Dword)
                .unwrap(),
            0
        );
        assert_eq!(
            peer1
                .binding
                .read_bar(peer1.shared_bar + TEST_OFFSET, AccessWidth::Qword)
                .unwrap(),
            TEST_VALUE
        );
    }

    #[test]
    fn bar2_derives_direct_mappings_for_each_peer_view() {
        let registry = test_registry();
        let stage2_peer0 = Arc::new(TestStage2Remap::default());
        let peer0 = build_endpoint(
            "ivshmem0",
            model_for(&registry, 1, 0),
            Arc::clone(&stage2_peer0),
        );
        let stage2_peer1 = Arc::new(TestStage2Remap::default());
        let peer1 = build_endpoint(
            "ivshmem1",
            model_for(&registry, 1, 1),
            Arc::clone(&stage2_peer1),
        );
        peer0.enable_memory();
        peer1.enable_memory();

        // Each endpoint committed three mappings: the read-only state table,
        // its own writable output, and the peer's read-only output.
        for (endpoint, own_peer) in [(&peer0, 0u16), (&peer1, 1u16)] {
            let committed = endpoint
                .stage2
                .committed
                .lock()
                .unwrap()
                .values()
                .flatten()
                .cloned()
                .collect::<Vec<_>>();
            assert_eq!(committed.len(), 3);
            // The state table maps read-only at the resolved BAR2 address.
            assert_eq!(committed[0].gpa_base(), endpoint.shared_bar);
            assert!(!committed[0].writable());
            // The peer's own output maps read-write, the other read-only.
            assert_eq!(committed[1].writable(), own_peer == 0);
            assert_eq!(committed[2].writable(), own_peer == 1);
        }
        // Both peers map the same backing: identical HPA bases.
        let hpa0 = peer0
            .stage2
            .committed
            .lock()
            .unwrap()
            .values()
            .flatten()
            .next()
            .unwrap()
            .hpa_base();
        let hpa1 = peer1
            .stage2
            .committed
            .lock()
            .unwrap()
            .values()
            .flatten()
            .next()
            .unwrap()
            .hpa_base();
        assert_eq!(hpa0, hpa1);
    }

    #[test]
    fn state_writes_propagate_into_the_shared_state_table() {
        let registry = test_registry();
        let stage2_peer0 = Arc::new(TestStage2Remap::default());
        let peer0 = build_endpoint(
            "ivshmem0",
            model_for(&registry, 1, 0),
            Arc::clone(&stage2_peer0),
        );
        let stage2_peer1 = Arc::new(TestStage2Remap::default());
        let peer1 = build_endpoint(
            "ivshmem1",
            model_for(&registry, 1, 1),
            Arc::clone(&stage2_peer1),
        );
        peer0.enable_memory();
        peer1.enable_memory();

        // The BAR0 State write publishes into the state-table entry, which
        // is readable through every peer's BAR2 mapping.
        peer0.write_register(STATE_OFFSET, 0x0001_0002).unwrap();
        assert_eq!(
            peer0
                .binding
                .read_bar(peer0.shared_bar, AccessWidth::Dword)
                .unwrap(),
            0x0001_0002
        );
        assert_eq!(
            peer1
                .binding
                .read_bar(peer1.shared_bar, AccessWidth::Dword)
                .unwrap(),
            0x0001_0002
        );

        // Each peer owns its own entry: peer 1 writes its state and both
        // peers observe the same values at the same offsets.
        assert_eq!(
            peer1
                .binding
                .read_bar(peer1.shared_bar + 4, AccessWidth::Dword)
                .unwrap(),
            0
        );
        peer1.write_register(STATE_OFFSET, 2).unwrap();
        assert_eq!(
            peer0
                .binding
                .read_bar(peer0.shared_bar + 4, AccessWidth::Dword)
                .unwrap(),
            2
        );

        // Reserved bytes inside the state-table page read zero.
        assert_eq!(
            peer0
                .binding
                .read_bar(peer0.shared_bar + 8, AccessWidth::Dword)
                .unwrap(),
            0
        );
        assert_eq!(
            peer0
                .binding
                .read_bar(peer0.shared_bar + 0xffc, AccessWidth::Dword)
                .unwrap(),
            0
        );
    }

    #[test]
    fn output_section_permissions_follow_the_owner_matrix() {
        let registry = test_registry();
        let stage2_peer0 = Arc::new(TestStage2Remap::default());
        let peer0 = build_endpoint(
            "ivshmem0",
            model_for(&registry, 1, 0),
            Arc::clone(&stage2_peer0),
        );
        let stage2_peer1 = Arc::new(TestStage2Remap::default());
        let peer1 = build_endpoint(
            "ivshmem1",
            model_for(&registry, 1, 1),
            Arc::clone(&stage2_peer1),
        );
        peer0.enable_memory();
        peer1.enable_memory();

        // Peer 0 writes its own output section (0x1000..0x8000); peer 1
        // reads the same bytes through its own BAR2.
        peer0
            .binding
            .write_bar(peer0.shared_bar + 0x1000, AccessWidth::Dword, 0x0aa0)
            .unwrap();
        assert_eq!(
            peer1
                .binding
                .read_bar(peer1.shared_bar + 0x1000, AccessWidth::Dword)
                .unwrap(),
            0x0aa0
        );

        // Peer 1 may not write peer 0's output: the write is denied and the
        // owner's data stays intact.
        let denied =
            peer1
                .binding
                .write_bar(peer1.shared_bar + 0x1000, AccessWidth::Dword, 0x0bad_0bad);
        let Err(DeviceError::AccessDenied { detail, .. }) = denied else {
            panic!("cross-peer output write must be denied");
        };
        assert!(detail.contains("peer 1 may not write output section"));
        assert_eq!(
            peer0
                .binding
                .read_bar(peer0.shared_bar + 0x1000, AccessWidth::Dword)
                .unwrap(),
            0x0aa0
        );

        // Peer 1's own output section starts at 0x8000 and is writable.
        peer1
            .binding
            .write_bar(peer1.shared_bar + 0x8000, AccessWidth::Dword, 0x0bb1)
            .unwrap();
        assert_eq!(
            peer0
                .binding
                .read_bar(peer0.shared_bar + 0x8000, AccessWidth::Dword)
                .unwrap(),
            0x0bb1
        );

        // The reserved tail is silently ignored: the write succeeds without
        // touching the backing, so reads still observe zeroes.
        peer0
            .binding
            .write_bar(peer0.shared_bar + 0xf000, AccessWidth::Dword, 0x0cc2)
            .unwrap();
        assert_eq!(
            peer0
                .binding
                .read_bar(peer0.shared_bar + 0xf000, AccessWidth::Dword)
                .unwrap(),
            0
        );
    }

    #[test]
    fn state_table_writes_from_bar2_are_denied() {
        let registry = test_registry();
        let stage2_peer0 = Arc::new(TestStage2Remap::default());
        let peer0 = build_endpoint(
            "ivshmem0",
            model_for(&registry, 1, 0),
            Arc::clone(&stage2_peer0),
        );
        let stage2_peer1 = Arc::new(TestStage2Remap::default());
        let peer1 = build_endpoint(
            "ivshmem1",
            model_for(&registry, 1, 1),
            Arc::clone(&stage2_peer1),
        );
        peer0.enable_memory();
        peer1.enable_memory();

        // Entry 0 (peer 0) and the last word of the state-table page both
        // reject direct BAR2 writes.
        for offset in [0, 0xffc] {
            let denied =
                peer0
                    .binding
                    .write_bar(peer0.shared_bar + offset, AccessWidth::Dword, 0x1234);
            assert!(
                matches!(denied, Err(DeviceError::AccessDenied { .. })),
                "offset {offset:#x} must be rejected with AccessDenied"
            );
        }
        // The denied writes did not corrupt the entries.
        assert_eq!(
            peer0
                .binding
                .read_bar(peer0.shared_bar, AccessWidth::Dword)
                .unwrap(),
            0
        );
        // Shared-region writes keep working past the state-table page.
        peer0
            .binding
            .write_bar(peer0.shared_bar + 0x1000, AccessWidth::Dword, 0xaa)
            .unwrap();
        assert_eq!(
            peer1
                .binding
                .read_bar(peer1.shared_bar + 0x1000, AccessWidth::Dword)
                .unwrap(),
            0xaa
        );
    }

    #[test]
    fn endpoint_reset_clears_only_the_owning_state_entry() {
        let registry = test_registry();
        let stage2_peer0 = Arc::new(TestStage2Remap::default());
        let peer0 = build_endpoint(
            "ivshmem0",
            model_for(&registry, 1, 0),
            Arc::clone(&stage2_peer0),
        );
        let stage2_peer1 = Arc::new(TestStage2Remap::default());
        let peer1 = build_endpoint(
            "ivshmem1",
            model_for(&registry, 1, 1),
            Arc::clone(&stage2_peer1),
        );
        peer0.enable_memory();
        peer1.enable_memory();

        peer0.write_register(STATE_OFFSET, 0x11).unwrap();
        peer1.write_register(STATE_OFFSET, 0x22).unwrap();
        peer0
            .binding
            .write_bar(peer0.shared_bar + 0x1000, AccessWidth::Dword, 0x33)
            .unwrap();

        peer0.binding.reset().unwrap();
        peer0.enable_memory();

        // Peer 0's entry is zero; peer 1's entry and the shared bytes stay.
        assert_eq!(
            peer0
                .binding
                .read_bar(peer0.shared_bar, AccessWidth::Dword)
                .unwrap(),
            0
        );
        assert_eq!(
            peer1
                .binding
                .read_bar(peer1.shared_bar + 4, AccessWidth::Dword)
                .unwrap(),
            0x22
        );
        assert_eq!(
            peer1
                .binding
                .read_bar(peer1.shared_bar + 0x1000, AccessWidth::Dword)
                .unwrap(),
            0x33
        );
    }

    #[test]
    fn doorbell_reads_stay_zero_and_reset_clears_pending_events() {
        let registry = test_registry();
        let stage2_peer0 = Arc::new(TestStage2Remap::default());
        let peer0 = build_endpoint(
            "ivshmem0",
            model_for(&registry, 1, 0),
            Arc::clone(&stage2_peer0),
        );
        let stage2_peer1 = Arc::new(TestStage2Remap::default());
        let peer1 = build_endpoint(
            "ivshmem1",
            model_for(&registry, 1, 1),
            Arc::clone(&stage2_peer1),
        );
        peer0.enable_memory();
        peer1.enable_memory();

        peer0
            .write_register(DOORBELL_OFFSET, doorbell(1, 0))
            .unwrap();
        // The doorbell register is write-only by specification.
        assert_eq!(
            peer0
                .read_register(DOORBELL_OFFSET, AccessWidth::Dword)
                .unwrap(),
            0
        );

        // Endpoint reset clears the pending event; the link and the sink
        // keep working for the next doorbell.
        peer1.binding.reset().unwrap();
        peer1.enable_memory();
        assert_eq!(event_status(&peer1), 0);
        peer0
            .write_register(DOORBELL_OFFSET, doorbell(1, 0))
            .unwrap();
        assert_eq!(event_status(&peer1), 1);
    }
}
