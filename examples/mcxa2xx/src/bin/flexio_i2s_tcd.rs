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
//! at the end of every buffer.  `I2sStream::next_half()` awaits those
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

const BUF_LEN: usize = 512;

static TCDS: ConstStaticCell<PingPongPool> = ConstStaticCell::new(PingPongPool::new());
static BUF_A: ConstStaticCell<[u32; BUF_LEN]> = ConstStaticCell::new([0; BUF_LEN]);
static BUF_B: ConstStaticCell<[u32; BUF_LEN]> = ConstStaticCell::new([0; BUF_LEN]);


pub struct FlexioI2sTx<'d> {
    shifter: Shifter<'d, 0>,
    #[allow(dead_code)]
    bclk_timer: FlexTimer<'d, 0>,
    #[allow(dead_code)]
    ws_timer: FlexTimer<'d, 1>,
    dma: DmaChannel<'d>,
}

/// Active streaming state returned by [`FlexioI2sTx::start_stream`].
pub struct I2sStream<'d> {
    _shifter: Shifter<'d, 0>,
    _bclk_timer: FlexTimer<'d, 0>,
    _ws_timer: FlexTimer<'d, 1>,
    transfer: PingPongTransfer<'d, u32>,
}

impl<'d> I2sStream<'d> {
    /// Wait for the next buffer to finish transmitting.
    ///
    /// Returns `Ok(0)` when `buf_a` completed, `Ok(1)` when `buf_b` completed.
    pub async fn next_half(&mut self) -> Result<u8, Error> {
        self.transfer.next_half().await.map_err(Error::from)
    }

    /// Refill the buffer that [`next_half`] just reported as idle.
    ///
    /// Calls `f` with a `&mut [u32]` slice of that buffer.  Because this takes
    /// `&mut self`, the borrow checker prevents calling `next_half` again until
    /// `f` returns — enforcing that writes finish before the DMA loops back.
    pub fn fill_idle<F: FnOnce(&mut [u32])>(&mut self, idx: u8, f: F) {
        self.transfer.fill_idle(idx, f);
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

    /// Consume the driver and start a continuous ping-pong DMA stream.
    ///
    /// Both audio buffers and the TCD pool are passed in as `&'static mut`
    /// references (obtained from `ConstStaticCell::take()`).  They are consumed
    /// here, so no other code holds a live alias while DMA is running.
    ///
    /// Pre-fill `buf_a` (and optionally `buf_b`) before calling so the first
    /// buffer period contains valid audio rather than silence.
    pub fn start_stream(
        self,
        pool: &'static mut PingPongPool,
        buf_a: &'static mut [u32],
        buf_b: &'static mut [u32],
    ) -> Result<I2sStream<'d>, Error> {
        let peri_addr = self.shifter.dma_address_bis();
        let len = buf_a.len();
        // SAFETY:
        // - buf_a / buf_b are 'static, and we consume them here (no live &mut alias).
        // - len is their known length; both slices were created with the same BUF_LEN.
        // - peri_addr is the FlexIO shifter register, always valid.
        // - After this call the only CPU write path is through I2sStream::fill_idle.
        let transfer = unsafe {
            self.dma.ping_pong_write_to_peripheral(
                pool,
                buf_a.as_mut_ptr(),
                buf_b.as_mut_ptr(),
                len,
                peri_addr,
                Shifter::<0>::dma_request(),
            )
        }?;
        Ok(I2sStream {
            _shifter: self.shifter,
            _bclk_timer: self.bclk_timer,
            _ws_timer: self.ws_timer,
            transfer,
        })
    }
}

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

    let i2s = FlexioI2sTx::new(
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

    // Square-wave generator used as a stand-in for a real audio source.
    // Each u32 word packs one 16-bit left sample (upper half) and one 16-bit
    // right sample (lower half).
    let mut phase = 0u32;
    let mut fill = |buf: &mut [u32]| {
        for word in buf.iter_mut() {
            let s: u32 = if phase < 256 { 0x3FFF } else { 0xC001 };
            *word = (s << 16) | s;
            phase = (phase + 1) % 512;
        }
    };
    
    let mut stream = i2s.start_stream(TCDS.take(), BUF_A.take(), BUF_B.take()).unwrap();

    // Main loop: wait for a half to finish, refill it, repeat.
    //
    // The CPU has one full buffer period (512 frames ≈ 10.9 ms at 46 875 Hz)
    // to complete the fill before the hardware needs it again.
    loop {
        let idx = stream.next_half().await.unwrap();
        stream.fill_idle(idx, |buf| fill(buf));
    }
}
