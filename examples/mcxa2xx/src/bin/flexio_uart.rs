//! FlexIO UART Tx personality example (blocking).
//!
//! Demonstrates using the FlexIO HAL to implement a software UART transmitter
//! in application code. Only the HAL building-blocks live in `embassy-mcxa`;
//! the protocol personality (`FlexioUartTx`) lives here.
//!
//! Hardware: FRDM-MCXA256 (mcxa2xx).
//!
//! Wiring: connect a USB-UART adapter to P0_16 (FXIO_D0, alt-6).
//!
//! Terminal: 115200 8N1.

#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_time::Timer;
use hal::clocks::PoweredClock;
use hal::clocks::periph_helpers::{Div4, FlexioClockSel, FlexioConfig};
use hal::Peri;
use hal::flexio::{
    Flexio, FlexioPin, Insrc, ShiftctlPincfg, ShiftctlPinpol, ShifterConfig, Shifter, Smod,
    Sstart, Sstop, TimctlPincfg, TimctlPinpol, Timdec, Timdis, Timena, Timod, TimerConfig,
    TimerTrigger, Timer as FlexTimer, Timpol, Timout, Timrst, Trgpol, Tstop,
};
use hal::peripherals::FLEXIO0;
use embassy_mcxa::clocks::config::Div8;
use {defmt_rtt as _, embassy_mcxa as hal, panic_probe as _};

/// 8N1 UART transmitter. 
/// Lane generic removed from struct!
pub struct FlexioUartTx<'d> {
    shifter: Shifter<'d, 0>,
    #[allow(dead_code)]
    timer: FlexTimer<'d, 0>,
}

impl<'d> FlexioUartTx<'d> {
    /// The generic 'L' is now local to this function.
    pub fn new<P: FlexioPin<FLEXIO0, L>, const L: usize>(
        mut shifter: Shifter<'d, 0>,
        mut timer: FlexTimer<'d, 0>,
        tx_pin: Peri<'d, P>,
        baud: u32,
        flexio_clk: u32,
    ) -> Self {
        tx_pin.as_flexio_lane();

        // Extract the lane number from the generic L
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
            pin_select: lane, // Use the extracted lane
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
            pin_select: lane, // Use the extracted lane
            pin_cfg: ShiftctlPincfg::Output,
            pin_pol: ShiftctlPinpol::ActiveHigh,
            timer_pol: Timpol::Negedge,
            timer_select: 0,
            start_bit: Sstart::Value10,
            stop_bit: Sstop::Value11,
            input_source: Insrc::Pin,
        });

        Self { shifter, timer }
    }

    pub fn write_byte(&mut self, byte: u8) {
        while !self.shifter.is_status_set() {}
        let word = (byte as u32) | 0xFFFFFF00;
        self.shifter.write_buffer(word);
    }

    pub fn write(&mut self, data: &[u8]) {
        for &b in data {
            self.write_byte(b);
        }
    }
}

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let mut cfg = hal::config::Config::default();
    cfg.clock_cfg.sirc.fro_12m_enabled = true;
    cfg.clock_cfg.sirc.fro_lf_div = Some(Div8::no_div());
    let p = hal::init(cfg);

    defmt::info!("FlexIO UART Tx example");

    let flexio_clk_hz = 24_000_000u32; // 24 MHz
    let flexio_cfg = FlexioConfig {
        power: PoweredClock::NormalEnabledDeepSleepDisabled,
        source: FlexioClockSel::FroHfGated,
        div: Div4::from_divisor(4).unwrap(), // Divide 96MHz by 4 to get 24MHz
    };
    let flexio = Flexio::new(p.FLEXIO0, flexio_cfg).expect("FlexIO init failed");
    let (mut shifters, mut timers) = flexio.split();

    let shifter0 = shifters.take::<0>().unwrap();
    let timer0 = timers.take::<0>().unwrap();

    let mut uart = FlexioUartTx::new(shifter0, timer0, p.P3_28, 115_200, flexio_clk_hz);

    loop {
        uart.write(b"hello world");
        Timer::after_millis(1000).await;
    }
}
