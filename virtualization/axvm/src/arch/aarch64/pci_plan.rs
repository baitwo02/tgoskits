//! AArch64 generic-ECAM host construction and resolved firmware view.

use std::sync::Arc;

use axdevice::*;

use crate::{AxVmError, AxVmResult, boot::fdt::core::pci::GuestPciHost, config::AxVMConfig};

const PCI_HOST_ID: &str = "pci-host";
const PCI_HOST_KEY: &str = "aarch64-ecam";
const ECAM_SLOT: &str = "ecam";
const MEMORY_SLOT: &str = "memory-aperture";
const PCI_MEMORY_APERTURE_SIZE: u64 = 0x0400_0000;

pub(super) fn host_key() -> PciHostKey {
    PciHostKey::new(PCI_HOST_KEY).expect("static AArch64 PCI host key is valid")
}

pub(super) fn provider(controller: &DeviceNodeId) -> DeviceManagerResult<PciHostProvider> {
    let host_id = DeviceNodeId::new(PCI_HOST_ID)?;
    let model: Arc<dyn DeviceModel> = Arc::new(Aarch64PciHostModel {
        host_id: host_id.clone(),
    });
    let node = DeviceNodeSpec::virtual_device(host_id, model).with_dependency(controller.clone());
    Ok(PciHostProvider::new(
        host_key(),
        node,
        ResourceSlot::new(MEMORY_SLOT)?,
    ))
}

struct Aarch64PciHostModel {
    host_id: DeviceNodeId,
}

impl DeviceModel for Aarch64PciHostModel {
    fn requirements(&self) -> DeviceManagerResult<DeviceRequirements> {
        DeviceRequirements::new()
            .with_mmio(
                ResourceSlot::new(ECAM_SLOT)?,
                PCI_BUS_ZERO_ECAM_SIZE,
                PCI_BUS_ZERO_ECAM_SIZE,
                ResourceRequest::Auto,
            )?
            .with_mmio(
                ResourceSlot::new(MEMORY_SLOT)?,
                PCI_MEMORY_APERTURE_SIZE,
                PCI_MEMORY_APERTURE_SIZE,
                ResourceRequest::Auto,
            )
    }

    fn firmware(&self) -> DeviceFirmwareSpec {
        // The AArch64 firmware adapter emits the generic ECAM node from the
        // graph-resolved host ranges after validating the input DTB.
        DeviceFirmwareSpec::None
    }

    fn build(&self, context: &mut DeviceBuildContext<'_>) -> DeviceManagerResult<DeviceBundle> {
        let ecam = context.mmio(ECAM_SLOT)?;
        let memory = context.mmio(MEMORY_SLOT)?;
        let memory_end =
            memory
                .0
                .checked_add(memory.1)
                .ok_or_else(|| DeviceManagerError::InvalidConfig {
                    operation: "build AArch64 PCI host",
                    detail: "resolved PCI memory aperture overflows u64".into(),
                })?;
        let topology = context
            .pci_host_topology()
            .ok_or_else(|| DeviceManagerError::InvalidState {
                operation: "build AArch64 PCI host",
                detail: "resolved graph did not attach PCI topology metadata".into(),
            })?
            .clone();
        if ecam.1 != PCI_BUS_ZERO_ECAM_SIZE
            || ecam.0 & (PCI_BUS_ZERO_ECAM_SIZE - 1) != 0
            || topology.memory_aperture() != &(memory.0..memory_end)
        {
            return Err(DeviceManagerError::InvalidConfig {
                operation: "build AArch64 PCI host",
                detail: "resolved PCI host resources differ from the AArch64 provider".into(),
            });
        }

        let root = Arc::new(PciRootState::new(topology));
        let binding = Arc::new(PciRootBinding::new(self.host_id.clone(), root.clone()));
        let mut bundle = DeviceBundle::new();
        bundle.add_device(Arc::new(PciEcamFrontend::new(ecam.0, root.clone())));
        bundle.add_device(Arc::new(PciMmioApertureDevice::new(
            memory.0,
            memory.1,
            binding.clone(),
        )));
        bundle.add_lifecycle(Arc::new(PciRootStateLifecycle::new(binding.clone())));
        bundle.provide_service::<PciRootBindingKey>(binding)?;
        Ok(bundle)
    }
}

#[derive(Debug)]
pub(super) struct Aarch64PciPlan {
    firmware: Option<GuestPciHost>,
}

impl Aarch64PciPlan {
    pub(super) fn resolve(config: &AxVMConfig, graph: &ResolvedDeviceGraph) -> AxVmResult<Self> {
        let host_id = DeviceNodeId::new(PCI_HOST_ID)?;
        let topology = graph.pci_topology(&host_key()).ok_or_else(|| {
            AxVmError::invalid_config("AArch64 device graph has no resolved PCI host")
        })?;
        let has_endpoint = topology
            .functions()
            .any(|function| function.owner() != &host_id);
        if has_endpoint && config.image_config().dtb_load_gpa.is_none() {
            return Err(AxVmError::unsupported(
                "create AArch64 virtual PCI host",
                "configured PCI endpoints require a guest DTB; UEFI/ACPI PCI is not implemented",
            ));
        }

        let resources = graph.resources_for(&host_id)?;
        let ecam = resources.mmio(&ResourceSlot::new(ECAM_SLOT)?)?;
        let memory = resources.mmio(&ResourceSlot::new(MEMORY_SLOT)?)?;
        let firmware = has_endpoint
            .then(|| GuestPciHost::new(ecam, memory))
            .transpose()?;
        Ok(Self { firmware })
    }

    pub(super) const fn firmware(&self) -> Option<GuestPciHost> {
        self.firmware
    }
}

#[cfg(test)]
mod tests {
    use axdevice_base::{Device, DeviceAccess, DeviceContext, DeviceError, DeviceResult, Resource};
    use axvm_types::GuestPhysAddr;

    use super::*;
    use crate::config::{AxVMConfigParams, PhysCpuList, VMImageConfig};

    struct TestEndpointModel;
    struct TestFunction;

    impl DeviceModel for TestEndpointModel {
        fn requirements(&self) -> DeviceManagerResult<DeviceRequirements> {
            let bar = PciMemoryBar::new(PciBarIndex::new(2)?, 0x1_0000)?;
            let function = PciFunctionRequirement::new(
                host_key(),
                PciEndpointIdentity::new(0x1af4, 0x1110, PciClass::new(0x05, 0, 0)),
            )
            .with_bar(bar)?;
            DeviceRequirements::new().with_pci_function(function)
        }

        fn firmware(&self) -> DeviceFirmwareSpec {
            DeviceFirmwareSpec::None
        }

        fn build(
            &self,
            _context: &mut DeviceBuildContext<'_>,
        ) -> DeviceManagerResult<DeviceBundle> {
            let mut bundle = DeviceBundle::new();
            bundle.add_pci_function(Arc::new(TestFunction))?;
            Ok(bundle)
        }
    }

    impl Device for TestFunction {
        fn name(&self) -> &str {
            "aarch64-test-pci"
        }

        fn resources(&self) -> &[Resource] {
            &[]
        }

        fn read(
            &self,
            _access: &DeviceAccess,
            _context: &mut dyn DeviceContext,
        ) -> DeviceResult<u64> {
            Err(DeviceError::NotFound)
        }

        fn write(
            &self,
            _access: &DeviceAccess,
            _value: u64,
            _context: &mut dyn DeviceContext,
        ) -> DeviceResult {
            Err(DeviceError::NotFound)
        }
    }

    impl PciFunction for TestFunction {
        fn read_bar(
            &self,
            _access: PciBarAccess,
            _context: &mut dyn DeviceContext,
        ) -> DeviceResult<u64> {
            Ok(0)
        }

        fn write_bar(
            &self,
            _access: PciBarAccess,
            _value: u64,
            _context: &mut dyn DeviceContext,
        ) -> DeviceResult {
            Ok(())
        }
    }

    fn config(with_dtb: bool) -> AxVMConfig {
        AxVMConfig::new(AxVMConfigParams {
            phys_cpu_ls: PhysCpuList::new(1, None, None),
            image_config: VMImageConfig {
                dtb_load_gpa: with_dtb.then(|| GuestPhysAddr::from_usize(0x4000_0000)),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    fn resolved_graph(with_endpoint: bool) -> ResolvedDeviceGraph {
        let controller = DeviceNodeId::new("vgic").unwrap();
        let mut graph = DeviceGraphBuilder::new();
        graph
            .add(DeviceNodeSpec::firmware_only(controller.clone()))
            .unwrap();
        if with_endpoint {
            graph
                .add(DeviceNodeSpec::virtual_device(
                    DeviceNodeId::new("endpoint0").unwrap(),
                    Arc::new(TestEndpointModel),
                ))
                .unwrap();
        }
        graph
            .register_pci_host(provider(&controller).unwrap())
            .unwrap();
        let mut pools = ResourcePools::new();
        pools.add_auto_mmio(0x0b00_0000..0x1100_0000).unwrap();
        graph.declare().unwrap().resolve(pools).unwrap()
    }

    #[test]
    fn endpoint_resolves_one_generic_ecam_firmware_view_and_runtime_root() {
        let graph = resolved_graph(true);
        let ids = graph
            .nodes()
            .map(|node| node.id().as_str())
            .collect::<std::vec::Vec<_>>();
        assert_eq!(ids, ["vgic", "pci-host", "endpoint0"]);

        let plan = Aarch64PciPlan::resolve(&config(true), &graph).unwrap();
        let firmware = plan.firmware().unwrap();
        assert_eq!(firmware.ecam_base(), 0x0b00_0000);
        assert_eq!(firmware.memory_base(), 0x0c00_0000);
        assert_eq!(firmware.memory_size(), PCI_MEMORY_APERTURE_SIZE);

        let mut runtime = DeviceRuntimeBuilder::new(RuntimeAccessPorts::new());
        for node in graph.nodes() {
            runtime
                .build_graph_node(node, graph.resource_plan())
                .unwrap();
        }
        runtime.finish(graph.resource_plan()).unwrap();
    }

    #[test]
    fn host_without_endpoints_reserves_no_firmware_node() {
        let graph = resolved_graph(false);
        let plan = Aarch64PciPlan::resolve(&config(false), &graph).unwrap();
        assert!(plan.firmware().is_none());
    }

    #[test]
    fn endpoint_without_guest_dtb_is_rejected() {
        let graph = resolved_graph(true);
        let error = Aarch64PciPlan::resolve(&config(false), &graph).unwrap_err();
        assert!(matches!(error, AxVmError::Unsupported { .. }));
        assert!(error.to_string().contains("require a guest DTB"));
    }
}
