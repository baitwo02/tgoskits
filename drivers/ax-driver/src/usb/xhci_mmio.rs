extern crate alloc;

use alloc::format;

use log::info;
use rdrive::{PlatformDevice, probe::OnProbeError, register::FdtInfo};

use super::{PlatformDeviceUsbHost, fdt_irq_source, usb_kernel};

const DRIVER_NAME: &str = "usb-xhci-mmio";

crate::model_register!(
    name: "USB xHCI MMIO",
    level: ProbeLevel::PostKernel,
    priority: ProbePriority::DEFAULT,
    probe_kinds: &[ProbeKind::Fdt {
        compatibles: &["generic-xhci", "xhci-platform"],
        on_probe: probe
    }],
);

fn probe(info: FdtInfo<'_>, plat_dev: PlatformDevice) -> Result<(), OnProbeError> {
    let base_reg =
        info.node.regs().into_iter().next().ok_or_else(|| {
            OnProbeError::other(alloc::format!("[{}] has no reg", info.node.name()))
        })?;

    let mmio_size = base_reg.size.unwrap_or(0x1000) as usize;
    let mmio = crate::mmio::iomap(base_reg.address as usize, mmio_size)?;
    let irq_source = fdt_irq_source(&info);

    let host = crab_usb::USBHost::new_xhci(mmio, usb_kernel()).map_err(|err| {
        OnProbeError::other(format!(
            "failed to create xHCI host for [{}]: {err}",
            info.node.name()
        ))
    })?;

    plat_dev.register_usb_host(DRIVER_NAME, host, irq_source.clone());
    info!(
        "xHCI MMIO host registered successfully for {} with irq {:?}",
        info.node.name(),
        irq_source
    );
    Ok(())
}
