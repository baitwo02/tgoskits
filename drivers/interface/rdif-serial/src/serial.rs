use alloc::{boxed::Box, string::String, sync::Arc};
use core::num::NonZeroU32;

use rdif_base::DriverGeneric;
use spin::Mutex;

use super::{
    BIrqHandler, BRxQueue, BSerial, BTxQueue, InterfaceRaw, InterruptMask, SerialDirection,
    SerialEvent, SetBackError, TIrqHandler, TRxQueue, TTxQueue, TransBytesError,
};

pub struct SerialDyn<T: InterfaceRaw> {
    name: String,
    inner: Arc<Mutex<T>>,
    tx_taken: Arc<Mutex<bool>>,
    rx_taken: Arc<Mutex<bool>>,
    irq_taken: Arc<Mutex<bool>>,
}

impl<T: InterfaceRaw> SerialDyn<T> {
    pub fn new_boxed(inner: T) -> BSerial {
        let name = String::from(inner.name());
        Box::new(Self {
            name,
            inner: Arc::new(Mutex::new(inner)),
            tx_taken: Arc::new(Mutex::new(false)),
            rx_taken: Arc::new(Mutex::new(false)),
            irq_taken: Arc::new(Mutex::new(false)),
        })
    }

    fn inner_base_addr(&self) -> usize {
        self.inner.lock().base_addr()
    }
}

impl<T: InterfaceRaw> super::Interface for SerialDyn<T> {
    fn base_addr(&self) -> usize {
        self.inner_base_addr()
    }

    fn set_config(&mut self, config: &crate::Config) -> Result<(), crate::ConfigError> {
        self.inner.lock().set_config(config)
    }

    fn baudrate(&self) -> u32 {
        self.inner.lock().baudrate()
    }

    fn data_bits(&self) -> crate::DataBits {
        self.inner.lock().data_bits()
    }

    fn stop_bits(&self) -> crate::StopBits {
        self.inner.lock().stop_bits()
    }

    fn parity(&self) -> crate::Parity {
        self.inner.lock().parity()
    }

    fn clock_freq(&self) -> Option<NonZeroU32> {
        self.inner.lock().clock_freq()
    }

    fn open(&mut self) {
        self.inner.lock().open();
    }

    fn close(&mut self) {
        self.inner.lock().close();
    }

    fn enable_loopback(&mut self) {
        self.inner.lock().enable_loopback()
    }

    fn disable_loopback(&mut self) {
        self.inner.lock().disable_loopback()
    }

    fn is_loopback_enabled(&self) -> bool {
        self.inner.lock().is_loopback_enabled()
    }

    fn set_irq_mask(&mut self, mask: InterruptMask) {
        self.inner.lock().set_irq_mask(mask);
    }

    fn get_irq_mask(&self) -> InterruptMask {
        self.inner.lock().get_irq_mask()
    }

    fn take_tx(&mut self) -> Option<BTxQueue> {
        let mut taken = self.tx_taken.lock();
        if *taken {
            return None;
        }
        *taken = true;
        drop(taken);
        Some(Box::new(TxQueue {
            inner: self.inner.clone(),
            taken: self.tx_taken.clone(),
            base_addr: self.inner_base_addr(),
        }))
    }

    fn take_rx(&mut self) -> Option<BRxQueue> {
        let mut taken = self.rx_taken.lock();
        if *taken {
            return None;
        }
        *taken = true;
        drop(taken);
        Some(Box::new(RxQueue {
            inner: self.inner.clone(),
            taken: self.rx_taken.clone(),
            base_addr: self.inner_base_addr(),
        }))
    }

    fn take_irq_handler(&mut self) -> Option<BIrqHandler> {
        let mut taken = self.irq_taken.lock();
        if *taken {
            return None;
        }
        *taken = true;
        drop(taken);
        Some(Box::new(IrqHandler {
            inner: self.inner.clone(),
            taken: self.irq_taken.clone(),
            base_addr: self.inner_base_addr(),
        }))
    }

    fn set_tx(&mut self, tx: BTxQueue) -> Result<(), SetBackError> {
        ensure_same_base(self.base_addr(), tx.base_addr())?;
        *self.tx_taken.lock() = false;
        Ok(())
    }

    fn set_rx(&mut self, rx: BRxQueue) -> Result<(), SetBackError> {
        ensure_same_base(self.base_addr(), rx.base_addr())?;
        *self.rx_taken.lock() = false;
        Ok(())
    }

    fn set_irq_handler(&mut self, irq: BIrqHandler) -> Result<(), SetBackError> {
        ensure_same_base(self.base_addr(), irq.base_addr())?;
        *self.irq_taken.lock() = false;
        Ok(())
    }
}

impl<T: InterfaceRaw> DriverGeneric for SerialDyn<T> {
    fn name(&self) -> &str {
        &self.name
    }

    fn raw_any(&self) -> Option<&dyn core::any::Any> {
        Some(self)
    }

    fn raw_any_mut(&mut self) -> Option<&mut dyn core::any::Any> {
        Some(self)
    }
}

pub struct TxQueue<T: InterfaceRaw> {
    inner: Arc<Mutex<T>>,
    taken: Arc<Mutex<bool>>,
    base_addr: usize,
}

impl<T: InterfaceRaw> Drop for TxQueue<T> {
    fn drop(&mut self) {
        *self.taken.lock() = false;
    }
}

impl<T: InterfaceRaw> TTxQueue for TxQueue<T> {
    fn base_addr(&self) -> usize {
        self.base_addr
    }

    fn poll(&mut self) -> SerialEvent {
        let mut inner = self.inner.lock();
        if inner.pending(SerialDirection::Output) {
            SerialEvent::TX_READY
        } else {
            SerialEvent::empty()
        }
    }

    fn try_write(&mut self, bytes: &[u8]) -> usize {
        self.inner.lock().try_write(bytes)
    }
}

pub struct RxQueue<T: InterfaceRaw> {
    inner: Arc<Mutex<T>>,
    taken: Arc<Mutex<bool>>,
    base_addr: usize,
}

impl<T: InterfaceRaw> Drop for RxQueue<T> {
    fn drop(&mut self) {
        *self.taken.lock() = false;
    }
}

impl<T: InterfaceRaw> TRxQueue for RxQueue<T> {
    fn base_addr(&self) -> usize {
        self.base_addr
    }

    fn poll(&mut self) -> SerialEvent {
        let mut inner = self.inner.lock();
        inner.poll() & (SerialEvent::RX_READY | SerialEvent::RX_ERROR | SerialEvent::OVERRUN)
    }

    fn try_read(&mut self, bytes: &mut [u8]) -> Result<usize, TransBytesError> {
        self.inner.lock().try_read(bytes)
    }
}

pub struct IrqHandler<T: InterfaceRaw> {
    inner: Arc<Mutex<T>>,
    taken: Arc<Mutex<bool>>,
    base_addr: usize,
}

impl<T: InterfaceRaw> Drop for IrqHandler<T> {
    fn drop(&mut self) {
        *self.taken.lock() = false;
    }
}

impl<T: InterfaceRaw> TIrqHandler for IrqHandler<T> {
    fn base_addr(&self) -> usize {
        self.base_addr
    }

    fn handle_irq(&self) -> SerialEvent {
        self.inner.lock().handle_irq()
    }
}

fn ensure_same_base(want: usize, actual: usize) -> Result<(), SetBackError> {
    if want == actual {
        Ok(())
    } else {
        Err(SetBackError::new(want, actual))
    }
}
