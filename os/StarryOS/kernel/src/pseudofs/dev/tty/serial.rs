use alloc::{format, string::String, sync::Arc, vec, vec::Vec};
use core::ptr::NonNull;

use ax_driver::serial::{
    self as ax_serial, BIrqHandler, BRxQueue, BTxQueue, SerialDevice, SerialEvent,
};
use ax_errno::AxResult;
use ax_kspin::SpinNoIrq;
use axpoll::PollSet;
use spin::LazyLock;
use starry_process::Process;

use super::{
    Tty,
    ntty::N_TTY,
    terminal::{
        Terminal,
        ldisc::{ProcessMode, TtyConfig, TtyRead, TtyWrite},
        termios::Termios2,
    },
};
use crate::pseudofs::DeviceOps;

pub type SerialTtyDriver = Tty<SerialReader, SerialWriter>;

pub struct SerialTtyEntry {
    number: usize,
    tty: Arc<SerialTtyDriver>,
}

impl SerialTtyEntry {
    pub fn number(&self) -> usize {
        self.number
    }

    pub fn tty(&self) -> Arc<SerialTtyDriver> {
        self.tty.clone()
    }
}

struct SerialRegistry {
    entries: Vec<SerialTtyEntry>,
    console_index: Option<usize>,
}

struct SerialBackend {
    name: String,
    tty_name: String,
    number: usize,
    device: SpinNoIrq<SerialDevice>,
    tx: SpinNoIrq<BTxQueue>,
    rx: SpinNoIrq<BRxQueue>,
    irq_handler: Option<BIrqHandler>,
    input_source: Arc<PollSet>,
}

#[derive(Clone)]
pub struct SerialReader {
    backend: Arc<SerialBackend>,
}

#[derive(Clone)]
pub struct SerialWriter {
    backend: Arc<SerialBackend>,
}

static SERIAL_REGISTRY: LazyLock<SerialRegistry> = LazyLock::new(SerialRegistry::discover);

pub fn serial_tty_entries() -> &'static [SerialTtyEntry] {
    &SERIAL_REGISTRY.entries
}

impl SerialTtyDriver {
    pub fn serial_number(&self) -> usize {
        self.writer.backend.number
    }
}

pub fn console_device() -> Arc<dyn DeviceOps> {
    SERIAL_REGISTRY
        .console_index
        .and_then(|index| SERIAL_REGISTRY.entries.get(index))
        .map(|entry| entry.tty() as Arc<dyn DeviceOps>)
        .unwrap_or_else(|| N_TTY.clone() as Arc<dyn DeviceOps>)
}

pub fn bind_console_to(proc: &Process) -> AxResult<()> {
    if let Some(index) = SERIAL_REGISTRY.console_index
        && let Some(entry) = SERIAL_REGISTRY.entries.get(index)
    {
        return entry.tty.bind_to(proc);
    }
    N_TTY.bind_to(proc)
}

impl SerialRegistry {
    fn discover() -> Self {
        let serials = ax_serial::take_serial_devices();
        let numbers = assign_tty_numbers(
            serials
                .iter()
                .map(|serial| serial.alias_index())
                .collect::<Vec<_>>()
                .as_slice(),
        );

        let mut entries = Vec::new();
        for (serial, number) in serials.into_iter().zip(numbers) {
            let Some(number) = number else {
                warn!(
                    "Skipping serial device {} at {} because ttyS number could not be assigned",
                    serial.name(),
                    serial.fdt_path()
                );
                continue;
            };
            match new_serial_tty(number, serial) {
                Ok(entry) => entries.push(entry),
                Err(err) => warn!("Skipping ttyS{number}: {err:?}"),
            }
        }
        entries.sort_by_key(|entry| entry.number);

        let selected = selected_console_tty(ax_runtime::hal::dtb::get_chosen_bootargs());
        let console_index = selected.and_then(|number| {
            entries
                .iter()
                .position(|entry| entry.number == number)
                .or_else(|| {
                    warn!("bootargs console=ttyS{number} did not match a discovered serial TTY");
                    None
                })
        });
        if let (Some(_), Some(index)) = (selected, console_index) {
            let number = entries[index].number;
            info!("/dev/console bound to ttyS{number}");
        }

        Self {
            entries,
            console_index,
        }
    }
}

fn new_serial_tty(number: usize, mut serial: SerialDevice) -> AxResult<SerialTtyEntry> {
    let tty_name = format!("ttyS{number}");
    let input_source = Arc::new(PollSet::new());
    let tx = serial.take_tx().ok_or(ax_errno::AxError::BadState)?;
    let rx = serial.take_rx().ok_or(ax_errno::AxError::BadState)?;
    let irq_handler = serial.take_irq_handler();
    let backend = Arc::new(SerialBackend {
        name: serial.name().into(),
        tty_name: tty_name.clone(),
        number,
        device: SpinNoIrq::new(serial),
        tx: SpinNoIrq::new(tx),
        rx: SpinNoIrq::new(rx),
        irq_handler,
        input_source,
    });
    let process_mode = serial_process_mode(&backend).unwrap_or(ProcessMode::Manual);
    let terminal = Arc::new(Terminal::default());
    let tty = Tty::new(
        terminal,
        TtyConfig {
            reader: SerialReader {
                backend: backend.clone(),
            },
            writer: SerialWriter { backend },
            process_mode,
        },
    );
    Ok(SerialTtyEntry { number, tty })
}

fn serial_process_mode(backend: &Arc<SerialBackend>) -> Option<ProcessMode> {
    let irq_num = backend.device.lock().irq_num()?;
    if backend.irq_handler.is_none() {
        warn!(
            "{} has irq {irq_num} but no serial IRQ handler; using polling mode",
            backend.tty_name
        );
        return None;
    }
    let data = NonNull::new(Arc::into_raw(backend.clone()) as *mut ()).unwrap();
    if ax_runtime::hal::irq::request_shared_irq(irq_num, serial_raw_irq_handler, data).is_err() {
        warn!(
            "Failed to register {} IRQ handler for irq {irq_num}; using polling mode",
            backend.tty_name
        );
        unsafe {
            Arc::decrement_strong_count(data.as_ptr() as *const SerialBackend);
        }
        return None;
    }
    backend.device.lock().enable_rx_interrupts();
    Some(ProcessMode::InterruptDriven(backend.input_source.clone()))
}

unsafe fn serial_raw_irq_handler(
    _ctx: ax_runtime::hal::irq::IrqContext,
    data: NonNull<()>,
) -> ax_runtime::hal::irq::IrqReturn {
    let backend = unsafe { &*(data.as_ptr() as *const SerialBackend) };
    if let Some(handler) = backend.irq_handler.as_ref() {
        let status = handler.handle_irq();
        if status.intersects(SerialEvent::RX_READY | SerialEvent::RX_ERROR | SerialEvent::OVERRUN) {
            backend.input_source.wake();
        }
    }
    ax_runtime::hal::irq::IrqReturn::Handled
}

impl TtyRead for SerialReader {
    fn read(&mut self, buf: &mut [u8]) -> usize {
        match self.backend.rx.lock().submit_rx(buf) {
            Ok(read) => read,
            Err(err) => {
                if err.bytes_transferred == 0 {
                    warn!(
                        "{} read error from {}: {:?}",
                        self.backend.tty_name, self.backend.name, err.kind
                    );
                }
                err.bytes_transferred
            }
        }
    }
}

impl TtyWrite for SerialWriter {
    fn write(&self, buf: &[u8]) {
        let mut tx = self.backend.tx.lock();
        let mut written = 0;
        while written < buf.len() {
            let next = tx.submit_tx(&buf[written..]);
            written += next;
            if next == 0 {
                drop(tx);
                ax_task::yield_now();
                tx = self.backend.tx.lock();
            }
        }
    }

    fn termios_changed(&self, old: &Termios2, new: &Termios2) {
        let Some(new_baud) = new.baudrate() else {
            return;
        };
        if old.baudrate() == Some(new_baud) {
            return;
        }
        if let Err(err) = self.backend.device.lock().set_baudrate(new_baud) {
            warn!(
                "{} failed to set baudrate {new_baud} on {}: {:?}",
                self.backend.tty_name, self.backend.name, err
            );
        }
    }
}

fn selected_console_tty(bootargs: Option<&str>) -> Option<usize> {
    bootargs?
        .split_ascii_whitespace()
        .filter_map(|arg| arg.strip_prefix("console="))
        .find_map(parse_serial_console)
}

fn parse_serial_console(spec: &str) -> Option<usize> {
    let name = spec.split(',').next().unwrap_or(spec);
    name.strip_prefix("ttyS")?.parse::<usize>().ok()
}

fn assign_tty_numbers(alias_indices: &[Option<usize>]) -> Vec<Option<usize>> {
    let mut assigned = vec![None; alias_indices.len()];
    let mut used = Vec::new();

    for (device_index, alias) in alias_indices.iter().copied().enumerate() {
        let Some(number) = alias else {
            continue;
        };
        if used.contains(&number) {
            warn!("Duplicate FDT serial{number} alias ignored for later serial device");
            continue;
        }
        assigned[device_index] = Some(number);
        used.push(number);
    }

    let mut next = 0usize;
    for number in &mut assigned {
        if number.is_some() {
            continue;
        }
        while used.contains(&next) {
            next += 1;
        }
        *number = Some(next);
        used.push(next);
    }

    assigned
}

#[cfg(test)]
mod tests {
    use super::{assign_tty_numbers, selected_console_tty};

    #[test]
    fn aliases_keep_linux_ttys_numbering() {
        assert_eq!(assign_tty_numbers(&[Some(0), Some(2)]), [Some(0), Some(2)]);
    }

    #[test]
    fn unaliased_serials_take_first_free_ttys_numbers() {
        assert_eq!(
            assign_tty_numbers(&[Some(0), None, Some(2), None]),
            [Some(0), Some(1), Some(2), Some(3)]
        );
    }

    #[test]
    fn duplicate_alias_keeps_first_device_and_reassigns_later_one() {
        assert_eq!(
            assign_tty_numbers(&[Some(1), Some(1), None]),
            [Some(1), Some(0), Some(2)]
        );
    }

    #[test]
    fn bootargs_select_first_serial_console_even_when_later_console_is_non_serial() {
        assert_eq!(
            selected_console_tty(Some("console=ttyS2,1500000 console=tty1")),
            Some(2)
        );
    }

    #[test]
    fn bootargs_missing_or_non_serial_console_falls_back() {
        assert_eq!(selected_console_tty(None), None);
        assert_eq!(
            selected_console_tty(Some("root=/dev/vda console=tty1")),
            None
        );
    }

    #[test]
    fn malformed_serial_console_does_not_panic() {
        assert_eq!(selected_console_tty(Some("console=ttySx")), None);
    }
}
