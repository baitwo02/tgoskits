extern crate alloc;

use alloc::boxed::Box;

use rd_net::{Interface, NetError};
use rdrive::{Device, DriverGeneric, IrqSource, probe::OnProbeError};

use crate::BindingInfo;

pub struct PlatformNetDevice {
    name: &'static str,
    info: BindingInfo,
    net: Option<rd_net::Net>,
}

impl PlatformNetDevice {
    fn new(name: &'static str, net: rd_net::Net, info: BindingInfo) -> Self {
        Self {
            name,
            info,
            net: Some(net),
        }
    }

    pub fn take_net(&mut self) -> Option<(rd_net::Net, &'static str, Option<IrqSource>)> {
        Some((self.net.take()?, self.name, self.info.irq_source().cloned()))
    }
}

pub fn take_rd_net_device(
    device: Device<PlatformNetDevice>,
) -> Result<(rd_net::Net, &'static str, Option<IrqSource>), NetError> {
    let mut dev = device
        .lock()
        .map_err(|_| NetError::Other(Box::new(rd_net::KError::Unknown("device locked"))))?;
    dev.take_net()
        .ok_or_else(|| NetError::Other(Box::new(rd_net::KError::Unknown("device already taken"))))
}

impl DriverGeneric for PlatformNetDevice {
    fn name(&self) -> &str {
        self.name
    }
}
pub trait PlatformDeviceNet {
    fn register_net<T>(self, name: &'static str, dev: T) -> Option<IrqSource>
    where
        T: Interface + 'static;

    fn register_net_with_irq<T>(
        self,
        name: &'static str,
        dev: T,
        irq_source: Option<IrqSource>,
    ) -> Option<IrqSource>
    where
        T: Interface + 'static;
}

impl PlatformDeviceNet for rdrive::PlatformDevice {
    fn register_net<T>(self, name: &'static str, dev: T) -> Option<IrqSource>
    where
        T: Interface + 'static,
    {
        register_net_with_info(self, name, dev, BindingInfo::empty())
    }

    fn register_net_with_irq<T>(
        self,
        name: &'static str,
        dev: T,
        irq_source: Option<IrqSource>,
    ) -> Option<IrqSource>
    where
        T: Interface + 'static,
    {
        register_net_with_info(self, name, dev, BindingInfo::with_irq_source(irq_source))
    }
}

pub trait ProbeFdtNet {
    fn register_net<T>(self, name: &'static str, dev: T) -> Option<IrqSource>
    where
        T: Interface + 'static;
}

impl ProbeFdtNet for rdrive::probe::fdt::ProbeFdt<'_> {
    fn register_net<T>(self, name: &'static str, dev: T) -> Option<IrqSource>
    where
        T: Interface + 'static,
    {
        let info = BindingInfo::from_fdt(self.info());
        register_net_with_info(self.into_platform_device(), name, dev, info)
    }
}

pub trait ProbePciNet {
    fn register_net_optional_irq<T>(self, name: &'static str, dev: T) -> Option<IrqSource>
    where
        T: Interface + 'static;

    fn register_net_required_irq<T>(
        self,
        name: &'static str,
        dev: T,
    ) -> Result<Option<IrqSource>, OnProbeError>
    where
        T: Interface + 'static;
}

impl ProbePciNet for rdrive::probe::pci::ProbePci<'_> {
    fn register_net_optional_irq<T>(self, name: &'static str, dev: T) -> Option<IrqSource>
    where
        T: Interface + 'static,
    {
        let info = BindingInfo::from_pci_optional(self.info());
        register_net_with_info(self.into_platform_device(), name, dev, info)
    }

    fn register_net_required_irq<T>(
        self,
        name: &'static str,
        dev: T,
    ) -> Result<Option<IrqSource>, OnProbeError>
    where
        T: Interface + 'static,
    {
        let info = BindingInfo::from_pci_required(self.info())?;
        Ok(register_net_with_info(
            self.into_platform_device(),
            name,
            dev,
            info,
        ))
    }
}

fn register_net_with_info<T>(
    plat_dev: rdrive::PlatformDevice,
    name: &'static str,
    dev: T,
    info: BindingInfo,
) -> Option<IrqSource>
where
    T: Interface + 'static,
{
    let irq_source = info.irq_source().cloned();
    let net = rd_net::Net::new(dev, axklib::dma::op());
    plat_dev.register(PlatformNetDevice::new(name, net, info));
    irq_source
}
