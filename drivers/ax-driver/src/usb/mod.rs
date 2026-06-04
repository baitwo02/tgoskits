extern crate alloc;

#[cfg(target_os = "none")]
use core::time::Duration;

#[cfg(target_os = "none")]
use crab_usb::USBHost;
#[cfg(target_os = "none")]
use dma_api::{DmaAllocHandle, DmaConstraints, DmaDirection, DmaError, DmaMapHandle, DmaOp};
use rdrive::{DriverGeneric, IrqSource, probe::OnProbeError};

use crate::BindingInfo;

#[cfg(all(feature = "rockchip-dwc-xhci", target_os = "none"))]
mod dwc;
#[cfg(all(feature = "xhci-mmio", target_os = "none"))]
mod xhci_mmio;
#[cfg(all(feature = "xhci-pci", target_os = "none"))]
mod xhci_pci;

pub type UsbHostDevice = rdrive::Device<PlatformUsbHost>;
pub type UsbHostDeviceGuard = rdrive::DeviceGuard<PlatformUsbHost>;

#[cfg(target_os = "none")]
struct UsbKernel;

#[cfg(target_os = "none")]
impl DmaOp for UsbKernel {
    fn page_size(&self) -> usize {
        axklib::dma::op().page_size()
    }

    unsafe fn alloc_contiguous(
        &self,
        constraints: DmaConstraints,
        layout: core::alloc::Layout,
    ) -> Option<DmaAllocHandle> {
        unsafe { axklib::dma::op().alloc_contiguous(constraints, layout) }
    }

    unsafe fn dealloc_contiguous(&self, handle: DmaAllocHandle) {
        unsafe { axklib::dma::op().dealloc_contiguous(handle) }
    }

    unsafe fn alloc_coherent(
        &self,
        constraints: DmaConstraints,
        layout: core::alloc::Layout,
    ) -> Option<DmaAllocHandle> {
        unsafe { axklib::dma::op().alloc_coherent(constraints, layout) }
    }

    unsafe fn dealloc_coherent(&self, handle: DmaAllocHandle) {
        unsafe { axklib::dma::op().dealloc_coherent(handle) }
    }

    unsafe fn map_streaming(
        &self,
        constraints: DmaConstraints,
        addr: core::ptr::NonNull<u8>,
        size: core::num::NonZeroUsize,
        direction: DmaDirection,
    ) -> Result<DmaMapHandle, DmaError> {
        unsafe { axklib::dma::op().map_streaming(constraints, addr, size, direction) }
    }

    unsafe fn unmap_streaming(&self, handle: DmaMapHandle) {
        unsafe { axklib::dma::op().unmap_streaming(handle) }
    }

    fn flush(&self, addr: core::ptr::NonNull<u8>, size: usize) {
        axklib::dma::op().flush(addr, size);
    }

    fn invalidate(&self, addr: core::ptr::NonNull<u8>, size: usize) {
        axklib::dma::op().invalidate(addr, size);
    }

    fn flush_invalidate(&self, addr: core::ptr::NonNull<u8>, size: usize) {
        axklib::dma::op().flush_invalidate(addr, size);
    }

    fn sync_alloc_for_device(
        &self,
        handle: &DmaAllocHandle,
        offset: usize,
        size: usize,
        direction: DmaDirection,
    ) {
        axklib::dma::op().sync_alloc_for_device(handle, offset, size, direction);
    }

    fn sync_alloc_for_cpu(
        &self,
        handle: &DmaAllocHandle,
        offset: usize,
        size: usize,
        direction: DmaDirection,
    ) {
        axklib::dma::op().sync_alloc_for_cpu(handle, offset, size, direction);
    }

    fn sync_map_for_device(
        &self,
        handle: &DmaMapHandle,
        offset: usize,
        size: usize,
        direction: DmaDirection,
    ) {
        axklib::dma::op().sync_map_for_device(handle, offset, size, direction);
    }

    fn sync_map_for_cpu(
        &self,
        handle: &DmaMapHandle,
        offset: usize,
        size: usize,
        direction: DmaDirection,
    ) {
        axklib::dma::op().sync_map_for_cpu(handle, offset, size, direction);
    }
}

#[cfg(target_os = "none")]
impl crab_usb::KernelOp for UsbKernel {
    fn delay(&self, duration: Duration) {
        axklib::time::busy_wait(duration);
    }
}

#[cfg(target_os = "none")]
static USB_KERNEL: UsbKernel = UsbKernel;

#[cfg(target_os = "none")]
pub fn usb_kernel() -> &'static dyn crab_usb::KernelOp {
    &USB_KERNEL
}

#[cfg(target_os = "none")]
pub struct PlatformUsbHost {
    name: &'static str,
    info: BindingInfo,
    host: USBHost,
}

#[cfg(not(target_os = "none"))]
pub struct PlatformUsbHost {
    name: &'static str,
    info: BindingInfo,
}

impl PlatformUsbHost {
    #[cfg(target_os = "none")]
    fn new(name: &'static str, host: USBHost, info: BindingInfo) -> Self {
        Self { name, info, host }
    }

    #[cfg(not(target_os = "none"))]
    fn new_stub(name: &'static str, info: BindingInfo) -> Self {
        Self { name, info }
    }

    #[cfg(target_os = "none")]
    pub fn host(&self) -> &USBHost {
        &self.host
    }

    #[cfg(target_os = "none")]
    pub fn host_mut(&mut self) -> &mut USBHost {
        &mut self.host
    }

    pub fn irq_num(&self) -> Option<usize> {
        self.info.irq_num()
    }

    pub fn irq_source(&self) -> Option<&IrqSource> {
        self.info.irq_source()
    }

    pub fn info(&self) -> &BindingInfo {
        &self.info
    }
}

impl DriverGeneric for PlatformUsbHost {
    fn name(&self) -> &str {
        self.name
    }
}

pub trait PlatformDeviceUsbHost {
    #[cfg(target_os = "none")]
    fn register_usb_host(self, name: &'static str, host: USBHost) -> Option<IrqSource>;

    #[cfg(target_os = "none")]
    fn register_usb_host_with_irq(
        self,
        name: &'static str,
        host: USBHost,
        irq_source: Option<IrqSource>,
    ) -> Option<IrqSource>;

    #[cfg(not(target_os = "none"))]
    fn register_usb_host_stub(self, name: &'static str) -> Option<IrqSource>;

    #[cfg(not(target_os = "none"))]
    fn register_usb_host_stub_with_irq(
        self,
        name: &'static str,
        irq_source: Option<IrqSource>,
    ) -> Option<IrqSource>;
}

impl PlatformDeviceUsbHost for rdrive::PlatformDevice {
    #[cfg(target_os = "none")]
    fn register_usb_host(self, name: &'static str, host: USBHost) -> Option<IrqSource> {
        register_usb_host_with_info(self, name, host, BindingInfo::empty())
    }

    #[cfg(target_os = "none")]
    fn register_usb_host_with_irq(
        self,
        name: &'static str,
        host: USBHost,
        irq_source: Option<IrqSource>,
    ) -> Option<IrqSource> {
        register_usb_host_with_info(self, name, host, BindingInfo::with_irq_source(irq_source))
    }

    #[cfg(not(target_os = "none"))]
    fn register_usb_host_stub(self, name: &'static str) -> Option<IrqSource> {
        register_usb_host_stub_with_info(self, name, BindingInfo::empty())
    }

    #[cfg(not(target_os = "none"))]
    fn register_usb_host_stub_with_irq(
        self,
        name: &'static str,
        irq_source: Option<IrqSource>,
    ) -> Option<IrqSource> {
        register_usb_host_stub_with_info(self, name, BindingInfo::with_irq_source(irq_source))
    }
}

pub trait ProbeFdtUsbHost {
    #[cfg(target_os = "none")]
    fn register_usb_host(self, name: &'static str, host: USBHost) -> Option<IrqSource>;

    #[cfg(not(target_os = "none"))]
    fn register_usb_host_stub(self, name: &'static str) -> Option<IrqSource>;
}

impl ProbeFdtUsbHost for rdrive::probe::fdt::ProbeFdt<'_> {
    #[cfg(target_os = "none")]
    fn register_usb_host(self, name: &'static str, host: USBHost) -> Option<IrqSource> {
        let info = BindingInfo::from_fdt(self.info());
        register_usb_host_with_info(self.into_platform_device(), name, host, info)
    }

    #[cfg(not(target_os = "none"))]
    fn register_usb_host_stub(self, name: &'static str) -> Option<IrqSource> {
        let info = BindingInfo::from_fdt(self.info());
        register_usb_host_stub_with_info(self.into_platform_device(), name, info)
    }
}

pub trait ProbePciUsbHost {
    #[cfg(target_os = "none")]
    fn register_usb_host_optional_irq(self, name: &'static str, host: USBHost)
    -> Option<IrqSource>;

    #[cfg(target_os = "none")]
    fn register_usb_host_required_irq(
        self,
        name: &'static str,
        host: USBHost,
    ) -> Result<Option<IrqSource>, OnProbeError>;

    #[cfg(not(target_os = "none"))]
    fn register_usb_host_stub_optional_irq(self, name: &'static str) -> Option<IrqSource>;

    #[cfg(not(target_os = "none"))]
    fn register_usb_host_stub_required_irq(
        self,
        name: &'static str,
    ) -> Result<Option<IrqSource>, OnProbeError>;
}

impl ProbePciUsbHost for rdrive::probe::pci::ProbePci<'_> {
    #[cfg(target_os = "none")]
    fn register_usb_host_optional_irq(
        self,
        name: &'static str,
        host: USBHost,
    ) -> Option<IrqSource> {
        let info = BindingInfo::from_pci_optional(self.info());
        register_usb_host_with_info(self.into_platform_device(), name, host, info)
    }

    #[cfg(target_os = "none")]
    fn register_usb_host_required_irq(
        self,
        name: &'static str,
        host: USBHost,
    ) -> Result<Option<IrqSource>, OnProbeError> {
        let info = BindingInfo::from_pci_required(self.info())?;
        Ok(register_usb_host_with_info(
            self.into_platform_device(),
            name,
            host,
            info,
        ))
    }

    #[cfg(not(target_os = "none"))]
    fn register_usb_host_stub_optional_irq(self, name: &'static str) -> Option<IrqSource> {
        let info = BindingInfo::from_pci_optional(self.info());
        register_usb_host_stub_with_info(self.into_platform_device(), name, info)
    }

    #[cfg(not(target_os = "none"))]
    fn register_usb_host_stub_required_irq(
        self,
        name: &'static str,
    ) -> Result<Option<IrqSource>, OnProbeError> {
        let info = BindingInfo::from_pci_required(self.info())?;
        Ok(register_usb_host_stub_with_info(
            self.into_platform_device(),
            name,
            info,
        ))
    }
}

#[cfg(target_os = "none")]
fn register_usb_host_with_info(
    plat_dev: rdrive::PlatformDevice,
    name: &'static str,
    host: USBHost,
    info: BindingInfo,
) -> Option<IrqSource> {
    let irq_source = info.irq_source().cloned();
    plat_dev.register(PlatformUsbHost::new(name, host, info));
    irq_source
}

#[cfg(not(target_os = "none"))]
fn register_usb_host_stub_with_info(
    plat_dev: rdrive::PlatformDevice,
    name: &'static str,
    info: BindingInfo,
) -> Option<IrqSource> {
    let irq_source = info.irq_source().cloned();
    plat_dev.register(PlatformUsbHost::new_stub(name, info));
    irq_source
}

#[cfg(all(feature = "xhci-pci", target_os = "none"))]
pub(crate) fn align_up_4k(size: usize) -> usize {
    const MASK: usize = 0xfff;
    (size + MASK) & !MASK
}

pub fn usb_host_device() -> Option<UsbHostDevice> {
    rdrive::get_one()
}
