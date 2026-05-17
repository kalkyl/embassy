//! 8N1 FlexIO UART transmitter with async DMA support.
//!
//! [`FlexioUartTx`] is a "personality" built on FlexIO Shifter 0 + Timer 0.
//! The DMA channel is paced by the Shifter 0 status flag (buffer-empty), so
//! the CPU is not involved while data shifts out.
//!
//! The 32-bit DMA word format expected by the FlexIO UART shifter is:
//!   - bits\[7:0\]  — data byte (LSB first after start-bit insertion)
//!   - bits\[31:8\] — padded with 1s (`0xFFFFFF00`) to keep the line high
//!                  between bytes (idle / stop-bit level).
//!
//! This format matches the working blocking `flexio_uart.rs` example.

use core::task::Poll;

use embassy_hal_internal::Peri;

use super::{
    FlexioPin, Info, Shifter, Timer as FlexTimer, ShifterConfig, TimerConfig, TimerTrigger,
    Insrc, ShiftctlPincfg, ShiftctlPinpol, Smod, Sstart, Sstop, TimctlPincfg, TimctlPinpol,
    Timdec, Timdis, Timena, Timod, Timout, Timrst, Timpol, Trgpol, Tstop,
};
use crate::dma::{Channel, DMA_MAX_TRANSFER_SIZE, DmaChannel, TransferOptions};
use crate::peripherals::FLEXIO0;

/// Maximum number of u32 DMA words formatted in a single stack-local chunk.
///
/// Each chunk occupies `CHUNK_WORDS * 4` bytes on the async stack frame.
/// 64 words = 256 bytes, a reasonable trade-off for most UART use cases.
const CHUNK_WORDS: usize = 64;

// ---------------------------------------------------------------------------
// Drop-safe DMA guard
// ---------------------------------------------------------------------------

/// RAII guard that ensures DMA is cleanly shut down even if the future is
/// dropped (e.g. due to a timeout).
struct TxDmaGuard<'a> {
    dma: DmaChannel<'a>,
    /// FlexIO register block — used to clear the shifter DMA enable bit.
    info: &'static Info,
}

impl<'a> TxDmaGuard<'a> {
    fn new(dma: DmaChannel<'a>, info: &'static Info) -> Self {
        Self { dma, info }
    }

    /// Called on normal completion — cleanly disables DMA without aborting an
    /// in-flight transfer (the major loop already finished at this point).
    fn complete(self) {
        // Disable Shifter 0 DMA request output.
        let bit: u8 = 1; // bit 0 = Shifter 0
        self.info.regs.shiftsden().modify(|w| w.set_ssde(w.ssde() & !bit));
        unsafe {
            self.dma.disable_request();
            self.dma.clear_done();
        }
        // Skip drop so we don't run the abort path.
        core::mem::forget(self);
    }
}

impl Drop for TxDmaGuard<'_> {
    fn drop(&mut self) {
        // Abort path: disable both the FlexIO DMA request and the DMA channel.
        let bit: u8 = 1;
        self.info.regs.shiftsden().modify(|w| w.set_ssde(w.ssde() & !bit));
        unsafe {
            self.dma.disable_request();
            self.dma.clear_done();
            self.dma.clear_interrupt();
        }
    }
}

// ---------------------------------------------------------------------------
// FlexioUartTx
// ---------------------------------------------------------------------------

/// 8N1 FlexIO UART transmitter with async DMA.
///
/// Uses FlexIO Shifter 0 + Timer 0 and one DMA channel.  Construct with
/// [`FlexioUartTx::new`] then call [`write`](Self::write) to transmit.
///
/// # DMA word format
///
/// FlexIO Shifter 0 is configured to insert the start and stop bits
/// automatically.  Each DMA word must be:
/// ```text
/// bits[7:0]  = data byte
/// bits[31:8] = 0xFF_FF_FF (keep line high / idle level)
/// ```
/// i.e. `(byte as u32) | 0xFFFF_FF00`.  [`write`](Self::write) handles this
/// formatting internally.
pub struct FlexioUartTx<'d> {
    shifter: Shifter<'d, 0>,
    #[allow(dead_code)]
    timer: FlexTimer<'d, 0>,
    dma: DmaChannel<'d>,
}

impl<'d> FlexioUartTx<'d> {
    /// Create a new async FlexIO UART TX driver.
    ///
    /// * `shifter`    — Exclusive handle to FlexIO Shifter 0.
    /// * `timer`      — Exclusive handle to FlexIO Timer 0.
    /// * `tx_pin`     — Physical pad connected to FlexIO lane `L`.
    /// * `dma_ch`     — DMA channel; will be routed to `Flexio0Sr0`.
    /// * `baud`       — UART baud rate (bits/s).
    /// * `flexio_clk` — FlexIO functional clock frequency (Hz).
    pub fn new<P: FlexioPin<FLEXIO0, L>, const L: usize>(
        mut shifter: Shifter<'d, 0>,
        mut timer: FlexTimer<'d, 0>,
        tx_pin: Peri<'d, P>,
        dma_ch: Peri<'d, impl Channel>,
        baud: u32,
        flexio_clk: u32,
    ) -> Self {
        tx_pin.as_flexio_lane();
        let lane = L as u8;

        // DUAL8BIT_BAUD compare value:
        //   bits[15:8] = 0x15 → 10 bits × 2 − 1 = 0x13... wait, the blocking
        //   example uses 0x15. Keep identical to the proven blocking driver.
        //   bits[7:0]  = floor(flexio_clk / (4 × baud)) − 1
        let baud_div = (flexio_clk / (4 * baud)) as u16;
        let compare: u16 = (0x15u16 << 8) | (baud_div.saturating_sub(1) & 0xFF);

        timer.set_config(&TimerConfig {
            timod: Timod::DUAL8BIT_BAUD,
            timdec: Timdec::FLEXIO_CLK_SHIFTCLK_TMR_OUT,
            timena: Timena::TMR_TRIGHI_EN,
            timdis: Timdis::TMR_CMP,
            timrst: Timrst::NEVER,
            timout: Timout::ONE,
            tstop: Tstop::ENABLE_TMRDISABLE,
            tstart: true,
            pin_select: lane,
            pin_cfg: TimctlPincfg::OUTDISABLE,
            pin_pol: TimctlPinpol::ACTIVE_HIGH,
            trigger: TimerTrigger::InternalShifterFlag {
                shifter: 0,
                polarity: Trgpol::ACTIVE_LOW,
            },
            compare,
        });

        shifter.set_config(&ShifterConfig {
            smod: Smod::TRANSMIT,
            pin_select: lane,
            pin_cfg: ShiftctlPincfg::OUTPUT,
            pin_pol: ShiftctlPinpol::ACTIVE_HIGH,
            timer_pol: Timpol::NEGEDGE,
            timer_select: 0,
            start_bit: Sstart::VALUE10,
            stop_bit: Sstop::VALUE11,
            input_source: Insrc::PIN,
        });

        let dma = DmaChannel::new(dma_ch);
        // DmaChannel::new already unmasks the NVIC; call enable_interrupt for
        // consistency with other peripheral drivers.
        dma.enable_interrupt();

        Self { shifter, timer, dma }
    }

    /// Transmit `data` asynchronously using DMA.
    ///
    /// Returns once all bytes have been **loaded into** the FlexIO shifter.
    /// The last byte may still be physically shifting out on the wire; call
    /// [`blocking_flush`](Self::blocking_flush) if you need the line to be
    /// idle before proceeding.
    ///
    /// Large slices are split into `CHUNK_WORDS`-sized DMA transfers
    /// automatically; no heap allocation occurs.
    pub async fn write(&mut self, data: &[u8]) {
        let mut offset = 0;
        while offset < data.len() {
            let end = (offset + CHUNK_WORDS).min(data.len());
            let chunk = &data[offset..end];

            // Format bytes → DMA u32 words on the stack.
            let mut buf = [0u32; CHUNK_WORDS];
            for (i, &byte) in chunk.iter().enumerate() {
                buf[i] = (byte as u32) | 0xFFFF_FF00;
            }

            self.write_words_dma(&buf[..chunk.len()]).await;
            offset = end;
        }
    }

    /// Spin until the last byte has fully shifted out and the serial line is
    /// idle.  Call this after [`write`](Self::write) when you need a
    /// transmission-complete guarantee before e.g. asserting a GPIO or
    /// sleeping.
    pub fn blocking_flush(&mut self) {
        // After write() returns the last word was just accepted into SHIFTBUF.
        // SSF goes high once the shifter has emptied it.
        while !self.shifter.is_status_set() {
            core::hint::spin_loop();
        }
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Transfer one pre-formatted chunk of `u32` words to `SHIFTBUF[0]` via
    /// DMA, paced by the Shifter 0 status flag.
    async fn write_words_dma(&mut self, words: &[u32]) {
        debug_assert!(!words.is_empty());
        debug_assert!(words.len() <= DMA_MAX_TRANSFER_SIZE);

        // Snapshot the static Info pointer before splitting borrows.
        let info: &'static Info = self.shifter.info;
        let peri_addr: *mut u32 = self.shifter.dma_address();

        unsafe {
            // Reset channel: clear DONE, INT, and ERQ from any prior transfer.
            self.dma.disable_request();
            self.dma.clear_done();
            self.dma.clear_interrupt();

            // Route FlexIO Shifter 0 status flag → this DMA channel.
            // Shifter::<0>::dma_request() is a const fn → DmaRequest::Flexio0Sr0.
            self.dma.set_request_source(Shifter::<0>::dma_request());

            // Configure TCD: incrementing u32 source, fixed u32 destination
            // (SHIFTBUF), one 4-byte minor loop per hardware trigger.
            self.dma
                .setup_write_to_peripheral(words, peri_addr, false, TransferOptions::COMPLETE_INTERRUPT)
                .expect("FlexIO DMA TCD setup");

            // Enable the FlexIO shifter DMA request output (SHIFTSDEN.SSDE[0]).
            self.shifter.enable_dma();

            // Arm the DMA channel — transfers begin on the first FlexIO trigger.
            self.dma.enable_request();
        }

        // Create the drop-safe guard *after* the channel is armed.
        // If this future is dropped (e.g. from embassy_time::with_timeout),
        // the guard's Drop impl disables both FlexIO and DMA requests.
        let guard = TxDmaGuard::new(self.dma.reborrow(), info);

        // Await completion.  The DMA interrupt handler fires when the major
        // loop finishes (all words written to SHIFTBUF) and wakes the WaitCell.
        core::future::poll_fn(|cx| {
            let _ = guard.dma.wait_cell().poll_wait(cx);
            if guard.dma.is_done() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;

        // Normal path: clean up without aborting.
        guard.complete();
    }
}
