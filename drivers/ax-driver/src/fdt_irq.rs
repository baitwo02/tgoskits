use log::warn;
use rdrive::register::FdtInfo;

pub(crate) fn decode_fdt_irq(info: &FdtInfo<'_>) -> Option<usize> {
    let interrupt = info.interrupts().into_iter().next()?;
    decode_interrupt(info, &interrupt)
}

pub(crate) fn decode_interrupt(
    info: &FdtInfo<'_>,
    interrupt: &rdrive::probe::fdt::InterruptRef,
) -> Option<usize> {
    let parent = match info.phandle_to_device_id(interrupt.interrupt_parent) {
        Some(parent) => parent,
        None => {
            warn!(
                "failed to resolve IRQ parent phandle {} for {} interrupt {:?}",
                interrupt.interrupt_parent,
                info.node.name(),
                interrupt.name
            );
            return None;
        }
    };
    let intc = match rdrive::get::<rdif_intc::Intc>(parent) {
        Ok(intc) => intc,
        Err(err) => {
            warn!(
                "failed to get IRQ parent device {:?} for {} interrupt {:?}: {:?}",
                parent,
                info.node.name(),
                interrupt.name,
                err
            );
            return None;
        }
    };
    let mut intc = match intc.lock() {
        Ok(intc) => intc,
        Err(err) => {
            warn!(
                "failed to lock IRQ parent device {:?} for {} interrupt {:?}: {:?}",
                parent,
                info.node.name(),
                interrupt.name,
                err
            );
            return None;
        }
    };
    let irq: usize = intc.setup_irq_by_fdt(&interrupt.specifier).into();
    Some(irq)
}

#[cfg(test)]
mod tests {
    use rdif_intc::Interface;

    struct PlicLikeIntc;

    impl rdrive::DriverGeneric for PlicLikeIntc {
        fn name(&self) -> &str {
            "PLIC-like test interrupt controller"
        }
    }

    impl Interface for PlicLikeIntc {
        fn setup_irq_by_fdt(&mut self, irq_prop: &[u32]) -> rdrive::IrqId {
            (irq_prop.first().copied().unwrap_or(0) as usize).into()
        }
    }

    #[test]
    fn interrupt_controller_decodes_plic_two_cell_specifier() {
        let mut intc = rdif_intc::Intc::new(PlicLikeIntc);
        let irq: usize = intc.setup_irq_by_fdt(&[44, 4]).into();

        assert_eq!(irq, 44);
    }
}
