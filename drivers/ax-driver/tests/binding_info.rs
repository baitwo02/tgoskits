use ax_driver::BindingInfo;
use rdrive::{
    IrqSource,
    probe::pci::{PciAddress, PciInfo},
};
#[cfg(feature = "plat-dyn")]
use {
    axklib::{
        AxError, AxResult, IrqCpuMask, IrqHandle, Klib, PhysAddr, RawIrqHandler, VirtAddr,
        impl_trait,
    },
    core::time::Duration,
    fdt_edit::{Fdt, Node, Phandle, Property},
    rdrive::{
        Platform,
        probe::OnProbeError,
        register::{DriverRegister, ProbeFdt, ProbeKind, ProbeLevel, ProbePriority},
    },
    std::ptr::NonNull,
    std::sync::Mutex,
};

#[cfg(feature = "plat-dyn")]
static CAPTURED_INFO: Mutex<Option<BindingInfo>> = Mutex::new(None);

#[cfg(feature = "plat-dyn")]
static TEST_PROBE_KINDS: &[ProbeKind] = &[ProbeKind::Fdt {
    compatibles: &["test,binding-info"],
    on_probe: capture_binding_info,
}];

#[cfg(feature = "plat-dyn")]
struct KlibImpl;

#[cfg(feature = "plat-dyn")]
impl_trait! {
    impl Klib for KlibImpl {
        fn mem_iomap(_addr: PhysAddr, _size: usize) -> AxResult<VirtAddr> {
            Err(AxError::Unsupported)
        }

        fn mem_virt_to_phys(addr: VirtAddr) -> PhysAddr {
            PhysAddr::from_usize(addr.as_usize())
        }

        fn mem_make_dma_coherent_uncached(_addr: VirtAddr, _size: usize) -> AxResult {
            Err(AxError::Unsupported)
        }

        fn mem_restore_dma_cached(_addr: VirtAddr, _size: usize) -> AxResult {
            Err(AxError::Unsupported)
        }

        fn dma_alloc_pages(_dma_mask: u64, _num_pages: usize, _align: usize) -> AxResult<VirtAddr> {
            Err(AxError::Unsupported)
        }

        fn dma_dealloc_pages(_addr: VirtAddr, _num_pages: usize) {}

        fn time_busy_wait(_dur: Duration) {}

        fn time_monotonic_nanos() -> u64 {
            0
        }

        fn time_try_init_epoch_offset(_epoch_time_nanos: u64) -> bool {
            false
        }

        fn irq_set_enable(_irq: usize, _enabled: bool) {}

        fn irq_request_shared(
            _irq: usize,
            _handler: RawIrqHandler,
            _data: core::ptr::NonNull<()>,
        ) -> AxResult<IrqHandle> {
            Err(AxError::Unsupported)
        }

        fn irq_request_percpu(
            _irq: usize,
            _cpus: IrqCpuMask,
            _handler: RawIrqHandler,
            _data: core::ptr::NonNull<()>,
        ) -> AxResult<IrqHandle> {
            Err(AxError::Unsupported)
        }

        fn irq_free(_handle: IrqHandle) -> AxResult {
            Err(AxError::Unsupported)
        }

        fn irq_enable(_handle: IrqHandle) -> AxResult {
            Err(AxError::Unsupported)
        }

        fn irq_disable(_handle: IrqHandle) -> AxResult {
            Err(AxError::Unsupported)
        }
    }
}

#[test]
fn empty_binding_info_has_no_irq() {
    let info = BindingInfo::empty();

    assert_eq!(info.irq_source(), None);
    assert_eq!(info.irq_num(), None);
}

#[test]
fn explicit_binding_info_reports_numbered_irq() {
    let info = BindingInfo::with_irq_source(Some(IrqSource::Number(33)));

    assert_eq!(info.irq_source(), Some(&IrqSource::Number(33)));
    assert_eq!(info.irq_num(), Some(33));
}

#[test]
fn optional_pci_binding_info_can_be_empty() {
    let info = BindingInfo::from_pci_optional(PciInfo {
        address: PciAddress::new(0, 0, 0, 0),
        interrupt_pin: 0,
        interrupt_line: 0,
    });

    assert_eq!(info.irq_source(), None);
    assert_eq!(info.irq_num(), None);
}

#[test]
fn required_pci_binding_info_reports_unresolved_irq() {
    let err = BindingInfo::from_pci_required(PciInfo {
        address: PciAddress::new(0, 0, 0, 0),
        interrupt_pin: 0,
        interrupt_line: 0,
    })
    .unwrap_err();

    assert!(err.to_string().contains("failed to resolve IRQ"));
}

#[cfg(feature = "plat-dyn")]
#[test]
fn fdt_binding_info_uses_first_fdt_irq_source() {
    *CAPTURED_INFO.lock().unwrap() = None;

    let fdt_data = Box::leak(Box::new(minimal_irq_fdt().encode()));
    let fdt_addr = NonNull::new(fdt_data.as_ref().as_ptr() as *mut u8).unwrap();

    rdrive::init(Platform::Fdt { addr: fdt_addr }).unwrap();
    rdrive::register_add(DriverRegister {
        name: "binding-info-fdt-test",
        level: ProbeLevel::PostKernel,
        priority: ProbePriority::DEFAULT,
        probe_kinds: TEST_PROBE_KINDS,
    });
    rdrive::probe_all(true).unwrap();

    let info = CAPTURED_INFO.lock().unwrap().take().unwrap();
    let Some(IrqSource::Fdt(source)) = info.irq_source() else {
        panic!("expected FDT IRQ source");
    };

    assert_eq!(info.irq_num(), None);
    assert_eq!(source.parent_phandle, Phandle::from(1));
    assert_eq!(source.cells, 3);
    assert_eq!(source.specifier, [0, 42, 4]);
    assert_eq!(source.name.as_deref(), Some("main"));
    assert_eq!(source.node_path.as_deref(), Some("/device@0"));
}

#[cfg(feature = "plat-dyn")]
fn capture_binding_info(probe: ProbeFdt<'_>) -> Result<(), OnProbeError> {
    *CAPTURED_INFO.lock().unwrap() = Some(BindingInfo::from_fdt(probe.info()));
    Ok(())
}

#[cfg(feature = "plat-dyn")]
fn minimal_irq_fdt() -> Fdt {
    let mut fdt = Fdt::new();
    let root = fdt.root_id();
    fdt.node_mut(root)
        .unwrap()
        .set_property(prop_u32s("#address-cells", &[1]));
    fdt.node_mut(root)
        .unwrap()
        .set_property(prop_u32s("#size-cells", &[1]));

    let intc = fdt.add_node(root, Node::new("interrupt-controller@0"));
    fdt.node_mut(intc)
        .unwrap()
        .set_property(prop_u32s("phandle", &[1]));
    fdt.node_mut(intc)
        .unwrap()
        .set_property(Property::new("interrupt-controller", Vec::new()));
    fdt.node_mut(intc)
        .unwrap()
        .set_property(prop_u32s("#interrupt-cells", &[3]));

    let dev = fdt.add_node(root, Node::new("device@0"));
    fdt.node_mut(dev).unwrap().set_property(prop_strs(
        "compatible",
        &["test,binding-info", "test,binding-info-fallback"],
    ));
    fdt.node_mut(dev)
        .unwrap()
        .set_property(prop_u32s("interrupt-parent", &[1]));
    fdt.node_mut(dev)
        .unwrap()
        .set_property(prop_u32s("interrupts", &[0, 42, 4, 0, 43, 4]));
    fdt.node_mut(dev)
        .unwrap()
        .set_property(prop_strs("interrupt-names", &["main", "backup"]));

    fdt
}

#[cfg(feature = "plat-dyn")]
fn prop_u32s(name: &str, values: &[u32]) -> Property {
    let mut data = Vec::new();
    for value in values {
        data.extend_from_slice(&value.to_be_bytes());
    }
    Property::new(name, data)
}

#[cfg(feature = "plat-dyn")]
fn prop_strs(name: &str, values: &[&str]) -> Property {
    let mut data = Vec::new();
    for value in values {
        data.extend_from_slice(value.as_bytes());
        data.push(0);
    }
    Property::new(name, data)
}
