//! Typed declarations connecting PCI functions to the unified device graph.

use alloc::{
    string::{String, ToString},
    vec::Vec,
};
use core::fmt;

use super::{PciBdf, PciEndpointIdentity, PciError, PciFunctionSpec, PciMemoryBar, PciResult};
use crate::{DeviceManagerError, DeviceNodeId, DeviceNodeSpec, ResourceRequest, ResourceSlot};

/// Stable key selecting one architecture-provided PCI host.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PciHostKey(String);

impl PciHostKey {
    /// Creates a validated host key.
    pub fn new(value: impl Into<String>) -> Result<Self, DeviceManagerError> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'@')
            });
        if !valid {
            return Err(DeviceManagerError::InvalidInput {
                operation: "create PCI host key",
                detail: alloc::format!("'{value}' is not a stable PCI host key"),
            });
        }
        Ok(Self(value))
    }

    /// Returns the stable textual key.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PciHostKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// One ordinary model's request to appear as a PCI function.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PciFunctionRequirement {
    pub(crate) host: PciHostKey,
    pub(crate) identity: PciEndpointIdentity,
    pub(crate) bdf: ResourceRequest<PciBdf>,
    pub(crate) bars: Vec<PciMemoryBar>,
    pub(crate) msix: Option<super::msix::PciMsixDeclaration>,
}

impl PciFunctionRequirement {
    /// Creates an automatically placed function with no BARs.
    pub fn new(host: PciHostKey, identity: PciEndpointIdentity) -> Self {
        Self {
            host,
            identity,
            bdf: ResourceRequest::Auto,
            bars: Vec::new(),
            msix: None,
        }
    }

    /// Selects automatic or fixed BDF placement.
    pub const fn with_bdf(mut self, bdf: ResourceRequest<PciBdf>) -> Self {
        self.bdf = bdf;
        self
    }

    /// Adds one memory BAR.
    pub fn with_bar(mut self, bar: PciMemoryBar) -> PciResult<Self> {
        if self
            .bars
            .iter()
            .any(|existing| existing.index() == bar.index())
        {
            return Err(PciError::InvalidBar {
                bar: bar.index(),
                detail: "BAR slot is already occupied by this function".into(),
            });
        }
        self.bars.push(bar);
        Ok(self)
    }

    /// Declares an MSI-X capability for this function.
    ///
    /// # Errors
    ///
    /// Returns [`PciError::InvalidMsix`] when the capability is declared
    /// twice. The PCI layer validates that BAR 1 is declared and at least
    /// [`MSIX_BAR_SIZE`](super::msix::MSIX_BAR_SIZE) large, because the
    /// Table/PBA BIR fields are frozen to BAR 1.
    pub fn with_msix(mut self, msix: super::msix::PciMsixDeclaration) -> PciResult<Self> {
        if self.msix.is_some() {
            return Err(PciError::InvalidMsix {
                detail: "the MSI-X capability is already declared".into(),
            });
        }
        let bar1 = self
            .bars
            .iter()
            .find(|bar| bar.index().value() == super::msix::MSIX_BAR_INDEX)
            .ok_or(PciError::InvalidMsix {
                detail: "MSI-X requires BAR 1 to be declared for the table/PBA window".into(),
            })?;
        if bar1.size() < super::msix::MSIX_BAR_SIZE {
            return Err(PciError::InvalidMsix {
                detail: alloc::format!(
                    "MSI-X requires BAR 1 of at least {:#x}, got {:#x}",
                    super::msix::MSIX_BAR_SIZE,
                    bar1.size()
                ),
            });
        }
        self.msix = Some(msix);
        Ok(self)
    }

    /// Returns the selected host key.
    pub const fn host(&self) -> &PciHostKey {
        &self.host
    }

    pub(crate) fn function_spec(&self, id: DeviceNodeId) -> PciResult<PciFunctionSpec> {
        let mut spec = PciFunctionSpec::new(id, self.identity).with_bdf(self.bdf);
        for bar in &self.bars {
            spec = spec.with_bar(bar.clone())?;
        }
        if let Some(msix) = self.msix {
            spec = spec.with_msix(msix);
        }
        Ok(spec)
    }
}

/// Architecture-owned description of one PCI host graph node.
pub struct PciHostProvider {
    pub(crate) key: PciHostKey,
    pub(crate) node: DeviceNodeSpec,
    pub(crate) memory_aperture_slot: ResourceSlot,
    pub(crate) platform_functions: Vec<PciFunctionSpec>,
    pub(crate) reserved_bdfs: Vec<PciBdf>,
}

impl PciHostProvider {
    /// Creates a provider backed by an ordinary graph node and MMIO slot.
    pub fn new(key: PciHostKey, node: DeviceNodeSpec, memory_aperture_slot: ResourceSlot) -> Self {
        Self {
            key,
            node,
            memory_aperture_slot,
            platform_functions: Vec::new(),
            reserved_bdfs: Vec::new(),
        }
    }

    /// Returns the stable key selected by endpoint requirements.
    pub const fn key(&self) -> &PciHostKey {
        &self.key
    }

    /// Adds one platform-owned fixed function.
    pub fn with_platform_function(mut self, function: PciFunctionSpec) -> PciResult<Self> {
        if self
            .platform_functions
            .iter()
            .any(|existing| existing.id() == function.id())
        {
            return Err(PciError::DuplicateFunction {
                function: function.id().to_string(),
            });
        }
        self.platform_functions.push(function);
        Ok(self)
    }

    /// Reserves one BDF from endpoint allocation.
    pub fn with_reserved_bdf(mut self, bdf: PciBdf) -> Self {
        self.reserved_bdfs.push(bdf);
        self
    }
}
