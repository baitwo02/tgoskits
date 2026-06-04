use rdrive::{IrqSource, register::FdtInfo};

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
