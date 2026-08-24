//! Code-registered constructors for open-ended virtual-device models.

use core::fmt;
use std::{collections::BTreeMap, string::String, sync::Arc, vec::Vec};

use axdevice::*;
use axdevice_base::{ControllerInputId, InterruptControllerId, InterruptSharing, InterruptTrigger};
use axvmconfig::VirtualDeviceRequest;

use crate::{machine::GuestSerialFirmwareIdentity, *};

mod append;
mod ivc;

pub use append::DefaultVirtualDeviceIntent;
#[cfg(any(test, not(target_arch = "aarch64")))]
pub(crate) use append::append_configured_devices;
#[cfg(target_arch = "aarch64")]
pub(crate) use append::collect_configured_devices;

/// Creates one graph node from a validated, model-specific request.
pub type ConfiguredModelConstructor = for<'a> fn(
    DeviceNodeId,
    &VirtualDeviceRequest,
    &'a DeviceInstantiationContext,
) -> Result<DeviceNodeSpec, ConfiguredDeviceError>;

pub type ConfiguredDefaultFixedResources =
    fn(&DeviceInstantiationContext) -> Result<FixedDeviceBindings, ConfiguredDeviceError>;

/// One explicit platform-device catalog entry.
#[derive(Clone, Copy)]
pub struct ConfiguredModelRegistration {
    pub model: &'static str,
    pub create: ConfiguredModelConstructor,
    pub default_fixed_resources: Option<ConfiguredDefaultFixedResources>,
}

/// Creates one PCI endpoint declaration and its build-time model.
pub type ConfiguredPciModelConstructor =
    for<'a> fn(
        DeviceNodeId,
        &VirtualDeviceRequest,
        &'a DeviceInstantiationContext,
    ) -> Result<ConfiguredPciEndpoint, ConfiguredDeviceError>;

/// One explicit PCI endpoint catalog entry.
#[derive(Clone, Copy)]
pub struct ConfiguredPciModelRegistration {
    pub model: &'static str,
    pub create: ConfiguredPciModelConstructor,
    pub default_fixed_resources: Option<ConfiguredDefaultFixedResources>,
}

/// Typed PCI attachment returned by a configured endpoint constructor.
pub struct ConfiguredPciEndpoint {
    function: PciFunctionSpec,
    model: Arc<dyn PciEndpointModel>,
}

impl ConfiguredPciEndpoint {
    /// Pairs one pure PCI function declaration with its build-time model.
    pub fn new(function: PciFunctionSpec, model: Arc<dyn PciEndpointModel>) -> Self {
        Self { function, model }
    }

    /// Returns the pure function declaration.
    pub const fn function(&self) -> &PciFunctionSpec {
        &self.function
    }

    /// Splits this attachment into its declaration and build-time model.
    pub fn into_parts(self) -> (PciFunctionSpec, Arc<dyn PciEndpointModel>) {
        (self.function, self.model)
    }
}

impl fmt::Debug for ConfiguredPciEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConfiguredPciEndpoint")
            .field("function", &self.function)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy)]
enum RegisteredModel {
    Platform(ConfiguredModelRegistration),
    Pci(ConfiguredPciModelRegistration),
}

impl RegisteredModel {
    const fn default_fixed_resources(self) -> Option<ConfiguredDefaultFixedResources> {
        match self {
            Self::Platform(registration) => registration.default_fixed_resources,
            Self::Pci(registration) => registration.default_fixed_resources,
        }
    }
}

pub(crate) enum ConfiguredDeviceAttachment {
    Platform(DeviceNodeSpec),
    Pci(ConfiguredPciEndpoint),
}

#[derive(Clone, Debug)]
pub struct FixedWiredBinding {
    pub controller: InterruptControllerId,
    pub input: ControllerInputId,
    pub trigger: InterruptTrigger,
    pub sharing: InterruptSharing,
}

/// Planner-only fixed resources derived from a machine profile or host
/// firmware. These values never cross the user configuration boundary.
#[derive(Clone, Debug, Default)]
pub struct FixedDeviceBindings {
    mmio: BTreeMap<ResourceSlot, (u64, u64)>,
    pio: BTreeMap<ResourceSlot, (u16, u16)>,
    wired: BTreeMap<ResourceSlot, FixedWiredBinding>,
}

impl FixedDeviceBindings {
    pub fn with_mmio(mut self, slot: ResourceSlot, base: u64, size: u64) -> Self {
        self.mmio.insert(slot, (base, size));
        self
    }

    pub fn with_pio(mut self, slot: ResourceSlot, base: u16, size: u16) -> Self {
        self.pio.insert(slot, (base, size));
        self
    }

    pub fn with_wired(mut self, slot: ResourceSlot, binding: FixedWiredBinding) -> Self {
        self.wired.insert(slot, binding);
        self
    }

    pub fn mmio(&self, slot: &ResourceSlot) -> Option<(u64, u64)> {
        self.mmio.get(slot).copied()
    }

    pub fn pio(&self, slot: &ResourceSlot) -> Option<(u16, u16)> {
        self.pio.get(slot).copied()
    }

    pub fn wired(&self, slot: &ResourceSlot) -> Option<&FixedWiredBinding> {
        self.wired.get(slot)
    }
}

#[derive(Clone)]
pub struct DeviceInstantiationContext {
    vm_id: Option<usize>,
    default_wired_controller: Option<(DeviceNodeId, InterruptControllerId)>,
    fixed: FixedDeviceBindings,
    firmware_binding: DeviceFirmwareBinding,
    serial_profile: Option<crate::machine::GuestSerialProfile>,
    serial_backend_factory: Arc<dyn SerialBackendFactory>,
    host_console_by_default: bool,
}

impl DeviceInstantiationContext {
    pub fn new() -> Self {
        Self {
            vm_id: None,
            default_wired_controller: None,
            fixed: FixedDeviceBindings::default(),
            firmware_binding: DeviceFirmwareBinding::None,
            serial_profile: None,
            serial_backend_factory: Arc::new(NullSerialBackendFactory),
            host_console_by_default: false,
        }
    }

    pub(crate) fn with_vm_id(mut self, vm_id: usize) -> Self {
        self.vm_id = Some(vm_id);
        self
    }

    pub fn vm_id(&self) -> Option<usize> {
        self.vm_id
    }

    pub fn with_default_wired_controller(
        mut self,
        node: DeviceNodeId,
        controller: InterruptControllerId,
    ) -> Self {
        self.default_wired_controller = Some((node, controller));
        self
    }

    pub fn default_wired_controller(&self) -> Option<InterruptControllerId> {
        self.default_wired_controller
            .as_ref()
            .map(|(_, controller)| *controller)
    }

    /// Returns the graph node that must precede users of the default wired domain.
    pub fn default_wired_controller_node(&self) -> Option<&DeviceNodeId> {
        self.default_wired_controller.as_ref().map(|(node, _)| node)
    }

    pub fn fixed_bindings(&self) -> &FixedDeviceBindings {
        &self.fixed
    }

    pub(crate) fn with_fixed_bindings(mut self, fixed: FixedDeviceBindings) -> Self {
        self.fixed = fixed;
        self
    }

    pub fn firmware_binding(&self) -> &DeviceFirmwareBinding {
        &self.firmware_binding
    }

    pub(crate) fn with_serial_defaults(
        mut self,
        profile: crate::machine::GuestSerialProfile,
        backend_factory: Arc<dyn SerialBackendFactory>,
        fixed: FixedDeviceBindings,
        firmware_binding: DeviceFirmwareBinding,
        host_console_by_default: bool,
    ) -> Self {
        self.serial_profile = Some(profile);
        self.serial_backend_factory = backend_factory;
        self.fixed = fixed;
        self.firmware_binding = firmware_binding;
        self.host_console_by_default = host_console_by_default;
        self
    }

    pub(crate) const fn serial_profile(&self) -> Option<crate::machine::GuestSerialProfile> {
        self.serial_profile
    }

    pub(crate) fn serial_backend_factory(&self) -> Arc<dyn SerialBackendFactory> {
        self.serial_backend_factory.clone()
    }

    pub(crate) const fn host_console_by_default(&self) -> bool {
        self.host_console_by_default
    }
}

impl Default for DeviceInstantiationContext {
    fn default() -> Self {
        Self::new()
    }
}

pub struct ConfiguredDeviceCatalog {
    registrations: BTreeMap<String, RegisteredModel>,
}

impl ConfiguredDeviceCatalog {
    pub fn new() -> Self {
        let mut catalog = Self {
            registrations: BTreeMap::new(),
        };
        for registration in crate::machine::SERIAL_REGISTRATIONS {
            let previous = catalog.registrations.insert(
                registration.model.into(),
                RegisteredModel::Platform(*registration),
            );
            debug_assert!(previous.is_none());
        }
        for registration in ivc::IVC_REGISTRATIONS {
            let previous = catalog.registrations.insert(
                registration.model.into(),
                RegisteredModel::Platform(*registration),
            );
            debug_assert!(previous.is_none());
        }
        catalog
    }

    pub fn register(
        &mut self,
        registration: ConfiguredModelRegistration,
    ) -> Result<(), ConfiguredDeviceError> {
        let name = registration.model;
        validate_model_name(name)?;
        if self.registrations.contains_key(name) {
            return Err(ConfiguredDeviceError::DuplicateModel { model: name.into() });
        }
        self.registrations
            .insert(name.into(), RegisteredModel::Platform(registration));
        Ok(())
    }

    /// Registers a configured model that attaches below an architecture PCI bus.
    pub fn register_pci_model(
        &mut self,
        registration: ConfiguredPciModelRegistration,
    ) -> Result<(), ConfiguredDeviceError> {
        let name = registration.model;
        validate_model_name(name)?;
        if self.registrations.contains_key(name) {
            return Err(ConfiguredDeviceError::DuplicateModel { model: name.into() });
        }
        self.registrations
            .insert(name.into(), RegisteredModel::Pci(registration));
        Ok(())
    }

    pub fn instantiate_node(
        &self,
        request: &VirtualDeviceRequest,
        context: &DeviceInstantiationContext,
    ) -> Result<DeviceNodeSpec, ConfiguredDeviceError> {
        let id = request_device_id(request)?;
        match self.registrations.get(&request.model) {
            Some(RegisteredModel::Platform(registration)) => {
                (registration.create)(id, request, context)
            }
            Some(RegisteredModel::Pci(_)) => Err(attachment_mismatch(request, "platform")),
            None => Err(unknown_model(request)),
        }
    }

    pub(crate) fn instantiate(
        &self,
        request: &VirtualDeviceRequest,
        context: &DeviceInstantiationContext,
    ) -> Result<ConfiguredDeviceAttachment, ConfiguredDeviceError> {
        let id = request_device_id(request)?;
        match self.registrations.get(&request.model) {
            Some(RegisteredModel::Platform(registration)) => {
                (registration.create)(id, request, context)
                    .map(ConfiguredDeviceAttachment::Platform)
            }
            Some(RegisteredModel::Pci(registration)) => {
                let requested_id = id.clone();
                let endpoint = (registration.create)(id, request, context)?;
                if endpoint.function().id() != &requested_id {
                    return Err(ConfiguredDeviceError::Instantiation {
                        device: request.id.clone(),
                        model: request.model.clone(),
                        detail: std::format!(
                            "PCI constructor returned function id '{}'",
                            endpoint.function().id()
                        ),
                    });
                }
                Ok(ConfiguredDeviceAttachment::Pci(endpoint))
            }
            None => Err(unknown_model(request)),
        }
    }

    pub fn default_fixed_resources(
        &self,
        model: &str,
        context: &DeviceInstantiationContext,
    ) -> Result<Option<FixedDeviceBindings>, ConfiguredDeviceError> {
        self.registrations
            .get(model)
            .copied()
            .and_then(RegisteredModel::default_fixed_resources)
            .map(|fixed| fixed(context))
            .transpose()
    }
}

impl Default for ConfiguredDeviceCatalog {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for ConfiguredDeviceCatalog {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConfiguredDeviceCatalog")
            .field("models", &self.registrations.keys().collect::<Vec<_>>())
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ConfiguredDeviceError {
    #[error("unknown virtual device model '{model}'")]
    UnknownVirtualDeviceModel { model: String },
    #[error("virtual device model '{model}' is registered more than once")]
    DuplicateModel { model: String },
    #[error("invalid virtual device model name '{model}'")]
    InvalidModelName { model: String },
    #[error("invalid options for virtual device '{device}' ({model}): {detail}")]
    InvalidOptions {
        device: String,
        model: String,
        detail: String,
    },
    #[error("failed to instantiate virtual device '{device}' ({model}): {detail}")]
    Instantiation {
        device: String,
        model: String,
        detail: String,
    },
    #[error("invalid virtual device id '{device}': {detail}")]
    InvalidDeviceId { device: String, detail: String },
    #[error("virtual device '{device}' ({model}) is not attached as a {expected} device")]
    ModelAttachmentMismatch {
        device: String,
        model: String,
        expected: &'static str,
    },
}

fn request_device_id(
    request: &VirtualDeviceRequest,
) -> Result<DeviceNodeId, ConfiguredDeviceError> {
    DeviceNodeId::new(request.id.clone()).map_err(|error| ConfiguredDeviceError::InvalidDeviceId {
        device: request.id.clone(),
        detail: std::format!("{error}"),
    })
}

fn unknown_model(request: &VirtualDeviceRequest) -> ConfiguredDeviceError {
    ConfiguredDeviceError::UnknownVirtualDeviceModel {
        model: request.model.clone(),
    }
}

fn attachment_mismatch(
    request: &VirtualDeviceRequest,
    expected: &'static str,
) -> ConfiguredDeviceError {
    ConfiguredDeviceError::ModelAttachmentMismatch {
        device: request.id.clone(),
        model: request.model.clone(),
        expected,
    }
}

fn validate_model_name(name: &str) -> Result<(), ConfiguredDeviceError> {
    let valid = !name.is_empty()
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'.')
        });
    if valid {
        Ok(())
    } else {
        Err(ConfiguredDeviceError::InvalidModelName { model: name.into() })
    }
}

#[cfg(test)]
mod tests {
    use axdevice_base::{BusResponse, DeviceAccess, DeviceResult};

    use super::*;
    use crate::config::{AxVMConfig, AxVMConfigParams, PhysCpuList};

    const PCI_REGISTRATION: ConfiguredPciModelRegistration = ConfiguredPciModelRegistration {
        model: "test-pci",
        create: create_pci_endpoint,
        default_fixed_resources: None,
    };
    const PLATFORM_REGISTRATION: ConfiguredModelRegistration = ConfiguredModelRegistration {
        model: "test-pci",
        create: create_platform_device,
        default_fixed_resources: None,
    };

    struct TestPciFunction;

    impl PciFunction for TestPciFunction {
        fn name(&self) -> &str {
            "test-pci"
        }

        fn access_bar(
            &self,
            _access: &PciBarAccess,
            _context: &mut dyn DeviceAccess,
        ) -> DeviceResult<BusResponse> {
            Ok(BusResponse::Write)
        }
    }

    struct TestPciEndpointModel;

    impl PciEndpointModel for TestPciEndpointModel {
        fn requirements(&self) -> DeviceManagerResult<DeviceRequirements> {
            Ok(DeviceRequirements::new())
        }

        fn build(
            &self,
            _context: &mut DeviceBuildContext<'_>,
        ) -> DeviceManagerResult<PciEndpointBundle> {
            Ok(PciEndpointBundle::new(Arc::new(TestPciFunction)))
        }
    }

    fn create_platform_device(
        id: DeviceNodeId,
        _request: &VirtualDeviceRequest,
        _context: &DeviceInstantiationContext,
    ) -> Result<DeviceNodeSpec, ConfiguredDeviceError> {
        Ok(DeviceNodeSpec::firmware_only(id))
    }

    fn create_pci_endpoint(
        id: DeviceNodeId,
        _request: &VirtualDeviceRequest,
        _context: &DeviceInstantiationContext,
    ) -> Result<ConfiguredPciEndpoint, ConfiguredDeviceError> {
        Ok(test_pci_endpoint(id))
    }

    fn create_pci_endpoint_with_wrong_id(
        _id: DeviceNodeId,
        _request: &VirtualDeviceRequest,
        _context: &DeviceInstantiationContext,
    ) -> Result<ConfiguredPciEndpoint, ConfiguredDeviceError> {
        Ok(test_pci_endpoint(DeviceNodeId::new("wrong-id").unwrap()))
    }

    fn test_pci_endpoint(id: DeviceNodeId) -> ConfiguredPciEndpoint {
        let identity = PciEndpointIdentity::new(0x110a, 0x4106, PciClass::new(0xff, 0, 0));
        let function = PciFunctionSpec::new(id, identity)
            .with_bar(
                PciMemoryBar::new(
                    PciBarIndex::new(0).unwrap(),
                    0x1000,
                    PciMemoryBarWidth::Bits32,
                )
                .unwrap(),
            )
            .unwrap();
        ConfiguredPciEndpoint::new(function, Arc::new(TestPciEndpointModel))
    }

    fn request() -> VirtualDeviceRequest {
        VirtualDeviceRequest {
            id: "pci0".into(),
            model: "test-pci".into(),
            options: Default::default(),
        }
    }

    #[test]
    fn pci_registration_preserves_typed_bus_attachment() {
        let mut catalog = ConfiguredDeviceCatalog::new();
        catalog.register_pci_model(PCI_REGISTRATION).unwrap();
        let endpoint = catalog
            .instantiate(&request(), &DeviceInstantiationContext::new())
            .unwrap();
        let ConfiguredDeviceAttachment::Pci(endpoint) = endpoint else {
            panic!("test-pci must retain its PCI attachment");
        };

        assert_eq!(endpoint.function().id().as_str(), "pci0");
        assert!(matches!(
            catalog.instantiate_node(&request(), &DeviceInstantiationContext::new()),
            Err(ConfiguredDeviceError::ModelAttachmentMismatch { .. })
        ));
    }

    #[test]
    fn pci_constructor_must_preserve_the_requested_device_id() {
        let mut catalog = ConfiguredDeviceCatalog::new();
        catalog
            .register_pci_model(ConfiguredPciModelRegistration {
                model: "test-pci",
                create: create_pci_endpoint_with_wrong_id,
                default_fixed_resources: None,
            })
            .unwrap();

        let error = catalog
            .instantiate(&request(), &DeviceInstantiationContext::new())
            .err()
            .unwrap();
        assert!(matches!(error, ConfiguredDeviceError::Instantiation { .. }));
        assert!(error.to_string().contains("wrong-id"));
    }

    #[test]
    fn configured_collection_separates_platform_and_pci_attachments() {
        let mut catalog = ConfiguredDeviceCatalog::new();
        catalog.register_pci_model(PCI_REGISTRATION).unwrap();
        let config = AxVMConfig::new(AxVMConfigParams {
            phys_cpu_ls: PhysCpuList::new(1, None, None),
            virtual_device_requests: vec![request()],
            virtual_device_catalog: Some(Arc::new(catalog)),
            ..Default::default()
        });
        let controller = DeviceNodeId::new("controller").unwrap();
        let configured =
            append::collect_configured_devices(&config, &controller, InterruptControllerId::new(0))
                .unwrap();

        assert_eq!(configured.platform_nodes.len(), 1);
        assert_eq!(configured.pci_endpoints.len(), 1);
        assert_eq!(configured.pci_endpoints[0].function().id().as_str(), "pci0");

        let mut nodes = Vec::new();
        assert!(matches!(
            append::append_configured_devices(
                &config,
                &mut nodes,
                &controller,
                InterruptControllerId::new(0),
            ),
            Err(AxVmError::Unsupported { .. })
        ));
        assert!(nodes.is_empty());
    }

    #[test]
    fn platform_and_pci_models_share_one_name_namespace() {
        let mut catalog = ConfiguredDeviceCatalog::new();
        catalog.register_pci_model(PCI_REGISTRATION).unwrap();

        assert!(matches!(
            catalog.register_pci_model(PCI_REGISTRATION),
            Err(ConfiguredDeviceError::DuplicateModel { .. })
        ));
        assert!(matches!(
            catalog.register(PLATFORM_REGISTRATION),
            Err(ConfiguredDeviceError::DuplicateModel { .. })
        ));
    }
}
