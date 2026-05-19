//! FlexIO I2S TX DMA example (async).
//!
//! Streams audio from a 1024-word circular buffer to the shifter via DMA.
//! INTHALF and INTMAJOR interrupts fire at the midpoint and end of each pass,
//! giving the CPU one half-period to refill the idle half while hardware
//! streams the other.
//!
//! Hardware: FRDM-MCXA266 (mcxa2xx).
//!
//! Wiring (Arduino header, alt-6):
//!   P4_4 → BCLK  (FXIO_D10, lane 10)
//!   P4_1 → WS    (FXIO_D11, lane 11)
//!   P0_7 → SD    (FXIO_D12, lane 12)
//!
//! Note: BCLK must be on a lane ≤ 15 so that the Timer 1 trigger-select
//! value (4 × lane) fits in the 6-bit TRGSEL field.

#![no_std]
#![no_main]

use embassy_executor::Spawner;
use hal::clocks::PoweredClock;
use hal::clocks::periph_helpers::{Div4, FlexioClockSel, FlexioConfig};
use hal::dma::{Channel, InvalidParameters, TransferErrors};
use hal::flexio::{
    Flexio, FlexioPin, Insrc, ShiftctlPincfg, ShiftctlPinpol, Shifter, ShifterConfig, ShifterDma,
    ShifterStream, Smod, Sstart, Sstop, TimctlPincfg, TimctlPinpol, Timdec, Timdis, Timena,
    Timod, TimerConfig, TimerTrigger, Timer as FlexTimer, Timpol, Timout, Timrst, Trgpol, Tstop,
};
use hal::Peri;
use hal::peripherals::FLEXIO0;
use embassy_mcxa::clocks::config::Div8;
use static_cell::ConstStaticCell;
use {defmt_rtt as _, embassy_mcxa as hal, panic_probe as _};

#[derive(Debug, defmt::Format)]
pub enum Error {
    InvalidParameters,
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

const HALF_LEN: usize = 512;

static BUF: ConstStaticCell<[u32; HALF_LEN * 2]> = ConstStaticCell::new([0; HALF_LEN * 2]);

pub struct FlexioI2sTx<'d> {
    sdma: ShifterDma<'d, 'd, 0>,
    #[allow(dead_code)]
    bclk_timer: FlexTimer<'d, 0>,
    #[allow(dead_code)]
    ws_timer: FlexTimer<'d, 1>,
}

/// Active streaming state returned by [`FlexioI2sTx::start_stream`].
pub struct I2sStream<'d> {
    stream: ShifterStream<'d, 'd, 0>,
    #[allow(dead_code)]
    _bclk_timer: FlexTimer<'d, 0>,
    #[allow(dead_code)]
    _ws_timer: FlexTimer<'d, 1>,
}

impl<'d> I2sStream<'d> {
    /// Wait for the next half-buffer to finish.
    ///
    /// Returns `Ok(0)` when the first half finished, `Ok(1)` when the second.
    /// Alternates strictly 0, 1, 0, 1, …
    pub async fn next_half(&mut self) -> Result<u8, Error> {
        self.stream.next_half().await.map_err(Error::from)
    }

    /// Call `f` with the half that [`next_half`] just reported idle.
    pub fn fill_idle<F: FnOnce(&mut [u32])>(&mut self, idx: u8, f: F) {
        self.stream.fill_idle(idx, f);
    }
}

impl<'d> FlexioI2sTx<'d> {
    pub fn new<
        PB: FlexioPin<FLEXIO0, LB>,
        PW: FlexioPin<FLEXIO0, LW>,
        PS: FlexioPin<FLEXIO0, LS>,
        const LB: usize,
        const LW: usize,
        const LS: usize,
    >(
        mut shifter: Shifter<'d, 0>,
        mut bclk_timer: FlexTimer<'d, 0>,
        mut ws_timer: FlexTimer<'d, 1>,
        sclk_pin: Peri<'d, PB>,
        ws_pin: Peri<'d, PW>,
        sd_pin: Peri<'d, PS>,
        dma_ch: Peri<'d, impl Channel>,
        sample_rate: u32,
        flexio_clk: u32,
    ) -> Self {
        assert!(LB <= 15, "BCLK lane must be ≤ 15 (TRGSEL is 6-bit)");

        sclk_pin.as_flexio_lane();
        ws_pin.as_flexio_lane();
        sd_pin.as_flexio_lane();

        let bclk_lane = LB as u8;
        let ws_lane = LW as u8;
        let sd_lane = LS as u8;

        let bclk_freq = sample_rate.saturating_mul(32);
        let t0_lower = ((flexio_clk + bclk_freq) / (2 * bclk_freq)).saturating_sub(1) as u16;
        let t0_compare = (31u16 << 8) | t0_lower;
        let ws_compare = 32 * (t0_lower + 1) - 1;

        ws_timer.set_config(&TimerConfig {
            timod: Timod::Single16bit,
            timdec: Timdec::FlexioClkShiftclkTmrOut,
            timena: Timena::TmrNminus1En,
            timdis: Timdis::Never,
            timrst: Timrst::Never,
            timout: Timout::One,
            tstop: Tstop::StopDisable,
            tstart: false,
            pin_select: ws_lane,
            pin_cfg: TimctlPincfg::Output,
            pin_pol: TimctlPinpol::ActiveLow,
            trigger: TimerTrigger::None,
            compare: ws_compare,
        });

        bclk_timer.set_config(&TimerConfig {
            timod: Timod::Dual8bitBaud,
            timdec: Timdec::FlexioClkShiftclkTmrOut,
            timena: Timena::TmrTrighiEn,
            timdis: Timdis::Never,
            timrst: Timrst::Never,
            timout: Timout::One,
            tstop: Tstop::StopDisable,
            tstart: true,
            pin_select: bclk_lane,
            pin_cfg: TimctlPincfg::Output,
            pin_pol: TimctlPinpol::ActiveHigh,
            trigger: TimerTrigger::InternalShifterFlag {
                shifter: 0,
                polarity: Trgpol::ActiveLow,
            },
            compare: t0_compare,
        });

        shifter.set_config(&ShifterConfig {
            smod: Smod::Transmit,
            pin_select: sd_lane,
            pin_cfg: ShiftctlPincfg::Output,
            pin_pol: ShiftctlPinpol::ActiveHigh,
            timer_pol: Timpol::Posedge,
            timer_select: 0,
            start_bit: Sstart::Value00,
            stop_bit: Sstop::Value00,
            input_source: Insrc::Pin,
        });

        Self {
            sdma: shifter.with_dma(dma_ch),
            bclk_timer,
            ws_timer,
        }
    }

    /// Consume the driver and start streaming `buf` to the I2S shifter.
    /// Pre-fill both halves before calling so the stream opens with valid audio.
    pub fn start_stream(self, buf: &'static mut [u32]) -> Result<I2sStream<'d>, Error> {
        let stream = self.sdma.start_stream_msb(buf)?;
        Ok(I2sStream {
            stream,
            _bclk_timer: self.bclk_timer,
            _ws_timer: self.ws_timer,
        })
    }
}

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let mut cfg = hal::config::Config::default();
    cfg.clock_cfg.sirc.fro_12m_enabled = true;
    cfg.clock_cfg.sirc.fro_lf_div = Some(Div8::no_div());
    let p = hal::init(cfg);

    defmt::info!("FlexIO I2S TX DMA example");

    // FRO_HF defaults to 45 MHz on MCXA2xx; 45 / 3 = 15 MHz.
    let flexio_clk_hz = 15_000_000u32;
    let flexio_cfg = FlexioConfig {
        power: PoweredClock::NormalEnabledDeepSleepDisabled,
        source: FlexioClockSel::FroHfGated,
        div: Div4::from_divisor(3).unwrap(),
    };

    let flexio = Flexio::new(p.FLEXIO0, flexio_cfg).expect("FlexIO init failed");
    let (mut shifters, mut timers) = flexio.split();

    let i2s = FlexioI2sTx::new(
        shifters.take::<0>().unwrap(),
        timers.take::<0>().unwrap(),
        timers.take::<1>().unwrap(),
        p.P4_4,    // BCLK → FXIO_D10, lane 10
        p.P4_1,    // WS   → FXIO_D11, lane 11
        p.P0_7,    // SD   → FXIO_D12, lane 12
        p.DMA0_CH0,
        48_000,
        flexio_clk_hz,
    );

    // Square-wave generator — each u32 packs a 16-bit left and right sample.
    let mut phase = 0u32;
    let mut fill = |half: &mut [u32]| {
        for word in half.iter_mut() {
            let s: u32 = if phase < 256 { 0x3FFF } else { 0xC001 };
            *word = (s << 16) | s;
            phase = (phase + 1) % 512;
        }
    };

    let mut stream = i2s.start_stream(BUF.take()).unwrap();

    loop {
        let idx = stream.next_half().await.unwrap();
        stream.fill_idle(idx, |half| fill(half));
    }
}
