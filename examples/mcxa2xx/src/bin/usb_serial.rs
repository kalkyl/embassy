#![no_std]
#![no_main]

use defmt::*;
use embassy_executor::Spawner;
use embassy_futures::join::join3;
use embassy_mcxa::bind_interrupts;
use embassy_mcxa::config::Config;
use embassy_mcxa::usb::{Driver, InterruptHandler};
use embassy_time::Timer;
use embassy_usb::Builder;
use embassy_usb::class::cdc_acm::{CdcAcmClass, State};
use embassy_usb_driver::EndpointError;
use {defmt_rtt as _, embassy_mcxa as hal, panic_probe as _};

bind_interrupts!(struct Irqs {
    USB0 => InterruptHandler;
});

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let p = hal::init(Config::default());

    let driver = Driver::new(p.USB0, Irqs);
    
    let mut config = embassy_usb::Config::new(0xc0de, 0xcafe);
    config.manufacturer = Some("Embassy");
    config.product = Some("USB Serial (MCXA)");
    config.serial_number = Some("00000001");
    config.device_class = 0xef;
    config.device_sub_class = 0x02;
    config.device_protocol = 0x01;
    config.composite_with_iads = true;
    
    let mut config_descriptor = [0u8; 256];
    let mut bos_descriptor = [0u8; 256];
    let mut control_buf = [0u8; 64];
    let mut state = State::new();
    
    let mut builder = Builder::new(
        driver,
        config,
        &mut config_descriptor,
        &mut bos_descriptor,
        &mut [],
        &mut control_buf,
    );
    
    let mut class = CdcAcmClass::new(&mut builder, &mut state, 64);
    let mut usb = builder.build();
    info!("build");


    let usb_fut = usb.run();
    let echo_fut = async {
        loop {
            info!("waiting for connection...");
            class.wait_connection().await;
            info!("USB CDC connected");
            let _ = echo(&mut class).await;
            info!("USB CDC disconnected");
        }
    };

    let debug_fut = async {
        loop {
            Timer::after_secs(2).await;
            embassy_mcxa::usb::debug_dump_registers();
        }
    };

    join3(usb_fut, echo_fut, debug_fut).await;
}

async fn echo<'d>(class: &mut CdcAcmClass<'d, Driver<'d>>) -> Result<(), EndpointError> {
    let mut buf = [0u8; 64];
    loop {
        let n = class.read_packet(&mut buf).await?;
        class.write_packet(&buf[..n]).await?;
    }
}
