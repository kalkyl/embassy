//! FlexIO I2S TX DMA example (async, double-buffer).
//!
//! Implements a software I2S transmitter using three FlexIO resources:
//!   - Timer 0: BCLK (bit clock), continuous, ≈1.5 MHz → ≈46.9 kHz sample rate
//!   - Timer 1: WS (word-select / frame sync), chained on BCLK via trigger
//!   - Shifter 0: SD (serial data), 32-bit MSB-first words via DMA
//!
//! `FlexioI2sTx::write` eagerly starts the DMA transfer before returning, so
//! the caller can prepare the next audio buffer while hardware is busy.
//!
//! Hardware: FRDM-MCXA256 (mcxa2xx).
//!
//! Wiring (Arduino header, alt-6):
//!   P3_2 → BCLK  (FXIO_D10)
//!   P3_3 → WS    (FXIO_D11)
//!   P3_4 → SD    (FXIO_D12)
//!
//! Note: BCLK must be on a lane ≤ 15 so that the Timer 1 trigger-select
//! value (4 × lane) fits in the 6-bit TRGSEL field.

#![no_std]
#![no_main]

use core::future::Future;

use embassy_executor::Spawner;
use hal::clocks::PoweredClock;
use hal::clocks::periph_helpers::{Div4, FlexioClockSel, FlexioConfig};
use hal::dma::{Channel, DmaChannel, InvalidParameters, TransferErrors, TransferOptions};
use hal::flexio::{
    Flexio, FlexioPin, Insrc, ShiftctlPincfg, ShiftctlPinpol, Shifter, ShifterConfig, Smod,
    Sstart, Sstop, TimctlPincfg, TimctlPinpol, Timdec, Timdis, Timena, Timod, TimerConfig,
    TimerTrigger, Timer as FlexTimer, Timpol, Timout, Timrst, Trgpol, Tstop,
};
use hal::Peri;
use hal::peripherals::FLEXIO0;
use embassy_mcxa::clocks::config::Div8;
use {defmt_rtt as _, embassy_mcxa as hal, panic_probe as _};

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

pub struct FlexioI2sTx<'d> {
    shifter: Shifter<'d, 0>,
    #[allow(dead_code)]
    bclk_timer: FlexTimer<'d, 0>,
    #[allow(dead_code)]
    ws_timer: FlexTimer<'d, 1>,
    dma: DmaChannel<'d>,
}

impl<'d> FlexioI2sTx<'d> {
    /// Initialise and configure FlexIO for I2S TX.
    ///
    /// # Type parameters
    /// `LB`, `LW`, `LS` are the FXIO lane indices for BCLK, WS, and SD.
    /// `LB` must be ≤ 15 so that `4 × LB` fits in the 6-bit TIMCTL.TRGSEL field.
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

        // ── Timing calculations ────────────────────────────────────────────
        //
        // BCLK = sample_rate × 32 bits/frame (16-bit stereo L+R).
        // t0_lower = round(flexio_clk / (2 × bclk)) − 1.
        // At 15 MHz / 48 kHz: t0_lower = 4 → BCLK = 15 M / 10 = 1.5 MHz → 46 875 Hz.
        let bclk_freq = sample_rate.saturating_mul(32);
        let t0_lower =
            ((flexio_clk + bclk_freq) / (2 * bclk_freq)).saturating_sub(1) as u16;
        // Upper = bitWidth*2 - 1 = 16*2 - 1 = 31: 32 BCLKs per DMA trigger (one stereo frame).
        let t0_compare = (31u16 << 8) | t0_lower;

        // WS half-period in FlexIO clock cycles = 16 BCLKs × BCLK_period
        //   = 16 × 2 × (t0_lower + 1) FlexIO cycles
        //   = 32 × (t0_lower + 1) − 1  (for SINGLE16BIT compare)
        // At 15 MHz / 46 875 Hz: ws_compare = 32 × 5 − 1 = 159.
        let ws_compare = 32 * (t0_lower + 1) - 1;

        // ── Timer 1: WS ───────────────────────────────────────────────────
        //
        // Configured FIRST so that TMR_NMINUS1_EN catches Timer 0's rising
        // enable edge when Timer 0 is configured below.
        //
        // SINGLE16BIT mode: the 16-bit counter decrements on every FlexIO
        // clock cycle and TOGGLES the pin output when it reaches zero, then
        // reloads.  With ws_compare = 255 this produces a 50 % duty-cycle WS
        // that toggles every 16 BCLKs.  This matches the NXP SDK reference
        // implementation (fsl_flexio_i2s.c).
        ws_timer.set_config(&TimerConfig {
            timod: Timod::SINGLE16BIT,
            timdec: Timdec::FLEXIO_CLK_SHIFTCLK_TMR_OUT,
            timena: Timena::TMR_NMINUS1_EN,
            timdis: Timdis::NEVER,
            timrst: Timrst::NEVER,
            timout: Timout::ONE,
            tstop: Tstop::STOP_DISABLE,
            tstart: false,
            pin_select: ws_lane,
            pin_cfg: TimctlPincfg::OUTPUT,
            pin_pol: TimctlPinpol::ACTIVE_LOW,
            trigger: TimerTrigger::None,
            compare: ws_compare,
        });

        // ── Timer 0: BCLK ─────────────────────────────────────────────────
        //
        // Configured AFTER Timer 1 so that Timer 1's TMR_NMINUS1_EN fires on
        // the rising enable edge produced here.  Both timers then run from the
        // same FlexIO clock, keeping WS phase-locked to BCLK.
        bclk_timer.set_config(&TimerConfig {
            timod: Timod::DUAL8BIT_BAUD,
            timdec: Timdec::FLEXIO_CLK_SHIFTCLK_TMR_OUT,
            timena: Timena::TMR_TRIGHI_EN,
            timdis: Timdis::NEVER,
            timrst: Timrst::NEVER,
            timout: Timout::ONE,
            tstop: Tstop::STOP_DISABLE,
            tstart: true,
            pin_select: bclk_lane,
            pin_cfg: TimctlPincfg::OUTPUT,
            pin_pol: TimctlPinpol::ACTIVE_HIGH,
            trigger: TimerTrigger::InternalShifterFlag {
                shifter: 0,
                polarity: Trgpol::ACTIVE_LOW,
            },
            compare: t0_compare,
        });

        // ── Shifter 0: SD ─────────────────────────────────────────────────
        //
        // Transmit mode, 32-bit MSB-first (standard I2S, no bit-reversal).
        // Data shifts out on the falling edge of BCLK (NEGEDGE), stable for
        // the receiver to sample on the rising edge.
        // No start/stop bits: the 1-bit WS-to-data delay is controlled by
        // the timer relationship established above.
        shifter.set_config(&ShifterConfig {
            smod: Smod::TRANSMIT,
            pin_select: sd_lane,
            pin_cfg: ShiftctlPincfg::OUTPUT,
            pin_pol: ShiftctlPinpol::ACTIVE_HIGH,
            timer_pol: Timpol::POSEDGE,
            timer_select: 0,
            start_bit: Sstart::VALUE00,
            stop_bit: Sstop::VALUE00,
            input_source: Insrc::PIN,
        });

        // DMA enable is permanent: the DMA channel's ERQ bit gates actual
        // transfers, so there are no spurious requests between writes.
        shifter.enable_dma();

        Self {
            shifter,
            bclk_timer,
            ws_timer,
            dma: DmaChannel::new(dma_ch),
        }
    }

    /// Eagerly start a DMA transfer of `data` to the I2S shifter buffer.
    ///
    /// The hardware begins clocking out samples **before** the returned future
    /// is polled, so the caller can prepare the next audio buffer concurrently:
    ///
    /// ```rust,ignore
    /// let send = i2s.write(&front)?;   // DMA starts now
    /// prepare(&mut back);              // CPU fills next buffer
    /// send.await?;                     // wait for DMA to finish
    /// core::mem::swap(&mut front, &mut back);
    /// ```
    pub fn write<'a>(
        &'a mut self,
        data: &'a [u32],
    ) -> Result<impl Future<Output = Result<(), Error>> + 'a, Error> {
        let peri_addr = self.shifter.dma_address_bis();
        // write_to_peripheral calls start_transfer() before returning, so the
        // DMA is already running when this function returns.
        let xfr = unsafe {
            self.dma
                .write_to_peripheral(
                    data,
                    peri_addr,
                    Shifter::<0>::dma_request(),
                    TransferOptions::COMPLETE_INTERRUPT,
                )
                .map_err(Error::from)?
        };
        Ok(async move { xfr.await.map_err(Error::from) })
    }
}

// ---------------------------------------------------------------------------
// Application
// ---------------------------------------------------------------------------

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let mut cfg = hal::config::Config::default();
    cfg.clock_cfg.sirc.fro_12m_enabled = true;
    cfg.clock_cfg.sirc.fro_lf_div = Some(Div8::no_div());
    let p = hal::init(cfg);

    defmt::info!("FlexIO I2S TX DMA example");

    // FlexIO clock: FRO_HF default 45 MHz ÷ 3 = 15 MHz
    let flexio_cfg = FlexioConfig {
        power: PoweredClock::NormalEnabledDeepSleepDisabled,
        source: FlexioClockSel::FroHfGated,
        div: Div4::from_divisor(3).unwrap(),
    };

    let flexio = Flexio::new(p.FLEXIO0, flexio_cfg).expect("FlexIO init failed");
    let (mut shifters, mut timers) = flexio.split();

    let mut i2s = FlexioI2sTx::new(
        shifters.take::<0>().unwrap(),
        timers.take::<0>().unwrap(),
        timers.take::<1>().unwrap(),
        p.P4_4,    // BCLK → FXIO_D10, lane 10
        p.P4_1,    // WS   → FXIO_D11, lane 11
        p.P0_7,    // SD   → FXIO_D12, lane 12
        p.DMA0_CH0,
        48_000,
        15_000_000,
    );

    // Double-buffer: `front` is always the buffer being sent by DMA.
    // `back` is the buffer being filled by the CPU.
    let mut front = [0u32; 512];
    let mut back = [0u32; 512];

    // Generate a simple square-wave pattern as a placeholder audio source.
    // Each u32 word packs one 16-bit left sample (upper half) and one 16-bit
    // right sample (lower half).
    let mut phase = 0u32;
    let mut fill = |buf: &mut [u32; 512]| {
        for word in buf.iter_mut() {
            let s: u32 = if phase < 256 { 0x3FFF } else { 0xC001 };
            *word = (s << 16) | s;
            phase = (phase + 1) % 512;
        }
    };

    fill(&mut front);

    loop {
        // Eagerly start DMA on `front` — hardware is clocking out bits now.
        let send = i2s.write(&front).unwrap();
        //defmt::trace!("frame");

        // CPU prepares `back` while DMA is running.
        fill(&mut back);

        // Block until the DMA transfer completes.
        send.await.unwrap();

        // `front` is now free; swap so the freshly prepared buffer goes out next.
        core::mem::swap(&mut front, &mut back);

    }
}
