use alloc::format;

use rdrive::{
    IrqSource,
    probe::{OnProbeError, pci::PciInfo},
    register::FdtInfo,
};

#[derive(Clone, Debug, Default)]
pub struct BindingInfo {
    irq_source: Option<IrqSource>,
}

impl BindingInfo {
    pub const fn empty() -> Self {
        Self { irq_source: None }
    }

    pub fn from_fdt(info: &FdtInfo<'_>) -> Self {
        Self::with_irq_source(rdrive::first_fdt_irq_source(info))
    }

    pub fn from_pci_optional(info: PciInfo) -> Self {
        Self::with_irq_source(pci_irq_source(info))
    }

    pub fn from_pci_required(info: PciInfo) -> Result<Self, OnProbeError> {
        Ok(Self::with_irq_source(Some(
            pci_irq_source(info).ok_or_else(|| {
                OnProbeError::other(format!(
                    "failed to resolve IRQ for PCI endpoint {}",
                    info.address
                ))
            })?,
        )))
    }

    pub const fn with_irq_source(irq_source: Option<IrqSource>) -> Self {
        Self { irq_source }
    }

    pub fn irq_source(&self) -> Option<&IrqSource> {
        self.irq_source.as_ref()
    }

    pub fn irq_num(&self) -> Option<usize> {
        match self.irq_source.as_ref() {
            Some(IrqSource::Number(irq)) => Some(*irq),
            _ => None,
        }
    }
}

fn pci_irq_source(info: PciInfo) -> Option<IrqSource> {
    #[cfg(all(plat_dyn, target_os = "none"))]
    {
        if info.interrupt_pin != 0 {
            match crate::pci::acpi_irq_for_endpoint(info.address, info.interrupt_pin) {
                Ok(Some(irq)) => return Some(IrqSource::Number(irq)),
                Ok(None) => {}
                Err(err) => log::warn!(
                    "failed to resolve ACPI IRQ for PCI endpoint {}: {err}",
                    info.address
                ),
            }
        }
    }

    #[cfg(all(plat_dyn, target_os = "none"))]
    {
        if info.interrupt_pin != 0 {
            match crate::pci::fdt_irq_for_endpoint(info.address, info.interrupt_pin) {
                Ok(Some(irq)) => return Some(IrqSource::Number(irq)),
                Ok(None) => {}
                Err(err) => log::warn!(
                    "failed to resolve FDT IRQ for PCI endpoint {}: {err}",
                    info.address
                ),
            }
        }
    }

    if let Some(irq) = crate::pci::legacy_irq_for_endpoint(info.address, info.interrupt_pin) {
        return Some(IrqSource::Number(irq));
    }

    if info.interrupt_line == 0 || info.interrupt_line == u8::MAX {
        return None;
    }
    Some(IrqSource::Number(crate::pci::legacy_line_to_irq(
        info.interrupt_line,
    )))
}
