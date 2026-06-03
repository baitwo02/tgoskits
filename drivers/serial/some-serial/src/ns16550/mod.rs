//! NS16550/16450 UART 驱动模块
//!
//! 提供两种访问方式：
//! - IO Port 版本（x86_64 架构）
//! - MMIO 版本（通用嵌入式平台）

// 公共寄存器定义
mod registers;

use bitflags::Flags;
use rdif_serial::{
    Config, ConfigError, DataBits, InterfaceRaw, InterruptMask, Parity, SerialEvent, SetBackError,
    StopBits, TIrqHandler, TRxQueue, TTxQueue, TransBytesError, TransferError,
};
use registers::*;

pub mod dw_apb;
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
mod pio;
pub mod rockchip_fiq;
// MMIO 版本（通用）
mod mmio;

pub use dw_apb::*;
pub use mmio::*;
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
pub use pio::*;
pub use rockchip_fiq::*;

pub trait Kind: Clone + Send + Sync + 'static {
    fn read_reg(&self, reg: u8) -> u8;
    fn write_reg(&self, reg: u8, val: u8);
    fn get_base(&self) -> usize;

    fn set_baudrate(&self, clock_freq: u32, baudrate: u32) -> Result<(), ConfigError> {
        if baudrate == 0 || clock_freq == 0 {
            return Err(ConfigError::InvalidBaudrate);
        }

        let divisor = clock_freq / (16 * baudrate);
        if divisor == 0 || divisor > 0xFFFF {
            return Err(ConfigError::InvalidBaudrate);
        }

        let mut lcr: LineControlFlags = self.read_flags(UART_LCR);
        lcr.insert(LineControlFlags::DIVISOR_LATCH_ACCESS);
        self.write_flags(UART_LCR, lcr);

        self.write_reg(UART_DLL, (divisor & 0xFF) as u8);
        self.write_reg(UART_DLH, ((divisor >> 8) & 0xFF) as u8);

        lcr.remove(LineControlFlags::DIVISOR_LATCH_ACCESS);
        self.write_flags(UART_LCR, lcr);

        Ok(())
    }

    fn baudrate(&self, clock_freq: u32) -> u32 {
        let dll = self.read_reg(UART_DLL) as u16;
        let dlh = self.read_reg(UART_DLH) as u16;
        let divisor = dll | (dlh << 8);

        if divisor == 0 {
            return 0;
        }

        clock_freq / (16 * divisor as u32)
    }

    fn init(&self) {
        self.write_flags(UART_IER, InterruptEnableFlags::empty());

        let mut mcr: ModemControlFlags = self.read_flags(UART_MCR);
        mcr.insert(ModemControlFlags::DATA_TERMINAL_READY | ModemControlFlags::REQUEST_TO_SEND);
        self.write_flags(UART_MCR, mcr);
    }

    // 类型安全的 bitflags 寄存器访问
    fn read_flags<F: Flags<Bits = u8>>(&self, reg: u8) -> F {
        F::from_bits_retain(self.read_reg(reg))
    }

    fn write_flags<F: Flags<Bits = u8>>(&self, reg: u8, val: F) {
        self.write_reg(reg, val.bits());
    }
}

pub struct Ns16550<T: Kind> {
    pub(crate) base: T,
    pub(crate) clock_freq: u32,
    pub(crate) tx_taken: bool,
    pub(crate) rx_taken: bool,
    pub(crate) irq_taken: bool,
}

impl<T: Kind> InterfaceRaw for Ns16550<T> {
    type TxQueue = Ns16550TxQueue<T>;
    type RxQueue = Ns16550RxQueue<T>;
    type IrqHandler = Ns16550IrqHandler<T>;

    fn name(&self) -> &str {
        "NS16550 UART"
    }

    fn base_addr(&self) -> usize {
        self.base.get_base()
    }

    fn set_config(&mut self, config: &Config) -> Result<(), ConfigError> {
        // 配置波特率
        if let Some(baudrate) = config.baudrate {
            self.set_baudrate_internal(baudrate)?;
        }

        // 配置数据位
        if let Some(data_bits) = config.data_bits {
            self.set_data_bits_internal(data_bits)?;
        }

        // 配置停止位
        if let Some(stop_bits) = config.stop_bits {
            self.set_stop_bits_internal(stop_bits)?;
        }

        // 配置奇偶校验
        if let Some(parity) = config.parity {
            self.set_parity_internal(parity)?;
        }
        Ok(())
    }

    fn baudrate(&self) -> u32 {
        self.base.baudrate(self.clock_freq)
    }

    fn data_bits(&self) -> DataBits {
        let lcr: LineControlFlags = self.read_flags(UART_LCR);
        let wlen = lcr & LineControlFlags::WORD_LENGTH_MASK;
        if wlen == LineControlFlags::WORD_LENGTH_5 {
            DataBits::Five
        } else if wlen == LineControlFlags::WORD_LENGTH_6 {
            DataBits::Six
        } else if wlen == LineControlFlags::WORD_LENGTH_7 {
            DataBits::Seven
        } else {
            DataBits::Eight // 默认值
        }
    }

    fn stop_bits(&self) -> StopBits {
        let lcr: LineControlFlags = self.read_flags(UART_LCR);
        if lcr.contains(LineControlFlags::STOP_BITS) {
            StopBits::Two
        } else {
            StopBits::One
        }
    }

    fn parity(&self) -> Parity {
        let lcr: LineControlFlags = self.read_flags(UART_LCR);

        if !lcr.contains(LineControlFlags::PARITY_ENABLE) {
            Parity::None
        } else if lcr.contains(LineControlFlags::STICK_PARITY) {
            // Stick parity
            if lcr.contains(LineControlFlags::EVEN_PARITY) {
                Parity::Space
            } else {
                Parity::Mark
            }
        } else {
            // Normal parity
            if lcr.contains(LineControlFlags::EVEN_PARITY) {
                Parity::Even
            } else {
                Parity::Odd
            }
        }
    }

    fn clock_freq(&self) -> Option<core::num::NonZeroU32> {
        self.clock_freq.try_into().ok()
    }

    fn open(&mut self) {
        Ns16550::open(self);
    }

    fn close(&mut self) {
        Ns16550::close(self);
    }

    fn enable_loopback(&mut self) {
        let mut mcr: ModemControlFlags = self.read_flags(UART_MCR);
        mcr.insert(ModemControlFlags::LOOPBACK_ENABLE);
        self.write_flags(UART_MCR, mcr);
    }

    fn disable_loopback(&mut self) {
        let mut mcr: ModemControlFlags = self.read_flags(UART_MCR);
        mcr.remove(ModemControlFlags::LOOPBACK_ENABLE);
        self.write_flags(UART_MCR, mcr);
    }

    fn is_loopback_enabled(&self) -> bool {
        let mcr: ModemControlFlags = self.read_flags(UART_MCR);
        mcr.contains(ModemControlFlags::LOOPBACK_ENABLE)
    }

    fn set_irq_mask(&mut self, mask: InterruptMask) {
        Ns16550::set_irq_mask(self, mask);
    }

    fn get_irq_mask(&self) -> InterruptMask {
        Ns16550::get_irq_mask(self)
    }

    fn take_tx(&mut self) -> Option<Self::TxQueue> {
        Ns16550::take_tx(self)
    }

    fn take_rx(&mut self) -> Option<Self::RxQueue> {
        Ns16550::take_rx(self)
    }

    fn take_irq_handler(&mut self) -> Option<Self::IrqHandler> {
        Ns16550::take_irq_handler(self)
    }

    fn set_tx(&mut self, tx: Self::TxQueue) -> Result<(), SetBackError> {
        Ns16550::set_tx(self, tx)
    }

    fn set_rx(&mut self, rx: Self::RxQueue) -> Result<(), SetBackError> {
        Ns16550::set_rx(self, rx)
    }

    fn set_irq_handler(&mut self, irq: Self::IrqHandler) -> Result<(), SetBackError> {
        Ns16550::set_irq_handler(self, irq)
    }
}

impl<T: Kind> Ns16550<T> {
    // 类型安全的 bitflags 寄存器访问
    fn read_flags<F: Flags<Bits = u8>>(&self, reg: u8) -> F {
        F::from_bits_retain(self.base.read_reg(reg))
    }

    fn write_flags<F: Flags<Bits = u8>>(&mut self, reg: u8, val: F) {
        self.base.write_reg(reg, val.bits());
    }

    fn submit_tx_from_base(base: &T, bytes: &[u8]) -> usize {
        let mut written = 0;
        for &byte in bytes {
            let lsr: LineStatusFlags = base.read_flags(UART_LSR);
            if !lsr.contains(LineStatusFlags::TRANSMITTER_HOLDING_EMPTY) {
                break;
            }
            base.write_reg(UART_THR, byte);
            written += 1;
        }
        written
    }

    fn submit_rx_from_base(base: &T, bytes: &mut [u8]) -> Result<usize, TransBytesError> {
        let mut read_count = 0;
        for byte in bytes.iter_mut() {
            match read_byte_from_kind(base) {
                Some(Ok(b)) => {
                    *byte = b;
                    read_count += 1;
                }
                Some(Err(e)) => {
                    return Err(TransBytesError {
                        bytes_transferred: read_count,
                        kind: e,
                    });
                }
                None => break,
            }
        }
        Ok(read_count)
    }

    fn handle_irq_from_base(base: &T) -> SerialEvent {
        let iir: InterruptIdentificationFlags = base.read_flags(UART_IIR);
        let mut event = SerialEvent::empty();

        if iir.contains(InterruptIdentificationFlags::NO_INTERRUPT_PENDING) {
            return event;
        }

        let interrupt_id = iir & InterruptIdentificationFlags::INTERRUPT_ID_MASK;
        if interrupt_id == InterruptIdentificationFlags::RECEIVER_LINE_STATUS {
            event |= serial_event_from_lsr(base.read_flags(UART_LSR))
                & (SerialEvent::RX_READY | SerialEvent::RX_ERROR | SerialEvent::OVERRUN);
            if event.is_empty() {
                event |= SerialEvent::RX_ERROR;
            }
        } else if interrupt_id == InterruptIdentificationFlags::RECEIVED_DATA_AVAILABLE
            || interrupt_id == InterruptIdentificationFlags::CHARACTER_TIMEOUT
        {
            event |= SerialEvent::RX_READY;
            event |= serial_event_from_lsr(base.read_flags(UART_LSR))
                & (SerialEvent::RX_ERROR | SerialEvent::OVERRUN);
        } else if interrupt_id == InterruptIdentificationFlags::TRANSMITTER_HOLDING_EMPTY {
            event |= SerialEvent::TX_READY;
        }

        event
    }

    pub fn open(&mut self) {
        self.init_core();
    }

    pub fn close(&mut self) {
        self.write_flags(UART_IER, InterruptEnableFlags::empty());

        let mut mcr: ModemControlFlags = self.read_flags(UART_MCR);
        mcr.remove(ModemControlFlags::DATA_TERMINAL_READY | ModemControlFlags::REQUEST_TO_SEND);
        self.write_flags(UART_MCR, mcr);
    }

    pub fn set_irq_mask(&mut self, mask: InterruptMask) {
        let mut ier = InterruptEnableFlags::empty();

        if mask.contains(InterruptMask::RX_AVAILABLE) {
            ier.insert(InterruptEnableFlags::RECEIVED_DATA_AVAILABLE);
            ier.insert(InterruptEnableFlags::RECEIVER_LINE_STATUS);
        }
        if mask.contains(InterruptMask::TX_EMPTY) {
            ier.insert(InterruptEnableFlags::TRANSMITTER_HOLDING_EMPTY);
        }

        self.write_flags(UART_IER, ier);
    }

    pub fn get_irq_mask(&self) -> InterruptMask {
        let ier: InterruptEnableFlags = self.read_flags(UART_IER);
        let mut mask = InterruptMask::empty();

        if ier.contains(InterruptEnableFlags::RECEIVED_DATA_AVAILABLE) {
            mask |= InterruptMask::RX_AVAILABLE;
        }
        if ier.contains(InterruptEnableFlags::TRANSMITTER_HOLDING_EMPTY) {
            mask |= InterruptMask::TX_EMPTY;
        }

        mask
    }

    pub fn take_tx(&mut self) -> Option<Ns16550TxQueue<T>> {
        if self.tx_taken {
            return None;
        }
        self.tx_taken = true;
        Some(Ns16550TxQueue {
            base: self.base.clone(),
        })
    }

    pub fn take_rx(&mut self) -> Option<Ns16550RxQueue<T>> {
        if self.rx_taken {
            return None;
        }
        self.rx_taken = true;
        Some(Ns16550RxQueue {
            base: self.base.clone(),
        })
    }

    pub fn take_irq_handler(&mut self) -> Option<Ns16550IrqHandler<T>> {
        if self.irq_taken {
            return None;
        }
        self.irq_taken = true;
        Some(Ns16550IrqHandler {
            base: self.base.clone(),
        })
    }

    pub fn set_tx(&mut self, tx: Ns16550TxQueue<T>) -> Result<(), SetBackError> {
        ensure_same_base(self.base.get_base(), tx.base.get_base())?;
        self.tx_taken = false;
        Ok(())
    }

    pub fn set_rx(&mut self, rx: Ns16550RxQueue<T>) -> Result<(), SetBackError> {
        ensure_same_base(self.base.get_base(), rx.base.get_base())?;
        self.rx_taken = false;
        Ok(())
    }

    pub fn set_irq_handler(&mut self, irq: Ns16550IrqHandler<T>) -> Result<(), SetBackError> {
        ensure_same_base(self.base.get_base(), irq.base.get_base())?;
        self.irq_taken = false;
        Ok(())
    }

    /// 检查是否为 16550+（支持 FIFO）
    pub fn is_16550_plus(&self) -> bool {
        // 通过读取 IIR 寄存器的 FIFO 位来判断
        // IIR 的位7-6在 16550+ 中会显示 FIFO 启用状态
        let fifo: InterruptIdentificationFlags = self.read_flags(UART_IIR);
        fifo.contains(InterruptIdentificationFlags::FIFO_ENABLE_MASK)
    }

    /// 设置波特率
    fn set_baudrate_internal(&mut self, baudrate: u32) -> Result<(), ConfigError> {
        self.base.set_baudrate(self.clock_freq, baudrate)
    }

    /// 设置数据位
    fn set_data_bits_internal(&mut self, bits: DataBits) -> Result<(), ConfigError> {
        let wlen = match bits {
            DataBits::Five => LineControlFlags::WORD_LENGTH_5,
            DataBits::Six => LineControlFlags::WORD_LENGTH_6,
            DataBits::Seven => LineControlFlags::WORD_LENGTH_7,
            DataBits::Eight => LineControlFlags::WORD_LENGTH_8,
        };

        let mut lcr: LineControlFlags = self.read_flags(UART_LCR);
        // 清除旧的数据位设置，然后设置新的
        lcr.remove(LineControlFlags::WORD_LENGTH_MASK);
        lcr.insert(wlen);
        self.write_flags(UART_LCR, lcr);

        Ok(())
    }

    /// 设置停止位
    fn set_stop_bits_internal(&mut self, bits: StopBits) -> Result<(), ConfigError> {
        let mut lcr: LineControlFlags = self.read_flags(UART_LCR);
        match bits {
            StopBits::One => lcr.remove(LineControlFlags::STOP_BITS),
            StopBits::Two => lcr.insert(LineControlFlags::STOP_BITS),
        }
        self.write_flags(UART_LCR, lcr);
        Ok(())
    }

    /// 设置奇偶校验
    fn set_parity_internal(&mut self, parity: Parity) -> Result<(), ConfigError> {
        let mut lcr: LineControlFlags = self.read_flags(UART_LCR);

        // 先清除所有校验相关位
        lcr.remove(
            LineControlFlags::PARITY_ENABLE
                | LineControlFlags::EVEN_PARITY
                | LineControlFlags::STICK_PARITY,
        );

        // 根据校验类型设置相应位
        match parity {
            Parity::None => {
                // 已经清除，无需额外操作
            }
            Parity::Odd => {
                lcr.insert(LineControlFlags::PARITY_ENABLE);
            }
            Parity::Even => {
                lcr.insert(LineControlFlags::PARITY_ENABLE | LineControlFlags::EVEN_PARITY);
            }
            Parity::Mark => {
                lcr.insert(LineControlFlags::PARITY_ENABLE | LineControlFlags::STICK_PARITY);
            }
            Parity::Space => {
                lcr.insert(
                    LineControlFlags::PARITY_ENABLE
                        | LineControlFlags::EVEN_PARITY
                        | LineControlFlags::STICK_PARITY,
                );
            }
        }

        self.write_flags(UART_LCR, lcr);
        Ok(())
    }

    /// 启用或禁用 FIFO
    pub fn enable_fifo(&mut self, enable: bool) {
        if enable && self.is_16550_plus() {
            let mut fcr = FifoControlFlags::ENABLE_FIFO;
            fcr.insert(FifoControlFlags::CLEAR_RECEIVER_FIFO);
            fcr.insert(FifoControlFlags::CLEAR_TRANSMITTER_FIFO);
            fcr.insert(FifoControlFlags::TRIGGER_1_BYTE);
            self.write_flags(UART_FCR, fcr);
        } else {
            self.write_flags(UART_FCR, FifoControlFlags::empty());
        }
    }

    /// 设置 FIFO 触发级别
    pub fn set_fifo_trigger_level(&mut self, level: u8) {
        if !self.is_16550_plus() {
            return;
        }

        let trigger_value = match level {
            0..=3 => FifoControlFlags::TRIGGER_1_BYTE,
            4..=7 => FifoControlFlags::TRIGGER_4_BYTES,
            8..=11 => FifoControlFlags::TRIGGER_8_BYTES,
            _ => FifoControlFlags::TRIGGER_14_BYTES,
        };

        // 读取当前 FCR 设置，清除触发级别位，然后设置新的触发级别
        let mut fcr: FifoControlFlags = self.read_flags(UART_FCR);
        fcr.remove(FifoControlFlags::TRIGGER_LEVEL_MASK);
        fcr.insert(trigger_value);
        self.write_flags(UART_FCR, fcr);
    }

    /// 初始化 UART
    fn init_core(&mut self) {
        self.base.init();
    }

    /// 检查 FIFO 是否启用
    pub fn is_fifo_enabled(&self) -> bool {
        if !self.is_16550_plus() {
            return false;
        }
        // 通过检查 IIR 的 FIFO 位来判断
        let iir: InterruptIdentificationFlags = self.read_flags(UART_IIR);
        iir.contains(InterruptIdentificationFlags::FIFO_ENABLE_MASK)
    }
}

pub struct Ns16550TxQueue<T: Kind> {
    pub(crate) base: T,
}

impl<T: Kind> Ns16550TxQueue<T> {
    pub fn base_addr(&self) -> usize {
        self.base.get_base()
    }

    pub fn poll(&mut self) -> SerialEvent {
        serial_event_from_lsr(self.base.read_flags(UART_LSR)) & SerialEvent::TX_READY
    }

    pub fn submit_tx(&mut self, bytes: &[u8]) -> usize {
        Ns16550::<T>::submit_tx_from_base(&self.base, bytes)
    }
}

impl<T: Kind> TTxQueue for Ns16550TxQueue<T> {
    fn base_addr(&self) -> usize {
        Ns16550TxQueue::base_addr(self)
    }

    fn poll(&mut self) -> SerialEvent {
        Ns16550TxQueue::poll(self)
    }

    fn submit_tx(&mut self, bytes: &[u8]) -> usize {
        Ns16550TxQueue::submit_tx(self, bytes)
    }
}

pub struct Ns16550RxQueue<T: Kind> {
    pub(crate) base: T,
}

impl<T: Kind> Ns16550RxQueue<T> {
    pub fn base_addr(&self) -> usize {
        self.base.get_base()
    }

    pub fn poll(&mut self) -> SerialEvent {
        serial_event_from_lsr(self.base.read_flags(UART_LSR))
            & (SerialEvent::RX_READY | SerialEvent::RX_ERROR | SerialEvent::OVERRUN)
    }

    pub fn submit_rx(&mut self, bytes: &mut [u8]) -> Result<usize, TransBytesError> {
        Ns16550::<T>::submit_rx_from_base(&self.base, bytes)
    }
}

impl<T: Kind> TRxQueue for Ns16550RxQueue<T> {
    fn base_addr(&self) -> usize {
        Ns16550RxQueue::base_addr(self)
    }

    fn poll(&mut self) -> SerialEvent {
        Ns16550RxQueue::poll(self)
    }

    fn submit_rx(&mut self, bytes: &mut [u8]) -> Result<usize, TransBytesError> {
        Ns16550RxQueue::submit_rx(self, bytes)
    }
}

pub struct Ns16550IrqHandler<T: Kind> {
    pub(crate) base: T,
}

unsafe impl<T: Kind> Sync for Ns16550IrqHandler<T> {}

impl<T: Kind> Ns16550IrqHandler<T> {
    pub fn base_addr(&self) -> usize {
        self.base.get_base()
    }

    pub fn handle_irq(&self) -> SerialEvent {
        Ns16550::<T>::handle_irq_from_base(&self.base)
    }
}

impl<T: Kind> TIrqHandler for Ns16550IrqHandler<T> {
    fn base_addr(&self) -> usize {
        Ns16550IrqHandler::base_addr(self)
    }

    fn handle_irq(&self) -> SerialEvent {
        Ns16550IrqHandler::handle_irq(self)
    }
}

fn serial_event_from_lsr(lsr: LineStatusFlags) -> SerialEvent {
    let mut event = SerialEvent::empty();
    if lsr.contains(LineStatusFlags::DATA_READY) {
        event |= SerialEvent::RX_READY;
    }
    if lsr.intersects(
        LineStatusFlags::PARITY_ERROR
            | LineStatusFlags::FRAMING_ERROR
            | LineStatusFlags::BREAK_INTERRUPT,
    ) {
        event |= SerialEvent::RX_ERROR;
    }
    if lsr.contains(LineStatusFlags::OVERRUN_ERROR) {
        event |= SerialEvent::RX_ERROR | SerialEvent::OVERRUN;
    }
    if lsr.contains(LineStatusFlags::TRANSMITTER_HOLDING_EMPTY) {
        event |= SerialEvent::TX_READY;
    }
    event
}

fn read_byte_from_kind<T: Kind>(base: &T) -> Option<Result<u8, TransferError>> {
    let lsr: LineStatusFlags = base.read_flags(UART_LSR);

    if lsr.contains(LineStatusFlags::OVERRUN_ERROR) {
        let b = base.read_reg(UART_RBR);
        return Some(Err(TransferError::Overrun(b)));
    }
    if lsr.contains(LineStatusFlags::PARITY_ERROR) {
        let _ = base.read_reg(UART_RBR);
        return Some(Err(TransferError::Parity));
    }
    if lsr.contains(LineStatusFlags::FRAMING_ERROR) {
        let _ = base.read_reg(UART_RBR);
        return Some(Err(TransferError::Framing));
    }
    if lsr.contains(LineStatusFlags::BREAK_INTERRUPT) {
        let _ = base.read_reg(UART_RBR);
        return Some(Err(TransferError::Break));
    }
    if lsr.contains(LineStatusFlags::DATA_READY) {
        return Some(Ok(base.read_reg(UART_RBR)));
    }
    None
}

fn ensure_same_base(want: usize, actual: usize) -> Result<(), SetBackError> {
    if want == actual {
        Ok(())
    } else {
        Err(SetBackError::new(want, actual))
    }
}
