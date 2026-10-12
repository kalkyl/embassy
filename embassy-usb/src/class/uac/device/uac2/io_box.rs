//! USB Audio Class 2.0 - audio I/O box
//!
//! Line out (host to device) and line in (device to host) in one audio function, in the
//! `IO_BOX` category. Both streams are asynchronous: line out reports the device's sample
//! rate through an explicit feedback endpoint, and line in sizes its packets after it.
//!
//! High speed only: one packet per microframe, and the feedback value is 16.16 samples per
//! microframe.
//!
//! ```text
//! CLOCK_SOURCE (internal programmable)
//! └── CLOCK_SELECTOR (one input)
//!     ├── IT (USB streaming) → [FU] → OT (line connector)   line out
//!     └── IT (line connector) → [FU] → OT (USB streaming)   line in
//! ```
//!
//! Both directions share one clock, so macOS groups them into one device. The clock selector
//! is for Linux, which otherwise doesn't start the feedback endpoint.

use core::cell::Cell;
#[cfg(target_has_atomic = "32")]
use core::sync::atomic::AtomicU32;
use core::sync::atomic::{AtomicBool, Ordering};

use embassy_sync::blocking_mutex::CriticalSectionMutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use heapless::Vec;
#[cfg(not(target_has_atomic = "32"))]
use portable_atomic::AtomicU32;

pub use super::super::Volume;
pub use super::super::function::FeatureUnitControls;
use super::super::{Channel, MAX_AUDIO_CHANNEL_COUNT, MAX_AUDIO_CHANNEL_INDEX, SampleWidth};
use super::channel_config;
use crate::class::uac::terminal_type::TerminalType;
use crate::class::uac::uac2_codes::{
    AUDIO_FUNCTION, FUNCTION_SUBCLASS_UNDEFINED, ac_descriptor, as_descriptor, control_selector, descriptor_type,
    endpoint_descriptor, format_type, function_category, function_protocol, interface, request_code,
};
use crate::control::{self, InResponse, OutResponse, Recipient, Request, RequestType};
use crate::descriptor::{SynchronizationType, UsageType};
use crate::driver::{Driver, Endpoint, EndpointError, EndpointIn, EndpointOut, EndpointType};
use crate::types::InterfaceNumber;
use crate::{Builder, Handler};

const CLOCK_SOURCE_ID: u8 = 0x01;
const CLOCK_SELECTOR_ID: u8 = 0x02;
const OUT_INPUT_TERMINAL_ID: u8 = 0x03;
const OUT_FEATURE_UNIT_ID: u8 = 0x04;
const OUT_OUTPUT_TERMINAL_ID: u8 = 0x05;
const IN_INPUT_TERMINAL_ID: u8 = 0x06;
const IN_FEATURE_UNIT_ID: u8 = 0x07;
const IN_OUTPUT_TERMINAL_ID: u8 = 0x08;

/// Audio Device Class release number (bcdADC).
const ADC_VERSION: u16 = 0x0200;

const CLOCK_TYPE_INTERNAL_PROGRAMMABLE: u8 = 0x03;

const CONTROL_RO: u8 = 0b01;
const CONTROL_RW: u8 = 0b11;

// Volume settings go from -100 dB to 0 dB in steps of 1 dB, as 8q8 values in units of dB.
const VOLUME_STEPS_PER_DB: i16 = 256;
const MIN_VOLUME_DB: i16 = -100;
const MAX_VOLUME_DB: i16 = 0;

/// The CUR setting for silence (-∞ dB), which every volume control accepts
/// regardless of its advertised range.
const VOLUME_SILENCE_8Q8_DB: i16 = i16::MIN;

const MAX_SAMPLE_RATE_COUNT: usize = 10;

/// Returns the control selector, the high byte of `wValue`, as defined in `uac2_codes`.
fn selector(req: &Request) -> u16 {
    req.value & 0xFF00
}

/// I/O box stream settings and optional host controls.
#[derive(Clone)]
pub struct Config<'d> {
    /// Supported sample rates in Hz, shared by both directions: one to ten discrete values.
    /// The first is reported before the host selects a rate.
    pub sample_rates_hz: &'d [u32],
    /// The audio sample resolution of both streams.
    pub sample_width: SampleWidth,
    /// Line out channels in stream order (one to twelve). Entries must be unique and follow
    /// `bmChannelConfig` bit order, as listed in [`Channel`].
    pub line_out_channels: &'d [Channel],
    /// Line in channels in stream order, as [`Self::line_out_channels`].
    pub line_in_channels: &'d [Channel],
    /// Controls the USB host can change on each line out channel.
    ///
    /// Use `heapless::Vec::new()` to omit the Feature Unit. Otherwise, provide
    /// `line_out_channels.len() + 1` entries: the master first, then each channel in stream
    /// order. Use [`FeatureUnitControls::empty`] for no controls on a channel.
    pub line_out_feature_unit: Vec<FeatureUnitControls, MAX_AUDIO_CHANNEL_COUNT, u8>,
    /// Controls the USB host can change on each line in channel, as
    /// [`Self::line_out_feature_unit`].
    pub line_in_feature_unit: Vec<FeatureUnitControls, MAX_AUDIO_CHANNEL_COUNT, u8>,
    /// Maximum bytes per line out packet: frames per microframe at the highest rate, plus one,
    /// times the frame size.
    pub line_out_max_packet_size: u16,
    /// Maximum bytes per line in packet, as [`Self::line_out_max_packet_size`].
    pub line_in_max_packet_size: u16,
}

impl<'d> Config<'d> {
    /// Creates an I/O box configuration without Feature Units.
    pub const fn new(
        sample_rates_hz: &'d [u32],
        sample_width: SampleWidth,
        line_out_channels: &'d [Channel],
        line_in_channels: &'d [Channel],
        line_out_max_packet_size: u16,
        line_in_max_packet_size: u16,
    ) -> Self {
        Self {
            sample_rates_hz,
            sample_width,
            line_out_channels,
            line_in_channels,
            line_out_feature_unit: Vec::new(),
            line_in_feature_unit: Vec::new(),
            line_out_max_packet_size,
            line_in_max_packet_size,
        }
    }
}

/// Internal state for the I/O box.
#[derive(Default)]
pub struct State<'d> {
    control: Option<Control<'d>>,
    shared: SharedControl<'d>,
}

impl<'d> State<'d> {
    /// Create a new `State`.
    pub fn new() -> Self {
        Self::default()
    }
}

/// USB Audio Class 2.0 I/O box: line out + line in with explicit feedback.
pub struct IoBox<'d, D: Driver<'d>> {
    /// Line out stream (host to device).
    pub line_out: LineOutStream<'d, D>,
    /// Line in stream (device to host).
    pub line_in: LineInStream<'d, D>,
    /// Line out's explicit feedback endpoint.
    pub feedback: Feedback<'d, D>,
    /// Control monitor.
    pub control_monitor: ControlMonitor<'d>,
}

impl<'d, D: Driver<'d>> IoBox<'d, D> {
    /// Creates a new [`IoBox`] function.
    ///
    /// # Panics
    ///
    /// If there is no sample rate or more than ten, no channel or more than twelve in a
    /// direction, a `max_packet_size` above the 1024-byte isochronous limit, a nonempty feature
    /// unit without one entry for the master and each channel, unsupported control bits, or a
    /// `control_buf` too small for the sample rate list.
    pub fn new(builder: &mut Builder<'d, D>, state: &'d mut State<'d>, config: Config<'d>) -> Self {
        let Config {
            sample_rates_hz,
            sample_width,
            line_out_channels,
            line_in_channels,
            line_out_feature_unit,
            line_in_feature_unit,
            line_out_max_packet_size,
            line_in_max_packet_size,
        } = config;

        assert!(
            (1..=MAX_SAMPLE_RATE_COUNT).contains(&sample_rates_hz.len()),
            "between one and ten sample rates"
        );
        for (channels, feature_unit) in [
            (line_out_channels, &line_out_feature_unit),
            (line_in_channels, &line_in_feature_unit),
        ] {
            assert!(
                (1..=MAX_AUDIO_CHANNEL_INDEX).contains(&channels.len()),
                "between one and twelve channels"
            );
            assert!(
                feature_unit.is_empty() || feature_unit.len() == channels.len() + 1,
                "a feature unit must be empty or contain the master and every audio channel"
            );
            assert!(
                feature_unit
                    .iter()
                    .all(|controls| FeatureUnitControls::all().contains(*controls)),
                "unsupported Feature Unit controls"
            );
        }
        assert!(
            line_out_max_packet_size <= 1024 && line_in_max_packet_size <= 1024,
            "at most 1024 bytes per isochronous packet"
        );
        assert!(
            builder.control_buf_len() >= 2 + 12 * sample_rates_hz.len(),
            "control_buf too small for the sample rate RANGE reply"
        );

        let mut func = builder.function(
            AUDIO_FUNCTION,
            FUNCTION_SUBCLASS_UNDEFINED,
            function_protocol::AF_VERSION_02_00,
        );

        let mut iface = func.interface();
        let control_interface = iface.interface_number();
        let mut alt = iface.alt_setting(
            interface::AUDIO,
            interface::subclass::AUDIOCONTROL,
            interface::protocol::IP_VERSION_02_00,
            None,
        );

        let clock_source = [
            ac_descriptor::CLOCK_SOURCE,
            CLOCK_SOURCE_ID,
            CLOCK_TYPE_INTERNAL_PROGRAMMABLE,
            CONTROL_RW | (CONTROL_RO << 2), // bmControls: frequency R/W, validity R/O
            0x00,                           // bAssocTerminal
            0x00,                           // iClockSource
        ];
        let clock_selector = [
            ac_descriptor::CLOCK_SELECTOR,
            CLOCK_SELECTOR_ID,
            0x01, // bNrInPins
            CLOCK_SOURCE_ID,
            CONTROL_RW, // bmControls: clock selector R/W
            0x00,       // iClockSelector
        ];

        let out_has_fu = !line_out_feature_unit.is_empty();
        let in_has_fu = !line_in_feature_unit.is_empty();

        let out_input_terminal = input_terminal(OUT_INPUT_TERMINAL_ID, TerminalType::UsbStreaming, line_out_channels);
        let out_feature_unit = feature_unit(OUT_FEATURE_UNIT_ID, OUT_INPUT_TERMINAL_ID, &line_out_feature_unit);
        let out_output_terminal = output_terminal(
            OUT_OUTPUT_TERMINAL_ID,
            TerminalType::ExtLineConnector,
            if out_has_fu {
                OUT_FEATURE_UNIT_ID
            } else {
                OUT_INPUT_TERMINAL_ID
            },
        );
        let in_input_terminal = input_terminal(IN_INPUT_TERMINAL_ID, TerminalType::ExtLineConnector, line_in_channels);
        let in_feature_unit = feature_unit(IN_FEATURE_UNIT_ID, IN_INPUT_TERMINAL_ID, &line_in_feature_unit);
        let in_output_terminal = output_terminal(
            IN_OUTPUT_TERMINAL_ID,
            TerminalType::UsbStreaming,
            if in_has_fu {
                IN_FEATURE_UNIT_ID
            } else {
                IN_INPUT_TERMINAL_ID
            },
        );

        let mut entities: Vec<&[u8], 8> = Vec::new();
        let _ = entities.push(&clock_source);
        let _ = entities.push(&clock_selector);
        let _ = entities.push(&out_input_terminal);
        if out_has_fu {
            let _ = entities.push(&out_feature_unit);
        }
        let _ = entities.push(&out_output_terminal);
        let _ = entities.push(&in_input_terminal);
        if in_has_fu {
            let _ = entities.push(&in_feature_unit);
        }
        let _ = entities.push(&in_output_terminal);

        // The total length covers the header and every entity, each with bLength and bDescriptorType.
        const HEADER_BODY: usize = 7;
        let total_len: usize = 2 + HEADER_BODY + entities.iter().map(|e| e.len() + 2).sum::<usize>();
        alt.descriptor(
            descriptor_type::CS_INTERFACE,
            &[
                ac_descriptor::HEADER,
                ADC_VERSION as u8,
                (ADC_VERSION >> 8) as u8,
                function_category::IO_BOX,
                total_len as u8,
                (total_len >> 8) as u8,
                0x00, // bmControls
            ],
        );
        for entity in entities {
            alt.descriptor(descriptor_type::CS_INTERFACE, entity);
        }

        let mut out_iface = func.interface();
        out_iface.alt_setting(
            interface::AUDIO,
            interface::subclass::AUDIOSTREAMING,
            interface::protocol::IP_VERSION_02_00,
            None,
        );
        let mut out_alt = out_iface.alt_setting(
            interface::AUDIO,
            interface::subclass::AUDIOSTREAMING,
            interface::protocol::IP_VERSION_02_00,
            None,
        );
        out_alt.descriptor(
            descriptor_type::CS_INTERFACE,
            &as_general(OUT_INPUT_TERMINAL_ID, line_out_channels),
        );
        out_alt.descriptor(descriptor_type::CS_INTERFACE, &format_type_i(sample_width));

        // Every microframe.
        let out_ep = out_alt.alloc_endpoint_out(EndpointType::Isochronous, None, line_out_max_packet_size, 0);
        // Allocated before the feedback endpoint, so that drivers allocating in order don't
        // give the feedback endpoint line out's endpoint number.
        let in_ep = out_alt.alloc_endpoint_in(EndpointType::Isochronous, None, line_in_max_packet_size, 0);

        out_alt.endpoint_descriptor(
            out_ep.info(),
            SynchronizationType::Asynchronous,
            UsageType::DataEndpoint,
            &[],
        );
        out_alt.descriptor(descriptor_type::CS_ENDPOINT, &iso_data_endpoint());

        let feedback_ep = out_alt.alloc_endpoint_in(EndpointType::Isochronous, None, 4, 1);
        out_alt.endpoint_descriptor(
            feedback_ep.info(),
            SynchronizationType::NoSynchronization,
            UsageType::FeedbackEndpoint,
            &[],
        );

        let mut in_iface = func.interface();
        in_iface.alt_setting(
            interface::AUDIO,
            interface::subclass::AUDIOSTREAMING,
            interface::protocol::IP_VERSION_02_00,
            None,
        );
        let mut in_alt = in_iface.alt_setting(
            interface::AUDIO,
            interface::subclass::AUDIOSTREAMING,
            interface::protocol::IP_VERSION_02_00,
            None,
        );
        in_alt.descriptor(
            descriptor_type::CS_INTERFACE,
            &as_general(IN_OUTPUT_TERMINAL_ID, line_in_channels),
        );
        in_alt.descriptor(descriptor_type::CS_INTERFACE, &format_type_i(sample_width));
        in_alt.endpoint_descriptor(
            in_ep.info(),
            SynchronizationType::Asynchronous,
            UsageType::DataEndpoint,
            &[],
        );
        in_alt.descriptor(descriptor_type::CS_ENDPOINT, &iso_data_endpoint());

        drop(func);

        let shared = &mut state.shared;
        shared.line_out_channels = line_out_channels;
        shared.line_in_channels = line_in_channels;
        shared.line_out_controls = line_out_feature_unit;
        shared.line_in_controls = line_in_feature_unit;
        shared.sample_rates_hz = sample_rates_hz;
        shared.sample_rate_hz.store(sample_rates_hz[0], Ordering::Relaxed);

        state.control = Some(Control {
            shared: &state.shared,
            control_interface_number: control_interface,
        });
        builder.handler(state.control.as_mut().unwrap());

        Self {
            line_out: LineOutStream { ep: out_ep },
            line_in: LineInStream { ep: in_ep },
            feedback: Feedback { ep: feedback_ep },
            control_monitor: ControlMonitor { shared: &state.shared },
        }
    }
}

// Descriptor bodies (without bLength / bDescriptorType).

fn input_terminal(id: u8, terminal_type: TerminalType, channels: &[Channel]) -> [u8; 15] {
    let terminal_type: u16 = terminal_type.into();
    let cfg = channel_config(channels);
    [
        ac_descriptor::INPUT_TERMINAL,
        id,
        terminal_type as u8,
        (terminal_type >> 8) as u8,
        0x00, // bAssocTerminal
        CLOCK_SELECTOR_ID,
        channels.len() as u8,
        cfg as u8,
        (cfg >> 8) as u8,
        (cfg >> 16) as u8,
        (cfg >> 24) as u8,
        0x00, // iChannelNames
        0x00,
        0x00, // bmControls
        0x00, // iTerminal
    ]
}

fn output_terminal(id: u8, terminal_type: TerminalType, source_id: u8) -> [u8; 10] {
    let terminal_type: u16 = terminal_type.into();
    [
        ac_descriptor::OUTPUT_TERMINAL,
        id,
        terminal_type as u8,
        (terminal_type >> 8) as u8,
        0x00, // bAssocTerminal
        source_id,
        CLOCK_SELECTOR_ID,
        0x00,
        0x00, // bmControls
        0x00, // iTerminal
    ]
}

const FEATURE_UNIT_MAX_BODY: usize = 3 + 4 * MAX_AUDIO_CHANNEL_COUNT + 1;

/// Feature Unit descriptor, with the master's controls first. Empty if there are no controls.
fn feature_unit(id: u8, source_id: u8, controls: &[FeatureUnitControls]) -> Vec<u8, FEATURE_UNIT_MAX_BODY> {
    let mut v = Vec::new();
    if controls.is_empty() {
        return v;
    }
    let _ = v.extend_from_slice(&[ac_descriptor::FEATURE_UNIT, id, source_id]);
    for c in controls {
        let mut bits = 0u32;
        if c.contains(FeatureUnitControls::MUTE) {
            bits |= CONTROL_RW as u32;
        }
        if c.contains(FeatureUnitControls::VOLUME) {
            bits |= (CONTROL_RW as u32) << 2;
        }
        let _ = v.extend_from_slice(&bits.to_le_bytes());
    }
    let _ = v.push(0x00); // iFeature
    v
}

fn as_general(terminal_link: u8, channels: &[Channel]) -> [u8; 14] {
    let cfg = channel_config(channels);
    let formats = format_type::Type1::PCM.bits();
    [
        as_descriptor::GENERAL,
        terminal_link,
        0x00, // bmControls
        format_type::I,
        formats as u8,
        (formats >> 8) as u8,
        (formats >> 16) as u8,
        (formats >> 24) as u8,
        channels.len() as u8,
        cfg as u8,
        (cfg >> 8) as u8,
        (cfg >> 16) as u8,
        (cfg >> 24) as u8,
        0x00, // iChannelNames
    ]
}

fn format_type_i(sample_width: SampleWidth) -> [u8; 4] {
    [
        as_descriptor::FORMAT_TYPE,
        format_type::I,
        sample_width as u8,
        sample_width.in_bit() as u8,
    ]
}

/// Lock delay of 8 samples.
fn iso_data_endpoint() -> [u8; 6] {
    [endpoint_descriptor::EP_GENERAL, 0x00, 0x00, 0x02, 0x08, 0x00]
}

/// Line out stream (host to device, isochronous OUT endpoint).
pub struct LineOutStream<'d, D: Driver<'d>> {
    ep: D::EndpointOut,
}

impl<'d, D: Driver<'d>> LineOutStream<'d, D> {
    /// Reads one audio packet from the host. Returns the number of bytes received.
    pub async fn read_packet(&mut self, data: &mut [u8]) -> Result<usize, EndpointError> {
        self.ep.read(data).await
    }

    /// Waits for the host to select the streaming alternate setting.
    pub async fn wait_connection(&mut self) {
        self.ep.wait_enabled().await;
    }

    /// The isochronous OUT endpoint's index.
    pub fn endpoint_index(&self) -> usize {
        self.ep.info().addr.index()
    }
}

/// Line in stream (device to host, isochronous IN endpoint).
pub struct LineInStream<'d, D: Driver<'d>> {
    ep: D::EndpointIn,
}

impl<'d, D: Driver<'d>> LineInStream<'d, D> {
    /// Writes one audio packet to the host: the frames captured since the last one.
    pub async fn write_packet(&mut self, data: &[u8]) -> Result<(), EndpointError> {
        self.ep.write(data).await
    }

    /// Waits for the host to select the streaming alternate setting.
    pub async fn wait_connection(&mut self) {
        self.ep.wait_enabled().await;
    }

    /// The isochronous IN endpoint's index.
    pub fn endpoint_index(&self) -> usize {
        self.ep.info().addr.index()
    }
}

/// Line out's explicit feedback endpoint: 16.16 samples per microframe.
///
/// Write it with [`Self::write_packet`], or from the SOF interrupt through
/// [`Self::endpoint_index`] if the driver supports that.
pub struct Feedback<'d, D: Driver<'d>> {
    ep: D::EndpointIn,
}

impl<'d, D: Driver<'d>> Feedback<'d, D> {
    /// Writes one feedback value.
    pub async fn write_packet(&mut self, value: u32) -> Result<(), EndpointError> {
        self.ep.write(&value.to_le_bytes()).await
    }

    /// Waits for the host to select line out's streaming alternate setting.
    pub async fn wait_connection(&mut self) {
        self.ep.wait_enabled().await;
    }

    /// The feedback endpoint's index.
    pub fn endpoint_index(&self) -> usize {
        self.ep.info().addr.index()
    }
}

/// Audio settings for a feature unit: mute and volume per channel, the master first.
#[derive(Clone, Copy, Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
struct AudioSettings {
    muted: [bool; MAX_AUDIO_CHANNEL_COUNT],
    volume_8q8_db: [i16; MAX_AUDIO_CHANNEL_COUNT],
}

impl Default for AudioSettings {
    fn default() -> Self {
        AudioSettings {
            muted: [false; MAX_AUDIO_CHANNEL_COUNT],
            volume_8q8_db: [MAX_VOLUME_DB * VOLUME_STEPS_PER_DB; MAX_AUDIO_CHANNEL_COUNT],
        }
    }
}

struct SharedControl<'d> {
    line_out_settings: CriticalSectionMutex<Cell<AudioSettings>>,
    line_in_settings: CriticalSectionMutex<Cell<AudioSettings>>,
    line_out_channels: &'d [Channel],
    line_in_channels: &'d [Channel],
    line_out_controls: Vec<FeatureUnitControls, MAX_AUDIO_CHANNEL_COUNT, u8>,
    line_in_controls: Vec<FeatureUnitControls, MAX_AUDIO_CHANNEL_COUNT, u8>,
    sample_rates_hz: &'d [u32],
    sample_rate_hz: AtomicU32,
    clock_valid: AtomicBool,
    changed: Signal<CriticalSectionRawMutex, ()>,
}

impl<'d> Default for SharedControl<'d> {
    fn default() -> Self {
        SharedControl {
            line_out_settings: CriticalSectionMutex::new(Cell::new(AudioSettings::default())),
            line_in_settings: CriticalSectionMutex::new(Cell::new(AudioSettings::default())),
            line_out_channels: &[],
            line_in_channels: &[],
            line_out_controls: Vec::new(),
            line_in_controls: Vec::new(),
            sample_rates_hz: &[],
            sample_rate_hz: AtomicU32::new(0),
            clock_valid: AtomicBool::new(true),
            changed: Signal::new(),
        }
    }
}

struct FeatureUnitRef<'a> {
    settings: &'a CriticalSectionMutex<Cell<AudioSettings>>,
    controls: &'a [FeatureUnitControls],
}

impl FeatureUnitRef<'_> {
    /// Controls for a USB channel number (0 = master). Missing entries have no controls.
    fn controls(&self, channel: u8) -> FeatureUnitControls {
        self.controls
            .get(channel as usize)
            .copied()
            .unwrap_or_else(FeatureUnitControls::empty)
    }
}

/// Control status change monitor.
///
/// Await [`Self::changed`], then read the updated settings.
pub struct ControlMonitor<'d> {
    shared: &'d SharedControl<'d>,
}

impl<'d> ControlMonitor<'d> {
    fn volume(
        settings: &CriticalSectionMutex<Cell<AudioSettings>>,
        channels: &[Channel],
        controls: &[FeatureUnitControls],
        channel: Channel,
    ) -> Option<Volume> {
        // The logical channels start at one (zero is the master channel).
        let index = channels.iter().position(|&c| c == channel)? + 1;
        let has = |i: usize| controls.get(i).is_some_and(|c| !c.is_empty());
        if !has(0) && !has(index) {
            return None;
        }
        let s = settings.lock(|x| x.get());
        if [0, index]
            .iter()
            .any(|&i| s.muted[i] || s.volume_8q8_db[i] == VOLUME_SILENCE_8Q8_DB)
        {
            return Some(Volume::Muted);
        }
        Some(Volume::DeciBel(
            (s.volume_8q8_db[0] as f32 + s.volume_8q8_db[index] as f32) / VOLUME_STEPS_PER_DB as f32,
        ))
    }

    /// A line out channel's volume with the master applied. `None` if the channel is absent or
    /// neither it nor the master has controls.
    pub fn line_out_volume(&self, channel: Channel) -> Option<Volume> {
        let s = self.shared;
        Self::volume(&s.line_out_settings, s.line_out_channels, &s.line_out_controls, channel)
    }

    /// A line in channel's volume with the master applied, as [`Self::line_out_volume`].
    pub fn line_in_volume(&self, channel: Channel) -> Option<Volume> {
        let s = self.shared;
        Self::volume(&s.line_in_settings, s.line_in_channels, &s.line_in_controls, channel)
    }

    /// Get the sample rate in Hz. Before the host sets one, and again after a bus reset, it is
    /// the first advertised rate.
    pub fn sample_rate_hz(&self) -> u32 {
        self.shared.sample_rate_hz.load(Ordering::Relaxed)
    }

    /// Set the clock validity reported to the host (valid by default). Report the clock as
    /// invalid while it doesn't run at the selected rate, for example during a rate change.
    pub fn set_clock_valid(&self, valid: bool) {
        self.shared.clock_valid.store(valid, Ordering::Relaxed);
    }

    /// Wait for the sample rate, a volume or a mute state to change. Single consumer: await this
    /// from at most one task.
    pub async fn changed(&self) {
        self.shared.changed.wait().await;
    }
}

struct Control<'d> {
    control_interface_number: InterfaceNumber,
    shared: &'d SharedControl<'d>,
}

impl<'d> Control<'d> {
    fn feature_unit(&self, entity: u8) -> Option<FeatureUnitRef<'d>> {
        let s = self.shared;
        match entity {
            OUT_FEATURE_UNIT_ID if !s.line_out_controls.is_empty() => Some(FeatureUnitRef {
                settings: &s.line_out_settings,
                controls: &s.line_out_controls,
            }),
            IN_FEATURE_UNIT_ID if !s.line_in_controls.is_empty() => Some(FeatureUnitRef {
                settings: &s.line_in_settings,
                controls: &s.line_in_controls,
            }),
            _ => None,
        }
    }

    fn set_request(&mut self, req: Request, data: &[u8]) -> Option<OutResponse> {
        if req.index as u8 != u8::from(self.control_interface_number) {
            return None;
        }
        if req.request != request_code::CUR {
            return Some(OutResponse::Rejected);
        }
        let entity = (req.index >> 8) as u8;
        let channel = req.value as u8;

        match entity {
            CLOCK_SOURCE_ID => {
                if selector(&req) != control_selector::clock_source::SAMPLING_FREQ_CONTROL || data.len() < 4 {
                    return Some(OutResponse::Rejected);
                }
                let rate = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                if !self.shared.sample_rates_hz.contains(&rate) {
                    debug!("Unsupported sample rate {} Hz", rate);
                    return Some(OutResponse::Rejected);
                }
                self.shared.sample_rate_hz.store(rate, Ordering::Relaxed);
                debug!("Set sample rate to {} Hz", rate);
                self.shared.changed.signal(());
                Some(OutResponse::Accepted)
            }
            // There is one input, so pin 1 is the only valid selection.
            CLOCK_SELECTOR_ID => {
                if selector(&req) != control_selector::clock_selector::CLOCK_SELECTOR_CONTROL
                    || data.first() != Some(&1)
                {
                    return Some(OutResponse::Rejected);
                }
                Some(OutResponse::Accepted)
            }
            _ => {
                let Some(fu) = self.feature_unit(entity) else {
                    return Some(OutResponse::Rejected);
                };
                let (control, len) = match selector(&req) {
                    control_selector::feature_unit::MUTE_CONTROL => (FeatureUnitControls::MUTE, 1),
                    control_selector::feature_unit::VOLUME_CONTROL => (FeatureUnitControls::VOLUME, 2),
                    _ => return Some(OutResponse::Rejected),
                };
                if !fu.controls(channel).contains(control) || data.len() < len {
                    return Some(OutResponse::Rejected);
                }
                let accepted = fu.settings.lock(|x| {
                    let mut s = x.get();
                    if control == FeatureUnitControls::MUTE {
                        s.muted[channel as usize] = data[0] != 0;
                    } else {
                        let volume = i16::from_le_bytes([data[0], data[1]]);
                        // CUR lies within the advertised range, except silence, which is always accepted.
                        if volume != VOLUME_SILENCE_8Q8_DB
                            && !(MIN_VOLUME_DB * VOLUME_STEPS_PER_DB..=MAX_VOLUME_DB * VOLUME_STEPS_PER_DB)
                                .contains(&volume)
                        {
                            return false;
                        }
                        s.volume_8q8_db[channel as usize] = volume;
                    }
                    x.set(s);
                    true
                });
                if !accepted {
                    return Some(OutResponse::Rejected);
                }
                self.shared.changed.signal(());
                Some(OutResponse::Accepted)
            }
        }
    }

    fn get_request<'r>(&'r mut self, req: Request, buf: &'r mut [u8]) -> Option<InResponse<'r>> {
        if req.index as u8 != u8::from(self.control_interface_number) {
            return None;
        }
        let entity = (req.index >> 8) as u8;
        let channel = req.value as u8;

        match (entity, req.request, selector(&req)) {
            (CLOCK_SOURCE_ID, request_code::CUR, control_selector::clock_source::SAMPLING_FREQ_CONTROL)
                if buf.len() >= 4 =>
            {
                buf[..4].copy_from_slice(&self.shared.sample_rate_hz.load(Ordering::Relaxed).to_le_bytes());
                Some(InResponse::Accepted(&buf[..4]))
            }
            (CLOCK_SOURCE_ID, request_code::CUR, control_selector::clock_source::CLOCK_VALID_CONTROL)
                if !buf.is_empty() =>
            {
                buf[0] = self.shared.clock_valid.load(Ordering::Relaxed) as u8;
                Some(InResponse::Accepted(&buf[..1]))
            }
            (CLOCK_SOURCE_ID, request_code::RANGE, control_selector::clock_source::SAMPLING_FREQ_CONTROL) => {
                let rates = self.shared.sample_rates_hz;
                let len = 2 + 12 * rates.len();
                if buf.len() < len {
                    return Some(InResponse::Rejected);
                }
                buf[..2].copy_from_slice(&(rates.len() as u16).to_le_bytes());
                for (i, &rate) in rates.iter().enumerate() {
                    let sub = &mut buf[2 + 12 * i..2 + 12 * (i + 1)];
                    sub[..4].copy_from_slice(&rate.to_le_bytes()); // dMIN
                    sub[4..8].copy_from_slice(&rate.to_le_bytes()); // dMAX
                    sub[8..].copy_from_slice(&0u32.to_le_bytes()); // dRES
                }
                Some(InResponse::Accepted(&buf[..len]))
            }
            (CLOCK_SELECTOR_ID, request_code::CUR, control_selector::clock_selector::CLOCK_SELECTOR_CONTROL)
                if !buf.is_empty() =>
            {
                buf[0] = 1;
                Some(InResponse::Accepted(&buf[..1]))
            }
            (CLOCK_SOURCE_ID | CLOCK_SELECTOR_ID, _, _) => Some(InResponse::Rejected),
            _ => {
                let Some(fu) = self.feature_unit(entity) else {
                    return Some(InResponse::Rejected);
                };
                let s = fu.settings.lock(|x| x.get());
                let controls = fu.controls(channel);
                match (req.request, selector(&req)) {
                    (request_code::CUR, control_selector::feature_unit::MUTE_CONTROL)
                        if controls.contains(FeatureUnitControls::MUTE) && !buf.is_empty() =>
                    {
                        buf[0] = s.muted[channel as usize] as u8;
                        Some(InResponse::Accepted(&buf[..1]))
                    }
                    (request_code::CUR, control_selector::feature_unit::VOLUME_CONTROL)
                        if controls.contains(FeatureUnitControls::VOLUME) && buf.len() >= 2 =>
                    {
                        buf[..2].copy_from_slice(&s.volume_8q8_db[channel as usize].to_le_bytes());
                        Some(InResponse::Accepted(&buf[..2]))
                    }
                    // One sub-range: MIN, MAX, RES.
                    (request_code::RANGE, control_selector::feature_unit::VOLUME_CONTROL)
                        if controls.contains(FeatureUnitControls::VOLUME) && buf.len() >= 8 =>
                    {
                        buf[..2].copy_from_slice(&1u16.to_le_bytes());
                        buf[2..4].copy_from_slice(&(MIN_VOLUME_DB * VOLUME_STEPS_PER_DB).to_le_bytes());
                        buf[4..6].copy_from_slice(&(MAX_VOLUME_DB * VOLUME_STEPS_PER_DB).to_le_bytes());
                        buf[6..8].copy_from_slice(&VOLUME_STEPS_PER_DB.to_le_bytes());
                        Some(InResponse::Accepted(&buf[..8]))
                    }
                    _ => Some(InResponse::Rejected),
                }
            }
        }
    }
}

impl<'d> Handler for Control<'d> {
    fn reset(&mut self) {
        let s = self.shared;
        s.line_out_settings.lock(|x| x.set(AudioSettings::default()));
        s.line_in_settings.lock(|x| x.set(AudioSettings::default()));
        if let Some(&rate) = s.sample_rates_hz.first() {
            s.sample_rate_hz.store(rate, Ordering::Relaxed);
        }
        s.changed.signal(());
    }

    fn control_out(&mut self, req: control::Request, data: &[u8]) -> Option<OutResponse> {
        match (req.request_type, req.recipient) {
            (RequestType::Class, Recipient::Interface) => self.set_request(req, data),
            _ => None,
        }
    }

    fn control_in<'a>(&'a mut self, req: Request, buf: &'a mut [u8]) -> Option<InResponse<'a>> {
        match (req.request_type, req.recipient) {
            (RequestType::Class, Recipient::Interface) => self.get_request(req, buf),
            _ => None,
        }
    }
}
