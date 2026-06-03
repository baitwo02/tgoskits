use core::ptr::NonNull;

use fdt_edit::{Fdt, FdtEncoder, Node, Property};
use rdrive::{
    DriverGeneric, Platform, PlatformDevice, get_list,
    probe::OnProbeError,
    register::{DriverRegister, FdtInfo, ProbeKind, ProbeLevel, ProbePriority},
};

struct UartProbeDevice;

impl DriverGeneric for UartProbeDevice {
    fn name(&self) -> &str {
        "UartProbeDevice"
    }
}

fn probe_uart(_info: FdtInfo<'_>, plat_dev: PlatformDevice) -> Result<(), OnProbeError> {
    plat_dev.register(UartProbeDevice);
    Ok(())
}

static UART_REGISTER: DriverRegister = DriverRegister {
    name: "multi uart test driver",
    level: ProbeLevel::PostKernel,
    priority: ProbePriority::DEFAULT,
    probe_kinds: &[ProbeKind::Fdt {
        compatibles: &["test,uart"],
        on_probe: probe_uart,
    }],
};

#[test]
fn fdt_driver_register_probes_every_matching_node() {
    let fdt = two_uart_fdt();
    let mut bytes = FdtEncoder::new(&fdt).encode().as_ref().to_vec();
    let ptr = NonNull::new(bytes.as_mut_ptr()).expect("fdt bytes must be non-null");

    rdrive::init(Platform::Fdt { addr: ptr }).expect("fdt platform should init");
    rdrive::register_add(UART_REGISTER.clone());
    rdrive::probe_all(true).expect("fdt probe should succeed");

    assert_eq!(get_list::<UartProbeDevice>().len(), 2);
}

fn two_uart_fdt() -> Fdt {
    let mut fdt = Fdt::new();
    let root = fdt.root_id();
    fdt.node_mut(root)
        .unwrap()
        .set_property(prop_u32_ls("#address-cells", &[2]));
    fdt.node_mut(root)
        .unwrap()
        .set_property(prop_u32_ls("#size-cells", &[1]));

    add_uart(&mut fdt, root, "uart@1000", 1, 0x1000);
    add_uart(&mut fdt, root, "uart@2000", 2, 0x2000);

    fdt
}

fn add_uart(fdt: &mut Fdt, parent: fdt_edit::NodeId, name: &str, phandle: u32, addr: u32) {
    let node = fdt.add_node(parent, Node::new(name));
    fdt.node_mut(node)
        .unwrap()
        .set_property(prop_strs("compatible", &["test,uart"]));
    fdt.node_mut(node)
        .unwrap()
        .set_property(prop_u32_ls("phandle", &[phandle]));
    fdt.node_mut(node)
        .unwrap()
        .set_property(prop_u32_ls("reg", &[0, addr, 0x100]));
    fdt.node_mut(node)
        .unwrap()
        .set_property(prop_str("status", "okay"));
}

fn prop_u32_ls(name: &str, values: &[u32]) -> Property {
    let mut bytes = Vec::with_capacity(values.len() * 4);
    for value in values {
        bytes.extend_from_slice(&value.to_be_bytes());
    }
    Property::new(name, bytes)
}

fn prop_str(name: &str, value: &str) -> Property {
    let mut bytes = Vec::from(value.as_bytes());
    bytes.push(0);
    Property::new(name, bytes)
}

fn prop_strs(name: &str, values: &[&str]) -> Property {
    let mut bytes = Vec::new();
    for value in values {
        bytes.extend_from_slice(value.as_bytes());
        bytes.push(0);
    }
    Property::new(name, bytes)
}
