use alloc::{string::String, vec::Vec};

use crate::{DeviceId, Phandle, probe::fdt::InterruptRef, register::FdtInfo};

/// Owned description of where an IRQ comes from.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum IrqSource {
    /// Already-resolved platform IRQ number.
    Number(usize),
    /// IRQ described by an FDT interrupt reference.
    Fdt(FdtIrqSource),
}

impl IrqSource {
    pub const fn number(irq: usize) -> Self {
        Self::Number(irq)
    }
}

impl From<usize> for IrqSource {
    fn from(value: usize) -> Self {
        Self::Number(value)
    }
}

/// Owned FDT IRQ reference that can outlive probe callbacks.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct FdtIrqSource {
    pub interrupt_parent: DeviceId,
    pub parent_phandle: Phandle,
    pub cells: u32,
    pub specifier: Vec<u32>,
    pub name: Option<String>,
    pub node_path: Option<String>,
}

impl FdtIrqSource {
    pub fn new(
        interrupt_parent: DeviceId,
        parent_phandle: Phandle,
        cells: u32,
        specifier: Vec<u32>,
        name: Option<String>,
        node_path: Option<String>,
    ) -> Self {
        Self {
            interrupt_parent,
            parent_phandle,
            cells,
            specifier,
            name,
            node_path,
        }
    }

    pub fn from_interrupt(info: &FdtInfo<'_>, interrupt: InterruptRef) -> Option<Self> {
        let interrupt_parent = info.phandle_to_device_id(interrupt.interrupt_parent)?;
        Some(Self::new(
            interrupt_parent,
            interrupt.interrupt_parent,
            interrupt.cells,
            interrupt.specifier,
            interrupt.name,
            Some(info.node.path()),
        ))
    }
}

pub fn fdt_irq_source(info: &FdtInfo<'_>, interrupt: InterruptRef) -> Option<IrqSource> {
    FdtIrqSource::from_interrupt(info, interrupt).map(IrqSource::Fdt)
}

pub fn first_fdt_irq_source(info: &FdtInfo<'_>) -> Option<IrqSource> {
    fdt_irq_source(info, info.interrupts().into_iter().next()?)
}

pub fn named_fdt_irq_source(info: &FdtInfo<'_>, name: &str) -> Option<IrqSource> {
    let interrupt = info
        .interrupts()
        .into_iter()
        .find(|interrupt| interrupt.name.as_deref() == Some(name))?;
    fdt_irq_source(info, interrupt)
}

impl core::fmt::Display for IrqSource {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Number(irq) => write!(f, "irq#{irq}"),
            Self::Fdt(source) => {
                if let Some(path) = &source.node_path {
                    write!(f, "fdt:{path}")?;
                } else {
                    write!(f, "fdt:phandle{:?}", source.parent_phandle)?;
                }
                if let Some(name) = &source.name {
                    write!(f, ":{name}")?;
                }
                write!(
                    f,
                    " parent={:?} spec={:?}",
                    source.interrupt_parent, source.specifier
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString;

    use super::*;

    #[test]
    fn number_source_is_displayable() {
        assert_eq!(IrqSource::number(44).to_string(), "irq#44");
    }

    #[test]
    fn fdt_source_keeps_parent_and_specifier() {
        let source = FdtIrqSource::new(
            DeviceId::from(3),
            Phandle::from(7),
            2,
            vec![44, 4],
            Some(String::from("serial")),
            Some(String::from("/serial@04140000")),
        );

        assert_eq!(source.interrupt_parent, DeviceId::from(3));
        assert_eq!(source.specifier, vec![44, 4]);
    }
}
