use core::{cell::UnsafeCell, fmt::Write, ptr::NonNull};

use byte_unit::{Byte, UnitType};
use kernutil::memory::{MemoryDescriptor, MemoryType};
#[cfg(target_arch = "x86_64")]
use some_serial::ns16550::Port;
use some_serial::{
    ns16550::{self, Mmio, Ns16550RxQueue, Ns16550TxQueue},
    pl011::{self, Pl011RxQueue, Pl011TxQueue},
};

use crate::{
    cmdline::EarlyconConfig,
    mem::{_fixmap_io, page_size},
};

pub(crate) static mut DEBUG_BASE: usize = 0;
pub(crate) static mut DEBUG_IS_MMIO: bool = false;

pub trait ArchConsoleOps {
    fn init() -> bool {
        false
    }

    fn read_byte() -> Option<u8> {
        None
    }
}

pub(crate) fn debug_to_memory_desc() -> Option<MemoryDescriptor> {
    let debug_base = unsafe { DEBUG_BASE };
    let debug_is_mmio = unsafe { DEBUG_IS_MMIO };
    if debug_base == 0 || !debug_is_mmio {
        return None;
    }

    Some(MemoryDescriptor::new_aligned(
        debug_base,
        100,
        MemoryType::Mmio,
        page_size(),
    ))
}

pub fn _print(args: core::fmt::Arguments) {
    let _ = ConFmt {}.write_fmt(args);
}

pub fn _write_bytes(bytes: &[u8]) -> usize {
    con().write_bytes(bytes)
}

pub fn _write_str(s: &str) {
    con().write_str(s);
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => ($crate::console::_print(core::format_args!($($arg)*)));
}

#[macro_export]
macro_rules! println {
    () => ($crate::print!("\n"));
    ($($arg:tt)*) => ($crate::console::_print(core::format_args!("{}{}", core::format_args!($($arg)*), "\n")));
}

#[macro_export]
macro_rules! pr_range {
    ($name:expr, $b:expr, $s:expr) => {
        $crate::println!(
            "{:<20}: [0x{:0>16x}, 0x{:0>16x}) ({:>5} Mb)",
            $name,
            $b,
            $b + $s,
            ($s) / 1024 / 1024
        );
    };
    ($name:expr, $b:expr, $s:expr, $($arg:tt)*) => {
        $crate::println!(
            "{:<20}: [0x{:0>16x}, 0x{:0>16x}) ({:>5} Mb) {}",
            $name,
            $b,
            $b + $s,
            ($s) / 1024 / 1024,
            core::format_args!($($arg)*)
        );
    };
}

pub fn print_mapping(name: &str, virt: usize, phys: usize, size: usize) {
    let fmt = Byte::from(size).get_appropriate_unit(UnitType::Binary);
    println!(
        "{:<20}: [0x{:0>16x}, 0x{:0>16x}) -> [0x{:0>16x}, 0x{:0>16x}) ({:#.2})",
        name,
        virt,
        virt + size,
        phys,
        phys + size,
        fmt
    );
}

#[allow(dead_code)]
struct ConFmt {}

impl Write for ConFmt {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let mut remaining = s;
        while let Some(pos) = remaining.find('\n') {
            // 打印 '\n' 之前的部分
            con().write_str(&remaining[..pos]);
            // 打印 "\r\n"
            con().write_str("\r\n");
            // 继续处理剩余部分
            remaining = &remaining[pos + 1..];
        }
        // 打印最后剩余的部分（如果有的话）
        if !remaining.is_empty() {
            con().write_str(remaining);
        }
        Ok(())
    }
}

fn con() -> &'static dyn Con {
    unsafe { CON }
}

pub(crate) trait Con: Send + Sync {
    fn write_bytes(&self, _bytes: &[u8]) -> usize {
        _bytes.len()
    }
    fn write_str(&self, s: &str) {
        let bytes = s.as_bytes();
        let mut buff = bytes;
        while !buff.is_empty() {
            let n = self.write_bytes(buff);
            buff = &buff[n..];
        }
    }
}

#[allow(dead_code)]
struct NoCon;
impl Con for NoCon {
    fn write_bytes(&self, _bytes: &[u8]) -> usize {
        _bytes.len()
    }
    fn write_str(&self, _s: &str) {
        // Do nothing
    }
}

static mut CON: &dyn Con = &NoCon;

pub(crate) unsafe fn set_out(v: &'static dyn Con) {
    unsafe {
        CON = v;
    }
}

pub enum EarlySerialTx {
    Ns16550Mmio(Ns16550TxQueue<Mmio>),
    #[cfg(target_arch = "x86_64")]
    Ns16550Port(Ns16550TxQueue<Port>),
    Pl011(Pl011TxQueue),
}

impl EarlySerialTx {
    pub fn poll(&mut self) -> some_serial::SerialEvent {
        match self {
            Self::Ns16550Mmio(tx) => tx.poll(),
            #[cfg(target_arch = "x86_64")]
            Self::Ns16550Port(tx) => tx.poll(),
            Self::Pl011(tx) => tx.poll(),
        }
    }

    pub fn submit_tx(&mut self, bytes: &[u8]) -> usize {
        match self {
            Self::Ns16550Mmio(tx) => tx.submit_tx(bytes),
            #[cfg(target_arch = "x86_64")]
            Self::Ns16550Port(tx) => tx.submit_tx(bytes),
            Self::Pl011(tx) => tx.submit_tx(bytes),
        }
    }
}

pub enum EarlySerialRx {
    Ns16550Mmio(Ns16550RxQueue<Mmio>),
    #[cfg(target_arch = "x86_64")]
    Ns16550Port(Ns16550RxQueue<Port>),
    Pl011(Pl011RxQueue),
}

impl EarlySerialRx {
    pub fn poll(&mut self) -> some_serial::SerialEvent {
        match self {
            Self::Ns16550Mmio(rx) => rx.poll(),
            #[cfg(target_arch = "x86_64")]
            Self::Ns16550Port(rx) => rx.poll(),
            Self::Pl011(rx) => rx.poll(),
        }
    }

    pub fn submit_rx(&mut self, bytes: &mut [u8]) -> Result<usize, some_serial::TransBytesError> {
        match self {
            Self::Ns16550Mmio(rx) => rx.submit_rx(bytes),
            #[cfg(target_arch = "x86_64")]
            Self::Ns16550Port(rx) => rx.submit_rx(bytes),
            Self::Pl011(rx) => rx.submit_rx(bytes),
        }
    }
}

pub fn set_earlycon_serial(tx: EarlySerialTx, rx: EarlySerialRx) {
    unsafe {
        *EARLYCON_TX.0.get() = Some(tx);
        *EARLYCON_RX.0.get() = Some(rx);
        set_out(&EARLYCON_TX);
    }
}

pub fn read_byte() -> Option<u8> {
    if let Some(byte) = <crate::arch::Arch as crate::ArchTrait>::Console::read_byte() {
        return Some(byte);
    }

    unsafe {
        if let Some(ref mut rx) = *EARLYCON_RX.0.get() {
            if !rx.poll().rx_ready() {
                return None;
            }
            let mut byte = [0];
            match rx.submit_rx(&mut byte) {
                Ok(1) => Some(byte[0]),
                _ => None,
            }
        } else {
            None
        }
    }
}

static EARLYCON_TX: EarlyconTxCell = EarlyconTxCell(UnsafeCell::new(None));
static EARLYCON_RX: EarlyconRxCell = EarlyconRxCell(UnsafeCell::new(None));

struct EarlyconTxCell(UnsafeCell<Option<EarlySerialTx>>);
struct EarlyconRxCell(UnsafeCell<Option<EarlySerialRx>>);

unsafe impl Sync for EarlyconTxCell {}
unsafe impl Sync for EarlyconRxCell {}

impl Con for EarlyconTxCell {
    fn write_bytes(&self, bytes: &[u8]) -> usize {
        unsafe {
            if let Some(ref mut tx) = *self.0.get() {
                let mut written = 0;
                while written < bytes.len() {
                    if !tx.poll().tx_ready() {
                        core::hint::spin_loop();
                        continue;
                    }
                    let n = tx.submit_tx(&bytes[written..]);
                    if n == 0 {
                        core::hint::spin_loop();
                        continue;
                    }
                    written += n;
                }
                written
            } else {
                // No sender available, simply return the length of bytes to indicate all bytes "written"
                bytes.len()
            }
        }
    }
}

pub fn set_earlycon_by_cmdline() -> Result<(), &'static str> {
    let config = crate::cmdline::earlycon().ok_or("No earlycon parameter found")?;
    let debug_is_mmio = match config.uart_type {
        "ns16550" => match config.io_type {
            "io" => {
                #[cfg(target_arch = "x86_64")]
                {
                    let base = config.base_addr.ok_or("missing io base address")? as u16;
                    let mut uart = some_serial::ns16550::Ns16550::new_port(base, 1_843_200);
                    uart.open();
                    let tx = uart.take_tx().ok_or("failed to take io tx queue")?;
                    let rx = uart.take_rx().ok_or("failed to take io rx queue")?;
                    set_earlycon_serial(
                        EarlySerialTx::Ns16550Port(tx),
                        EarlySerialRx::Ns16550Port(rx),
                    );
                    false
                }
                #[cfg(not(target_arch = "x86_64"))]
                {
                    return Err("io type not supported on this architecture");
                }
            }
            _ => {
                set_16550_mmio(&config)?;
                true
            }
        },
        "pl011" => {
            set_pl011(&config)?;
            true
        }
        _ => {
            return Err("unsupported earlycon uart type");
        }
    };
    unsafe {
        DEBUG_BASE = config.base_addr.unwrap_or(0);
        DEBUG_IS_MMIO = debug_is_mmio;
    }
    Ok(())
}

fn set_pl011(config: &EarlyconConfig) -> Result<(), &'static str> {
    let base_addr = config
        .base_addr
        .ok_or("No base address specified for pl011 earlycon")?;
    let base_addr =
        NonNull::new(_fixmap_io(base_addr)).ok_or("Invalid base address for pl011 earlycon")?;

    let mut serial = pl011::Pl011::new(base_addr, 0);
    serial.open();
    let tx = serial.take_tx().ok_or("failed to take pl011 tx queue")?;
    let rx = serial.take_rx().ok_or("failed to take pl011 rx queue")?;
    set_earlycon_serial(EarlySerialTx::Pl011(tx), EarlySerialRx::Pl011(rx));

    Ok(())
}

fn set_16550_mmio(config: &EarlyconConfig) -> Result<(), &'static str> {
    let base_addr = config
        .base_addr
        .ok_or("No base address specified for ns16550 earlycon")?;
    let base_addr =
        NonNull::new(_fixmap_io(base_addr)).ok_or("Invalid base address for ns16550 earlycon")?;
    let width = match config.io_type {
        "mmio" => 1,
        "mmio16" => 2,
        "mmio32" => 4,
        _ => return Err("Invalid io_type for ns16550 earlycon"),
    };

    let mut serial = ns16550::Ns16550::new_mmio(base_addr, 0, width);
    serial.open();
    let tx = serial.take_tx().ok_or("failed to take ns16550 tx queue")?;
    let rx = serial.take_rx().ok_or("failed to take ns16550 rx queue")?;
    set_earlycon_serial(
        EarlySerialTx::Ns16550Mmio(tx),
        EarlySerialRx::Ns16550Mmio(rx),
    );

    Ok(())
}
