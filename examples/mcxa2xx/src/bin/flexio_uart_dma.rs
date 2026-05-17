//! FlexIO UART Tx DMA example (async).
//!
//! Demonstrates using the FlexIO HAL to implement a software UART transmitter
//! that sends bytes via DMA — the CPU is not involved while data shifts out.
//!
//! The `FlexioUartTx` personality lives in `embassy-mcxa::flexio::uart_tx`
//! because it needs access to the internal (pub-crate) DMA abstractions to
//! arm the DMA channel hardware request and poll the completion WaitCell.
//!
//! Hardware: FRDM-MCXA256 (mcxa2xx).
//!
//! Wiring: connect a USB-UART adapter to P3_28 (FXIO_D0, alt-6).
//!         Use the same pin as the blocking flexio_uart.rs example.
//!
//! Terminal: 115200 8N1.

#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_time::Timer;
use hal::clocks::PoweredClock;
use hal::clocks::periph_helpers::{Div4, FlexioClockSel, FlexioConfig};
use hal::flexio::{Flexio, FlexioUartTx};
use {defmt_rtt as _, embassy_mcxa as hal, panic_probe as _};

// ─── FlexIO / clock constants ────────────────────────────────────────────────

/// FlexIO functional clock after the divider (24 MHz in this configuration).
const FLEXIO_CLK_HZ: u32 = 24_000_000;

/// UART baud rate.
const BAUD: u32 = 115_200;

// ─── Entry point ─────────────────────────────────────────────────────────────

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    // ── HAL init ──────────────────────────────────────────────────────────────
    let mut cfg = hal::config::Config::default();
    // Enable FRO 12 MHz (used as FRO HF gated source after PLL).
    cfg.clock_cfg.sirc.fro_12m_enabled = true;
    // Enable low-frequency FRO for the systick / time driver.
    use embassy_mcxa::clocks::config::Div8;
    cfg.clock_cfg.sirc.fro_lf_div = Some(Div8::no_div());
    let p = hal::init(cfg);

    defmt::info!("FlexIO UART DMA Tx example");

    // ── FlexIO clock ──────────────────────────────────────────────────────────
    // FRO HF runs at 96 MHz; divide by 4 → 24 MHz FlexIO clock.
    let flexio_cfg = FlexioConfig {
        power: PoweredClock::NormalEnabledDeepSleepDisabled,
        source: FlexioClockSel::FroHfGated,
        div: Div4::from_divisor(4).unwrap(),
    };

    // ── FlexIO init ───────────────────────────────────────────────────────────
    let flexio = Flexio::new(p.FLEXIO0, flexio_cfg).expect("FlexIO init failed");
    let (mut shifters, mut timers) = flexio.split();

    let shifter0 = shifters.take::<0>().unwrap();
    let timer0 = timers.take::<0>().unwrap();

    // ── Construct the async UART driver ───────────────────────────────────────
    //
    // P3_28 → FXIO_D0 (alt-6, lane 0).
    // DMA0_CH0 is routed to the Flexio0Sr0 request signal by the driver.
    let mut uart = FlexioUartTx::new(
        shifter0,   // Shifter 0
        timer0,     // Timer 0
        p.P3_28,    // TX pad (FXIO_D0, lane 0)
        p.DMA0_CH0, // DMA channel for async transfers
        BAUD,
        FLEXIO_CLK_HZ,
    );

    // ── Transmit loop ─────────────────────────────────────────────────────────
    let mut counter: u32 = 0;
    loop {
        // Async DMA transmit — CPU is free while data shifts out.
        uart.write(b"Hello from FlexIO UART DMA! count=").await;

        // Append a simple decimal counter.
        let mut num_buf = [0u8; 10];
        let digits = fmt_u32(counter, &mut num_buf);
        uart.write(digits).await;

        uart.write(b"\r\n").await;

        // Optional: wait for the line to go fully idle before sleeping.
        // This is only needed if you will de-assert a direction GPIO or
        // enter a power-saving mode immediately after the write.
        uart.blocking_flush();

        defmt::info!("Sent line {}", counter);

        counter = counter.wrapping_add(1);
        Timer::after_millis(1000).await;
    }
}

/// Format a `u32` as decimal ASCII into `buf`.  Returns the filled slice.
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
