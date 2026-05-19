//! FlexIO — Flexible I/O peripheral HAL.
//!
//! FlexIO is a collection of programmable Shifters and Timers that share a bank
//! of internal signal lines (lanes, `FXIO_Dn`). This HAL acts as a resource
//! manager: [`Flexio::new`] claims the peripheral and resets it, then
//! [`Flexio::split`] hands out independent [`ShifterCluster`] and
//! [`TimerCluster`] handles. Individual [`Shifter`] and [`Timer`] units are
//! obtained from those clusters.
//!
//! High-level protocol "personalities" (UART, SPI, I2S …) are meant to live in
//! application or example code, not here.

use core::marker::PhantomData;

use embassy_hal_internal::{Peri, PeripheralType};

use crate::pac::flexio as pac;

pub use pac::{
    Insrc, Latst, ShiftctlPincfg, ShiftctlPinpol, Smod, Ssize, Sstart, Sstop, Ssf, TimctlPincfg,
    TimctlPinpol, Timdec, Timdis, Timena, Timod, Timout, Timrst, Timpol, Trgpol, Trgsrc, Tsf,
    Tstop,
};

use crate::clocks::periph_helpers::FlexioConfig;
use crate::clocks::{ClockError, Gate, enable_and_reset};
use crate::dma::DmaRequest;

pub(crate) struct Info {
    pub(crate) regs: pac::Flexio,
    pub(crate) mrcc_disable: unsafe fn(),
}

unsafe impl Sync for Info {}

impl Info {
    #[inline(always)]
    fn regs(&self) -> pac::Flexio {
        self.regs
    }
}

pub(crate) trait SealedInstance: Gate<MrccPeriphConfig = FlexioConfig> {
    fn info() -> &'static Info;
}

/// Trait implemented for each FlexIO peripheral instance (e.g. `FLEXIO0`).
#[allow(private_bounds)]
pub trait Instance: SealedInstance + PeripheralType + 'static + Send {
    /// Number of Shifters in this instance (hardware parameter).
    const SHIFTER_COUNT: usize;
    /// Number of Timers in this instance (hardware parameter).
    const TIMER_COUNT: usize;
}

/// Implement [`Instance`] for a FlexIO peripheral numbered `$n`.
///
/// Called from `build.rs`-generated code.
#[doc(hidden)]
#[macro_export]
macro_rules! impl_flexio_instance {
    ($n:expr) => {
        ::paste::paste! {
            impl crate::flexio::SealedInstance for crate::peripherals::[<FLEXIO $n>] {
                fn info() -> &'static crate::flexio::Info {
                    use crate::clocks::Gate;
                    static INFO: crate::flexio::Info = crate::flexio::Info {
                        regs: crate::pac::[<FLEXIO $n>],
                        mrcc_disable: crate::clocks::disable::<crate::peripherals::[<FLEXIO $n>]>,
                    };
                    &INFO
                }
            }

            impl crate::flexio::Instance for crate::peripherals::[<FLEXIO $n>] {
                const SHIFTER_COUNT: usize = 4;
                const TIMER_COUNT: usize = 4;
            }
        }
    };
}

// Gate not yet in nxp-pac metadata; wired manually until then.
#[cfg(any(feature = "mcxa2xx", feature = "mcxa5xx"))]
crate::impl_cc_gate!(
    FLEXIO0,
    mrcc_glb_cc0,
    mrcc_glb_rst0,
    flexio0,
    FlexioConfig
);

/// Configuration passed to [`Shifter::set_config`].
#[derive(Copy, Clone, Debug)]
pub struct ShifterConfig {
    /// Shifter operating mode (Transmit, Receive, …).
    pub smod: Smod,
    /// Internal FXIO lane index this shifter drives / reads.
    pub pin_select: u8,
    /// Pin output mode for the shifter lane.
    pub pin_cfg: ShiftctlPincfg,
    /// Pin polarity.
    pub pin_pol: ShiftctlPinpol,
    /// Timer polarity (shift on rising or falling edge).
    pub timer_pol: Timpol,
    /// Which timer (0-3) clocks this shifter.
    pub timer_select: u8,
    /// Start bit configuration.
    pub start_bit: Sstart,
    /// Stop bit configuration.
    pub stop_bit: Sstop,
    /// Input source (pin or shifter N+1).
    pub input_source: Insrc,
}

impl Default for ShifterConfig {
    fn default() -> Self {
        Self {
            smod: Smod::Disable,
            pin_select: 0,
            pin_cfg: ShiftctlPincfg::Disable,
            pin_pol: ShiftctlPinpol::ActiveHigh,
            timer_pol: Timpol::Posedge,
            timer_select: 0,
            start_bit: Sstart::Value00,
            stop_bit: Sstop::Value00,
            input_source: Insrc::Pin,
        }
    }
}

/// Trigger source selection for a [`TimerConfig`].
#[derive(Copy, Clone, Debug)]
pub enum TimerTrigger {
    /// No external/internal trigger (timer is always enabled).
    None,
    /// Internal trigger from a FlexIO lane (BCLK/FS sync).
    ///
    /// `lane` is the FXIO_Dn index; `polarity` selects active-high/low.
    InternalLane { lane: u8, polarity: Trgpol },
    /// Internal trigger from shifter `n` status flag.
    InternalShifterFlag { shifter: u8, polarity: Trgpol },
    /// External trigger; raw `trgsel` value.
    External { trgsel: u8, polarity: Trgpol },
}

/// Configuration passed to [`Timer::set_config`].
#[derive(Copy, Clone, Debug)]
pub struct TimerConfig {
    /// Timer operating mode.
    pub timod: Timod,
    /// Clock / decrement source.
    pub timdec: Timdec,
    /// Timer enable condition.
    pub timena: Timena,
    /// Timer disable condition.
    pub timdis: Timdis,
    /// Timer reset condition.
    pub timrst: Timrst,
    /// Timer output initial value and reset behaviour.
    pub timout: Timout,
    /// Stop bit insertion.
    pub tstop: Tstop,
    /// Start bit insertion.
    pub tstart: bool,
    /// Timer pin lane index.
    pub pin_select: u8,
    /// Timer pin output mode.
    pub pin_cfg: TimctlPincfg,
    /// Timer pin polarity.
    pub pin_pol: TimctlPinpol,
    /// Trigger source — see [`TimerTrigger`].
    pub trigger: TimerTrigger,
    /// Compare value (16-bit).  Dual-8-bit baud: upper = bit count × 2 − 1,
    /// lower = (FlexIO_clk / (2 × baud)) − 1.
    pub compare: u16,
}

impl Default for TimerConfig {
    fn default() -> Self {
        Self {
            timod: Timod::Disable,
            timdec: Timdec::FlexioClkShiftclkTmrOut,
            timena: Timena::Enable,
            timdis: Timdis::Never,
            timrst: Timrst::Never,
            timout: Timout::One,
            tstop: Tstop::StopDisable,
            tstart: false,
            pin_select: 0,
            pin_cfg: TimctlPincfg::Outdisable,
            pin_pol: TimctlPinpol::ActiveHigh,
            trigger: TimerTrigger::None,
            compare: 0,
        }
    }
}

/// FlexIO peripheral manager.
///
/// Created with [`Flexio::new`].  Call [`Flexio::split`] to obtain independent
/// Shifter and Timer cluster handles.
pub struct Flexio<'d, INST: Instance> {
    info: &'static Info,
    _phantom: PhantomData<Peri<'d, INST>>,
}

impl<'d, INST: Instance> Flexio<'d, INST> {
    /// Claim the FlexIO peripheral, enable its clock, and reset it.
    pub fn new(_peri: Peri<'d, INST>, config: FlexioConfig) -> Result<Self, ClockError> {
        let _parts = unsafe { enable_and_reset::<INST>(&config) }?;

        let regs = INST::info().regs();

        // Software-reset to clear any residual state from before clock enable.
        regs.ctrl().write(|w| w.set_swrst(true));
        regs.ctrl().write(|w| w.set_swrst(false));

        regs.ctrl().modify(|w| w.set_flexen(true));

        Ok(Self {
            info: INST::info(),
            _phantom: PhantomData,
        })
    }

    /// Split into independent Shifter and Timer cluster handles.
    ///
    /// Consumes `self`; the peripheral clock is kept running until the
    /// returned clusters (and all resources derived from them) are dropped.
    pub fn split(self) -> (ShifterCluster<'d>, TimerCluster<'d>) {
        let info = self.info;
        // Prevent Drop from disabling the clock — sub-resources own the
        // peripheral lifetime now.
        core::mem::forget(self);
        (
            ShifterCluster {
                info,
                taken: [false; 4],
                _phantom: PhantomData,
            },
            TimerCluster {
                info,
                taken: [false; 4],
                _phantom: PhantomData,
            },
        )
    }
}

impl<INST: Instance> Drop for Flexio<'_, INST> {
    fn drop(&mut self) {
        self.info.regs().ctrl().modify(|w| w.set_flexen(false));
        unsafe { (self.info.mrcc_disable)() };
    }
}

/// Handle to the set of Shifters in a FlexIO instance.
///
/// Obtained from [`Flexio::split`].  Use [`ShifterCluster::take`] to obtain
/// an exclusive [`Shifter`] handle for a specific index.
pub struct ShifterCluster<'d> {
    info: &'static Info,
    taken: [bool; 4],
    _phantom: PhantomData<&'d mut ()>,
}

impl<'d> ShifterCluster<'d> {
    /// Take exclusive ownership of Shifter `N`.
    ///
    /// Returns `None` if that index has already been taken.
    ///
    /// # Panics
    ///
    /// Panics if `N >= 4`.
    pub fn take<const N: usize>(&mut self) -> Option<Shifter<'d, N>> {
        assert!(N < 4, "FlexIO Shifter index out of range");
        if self.taken[N] {
            return None;
        }
        self.taken[N] = true;
        Some(Shifter {
            info: self.info,
            _phantom: PhantomData,
        })
    }
}

/// Handle to the set of Timers in a FlexIO instance.
///
/// Obtained from [`Flexio::split`].  Use [`TimerCluster::take`] to obtain
/// an exclusive [`Timer`] handle for a specific index.
pub struct TimerCluster<'d> {
    info: &'static Info,
    taken: [bool; 4],
    _phantom: PhantomData<&'d mut ()>,
}

impl<'d> TimerCluster<'d> {
    /// Take exclusive ownership of Timer `N`.
    ///
    /// Returns `None` if that index has already been taken.
    ///
    /// # Panics
    ///
    /// Panics if `N >= 4`.
    pub fn take<const N: usize>(&mut self) -> Option<Timer<'d, N>> {
        assert!(N < 4, "FlexIO Timer index out of range");
        if self.taken[N] {
            return None;
        }
        self.taken[N] = true;
        Some(Timer {
            info: self.info,
            _phantom: PhantomData,
        })
    }
}

/// Exclusive handle for FlexIO Shifter `N`.
///
/// Obtained via [`ShifterCluster::take`].
pub struct Shifter<'d, const N: usize> {
    info: &'static Info,
    _phantom: PhantomData<&'d mut ()>,
}

impl<'d, const N: usize> Shifter<'d, N> {
    /// Apply a full shifter configuration (SHIFTCTL + SHIFTCFG registers).
    ///
    /// The shifter must be disabled (SMOD = Disable) before reconfiguring.
    pub fn set_config(&mut self, cfg: &ShifterConfig) {
        let regs = self.info.regs();
        // Must write SHIFTCFG before SHIFTCTL as per reference manual.
        regs.shiftcfg(N).write(|w| {
            w.set_sstart(cfg.start_bit);
            w.set_sstop(cfg.stop_bit);
            w.set_insrc(cfg.input_source);
        });
        regs.shiftctl(N).write(|w| {
            w.set_smod(cfg.smod);
            w.set_pinsel(cfg.pin_select);
            w.set_pincfg(cfg.pin_cfg);
            w.set_pinpol(cfg.pin_pol);
            w.set_timpol(cfg.timer_pol);
            w.set_timsel(cfg.timer_select);
        });
    }

    /// Write a 32-bit word to the shifter transmit buffer (SHIFTBUF).
    ///
    /// FlexIO transmits bit0 first (LSB-first). Use this for UART and other
    /// LSB-first protocols where the byte is placed in the low bits of the word.
    #[inline]
    pub fn write_buffer(&mut self, data: u32) {
        self.info.regs().shiftbuf(N).write(|w| w.set_shiftbuf(data));
    }

    /// Write a 32-bit word to the shifter via SHIFTBUFBIS (Bit-Swapped buffer).
    ///
    /// SHIFTBUFBIS maps bit0 of the written value to bit31 of the shift register,
    /// so FlexIO transmits bit31 of the written value first (MSB-first). Use this
    /// for I2S and other MSB-first protocols.
    #[inline]
    pub fn write_buffer_bis(&mut self, data: u32) {
        self.info.regs().shiftbufbis(N).write(|w| w.set_shiftbufbis(data));
    }

    /// Read a 32-bit word from the shifter receive buffer (SHIFTBUF).
    #[inline]
    pub fn read_buffer(&self) -> u32 {
        self.info.regs().shiftbuf(N).read().shiftbuf()
    }

    /// Read from the shifter via SHIFTBUFBIS (Bit-Swapped buffer).
    #[inline]
    pub fn read_buffer_bis(&self) -> u32 {
        self.info.regs().shiftbufbis(N).read().shiftbufbis()
    }

    /// Raw address of `SHIFTBUF[N]`, for DMA transfers that need LSB-first output.
    ///
    /// FlexIO transmits bit0 first from SHIFTBUF. Suitable for UART TX DMA.
    /// For MSB-first protocols (I2S, SPI), use [`dma_address_bis`](Self::dma_address_bis).
    #[inline]
    pub fn dma_address(&self) -> *mut u32 {
        self.info.regs().shiftbuf(N).as_ptr() as *mut u32
    }

    /// Raw address of `SHIFTBUFBIS[N]`, for DMA transfers that need MSB-first output.
    ///
    /// Writing to SHIFTBUFBIS reverses the bit order so that bit31 of the written
    /// value is transmitted first. Matches the NXP SDK I2S TX DMA address.
    #[inline]
    pub fn dma_address_bis(&self) -> *mut u32 {
        self.info.regs().shiftbufbis(N).as_ptr() as *mut u32
    }

    /// DMA request signal for this shifter's status flag.
    ///
    /// Pass this to the DMA controller when setting up a linked transfer.
    pub const fn dma_request() -> DmaRequest
    where
        [(); N]: ,
    {
        // Shifter DMA requests are Flexio0Sr0..Flexio0Sr3 (indices 71-74).
        // We use a match so the compiler can verify exhaustiveness.
        match N {
            0 => DmaRequest::Flexio0Sr0,
            1 => DmaRequest::Flexio0Sr1,
            2 => DmaRequest::Flexio0Sr2,
            3 => DmaRequest::Flexio0Sr3,
            _ => panic!("FlexIO Shifter N out of range"),
        }
    }

    /// Enable the DMA request for this shifter's status flag.
    pub fn enable_dma(&mut self) {
        let bit = 1u8 << N;
        self.info.regs().shiftsden().modify(|w| w.set_ssde(w.ssde() | bit));
    }

    /// Disable the DMA request for this shifter's status flag.
    pub fn disable_dma(&mut self) {
        let bit = 1u8 << N;
        self.info.regs().shiftsden().modify(|w| w.set_ssde(w.ssde() & !bit));
    }

    /// Returns `true` if the shifter status flag is set (buffer empty for TX,
    /// data available for RX).
    #[inline]
    pub fn is_status_set(&self) -> bool {
        let ssf = self.info.regs().shiftstat().read().ssf();
        (ssf.to_bits() >> N) & 1 != 0
    }

    /// Clear this shifter's status flag (write-1-to-clear).
    #[inline]
    pub fn clear_status(&mut self) {
        self.info.regs().shiftstat().write(|w| w.set_ssf(Ssf::from_bits(1 << N)));
    }
}

/// Exclusive handle for FlexIO Timer `N`.
///
/// Obtained via [`TimerCluster::take`].
pub struct Timer<'d, const N: usize> {
    info: &'static Info,
    _phantom: PhantomData<&'d mut ()>,
}

impl<'d, const N: usize> Timer<'d, N> {
    /// Apply a full timer configuration (TIMCTL + TIMCFG + TIMCMP registers).
    ///
    /// For dual-8-bit baud mode the `compare` field encodes both the bit count
    /// and the baud divisor:
    ///
    /// ```text
    /// compare[15:8] = bits_per_word × 2 − 1
    /// compare[7:0]  = (FlexIO_clk / (2 × baud_rate)) − 1
    /// ```
    pub fn set_config(&mut self, cfg: &TimerConfig) {
        let regs = self.info.regs();

        regs.timcmp(N).write(|w| w.set_cmp(cfg.compare));

        regs.timcfg(N).write(|w| {
            w.set_timdec(cfg.timdec);
            w.set_timena(cfg.timena);
            w.set_timdis(cfg.timdis);
            w.set_timrst(cfg.timrst);
            w.set_timout(cfg.timout);
            w.set_tstop(cfg.tstop);
            w.set_tstart(cfg.tstart);
        });

        let (trgsrc, trgpol, trgsel) = match cfg.trigger {
            TimerTrigger::None => (Trgsrc::ExtTrig, Trgpol::ActiveHigh, 0u8),
            TimerTrigger::InternalLane { lane, polarity } => {
                // TRGSEL = 4 × lane selects the pin/lane input.
                (Trgsrc::InternalTrig, polarity, 4 * lane)
            }
            TimerTrigger::InternalShifterFlag { shifter, polarity } => {
                // TRGSEL = 4 × shifter + 1 selects shifter status flag.
                (Trgsrc::InternalTrig, polarity, 4 * shifter + 1)
            }
            TimerTrigger::External { trgsel, polarity } => {
                (Trgsrc::ExtTrig, polarity, trgsel)
            }
        };

        regs.timctl(N).write(|w| {
            w.set_timod(cfg.timod);
            w.set_pinsel(cfg.pin_select);
            w.set_pincfg(cfg.pin_cfg);
            w.set_pinpol(cfg.pin_pol);
            w.set_trgsrc(trgsrc);
            w.set_trgpol(trgpol);
            w.set_trgsel(trgsel);
        });
    }

    /// Returns `true` if the timer status flag is set.
    #[inline]
    pub fn is_status_set(&self) -> bool {
        let tsf = self.info.regs().timstat().read().tsf();
        (tsf.to_bits() >> N) & 1 != 0
    }

    /// Clear this timer's status flag (write-1-to-clear).
    #[inline]
    pub fn clear_status(&mut self) {
        self.info.regs().timstat().write(|w| w.set_tsf(Tsf::from_bits(1 << N)));
    }
}

/// Marker trait linking a physical pad to a FlexIO internal lane.
///
/// `INST` is the peripheral type (e.g. `FLEXIO0`).
/// `LANE` is the zero-based internal lane index (matches the `Dn` signal).
///
/// Implementations are generated by the [`impl_flexio_pin!`] macro, which is
/// called from build.rs for every signal/pin pair in the metadata.
#[allow(private_bounds)]
pub trait FlexioPin<INST: Instance, const LANE: usize>:
    crate::gpio::SealedPin + crate::gpio::GpioPin
{
    /// IOMUX Alt-mode that connects this pad to the FlexIO signal.
    const MUX: crate::pac::port::Mux;

    /// Configure the pad for FlexIO use (set MUX, pull, and input buffer).
    fn as_flexio_lane(&self) {
        self.set_pull(crate::gpio::Pull::Disabled);
        self.set_function(Self::MUX);
        self.set_enable_input_buffer(true);
    }
}

/// Generate a [`FlexioPin`] implementation for a physical pad.
///
/// Arguments: `($pin, $inst, $lane, $mux_alt)`.
///
/// Called automatically from `build.rs`-generated code.
#[doc(hidden)]
#[macro_export]
macro_rules! impl_flexio_pin {
    ($pin:ident, $inst:ident, $lane:expr, $alt:ident) => {
        impl crate::flexio::FlexioPin<crate::peripherals::$inst, $lane> for crate::peripherals::$pin {
            const MUX: crate::pac::port::Mux = crate::pac::port::Mux::$alt;
        }
    };
}
