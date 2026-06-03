use alloc::{boxed::Box, sync::Arc};
use core::num::NonZeroU32;

use rdif_base::DriverGeneric;
use spin::Mutex;

use super::{
    BIrqHandler, BRxQueue, BSerial, BTxQueue, InterfaceRaw, InterruptMask, SerialEvent,
    SetBackError, TIrqHandler, TRxQueue, TTxQueue, TransBytesError,
};

pub struct SerialDyn<T: InterfaceRaw> {
    inner: T,
    tx: Arc<Mutex<Option<BTxQueue>>>,
    rx: Arc<Mutex<Option<BRxQueue>>>,
    irq: Arc<Mutex<Option<BIrqHandler>>>,
}

impl<T: InterfaceRaw> SerialDyn<T> {
    pub fn new_boxed(mut inner: T) -> BSerial {
        let tx: BTxQueue = Box::new(inner.take_tx().expect("serial TX queue is unavailable"));
        let rx: BRxQueue = Box::new(inner.take_rx().expect("serial RX queue is unavailable"));
        let irq = inner
            .take_irq_handler()
            .map(|irq| Box::new(irq) as BIrqHandler);

        Box::new(Self {
            inner,
            tx: Arc::new(Mutex::new(Some(tx))),
            rx: Arc::new(Mutex::new(Some(rx))),
            irq: Arc::new(Mutex::new(irq)),
        })
    }
}

impl<T: InterfaceRaw> super::Interface for SerialDyn<T> {
    fn base_addr(&self) -> usize {
        self.inner.base_addr()
    }

    fn set_config(&mut self, config: &crate::Config) -> Result<(), crate::ConfigError> {
        self.inner.set_config(config)
    }

    fn baudrate(&self) -> u32 {
        self.inner.baudrate()
    }

    fn data_bits(&self) -> crate::DataBits {
        self.inner.data_bits()
    }

    fn stop_bits(&self) -> crate::StopBits {
        self.inner.stop_bits()
    }

    fn parity(&self) -> crate::Parity {
        self.inner.parity()
    }

    fn clock_freq(&self) -> Option<NonZeroU32> {
        self.inner.clock_freq()
    }

    fn open(&mut self) {
        self.inner.open();
    }

    fn close(&mut self) {
        self.inner.close();
    }

    fn enable_loopback(&mut self) {
        self.inner.enable_loopback()
    }

    fn disable_loopback(&mut self) {
        self.inner.disable_loopback()
    }

    fn is_loopback_enabled(&self) -> bool {
        self.inner.is_loopback_enabled()
    }

    fn set_irq_mask(&mut self, mask: InterruptMask) {
        self.inner.set_irq_mask(mask);
    }

    fn get_irq_mask(&self) -> InterruptMask {
        self.inner.get_irq_mask()
    }

    fn take_tx(&mut self) -> Option<BTxQueue> {
        let tx = self.tx.lock().take()?;
        Some(Box::new(TxQueue {
            slot: self.tx.clone(),
            inner: Some(tx),
        }))
    }

    fn take_rx(&mut self) -> Option<BRxQueue> {
        let rx = self.rx.lock().take()?;
        Some(Box::new(RxQueue {
            slot: self.rx.clone(),
            inner: Some(rx),
        }))
    }

    fn take_irq_handler(&mut self) -> Option<BIrqHandler> {
        let irq = self.irq.lock().take()?;
        Some(Box::new(IrqHandler {
            slot: self.irq.clone(),
            inner: Some(irq),
        }))
    }

    fn set_tx(&mut self, tx: BTxQueue) -> Result<(), SetBackError> {
        if tx.base_addr() != self.base_addr() {
            return Err(SetBackError::new(self.base_addr(), tx.base_addr()));
        }
        *self.tx.lock() = Some(tx);
        Ok(())
    }

    fn set_rx(&mut self, rx: BRxQueue) -> Result<(), SetBackError> {
        if rx.base_addr() != self.base_addr() {
            return Err(SetBackError::new(self.base_addr(), rx.base_addr()));
        }
        *self.rx.lock() = Some(rx);
        Ok(())
    }

    fn set_irq_handler(&mut self, irq: BIrqHandler) -> Result<(), SetBackError> {
        if irq.base_addr() != self.base_addr() {
            return Err(SetBackError::new(self.base_addr(), irq.base_addr()));
        }
        *self.irq.lock() = Some(irq);
        Ok(())
    }
}

impl<T: InterfaceRaw> DriverGeneric for SerialDyn<T> {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn raw_any(&self) -> Option<&dyn core::any::Any> {
        Some(&self.inner)
    }

    fn raw_any_mut(&mut self) -> Option<&mut dyn core::any::Any> {
        Some(&mut self.inner)
    }
}

pub struct TxQueue {
    slot: Arc<Mutex<Option<BTxQueue>>>,
    inner: Option<BTxQueue>,
}

impl Drop for TxQueue {
    fn drop(&mut self) {
        *self.slot.lock() = self.inner.take();
    }
}

impl TTxQueue for TxQueue {
    fn base_addr(&self) -> usize {
        self.inner.as_ref().unwrap().base_addr()
    }

    fn poll(&mut self) -> SerialEvent {
        self.inner.as_mut().unwrap().poll()
    }

    fn submit_tx(&mut self, bytes: &[u8]) -> usize {
        self.inner.as_mut().unwrap().submit_tx(bytes)
    }
}

pub struct RxQueue {
    slot: Arc<Mutex<Option<BRxQueue>>>,
    inner: Option<BRxQueue>,
}

impl Drop for RxQueue {
    fn drop(&mut self) {
        *self.slot.lock() = self.inner.take();
    }
}

impl TRxQueue for RxQueue {
    fn base_addr(&self) -> usize {
        self.inner.as_ref().unwrap().base_addr()
    }

    fn poll(&mut self) -> SerialEvent {
        self.inner.as_mut().unwrap().poll()
    }

    fn submit_rx(&mut self, bytes: &mut [u8]) -> Result<usize, TransBytesError> {
        self.inner.as_mut().unwrap().submit_rx(bytes)
    }
}

pub struct IrqHandler {
    slot: Arc<Mutex<Option<BIrqHandler>>>,
    inner: Option<BIrqHandler>,
}

impl Drop for IrqHandler {
    fn drop(&mut self) {
        *self.slot.lock() = self.inner.take();
    }
}

impl TIrqHandler for IrqHandler {
    fn base_addr(&self) -> usize {
        self.inner.as_ref().unwrap().base_addr()
    }

    fn handle_irq(&self) -> SerialEvent {
        self.inner.as_ref().unwrap().handle_irq()
    }
}
