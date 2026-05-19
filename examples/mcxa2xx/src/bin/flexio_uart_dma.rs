//! FlexIO UART Tx DMA example (async).
//!
//! Demonstrates using the FlexIO HAL to implement a software UART transmitter
//! that sends bytes via DMA — the CPU is not involved while data shifts out.
//!
//! Hardware: FRDM-MCXA256 (mcxa2xx).
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
use hal::dma::{Channel, DmaChannel, DmaRequest, InvalidParameters, TransferErrors, TransferOptions};
use hal::flexio::{
    Flexio, FlexioPin, Insrc, ShiftctlPincfg, ShiftctlPinpol, Shifter, ShifterConfig, Smod,
    Sstart, Sstop, TimctlPincfg, TimctlPinpol, Timdec, Timdis, Timena, Timod, TimerConfig,
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

const CHUNK_WORDS: usize = 64;

pub struct FlexioUartTx<'d> {
    shifter: Shifter<'d, 0>,
    #[allow(dead_code)]
    timer: FlexTimer<'d, 0>,
    dma: DmaChannel<'d>,
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

        let baud_div = (flexio_clk / (4 * baud)) as u16;
        let compare: u16 = (0x15u16 << 8) | (baud_div.saturating_sub(1) & 0xFF);

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
            shifter,
            timer,
            dma: DmaChannel::new(dma_ch),
        }
    }

    pub async fn write(&mut self, data: &[u8]) -> Result<(), Error> {
        let mut offset = 0;
        while offset < data.len() {
            let end = (offset + CHUNK_WORDS).min(data.len());
            let chunk = &data[offset..end];

            let mut buf = [0u32; CHUNK_WORDS];
            for (i, &byte) in chunk.iter().enumerate() {
                buf[i] = (byte as u32) | 0xFFFF_FF00;
            }

            self.write_words_dma(&buf[..chunk.len()]).await?;
            offset = end;
        }
        Ok(())
    }

    /// Spin until the last byte has fully shifted out and the line is idle.
    pub fn blocking_flush(&mut self) {
        while !self.shifter.is_status_set() {
            core::hint::spin_loop();
        }
    }

    async fn write_words_dma(&mut self, words: &[u32]) -> Result<(), Error> {
        let peri_addr = self.shifter.dma_address();
        self.shifter.enable_dma();

        let transfer = unsafe {
            self.dma.write_to_peripheral(
                words,
                peri_addr,
                Shifter::<0>::dma_request(),
                TransferOptions::COMPLETE_INTERRUPT,
            )?
        };

        let result = transfer.await.map_err(Error::from);
        self.shifter.disable_dma();
        result
    }
}

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let mut cfg = hal::config::Config::default();
    cfg.clock_cfg.sirc.fro_12m_enabled = true;
    cfg.clock_cfg.sirc.fro_lf_div = Some(Div8::no_div());
    let p = hal::init(cfg);

    defmt::info!("FlexIO UART DMA Tx example");

    let flexio_cfg = FlexioConfig {
        power: PoweredClock::NormalEnabledDeepSleepDisabled,
        source: FlexioClockSel::FroHfGated,
        div: Div4::from_divisor(4).unwrap(),
    };

    let flexio = Flexio::new(p.FLEXIO0, flexio_cfg).expect("FlexIO init failed");
    let (mut shifters, mut timers) = flexio.split();

    let mut uart = FlexioUartTx::new(
        shifters.take::<0>().unwrap(),
        timers.take::<0>().unwrap(),
        p.P3_28,
        p.DMA0_CH0,
        115_200,
        24_000_000,
    );

    let mut counter: u32 = 0;
    loop {
        uart.write(b"Hello from FlexIO UART DMA! count=").await.unwrap();
        let mut buf = [0u8; 10];
        uart.write(fmt_u32(counter, &mut buf)).await.unwrap();
        uart.write(b"\r\n").await.unwrap();
        uart.blocking_flush();

        defmt::info!("Sent line {}", counter);
        counter = counter.wrapping_add(1);
        Timer::after_millis(1000).await;
    }
}

fn fmt_u32(mut n: u32, buf: &mut [u8; 10]) -> &[u8] {
    if n == 0 {
        buf[9] = b'0';
        return &buf[9..];
    }
    let mut i = 10usize;
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    &buf[i..]
}
