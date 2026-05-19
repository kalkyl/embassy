//! FlexIO UART Tx DMA example (async).
//!
//! Demonstrates using the FlexIO HAL to implement a software UART transmitter
//! that sends bytes via DMA — the CPU is not involved while data shifts out.
//!
//! Hardware: FRDM-MCXA266 (mcxa2xx).
//!
//! Wiring: connect a USB-UART adapter to P3_28 (FXIO_D0, alt-6).
//!
//! Terminal: 115200 8N1.

#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_time::Timer;
use hal::clocks::PoweredClock;
use hal::clocks::periph_helpers::{Div4, FlexioClockSel, FlexioConfig};
use hal::dma::{Channel, InvalidParameters, TransferErrors};
use hal::flexio::{
    Flexio, FlexioPin, Insrc, ShiftctlPincfg, ShiftctlPinpol, Shifter, ShifterConfig, ShifterDma,
    Smod, Sstart, Sstop, TimctlPincfg, TimctlPinpol, Timdec, Timdis, Timena, Timod, TimerConfig,
    TimerTrigger, Timer as FlexTimer, Timpol, Timout, Timrst, Trgpol, Tstop,
};
use hal::Peri;
use hal::peripherals::FLEXIO0;
use embassy_mcxa::clocks::config::Div8;
use {defmt_rtt as _, embassy_mcxa as hal, panic_probe as _};

#[derive(Debug, defmt::Format)]
pub enum Error {
    /// Buffer was empty or exceeded the DMA transfer size limit.
    InvalidParameters,
    /// DMA controller reported a transfer error.
    Transfer,
}

impl From<InvalidParameters> for Error {
    fn from(_: InvalidParameters) -> Self {
        Error::InvalidParameters
    }
}

impl From<TransferErrors> for Error {
    fn from(_: TransferErrors) -> Self {
        Error::Transfer
    }
}

pub struct FlexioUartTx<'d> {
    sdma: ShifterDma<'d, 'd, 0>,
    #[allow(dead_code)]
    timer: FlexTimer<'d, 0>,
}

impl<'d> FlexioUartTx<'d> {
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

        let baud_div = (flexio_clk / (2 * baud)) as u16;
        // upper byte = (10 bits × 2) − 1: upper counter decrements every half-period
        let compare: u16 = (0x13u16 << 8) | (baud_div.saturating_sub(1) & 0xFF);

        timer.set_config(&TimerConfig {
            timod: Timod::Dual8bitBaud,
            timdec: Timdec::FlexioClkShiftclkTmrOut,
            timena: Timena::TmrTrighiEn,
            timdis: Timdis::TmrCmp,
            timrst: Timrst::Never,
            timout: Timout::One,
            tstop: Tstop::EnableTmrdisable,
            tstart: true,
            pin_select: lane,
            pin_cfg: TimctlPincfg::Outdisable,
            pin_pol: TimctlPinpol::ActiveHigh,
            trigger: TimerTrigger::InternalShifterFlag {
                shifter: 0,
                polarity: Trgpol::ActiveLow,
            },
            compare,
        });

        shifter.set_config(&ShifterConfig {
            smod: Smod::Transmit,
            pin_select: lane,
            pin_cfg: ShiftctlPincfg::Output,
            pin_pol: ShiftctlPinpol::ActiveHigh,
            timer_pol: Timpol::Negedge,
            timer_select: 0,
            start_bit: Sstart::Value10,
            stop_bit: Sstop::Value11,
            input_source: Insrc::Pin,
        });

        Self {
            sdma: shifter.with_dma(dma_ch),
            timer,
        }
    }

    pub async fn write(&mut self, data: &[u8]) -> Result<(), Error> {
        const CHUNK: usize = 64;
        let mut offset = 0;
        while offset < data.len() {
            let end = (offset + CHUNK).min(data.len());
            let chunk = &data[offset..end];
            let mut buf = [0u16; CHUNK];
            for (i, &byte) in chunk.iter().enumerate() {
                // start(0) at bit 0, data in bits 1-8, stop(1) at bit 9
                buf[i] = ((byte as u16) << 1) | 0xFE00;
            }
            self.sdma.write(&buf[..chunk.len()]).await?;
            offset = end;
        }
        Ok(())
    }

    /// Spin until the last byte has fully shifted out and the line is idle.
    pub fn blocking_flush(&mut self) {
        while !self.sdma.shifter().is_status_set() {
            core::hint::spin_loop();
        }
    }
}

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let mut cfg = hal::config::Config::default();
    cfg.clock_cfg.sirc.fro_12m_enabled = true;
    cfg.clock_cfg.sirc.fro_lf_div = Some(Div8::no_div());
    let p = hal::init(cfg);

    defmt::info!("FlexIO UART DMA Tx example");

    // FRO_HF defaults to 45 MHz on MCXA2xx; 45 / 3 = 15 MHz.
    let flexio_clk_hz = 15_000_000u32;
    let flexio_cfg = FlexioConfig {
        power: PoweredClock::NormalEnabledDeepSleepDisabled,
        source: FlexioClockSel::FroHfGated,
        div: Div4::from_divisor(3).unwrap(),
    };

    let flexio = Flexio::new(p.FLEXIO0, flexio_cfg).expect("FlexIO init failed");
    let (mut shifters, mut timers) = flexio.split();

    let mut uart = FlexioUartTx::new(
        shifters.take::<0>().unwrap(),
        timers.take::<0>().unwrap(),
        p.P3_28,
        p.DMA0_CH0,
        115_200,
        flexio_clk_hz,
    );

    loop {
        uart.write(b"hello world\r\n").await.unwrap();
        uart.blocking_flush();
        Timer::after_millis(1000).await;
    }
}
