//! FlexIO I2S TX ping-pong DMA example (async, zero-gap TCD chaining).
//!
//! Identical FlexIO wiring to `flexio_i2s_dma.rs`, but uses eDMA scatter-gather
//! to create a hardware-level circular TCD chain so the CPU never has to restart
//! the DMA between buffers.
//!
//! # How it works
//!
//! Two 32-byte-aligned TCDs live in `static` memory.  TCD A points to `BUF_A`
//! and has `dlast_sga → &TCD_B`; TCD B points to `BUF_B` and has
//! `dlast_sga → &TCD_A`.  Both have `INTMAJOR=1` so the DMA interrupt fires
//! at the end of every buffer.  `PingPongTransfer::next_half()` awaits those
//! interrupts and returns 0 (A done) or 1 (B done), giving the CPU a full
//! buffer period to refill the idle half with no gap in the audio stream.
//!
//! Hardware: FRDM-MCXA256 (mcxa2xx).
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
use hal::dma::{Channel, DmaChannel, InvalidParameters, PingPongPool, PingPongTransfer, TransferErrors};
use hal::flexio::{
    Flexio, FlexioPin, Insrc, ShiftctlPincfg, ShiftctlPinpol, Shifter, ShifterConfig, Smod,
    Sstart, Sstop, TimctlPincfg, TimctlPinpol, Timdec, Timdis, Timena, Timod, TimerConfig,
    TimerTrigger, Timer as FlexTimer, Timpol, Timout, Timrst, Trgpol, Tstop,
};
use hal::Peri;
use hal::peripherals::FLEXIO0;
use embassy_mcxa::clocks::config::Div8;
use static_cell::ConstStaticCell;
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
// Static DMA storage
//
// Both the TCD pool and the audio buffers must be 'static so that the
// scatter-gather addresses and DMA source pointers remain valid forever.
// ConstStaticCell gives safe one-shot initialisation without a Mutex.
// ---------------------------------------------------------------------------

const BUF_LEN: usize = 512;

static TCDS: ConstStaticCell<PingPongPool> = ConstStaticCell::new(PingPongPool::new());
static BUF_A: ConstStaticCell<[u32; BUF_LEN]> = ConstStaticCell::new([0; BUF_LEN]);
static BUF_B: ConstStaticCell<[u32; BUF_LEN]> = ConstStaticCell::new([0; BUF_LEN]);

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

        shifter.enable_dma();

        Self {
            shifter,
            bclk_timer,
            ws_timer,
            dma: DmaChannel::new(dma_ch),
        }
    }

    /// Start a continuous ping-pong DMA stream.
    ///
    /// `buf_a` and `buf_b` must be `'static`; pass the pointers you obtained
    /// from `ConstStaticCell::take()`.  Fill `buf_a` before calling this so
    /// the very first half has valid audio.
    ///
    /// # Safety
    ///
    /// Only write to a buffer after `next_half()` reports it finished.
    pub unsafe fn start_ping_pong(
        &mut self,
        pool: &'static mut PingPongPool,
        buf_a: *const u32,
        buf_b: *const u32,
        len: usize,
    ) -> Result<PingPongTransfer<'_>, Error> {
        let peri_addr = self.shifter.dma_address_bis();
        unsafe {
            self.dma
                .ping_pong_write_to_peripheral(
                    pool,
                    buf_a,
                    buf_b,
                    len,
                    peri_addr,
                    Shifter::<0>::dma_request(),
                )
                .map_err(Error::from)
        }
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

    defmt::info!("FlexIO I2S TX ping-pong TCD example");

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

    // Take ownership of the static buffers and TCD pool.
    let buf_a: &'static mut [u32; BUF_LEN] = BUF_A.take();
    let buf_b: &'static mut [u32; BUF_LEN] = BUF_B.take();
    let tcds: &'static mut PingPongPool = TCDS.take();

    // Square-wave generator used as a stand-in for a real audio source.
    // Each u32 word packs one 16-bit left sample (upper half) and one 16-bit
    // right sample (lower half).
    let mut phase = 0u32;
    let mut fill = |buf: &mut [u32; BUF_LEN]| {
        for word in buf.iter_mut() {
            let s: u32 = if phase < 256 { 0x3FFF } else { 0xC001 };
            *word = (s << 16) | s;
            phase = (phase + 1) % 512;
        }
    };

    // Pre-fill both halves before handing their addresses to DMA so the first
    // two buffer periods never output silence.
    fill(buf_a);
    fill(buf_b);

    // Start the circular TCD chain.  DMA immediately begins streaming buf_a.
    // SAFETY: buf_a and buf_b are 'static, properly aligned, and we respect
    // the ownership protocol below (only write to the buffer that just finished).
    let mut stream = unsafe {
        i2s.start_ping_pong(tcds, buf_a.as_ptr(), buf_b.as_ptr(), BUF_LEN)
            .unwrap()
    };

    // Main loop: refill whichever buffer the hardware just finished.
    //
    //   half == 0 → buf_a just finished (DMA is now streaming buf_b)  → refill buf_a
    //   half == 1 → buf_b just finished (DMA is now streaming buf_a)  → refill buf_b
    //
    // The CPU has exactly one buffer period (512 frames ≈ 10.9 ms at 46875 Hz)
    // to complete the fill before that buffer is needed again.
    loop {
        match stream.next_half().await.unwrap() {
            0 => fill(buf_a),
            _ => fill(buf_b),
        }
    }
}
