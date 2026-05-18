//! USB full-speed device driver for MCXA2xx.

#![cfg(feature = "mcxa2xx")]

use core::cell::UnsafeCell;
use core::future::poll_fn;
use core::marker::PhantomData;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicU8, AtomicU16, Ordering, fence};
use core::task::Poll;

use embassy_hal_internal::Peri;
use embassy_sync::waitqueue::AtomicWaker;
use embassy_usb_driver::{
    Direction, EndpointAddress, EndpointAllocError, EndpointError, EndpointInfo, EndpointType, Event, Unsupported,
};

use crate::clocks::enable_and_reset;
use crate::clocks::periph_helpers::NoConfig;
use crate::interrupt;
use crate::interrupt::typelevel::{Binding, Handler, Interrupt};
use crate::peripherals::USB0;

pub(crate) mod pac;

const EP_COUNT: usize = 8;
const HW_EP_COUNT: usize = 16;
const BDT_ENTRY_COUNT: usize = HW_EP_COUNT * 4;
const EP0_MASK: u8 = 1;
const OUT_EMPTY: u16 = u16::MAX;

const BD_OWN: u32 = 1 << 7;
const BD_DATA01: u32 = 1 << 6;
const BD_DTS: u32 = 1 << 3;
const BD_STALL: u32 = 1 << 2;

const TOKEN_OUT: u32 = 0x01;
const TOKEN_IN: u32 = 0x09;
const TOKEN_SETUP: u32 = 0x0d;

const EVENT_RESET: u8 = 1 << 0;
const EVENT_SUSPEND: u8 = 1 << 1;
const EVENT_RESUME: u8 = 1 << 2;

const ISTAT_USBRST: u8 = 1 << 0;
const ISTAT_ERROR: u8 = 1 << 1;
const ISTAT_TOKDNE: u8 = 1 << 3;
const ISTAT_SLEEP: u8 = 1 << 4;
const ISTAT_RESUME: u8 = 1 << 5;
const ISTAT_STALL: u8 = 1 << 7;

const ENDPT_EPHSHK: u8 = 1 << 0;
const ENDPT_EPSTALL: u8 = 1 << 1;
const ENDPT_EPTXEN: u8 = 1 << 2;
const ENDPT_EPRXEN: u8 = 1 << 3;
const ENDPT_EPCTLDIS: u8 = 1 << 4;

#[repr(C)]
#[derive(Clone, Copy)]
struct BdEntry {
    ctrl: u32,
    addr: u32,
}

#[repr(C, align(512))]
struct BdtBuffer([u8; 512]);

struct SyncUnsafeCell<T>(UnsafeCell<T>);

// SAFETY: These statics are only accessed through this driver. Mutation is
// synchronized either by endpoint ownership/BDT state or by short critical
// sections when copying packet data.
unsafe impl<T> Sync for SyncUnsafeCell<T> {}

impl<T> SyncUnsafeCell<T> {
    const fn new(value: T) -> Self {
        Self(UnsafeCell::new(value))
    }

    fn get(&self) -> *mut T {
        self.0.get()
    }
}

static BDT: SyncUnsafeCell<BdtBuffer> = SyncUnsafeCell::new(BdtBuffer([0; 512]));
static EP_BUF_OUT: SyncUnsafeCell<[[[u8; 64]; 2]; EP_COUNT]> = SyncUnsafeCell::new([[[0; 64]; 2]; EP_COUNT]);
static EP_BUF_IN: SyncUnsafeCell<[[[u8; 64]; 2]; EP_COUNT]> = SyncUnsafeCell::new([[[0; 64]; 2]; EP_COUNT]);

struct UsbFsState {
    bus_waker: AtomicWaker,
    ep_in_wakers: [AtomicWaker; EP_COUNT],
    ep_out_wakers: [AtomicWaker; EP_COUNT],
    ep_out_ready: [AtomicU16; EP_COUNT],
    ep_out_token: [AtomicU8; EP_COUNT],
    pending_addr: AtomicU8,
    events: AtomicU8,
    ep_in_data1: AtomicU8,
    ep_out_data1: AtomicU8,
    ep_in_odd: AtomicU8,
    ep_out_odd: AtomicU8,
    ep_in_active_odd: AtomicU8,
    ep_out_done_odd: AtomicU8,
}

static STATE: UsbFsState = UsbFsState {
    bus_waker: AtomicWaker::new(),
    ep_in_wakers: [const { AtomicWaker::new() }; EP_COUNT],
    ep_out_wakers: [const { AtomicWaker::new() }; EP_COUNT],
    ep_out_ready: [const { AtomicU16::new(OUT_EMPTY) }; EP_COUNT],
    ep_out_token: [const { AtomicU8::new(0) }; EP_COUNT],
    pending_addr: AtomicU8::new(0),
    events: AtomicU8::new(0),
    ep_in_data1: AtomicU8::new(0),
    ep_out_data1: AtomicU8::new(0),
    ep_in_odd: AtomicU8::new(0),
    ep_out_odd: AtomicU8::new(0),
    ep_in_active_odd: AtomicU8::new(0),
    ep_out_done_odd: AtomicU8::new(0),
};

/// USB full-speed device driver.
pub struct Driver<'d> {
    _usb: Peri<'d, USB0>,
    ep_in_alloc: u8,
    ep_out_alloc: u8,
    ep_in_max_packet: [u16; EP_COUNT],
    ep_out_max_packet: [u16; EP_COUNT],
    ep_in_types: [Option<EndpointType>; EP_COUNT],
    ep_out_types: [Option<EndpointType>; EP_COUNT],
}

impl<'d> Driver<'d> {
    /// Create a USB full-speed device driver.
    pub fn new(usb: Peri<'d, USB0>, _irq: impl Binding<interrupt::typelevel::USB0, InterruptHandler> + 'd) -> Self {
        Self {
            _usb: usb,
            ep_in_alloc: EP0_MASK,
            ep_out_alloc: EP0_MASK,
            ep_in_max_packet: [0; EP_COUNT],
            ep_out_max_packet: [0; EP_COUNT],
            ep_in_types: [None; EP_COUNT],
            ep_out_types: [None; EP_COUNT],
        }
    }

    fn alloc_endpoint(
        alloc: &mut u8,
        sizes: &mut [u16; EP_COUNT],
        types: &mut [Option<EndpointType>; EP_COUNT],
        direction: Direction,
        ep_type: EndpointType,
        ep_addr: Option<EndpointAddress>,
        max_packet_size: u16,
        interval_ms: u8,
    ) -> Result<EndpointInfo, EndpointAllocError> {
        if ep_type == EndpointType::Isochronous || max_packet_size > 64 {
            return Err(EndpointAllocError);
        }

        let index = match ep_addr {
            Some(addr) => {
                if addr.direction() != direction {
                    return Err(EndpointAllocError);
                }
                addr.index()
            }
            None => (1..EP_COUNT)
                .find(|&ep| (*alloc & (1 << ep)) == 0)
                .ok_or(EndpointAllocError)?,
        };

        if index == 0 || index >= EP_COUNT || (*alloc & (1 << index)) != 0 {
            return Err(EndpointAllocError);
        }

        *alloc |= 1 << index;
        sizes[index] = max_packet_size;
        types[index] = Some(ep_type);

        Ok(EndpointInfo {
            addr: EndpointAddress::from_parts(index, direction),
            ep_type,
            max_packet_size,
            interval_ms,
        })
    }
}

/// USB0 interrupt handler.
pub struct InterruptHandler {
    _priv: (),
}

impl Handler<interrupt::typelevel::USB0> for InterruptHandler {
    unsafe fn on_interrupt() {
        on_interrupt()
    }
}

/// USB bus object.
pub struct Bus<'d> {
    _phantom: PhantomData<&'d mut USB0>,
    ep_in_max_packet: [u16; EP_COUNT],
    ep_out_max_packet: [u16; EP_COUNT],
    ep_in_types: [Option<EndpointType>; EP_COUNT],
    ep_out_types: [Option<EndpointType>; EP_COUNT],
    inited: bool,
}

/// Endpoint zero control pipe.
pub struct ControlPipe<'d> {
    _phantom: PhantomData<&'d mut USB0>,
    max_packet_size: u16,
}

/// USB IN endpoint.
pub struct EndpointIn<'d> {
    _phantom: PhantomData<&'d mut USB0>,
    info: EndpointInfo,
}

/// USB OUT endpoint.
pub struct EndpointOut<'d> {
    _phantom: PhantomData<&'d mut USB0>,
    info: EndpointInfo,
}

impl<'d> embassy_usb_driver::Driver<'d> for Driver<'d> {
    type EndpointOut = EndpointOut<'d>;
    type EndpointIn = EndpointIn<'d>;
    type ControlPipe = ControlPipe<'d>;
    type Bus = Bus<'d>;

    fn alloc_endpoint_out(
        &mut self,
        ep_type: EndpointType,
        ep_addr: Option<EndpointAddress>,
        max_packet_size: u16,
        interval_ms: u8,
    ) -> Result<Self::EndpointOut, EndpointAllocError> {
        let info = Self::alloc_endpoint(
            &mut self.ep_out_alloc,
            &mut self.ep_out_max_packet,
            &mut self.ep_out_types,
            Direction::Out,
            ep_type,
            ep_addr,
            max_packet_size,
            interval_ms,
        )?;

        Ok(EndpointOut {
            _phantom: PhantomData,
            info,
        })
    }

    fn alloc_endpoint_in(
        &mut self,
        ep_type: EndpointType,
        ep_addr: Option<EndpointAddress>,
        max_packet_size: u16,
        interval_ms: u8,
    ) -> Result<Self::EndpointIn, EndpointAllocError> {
        let info = Self::alloc_endpoint(
            &mut self.ep_in_alloc,
            &mut self.ep_in_max_packet,
            &mut self.ep_in_types,
            Direction::In,
            ep_type,
            ep_addr,
            max_packet_size,
            interval_ms,
        )?;

        Ok(EndpointIn {
            _phantom: PhantomData,
            info,
        })
    }

    fn start(mut self, control_max_packet_size: u16) -> (Self::Bus, Self::ControlPipe) {
        self.ep_in_max_packet[0] = control_max_packet_size;
        self.ep_out_max_packet[0] = control_max_packet_size;

        configure_usb_clock();
        // SAFETY: The driver owns USB0 through `Peri<'d, USB0>` and performs
        // peripheral clock/reset setup before enabling the USB interrupt.
        unsafe {
            _ = enable_and_reset::<USB0>(&NoConfig);
        }
        unhalt_usb_clock();

        let usb = usb();

        usb.usbtrc0().write_value(pac::regs::Usbtrc0(0x80));
        for _ in 0..200_000u32 {
            core::hint::spin_loop();
        }

        usb.ctl().write_value(pac::regs::Ctl(0));
        usb.ctl().write(|w| w.set_usbensofen(pac::vals::Usbensofen::EN_USB_SOF));

        usb.clk_recover_irc_en()
            .write(|w| w.set_irc_en(pac::vals::IrcEn::EN_IRC));
        usb.clk_recover_ctrl().write(|w| {
            w.set_clock_recover_en(pac::vals::ClockRecoverEn::EN_CLK_RECOVER);
            w.set_reset_resume_rough_en(pac::vals::ResetResumeRoughEn::KEEP_TRIM_FINE_ON_RESET);
            w.set_restart_ifrtrim_en(pac::vals::RestartIfrtrimEn::LOAD_TRIM_FINE_IFR);
        });

        reset_state();
        clear_bdt();
        configure_usb_ram_access();

        let bdt_addr = BDT.get() as u32;
        usb.bdtpage1()
            .write_value(pac::regs::Bdtpage1(((bdt_addr >> 8) as u8) & 0xfe));
        usb.bdtpage2().write_value(pac::regs::Bdtpage2((bdt_addr >> 16) as u8));
        usb.bdtpage3().write_value(pac::regs::Bdtpage3((bdt_addr >> 24) as u8));

        usb.usbctrl().write_value(pac::regs::Usbctrl(0x00));

        usb.istat().write_value(pac::regs::Istat(0xff));
        usb.errstat().write_value(pac::regs::Errstat(0xff));
        usb.erren().write_value(pac::regs::Erren(0xff));
        usb.inten().write_value(pac::regs::Inten(0xbb));
        usb.clk_recover_int_en().write_value(pac::regs::ClkRecoverIntEn(0x00));
        clear_clock_recovery_interrupt();

        usb.ctl().modify(|w| w.set_oddrst(true));
        usb.ctl().modify(|w| w.set_oddrst(false));

        prime_ep0_out_setup();
        endpoint_write(0, ENDPT_EPHSHK | ENDPT_EPRXEN | ENDPT_EPTXEN);

        usb.control()
            .write(|w| w.set_dppullupnonotg(pac::vals::Dppullupnonotg::EN_DEVICE_DP_PU));

        interrupt::typelevel::USB0::unpend();
        // SAFETY: The interrupt handler is bound by `Driver::new`, and USB0 is
        // fully initialized before the interrupt is unmasked.
        unsafe {
            interrupt::typelevel::USB0::enable();
        }

        (
            Bus {
                _phantom: PhantomData,
                ep_in_max_packet: self.ep_in_max_packet,
                ep_out_max_packet: self.ep_out_max_packet,
                ep_in_types: self.ep_in_types,
                ep_out_types: self.ep_out_types,
                inited: false,
            },
            ControlPipe {
                _phantom: PhantomData,
                max_packet_size: control_max_packet_size,
            },
        )
    }
}

impl embassy_usb_driver::Bus for Bus<'_> {
    async fn enable(&mut self) {}

    async fn disable(&mut self) {}

    async fn poll(&mut self) -> Event {
        poll_fn(|cx| {
            if !self.inited {
                self.inited = true;
                return Poll::Ready(Event::PowerDetected);
            }

            let events = STATE.events.swap(0, Ordering::Acquire);
            if events & EVENT_RESET != 0 {
                return Poll::Ready(Event::Reset);
            }
            if events & EVENT_SUSPEND != 0 {
                return Poll::Ready(Event::Suspend);
            }
            if events & EVENT_RESUME != 0 {
                return Poll::Ready(Event::Resume);
            }

            STATE.bus_waker.register(cx.waker());
            Poll::Pending
        })
        .await
    }

    fn endpoint_set_enabled(&mut self, ep_addr: EndpointAddress, enabled: bool) {
        let ep = ep_addr.index();
        if ep >= EP_COUNT {
            return;
        }

        let mut bits = endpoint_read(ep);
        if enabled {
            if ep_addr.is_in() {
                if self.ep_in_max_packet[ep] == 0 {
                    return;
                }
                let ephshk = self.ep_in_types[ep] != Some(EndpointType::Isochronous);
                if ephshk {
                    bits |= ENDPT_EPHSHK;
                } else {
                    bits &= !ENDPT_EPHSHK;
                }
                bits |= ENDPT_EPTXEN | ENDPT_EPCTLDIS;
                set_data1(&STATE.ep_in_data1, ep, false);
                set_odd(&STATE.ep_in_odd, ep, false);
                bd_release(bd_index(ep, true, false));
                bd_release(bd_index(ep, true, true));
            } else {
                if self.ep_out_max_packet[ep] == 0 {
                    return;
                }
                let ephshk = self.ep_out_types[ep] != Some(EndpointType::Isochronous);
                if ephshk {
                    bits |= ENDPT_EPHSHK;
                } else {
                    bits &= !ENDPT_EPHSHK;
                }
                bits |= ENDPT_EPRXEN | ENDPT_EPCTLDIS;
                set_data1(&STATE.ep_out_data1, ep, false);
                set_odd(&STATE.ep_out_odd, ep, false);
                prime_out_ep(ep, self.ep_out_max_packet[ep]);
            }
        } else if ep_addr.is_in() {
            bits &= !ENDPT_EPTXEN;
            bd_release(bd_index(ep, true, false));
            bd_release(bd_index(ep, true, true));
            STATE.ep_in_wakers[ep].wake();
        } else {
            bits &= !ENDPT_EPRXEN;
            bd_release(bd_index(ep, false, false));
            bd_release(bd_index(ep, false, true));
            STATE.ep_out_wakers[ep].wake();
        }
        endpoint_write(ep, bits);
    }

    fn endpoint_set_stalled(&mut self, ep_addr: EndpointAddress, stalled: bool) {
        let ep = ep_addr.index();
        if ep >= EP_COUNT {
            return;
        }

        if stalled {
            bd_stall(bd_index(ep, ep_addr.is_in(), false));
            bd_stall(bd_index(ep, ep_addr.is_in(), true));
            endpoint_write(ep, endpoint_read(ep) | ENDPT_EPSTALL);
        } else {
            endpoint_write(ep, endpoint_read(ep) & !ENDPT_EPSTALL);
            bd_release(bd_index(ep, ep_addr.is_in(), false));
            bd_release(bd_index(ep, ep_addr.is_in(), true));
            if ep_addr.is_out() {
                set_data1(&STATE.ep_out_data1, ep, false);
                set_odd(&STATE.ep_out_odd, ep, false);
                prime_out_ep(ep, self.ep_out_max_packet[ep]);
            } else {
                set_data1(&STATE.ep_in_data1, ep, false);
                set_odd(&STATE.ep_in_odd, ep, false);
            }
            if ep == 0 {
                usb().ctl().modify(|w| w.set_txsuspendtokenbusy(false));
            }
        }
    }

    fn endpoint_is_stalled(&mut self, ep_addr: EndpointAddress) -> bool {
        endpoint_read(ep_addr.index()) & ENDPT_EPSTALL != 0
    }

    async fn remote_wakeup(&mut self) -> Result<(), Unsupported> {
        Err(Unsupported)
    }
}

impl embassy_usb_driver::ControlPipe for ControlPipe<'_> {
    fn max_packet_size(&self) -> usize {
        self.max_packet_size as usize
    }

    async fn setup(&mut self) -> [u8; 8] {
        poll_fn(|cx| {
            STATE.ep_out_wakers[0].register(cx.waker());
            let bc = STATE.ep_out_ready[0].load(Ordering::Acquire);
            if bc != OUT_EMPTY {
                let token = STATE.ep_out_token[0].load(Ordering::Acquire) as u32;
                STATE.ep_out_ready[0].store(OUT_EMPTY, Ordering::Release);
                if token != TOKEN_SETUP {
                    prime_ep0_out_setup();
                    return Poll::Pending;
                }
                let mut setup = [0; 8];
                let odd = get_odd(&STATE.ep_out_done_odd, 0) as usize;
                copy_from_out_buf(0, odd != 0, &mut setup);
                set_data1(&STATE.ep_in_data1, 0, true);
                set_data1(&STATE.ep_out_data1, 0, true);
                return Poll::Ready(setup);
            }
            Poll::Pending
        })
        .await
    }

    async fn data_out(&mut self, buf: &mut [u8], first: bool, last: bool) -> Result<usize, EndpointError> {
        if first {
            set_data1(&STATE.ep_out_data1, 0, true);
            if STATE.ep_out_ready[0].load(Ordering::Acquire) == OUT_EMPTY {
                prime_out_ep(0, self.max_packet_size);
            }
        }

        let len = poll_fn(|cx| {
            let ready = STATE.ep_out_ready[0].load(Ordering::Acquire);
            if ready != OUT_EMPTY {
                let token = STATE.ep_out_token[0].load(Ordering::Acquire) as u32;
                if token == TOKEN_SETUP {
                    return Poll::Ready(Err(EndpointError::Disabled));
                }

                let len = ready as usize;
                if len > buf.len() {
                    STATE.ep_out_ready[0].store(OUT_EMPTY, Ordering::Release);
                    prime_out_ep(0, self.max_packet_size);
                    return Poll::Ready(Err(EndpointError::BufferOverflow));
                }

                let odd = get_odd(&STATE.ep_out_done_odd, 0) as usize;
                copy_from_out_buf(0, odd != 0, &mut buf[..len]);
                STATE.ep_out_ready[0].store(OUT_EMPTY, Ordering::Release);

                if !last {
                    prime_out_ep(0, self.max_packet_size);
                }
                return Poll::Ready(Ok(len));
            }

            STATE.ep_out_wakers[0].register(cx.waker());
            Poll::Pending
        })
        .await?;

        if last {
            start_ep0_in(&[])?;
            prime_ep0_out_setup();
            wait_in_done(0, || STATE.ep_out_ready[0].load(Ordering::Acquire) != OUT_EMPTY).await?;
        }

        Ok(len)
    }

    async fn data_in(&mut self, data: &[u8], first: bool, last: bool) -> Result<(), EndpointError> {
        if first {
            set_data1(&STATE.ep_in_data1, 0, true);
        }
        start_ep0_in(data)?;
        wait_in_done(0, || STATE.ep_out_ready[0].load(Ordering::Acquire) != OUT_EMPTY).await?;
        if last {
            set_data1(&STATE.ep_out_data1, 0, true);
            prime_out_ep(0, self.max_packet_size);
        }
        Ok(())
    }

    async fn accept(&mut self) {
        let _ = start_ep0_in(&[]);
        prime_ep0_out_setup();
        let _ = wait_in_done(0, || false).await;
    }

    async fn reject(&mut self) {
        bd_stall(bd_index(0, false, false));
        bd_stall(bd_index(0, false, true));
        bd_stall(bd_index(0, true, false));
        bd_stall(bd_index(0, true, true));
        endpoint_write(0, endpoint_read(0) | ENDPT_EPSTALL);
        resume_ep0_token_processing();
    }

    async fn accept_set_address(&mut self, addr: u8) {
        STATE.pending_addr.store(0x80 | (addr & 0x7f), Ordering::Release);
        let _ = start_ep0_in(&[]);
        prime_ep0_out_setup();
        let _ = wait_in_done(0, || false).await;
    }
}

impl embassy_usb_driver::Endpoint for EndpointOut<'_> {
    fn info(&self) -> &EndpointInfo {
        &self.info
    }

    async fn wait_enabled(&mut self) {
        let ep = self.info.addr.index();
        poll_fn(|cx| {
            if endpoint_read(ep) & ENDPT_EPRXEN != 0 {
                return Poll::Ready(());
            }
            STATE.ep_out_wakers[ep].register(cx.waker());
            Poll::Pending
        })
        .await
    }
}

impl embassy_usb_driver::EndpointOut for EndpointOut<'_> {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, EndpointError> {
        let ep = self.info.addr.index();
        poll_fn(|cx| {
            let bc = STATE.ep_out_ready[ep].load(Ordering::Acquire);
            if bc != OUT_EMPTY {
                let len = bc as usize;
                if len > buf.len() {
                    STATE.ep_out_ready[ep].store(OUT_EMPTY, Ordering::Release);
                    prime_out_ep(ep, self.info.max_packet_size);
                    return Poll::Ready(Err(EndpointError::BufferOverflow));
                }

                let odd = get_odd(&STATE.ep_out_done_odd, ep) as usize;
                copy_from_out_buf(ep, odd != 0, &mut buf[..len]);
                STATE.ep_out_ready[ep].store(OUT_EMPTY, Ordering::Release);
                prime_out_ep(ep, self.info.max_packet_size);
                return Poll::Ready(Ok(len));
            }

            if endpoint_read(ep) & ENDPT_EPRXEN == 0 {
                return Poll::Ready(Err(EndpointError::Disabled));
            }

            STATE.ep_out_wakers[ep].register(cx.waker());
            Poll::Pending
        })
        .await
    }
}

impl embassy_usb_driver::Endpoint for EndpointIn<'_> {
    fn info(&self) -> &EndpointInfo {
        &self.info
    }

    async fn wait_enabled(&mut self) {
        let ep = self.info.addr.index();
        poll_fn(|cx| {
            if endpoint_read(ep) & ENDPT_EPTXEN != 0 {
                return Poll::Ready(());
            }
            STATE.ep_in_wakers[ep].register(cx.waker());
            Poll::Pending
        })
        .await
    }
}

impl embassy_usb_driver::EndpointIn for EndpointIn<'_> {
    async fn write(&mut self, buf: &[u8]) -> Result<(), EndpointError> {
        let ep = self.info.addr.index();
        if endpoint_read(ep) & ENDPT_EPTXEN == 0 {
            return Err(EndpointError::Disabled);
        }
        start_in(ep, buf)?;
        wait_in_done(ep, || false).await
    }
}

fn usb() -> pac::Usb {
    // SAFETY: `USB0_BASE` comes from the same nxp-pac metadata as this local
    // register shim. Access is serialized by the driver and USB0 interrupt.
    unsafe { pac::Usb::from_ptr(pac::USB0_BASE as *mut ()) }
}

fn configure_usb_clock() {
    let scg = crate::pac::SCG0;
    let mrcc = crate::pac::MRCC0;

    scg.ldocsr().modify(|w| w.set_ldoen(true));

    while scg.sirccsr().read().sircvld() == crate::pac::scg::Sircvld::DisabledOrNotValid {}

    scg.spllcsr().modify(|w| {
        w.set_spllpwren(false);
        w.set_spllclken(false);
    });

    // SPLL: 12 MHz / 1 * 32 / 4 / 2 = 48 MHz.
    scg.spllctrl().write(|w| {
        w.set_source(crate::pac::scg::Source::Sirc);
        w.set_seli(19);
        w.set_selp(9);
    });

    let mut nd = crate::pac::scg::Spllndiv(0);
    nd.set_ndiv(1);
    scg.spllndiv().write_value(nd);
    nd.set_nreq(true);
    scg.spllndiv().write_value(nd);

    let mut pd = crate::pac::scg::Spllpdiv(0);
    pd.set_pdiv(4);
    scg.spllpdiv().write_value(pd);
    pd.set_preq(true);
    scg.spllpdiv().write_value(pd);

    let mut md = crate::pac::scg::Spllmdiv(0);
    md.set_mdiv(32);
    scg.spllmdiv().write_value(md);
    md.set_mreq(true);
    scg.spllmdiv().write_value(md);

    scg.trim_lock().write_value(crate::pac::scg::TrimLock(0x5a5a_0001));
    scg.splllock_cnfg().write(|w| w.set_lock_time(6300));

    scg.spllcsr().modify(|w| {
        w.set_spllpwren(true);
        w.set_spllclken(true);
    });

    while scg.spllcsr().read().spll_lock() != crate::pac::scg::SpllLock::EnabledAndValid {}

    mrcc.mrcc_usb0_clksel()
        .write(|w| w.set_mux(crate::pac::mrcc::UsbClkselMux::ClkrootSpll));
    mrcc.mrcc_glb_acc0().modify(|w| w.0 |= 0x10000800);
    mrcc.mrcc_glb_cc0().modify(|w| w.0 |= 0x10000000);
}

fn unhalt_usb_clock() {
    let syscon = crate::pac::SYSCON;
    let mrcc = crate::pac::MRCC0;

    syscon
        .clkunlock()
        .write(|w| w.set_unlock(crate::pac::syscon::Unlock::Enable));
    mrcc.mrcc_usb0_clkdiv()
        .write_value(crate::pac::mrcc::Clkdiv(0x6000_0000));
    mrcc.mrcc_usb0_clkdiv().write(|w| {
        w.set_halt(crate::pac::mrcc::ClkdivHalt::On);
        w.set_reset(crate::pac::mrcc::ClkdivReset::On);
        w.set_div(0);
    });
    syscon
        .clkunlock()
        .write(|w| w.set_unlock(crate::pac::syscon::Unlock::Freeze));
}

fn configure_usb_ram_access() {
    crate::pac::SYSCON
        .remap()
        .modify(|w| w.set_usb0(crate::pac::syscon::Usb0::Usb01));
}

fn endpoint_read(ep: usize) -> u8 {
    usb().endpoint(ep).endpt().read().0
}

fn endpoint_write(ep: usize, bits: u8) {
    usb().endpoint(ep).endpt().write_value(pac::regs::Endpt(bits));
}

fn bd_index(ep: usize, is_in: bool, odd: bool) -> usize {
    ep * 4 + if is_in { 2 } else { 0 } + odd as usize
}

fn bdt_ptr() -> *mut BdEntry {
    BDT.get().cast::<BdEntry>()
}

fn bd_ptr(index: usize) -> *mut BdEntry {
    debug_assert!(index < BDT_ENTRY_COUNT);
    bdt_ptr().wrapping_add(index)
}

fn bd_ctrl(index: usize) -> u32 {
    // SAFETY: `bd_ptr` points into the 512-byte aligned BDT static and callers
    // only pass KHCI BDT indices.
    unsafe { read_volatile(core::ptr::addr_of!((*bd_ptr(index)).ctrl)) }
}

fn bd_release(index: usize) {
    // SAFETY: `index` selects a BDT entry owned by this driver. Volatile writes
    // are required because the USB peripheral reads these fields asynchronously.
    unsafe {
        write_volatile(core::ptr::addr_of_mut!((*bd_ptr(index)).ctrl), 0);
    }
}

fn bd_stall(index: usize) {
    // SAFETY: `index` selects a BDT entry owned by this driver. The release
    // fence ensures descriptor contents are visible before OWN is set.
    unsafe {
        fence(Ordering::Release);
        write_volatile(
            core::ptr::addr_of_mut!((*bd_ptr(index)).ctrl),
            BD_OWN | BD_DTS | BD_STALL,
        );
    }
}

fn bd_prime(index: usize, buf: *const u8, len: u16, data1: bool) {
    let ctrl = ((len as u32) << 16) | BD_OWN | BD_DTS | if data1 { BD_DATA01 } else { 0 };
    // SAFETY: `buf` points to one of the static endpoint buffers and remains
    // valid while OWN is set. The descriptor is written with volatile stores
    // because it is shared with the USB peripheral.
    unsafe {
        write_volatile(core::ptr::addr_of_mut!((*bd_ptr(index)).addr), buf as u32);
        fence(Ordering::Release);
        write_volatile(core::ptr::addr_of_mut!((*bd_ptr(index)).ctrl), ctrl);
    }
}

fn out_buf_ptr(ep: usize, odd: bool) -> *const u8 {
    debug_assert!(ep < EP_COUNT);
    // SAFETY: Endpoint buffers are static and `ep`/`odd` select an in-bounds
    // buffer used as DMA storage by USB0.
    unsafe { (*EP_BUF_OUT.get())[ep][odd as usize].as_ptr() }
}

fn in_buf_ptr(ep: usize, odd: bool) -> *const u8 {
    debug_assert!(ep < EP_COUNT);
    // SAFETY: Endpoint buffers are static and `ep`/`odd` select an in-bounds
    // buffer used as DMA storage by USB0.
    unsafe { (*EP_BUF_IN.get())[ep][odd as usize].as_ptr() }
}

fn copy_from_out_buf(ep: usize, odd: bool, buf: &mut [u8]) {
    debug_assert!(ep < EP_COUNT);
    debug_assert!(buf.len() <= 64);
    // SAFETY: The interrupt marks an OUT buffer ready only after USB0 clears
    // OWN. The critical section prevents racing with re-priming this buffer.
    critical_section::with(|_| unsafe {
        buf.copy_from_slice(&(&(*EP_BUF_OUT.get())[ep][odd as usize])[..buf.len()]);
    });
}

fn copy_to_in_buf(ep: usize, odd: bool, data: &[u8]) {
    debug_assert!(ep < EP_COUNT);
    debug_assert!(data.len() <= 64);
    // SAFETY: The selected IN buffer is filled before its BDT entry is primed
    // with OWN. The critical section prevents interrupt-side aliasing.
    critical_section::with(|_| unsafe {
        (&mut (*EP_BUF_IN.get())[ep][odd as usize])[..data.len()].copy_from_slice(data);
    });
}

fn clear_bdt() {
    for i in 0..BDT_ENTRY_COUNT {
        // SAFETY: The BDT static contains exactly one entry for each
        // endpoint/direction/even-odd tuple in the KHCI block.
        unsafe {
            write_volatile(core::ptr::addr_of_mut!((*bd_ptr(i)).ctrl), 0);
            write_volatile(core::ptr::addr_of_mut!((*bd_ptr(i)).addr), 0);
        }
    }
}

fn reset_state() {
    STATE.pending_addr.store(0, Ordering::Release);
    STATE.events.store(0, Ordering::Release);
    STATE.ep_in_data1.store(0, Ordering::Release);
    STATE.ep_out_data1.store(0, Ordering::Release);
    STATE.ep_in_odd.store(0, Ordering::Release);
    STATE.ep_out_odd.store(0, Ordering::Release);
    STATE.ep_in_active_odd.store(0, Ordering::Release);
    STATE.ep_out_done_odd.store(0, Ordering::Release);
    for ep in 0..EP_COUNT {
        STATE.ep_out_ready[ep].store(OUT_EMPTY, Ordering::Release);
        STATE.ep_out_token[ep].store(0, Ordering::Release);
    }
}

fn set_data1(mask: &AtomicU8, ep: usize, data1: bool) {
    let bit = 1 << ep;
    if data1 {
        mask.fetch_or(bit, Ordering::AcqRel);
    } else {
        mask.fetch_and(!bit, Ordering::AcqRel);
    }
}

fn take_data1(mask: &AtomicU8, ep: usize) -> bool {
    let bit = 1 << ep;
    let old = mask.fetch_xor(bit, Ordering::AcqRel);
    old & bit != 0
}

fn set_odd(mask: &AtomicU8, ep: usize, odd: bool) {
    let bit = 1 << ep;
    if odd {
        mask.fetch_or(bit, Ordering::AcqRel);
    } else {
        mask.fetch_and(!bit, Ordering::AcqRel);
    }
}

fn get_odd(mask: &AtomicU8, ep: usize) -> bool {
    mask.load(Ordering::Acquire) & (1 << ep) != 0
}

fn take_next_odd(mask: &AtomicU8, ep: usize) -> bool {
    let bit = 1 << ep;
    let old = mask.fetch_xor(bit, Ordering::AcqRel);
    old & bit != 0
}

fn resume_ep0_token_processing() {
    usb().ctl().modify(|w| w.set_txsuspendtokenbusy(false));
}

fn clear_clock_recovery_interrupt() {
    // USBTRC0[4] can fire USB0 without a corresponding ISTAT bit.
    let usb = usb();
    usb.usbtrc0().write_value(pac::regs::Usbtrc0(0x10));
    usb.clk_recover_int_status()
        .write_value(pac::regs::ClkRecoverIntStatus(0x10));
}

fn prime_ep0_out_setup() {
    STATE.ep_out_ready[0].store(OUT_EMPTY, Ordering::Release);
    STATE.ep_out_token[0].store(0, Ordering::Release);
    set_data1(&STATE.ep_out_data1, 0, false);
    let odd = take_next_odd(&STATE.ep_out_odd, 0);
    bd_prime(bd_index(0, false, odd), out_buf_ptr(0, odd), 8, false);
    resume_ep0_token_processing();
}

fn prime_out_ep(ep: usize, max_packet_size: u16) {
    STATE.ep_out_ready[ep].store(OUT_EMPTY, Ordering::Release);
    STATE.ep_out_token[ep].store(0, Ordering::Release);
    let max_packet_size = max_packet_size.min(64);
    let data1 = take_data1(&STATE.ep_out_data1, ep);
    let odd = take_next_odd(&STATE.ep_out_odd, ep);
    bd_prime(bd_index(ep, false, odd), out_buf_ptr(ep, odd), max_packet_size, data1);
    if ep == 0 {
        resume_ep0_token_processing();
    }
}

fn start_ep0_in(data: &[u8]) -> Result<(), EndpointError> {
    start_in(0, data)
}

fn start_in(ep: usize, data: &[u8]) -> Result<(), EndpointError> {
    if data.len() > 64 {
        return Err(EndpointError::BufferOverflow);
    }

    let data1 = take_data1(&STATE.ep_in_data1, ep);
    let odd = take_next_odd(&STATE.ep_in_odd, ep);
    set_odd(&STATE.ep_in_active_odd, ep, odd);

    copy_to_in_buf(ep, odd, data);

    bd_prime(bd_index(ep, true, odd), in_buf_ptr(ep, odd), data.len() as u16, data1);
    if ep == 0 {
        resume_ep0_token_processing();
    }
    Ok(())
}

async fn wait_in_done(ep: usize, abort: impl Fn() -> bool) -> Result<(), EndpointError> {
    poll_fn(|cx| {
        if abort() {
            return Poll::Ready(Err(EndpointError::Disabled));
        }
        if endpoint_read(ep) & ENDPT_EPTXEN == 0 {
            return Poll::Ready(Err(EndpointError::Disabled));
        }
        let odd = get_odd(&STATE.ep_in_active_odd, ep);
        if bd_ctrl(bd_index(ep, true, odd)) & BD_OWN == 0 {
            return Poll::Ready(Ok(()));
        }
        STATE.ep_in_wakers[ep].register(cx.waker());
        Poll::Pending
    })
    .await
}

fn on_interrupt() {
    let usb = usb();

    let usbtrc0 = usb.usbtrc0().read();
    if usbtrc0.usb_clk_recovery_int() {
        clear_clock_recovery_interrupt();
    }

    let istat = usb.istat().read().0;

    if istat & ISTAT_USBRST != 0 {
        for ep in 1..HW_EP_COUNT {
            endpoint_write(ep, 0);
        }

        usb.addr().write_value(pac::regs::Addr(0));
        reset_state();
        clear_bdt();
        usb.ctl().modify(|w| w.set_oddrst(true));
        usb.ctl().modify(|w| w.set_oddrst(false));
        prime_ep0_out_setup();
        endpoint_write(0, ENDPT_EPHSHK | ENDPT_EPRXEN | ENDPT_EPTXEN);

        STATE.events.fetch_or(EVENT_RESET, Ordering::Release);
        STATE.bus_waker.wake();
        usb.istat().write_value(pac::regs::Istat(ISTAT_USBRST));
    }

    if istat & ISTAT_SLEEP != 0 {
        STATE.events.fetch_or(EVENT_SUSPEND, Ordering::Release);
        STATE.bus_waker.wake();
        usb.istat().write_value(pac::regs::Istat(ISTAT_SLEEP));
    }

    if istat & ISTAT_RESUME != 0 {
        STATE.events.fetch_or(EVENT_RESUME, Ordering::Release);
        STATE.bus_waker.wake();
        usb.istat().write_value(pac::regs::Istat(ISTAT_RESUME));
    }

    if istat & ISTAT_TOKDNE != 0 {
        let stat = usb.stat().read();
        let ep_num = stat.endp() as usize;
        let is_tx = stat.tx() == pac::vals::Tx::TX_TRANSACTION;
        let odd = stat.odd().to_bits() as usize;
        let bd_idx = bd_index(ep_num, is_tx, odd != 0);
        let ctrl = bd_ctrl(bd_idx);
        let bc = (ctrl >> 16) as u16;
        let token_pid = (ctrl >> 2) & 0x0f;

        if ep_num < EP_COUNT {
            if !is_tx {
                if token_pid == TOKEN_SETUP {
                    bd_release(bd_index(0, true, false));
                    bd_release(bd_index(0, true, true));
                    set_data1(&STATE.ep_in_data1, 0, true);
                    set_data1(&STATE.ep_out_data1, 0, true);
                    STATE.ep_out_token[0].store(token_pid as u8, Ordering::Release);
                    set_odd(&STATE.ep_out_done_odd, 0, odd != 0);
                    STATE.ep_out_ready[0].store(bc, Ordering::Release);
                    resume_ep0_token_processing();
                } else if token_pid == TOKEN_OUT {
                    STATE.ep_out_token[ep_num].store(token_pid as u8, Ordering::Release);
                    set_odd(&STATE.ep_out_done_odd, ep_num, odd != 0);
                    if ep_num == 0 && bc == 0 {
                        prime_ep0_out_setup();
                    } else {
                        STATE.ep_out_ready[ep_num].store(bc, Ordering::Release);
                    }
                }
                STATE.ep_out_wakers[ep_num].wake();
            } else {
                if ep_num == 0 && token_pid == TOKEN_IN {
                    let pa = STATE.pending_addr.load(Ordering::Acquire);
                    if pa & 0x80 != 0 {
                        usb.addr().write_value(pac::regs::Addr(pa & 0x7f));
                        STATE.pending_addr.store(0, Ordering::Release);
                    }
                }
                STATE.ep_in_wakers[ep_num].wake();
            }
        }

        usb.istat().write_value(pac::regs::Istat(ISTAT_TOKDNE));
    }

    if istat & ISTAT_STALL != 0 {
        endpoint_write(0, endpoint_read(0) & !ENDPT_EPSTALL);
        prime_ep0_out_setup();
        usb.istat().write_value(pac::regs::Istat(ISTAT_STALL));
    }

    if istat & ISTAT_ERROR != 0 {
        usb.errstat().write_value(pac::regs::Errstat(0xff));
        usb.istat().write_value(pac::regs::Istat(ISTAT_ERROR));
    }
}
