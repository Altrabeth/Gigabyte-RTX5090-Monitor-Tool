//! Pure frame builders, opcodes and read-back parsers. No I/O.
//!
//! Everything that ends up on the wire is built here, so the layouts below are
//! the single source of truth. They are copied from `docs/PROTOCOL.md` and
//! `aorus_lcd.py`, never invented. A command needs a named builder to exist:
//! there is deliberately no public way to frame arbitrary bytes, and no builder
//! for `FA` or for the `E1` "brightness" reading.

use std::fmt;

use crate::error::{Error, Result};

/// Length of every command frame and upload chunk.
pub const FRAME_LEN: usize = 256;

/// One full frame as it goes on the wire.
pub type Frame = [u8; FRAME_LEN];

/// Every command frame carries this after the opcode. Payload chunks do not.
/// `docs/PROTOCOL.md` section 2.
pub const MAGIC: [u8; 4] = [0xCB, 0x55, 0xAC, 0x38];

/// Where a command's parameters start.
const TAIL_OFFSET: usize = 1 + MAGIC.len();

/// Panel width in pixels.
pub const WIDTH: usize = 320;
/// Panel height in pixels.
pub const HEIGHT: usize = 170;
/// Pixels in one frame.
pub const FRAME_PIXELS: usize = WIDTH * HEIGHT;
/// Bytes in one little-endian RGB565 frame.
pub const FRAME_BYTES: usize = FRAME_PIXELS * 2;

/// Prepended to the pixels of image and text uploads, never to GIF payloads.
/// `docs/PROTOCOL.md` section 6.
pub const DESCRIPTOR: [u8; 12] = [
    0x01, 0x00, 0x0B, 0xA9, 0x01, 0x00, 0x40, 0x01, 0xAA, 0x00, 0x01, 0x00,
];

/// `AA`: persists the panel configuration to NVRAM. The same frame triggers the
/// text rainbow effect, so applying that effect also saves.
pub const OP_SAVE: u8 = 0xAA;
/// `D6`: reads the firmware version.
pub const OP_GET_FW_VERSION: u8 = 0xD6;
/// `DE`: reads the mode and on-state.
pub const OP_GET_MODE: u8 = 0xDE;
/// `DF`: reads the dashboard elements and interval.
pub const OP_GET_DISPLAY: u8 = 0xDF;
/// `E1`: configures the sensor dashboard.
pub const OP_SET_DISPLAY: u8 = 0xE1;
/// `E3`: pushes live sensor values.
pub const OP_SENSOR_FEED: u8 = 0xE3;
/// `E5`: selects the display mode.
pub const OP_SET_MODE: u8 = 0xE5;
/// `E7`: panel power.
pub const OP_OPEN_LCD: u8 = 0xE7;
/// `EA`: writes an overlay template.
pub const OP_SET_IMAGE_TPL: u8 = 0xEA;
/// `EB`: reads an overlay template. `EB 03` doubles as the presence probe.
pub const OP_GET_IMAGE_TPL: u8 = 0xEB;
/// `ED`: reads the second half of a template.
pub const OP_GET_IMAGE_TPL_DATA: u8 = 0xED;
/// `F1`: upload header.
pub const OP_UPLOAD_HEADER: u8 = 0xF1;
/// `F2`: upload begin and end marker.
pub const OP_UPLOAD_MARKER: u8 = 0xF2;
/// `F3`: sets the carousel.
pub const OP_SET_LOOP: u8 = 0xF3;
/// `F4`: reads one carousel bank.
pub const OP_GET_LOOP: u8 = 0xF4;

/// Name of an opcode for dumps, from the table in `docs/PROTOCOL.md` section 2.
///
/// `FA` is named so a dump can never mislabel one, but nothing here builds it.
pub fn opcode_name(opcode: u8) -> &'static str {
    match opcode {
        OP_SAVE => "Save",
        OP_GET_FW_VERSION => "GetFWVersion",
        OP_GET_MODE => "GetMode",
        OP_GET_DISPLAY => "GetDisplay",
        OP_SET_DISPLAY => "SetDisplay",
        OP_SENSOR_FEED => "SensorFeed",
        OP_SET_MODE => "SetMode",
        OP_OPEN_LCD => "OpenLcd",
        OP_SET_IMAGE_TPL => "SetImageTpl",
        OP_GET_IMAGE_TPL => "GetImageTpl",
        OP_GET_IMAGE_TPL_DATA => "GetImageTplData",
        OP_UPLOAD_HEADER => "UploadHeader",
        OP_UPLOAD_MARKER => "UploadMarker",
        OP_SET_LOOP => "SetLoop",
        OP_GET_LOOP => "GetLoop",
        0xFA => "PowerOff",
        _ => "unknown opcode",
    }
}

/// The frame primitive: opcode, magic, tail, zero padding.
///
/// Private so the only way to reach the wire is a builder that validated its
/// input. Panics on a tail that does not fit, which no builder can produce.
fn cmd_frame(opcode: u8, tail: &[u8]) -> Frame {
    let mut frame = [0; FRAME_LEN];
    frame[0] = opcode;
    frame[1..TAIL_OFFSET].copy_from_slice(&MAGIC);
    frame[TAIL_OFFSET..][..tail.len()].copy_from_slice(tail);
    frame
}

fn clamp_u8(value: u32) -> u8 {
    u8::try_from(value).unwrap_or(u8::MAX)
}

fn clamp_u16(value: u32) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

/// A display mode. `docs/PROTOCOL.md` section 6.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Mode {
    /// Built-in stat screen 1.
    Faith1 = 0,
    /// Built-in stat screen 2.
    Faith2 = 1,
    /// Built-in stat screen 3.
    Faith3 = 2,
    /// Static image.
    Image = 3,
    /// Text.
    Text = 4,
    /// Animated GIF.
    Gif = 5,
    /// Chibi clock.
    ChibiClock = 6,
    /// Built-in carousel. Not valid as an entry of a carousel.
    Carousel = 7,
    /// Wire byte `0x0B`, which the real 5090 reported at rest. Upstream notes on
    /// an RTX 5080 describe a rotation of the built-in screens plus stored user
    /// content. Not valid as an entry of a carousel.
    PresetRotation = 8,
}

impl Mode {
    /// The mode number the CLI and the references use.
    pub const fn number(self) -> u8 {
        self as u8
    }

    /// The `E5` byte. Mode 7 is the GCC quirk: it maps to internal mode 9, so
    /// the wire byte is `0x0A`, not `0x08`. Number 8 is only this tool's label
    /// for `0x0B`, so it is not `0x09`.
    pub const fn wire_byte(self) -> u8 {
        match self {
            Mode::Carousel => 0x0A,
            Mode::PresetRotation => 0x0B,
            mode => mode.number() + 1,
        }
    }
}

impl TryFrom<u8> for Mode {
    type Error = Error;

    /// Rejects anything above 8, so no other mode can reach the wire.
    fn try_from(number: u8) -> Result<Self> {
        match number {
            0 => Ok(Mode::Faith1),
            1 => Ok(Mode::Faith2),
            2 => Ok(Mode::Faith3),
            3 => Ok(Mode::Image),
            4 => Ok(Mode::Text),
            5 => Ok(Mode::Gif),
            6 => Ok(Mode::ChibiClock),
            7 => Ok(Mode::Carousel),
            8 => Ok(Mode::PresetRotation),
            _ => Err(Error::InvalidArgument(format!(
                "display mode must be 0 to 8, not {number}"
            ))),
        }
    }
}

/// The widgets shown when nothing else is asked for: the README's list, as
/// [`parse_widgets`] takes it.
pub const DEFAULT_WIDGETS: &str = "temp,clock,usage,fan,vram,power";

/// A sensor widget of the dashboard. `docs/PROTOCOL.md` section 3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Element {
    /// GPU temperature.
    GpuTemp = 0x01,
    /// GPU clock.
    GpuClock = 0x02,
    /// GPU usage.
    GpuUsage = 0x04,
    /// Fan speed.
    FanSpeed = 0x08,
    /// RAM clock.
    RamClock = 0x10,
    /// RAM usage.
    RamUsage = 0x20,
    /// Frames per second.
    Fps = 0x40,
    /// Board power.
    Tgp = 0x80,
}

impl Element {
    /// Every element, in the order of the `E1` flag bytes.
    pub const ALL: [Element; 8] = [
        Element::GpuTemp,
        Element::GpuClock,
        Element::GpuUsage,
        Element::FanSpeed,
        Element::RamClock,
        Element::RamUsage,
        Element::Fps,
        Element::Tgp,
    ];

    /// This element's bit in an [`Elements`] mask.
    pub const fn bit(self) -> u8 {
        self as u8
    }

    /// The name of this widget on the command line and in the config file.
    pub const fn name(self) -> &'static str {
        match self {
            Element::GpuTemp => "temp",
            Element::GpuClock => "clock",
            Element::GpuUsage => "usage",
            Element::FanSpeed => "fan",
            Element::RamClock => "ramclock",
            Element::RamUsage => "vram",
            Element::Fps => "fps",
            Element::Tgp => "power",
        }
    }

    /// The widget called `name`.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] naming `name` and listing the valid names.
    pub fn from_name(name: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|element| element.name() == name)
            .ok_or_else(|| {
                let valid = Self::ALL.map(Self::name).join(", ");
                Error::InvalidArgument(format!(
                    "unknown widget {name:?}: valid widgets are {valid}"
                ))
            })
    }
}

/// Parses a comma-separated widget list such as [`DEFAULT_WIDGETS`]. The order
/// of the names does not matter, because the frame has a fixed place for each.
///
/// # Errors
///
/// [`Error::InvalidArgument`] for an empty list, an unknown name or a name given
/// twice.
pub fn parse_widgets(list: &str) -> Result<Elements> {
    if list.is_empty() {
        return Err(Error::InvalidArgument("the widget list is empty".into()));
    }
    let mut elements = Elements::NONE;
    for name in list.split(',') {
        let element = Element::from_name(name)?;
        if elements.contains(element) {
            return Err(Error::InvalidArgument(format!(
                "widget {name:?} is listed twice"
            )));
        }
        elements = elements.with(element);
    }
    Ok(elements)
}

/// A set of dashboard elements. Every `u8` is a valid mask.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Elements(u8);

impl Elements {
    /// No widgets, which turns the dashboard off.
    pub const NONE: Elements = Elements(0);
    /// All widgets.
    pub const ALL: Elements = Elements(u8::MAX);

    /// A mask as `DF` reports it.
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits)
    }

    /// The mask as `DF` reports it.
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Whether `element` is in the set.
    pub const fn contains(self, element: Element) -> bool {
        self.0 & element.bit() != 0
    }

    /// This set plus `element`.
    #[must_use]
    pub const fn with(self, element: Element) -> Self {
        Self(self.0 | element.bit())
    }
}

impl FromIterator<Element> for Elements {
    fn from_iter<I: IntoIterator<Item = Element>>(elements: I) -> Self {
        elements.into_iter().fold(Self::NONE, Self::with)
    }
}

/// Builds `E7`: `[5]` is 1 for on, 2 for off.
pub fn power_frame(on: bool) -> Frame {
    cmd_frame(OP_OPEN_LCD, &[if on { 1 } else { 2 }])
}

/// Builds `E5`.
pub fn mode_frame(mode: Mode) -> Frame {
    cmd_frame(OP_SET_MODE, &[mode.wire_byte()])
}

/// Builds `E1`: one flag byte per element, then the rotation interval in
/// seconds. `docs/PROTOCOL.md` section 3.
///
/// Send it at most once on (re)connect, only if `DF` shows a need, and never
/// periodically: it is the prime suspect in dark-panel incidents (`RECOVERY.md`).
pub fn display_frame(elements: Elements, interval_s: u8) -> Frame {
    let mut tail = [0; 9];
    for (flag, element) in tail.iter_mut().zip(Element::ALL) {
        *flag = u8::from(elements.contains(element));
    }
    tail[8] = interval_s;
    cmd_frame(OP_SET_DISPLAY, &tail)
}

/// Length of an `E3` frame. Only these two exist because upstream found the
/// panel ignores 18, 64 and 96 bytes, and 97 to 127 are untested.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum FeedFrameLen {
    /// 256 bytes, the known-good spec.
    #[default]
    Full,
    /// 128 bytes, which costs less frame time.
    Truncated,
}

impl FeedFrameLen {
    /// Length in bytes.
    pub const fn bytes(self) -> usize {
        match self {
            FeedFrameLen::Full => FRAME_LEN,
            FeedFrameLen::Truncated => 128,
        }
    }
}

impl TryFrom<usize> for FeedFrameLen {
    type Error = Error;

    fn try_from(bytes: usize) -> Result<Self> {
        match bytes {
            256 => Ok(FeedFrameLen::Full),
            128 => Ok(FeedFrameLen::Truncated),
            _ => Err(Error::InvalidArgument(format!(
                "an E3 frame must be 128 or 256 bytes, not {bytes}"
            ))),
        }
    }
}

/// Builds `E3`, clamping every value to its field so an out-of-range reading
/// shows as the maximum rather than wrapping to a small number. 16-bit fields
/// are big-endian. `docs/PROTOCOL.md` section 4.
pub fn sensor_frame(sample: &SensorSample, len: FeedFrameLen) -> Vec<u8> {
    let mut tail = [0; 13];
    tail[0] = clamp_u8(sample.gpu_temp_c);
    tail[1..3].copy_from_slice(&clamp_u16(sample.gpu_clock_mhz).to_be_bytes());
    tail[3] = clamp_u8(sample.gpu_usage_percent);
    tail[4..6].copy_from_slice(&clamp_u16(sample.fan_speed).to_be_bytes());
    tail[6..8].copy_from_slice(&clamp_u16(sample.ram_clock_mhz).to_be_bytes());
    tail[8] = clamp_u8(sample.ram_usage_percent);
    tail[9..11].copy_from_slice(&clamp_u16(sample.fps).to_be_bytes());
    tail[11..13].copy_from_slice(&clamp_u16(sample.tgp_watts).to_be_bytes());
    cmd_frame(OP_SENSOR_FEED, &tail)[..len.bytes()].to_vec()
}

/// Template kind of `EA`. `docs/PROTOCOL.md` section 5.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum TemplateKind {
    /// GIF template.
    Gif = 1,
    /// Image template.
    Image = 2,
    /// Pet template.
    Pet = 3,
}

/// An overlay template, the input to [`template_frame`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Template {
    /// Which template this configures.
    pub kind: TemplateKind,
    /// Overlay color as red, green, blue.
    pub color: [u8; 3],
    /// Image position as x, y.
    pub image_pos: (u16, u16),
    /// Data position as x, y.
    pub data_pos: (u16, u16),
    /// Whether the overlay shows.
    pub enabled: bool,
}

/// Builds `EA`. Positions are big-endian. `docs/PROTOCOL.md` section 5.
pub fn template_frame(template: &Template) -> Frame {
    let mut tail = [0; 13];
    tail[0] = template.kind as u8;
    tail[1..4].copy_from_slice(&template.color);
    tail[4..6].copy_from_slice(&template.image_pos.0.to_be_bytes());
    tail[6..8].copy_from_slice(&template.image_pos.1.to_be_bytes());
    tail[8..10].copy_from_slice(&template.data_pos.0.to_be_bytes());
    tail[10..12].copy_from_slice(&template.data_pos.1.to_be_bytes());
    tail[12] = u8::from(template.enabled);
    cmd_frame(OP_SET_IMAGE_TPL, &tail)
}

/// Most entries that fit in an `F3` frame after the interval byte.
pub const CAROUSEL_MAX_ENTRIES: usize = FRAME_LEN - TAIL_OFFSET - 1;

/// Builds `F3`: the interval, then mode+1 in play order.
/// `docs/PROTOCOL.md` section 2.
///
/// Only modes 0 to 6 can be entries. Both references silently drop other
/// entries, which would play something the caller did not ask for, so this
/// rejects them, along with a list that would not fit. An empty list is also
/// rejected: what the firmware does with one is undocumented.
pub fn carousel_frame(interval: u8, modes: &[Mode]) -> Result<Frame> {
    if modes.is_empty() {
        return Err(Error::InvalidArgument(
            "a carousel needs at least one mode".into(),
        ));
    }
    if modes.len() > CAROUSEL_MAX_ENTRIES {
        return Err(Error::InvalidArgument(format!(
            "a carousel holds at most {CAROUSEL_MAX_ENTRIES} modes, not {}",
            modes.len()
        )));
    }
    let mut tail = vec![interval];
    for mode in modes {
        if matches!(mode, Mode::Carousel | Mode::PresetRotation) {
            return Err(Error::InvalidArgument(format!(
                "carousel modes must be 0 to 6, not {}",
                mode.number()
            )));
        }
        tail.push(mode.wire_byte());
    }
    Ok(cmd_frame(OP_SET_LOOP, &tail))
}

/// Builds `AA`.
///
/// This writes NVRAM, and the same frame is the text rainbow trigger, so
/// applying that effect also saves. The daemon must never send it.
pub fn save_frame() -> Frame {
    cmd_frame(OP_SAVE, &[])
}

/// Builds `EB 03`, the presence probe. Read [`PROBE_READ_LEN`] bytes after it.
pub fn probe_frame() -> Frame {
    cmd_frame(OP_GET_IMAGE_TPL, &[0x03])
}

/// Builds the `D6` query. Read [`STATUS_READ_LEN`] bytes after it.
pub fn firmware_query_frame() -> Frame {
    cmd_frame(OP_GET_FW_VERSION, &[])
}

/// Builds the `DE` query. Read [`STATUS_READ_LEN`] bytes after it.
pub fn mode_query_frame() -> Frame {
    cmd_frame(OP_GET_MODE, &[])
}

/// Builds the `DF` query. Read [`STATUS_READ_LEN`] bytes after it.
pub fn display_query_frame() -> Frame {
    cmd_frame(OP_GET_DISPLAY, &[])
}

/// First carousel bank `F4` can read.
pub const LOOP_BANK_FIRST: u8 = 1;
/// Last carousel bank `F4` can read.
pub const LOOP_BANK_LAST: u8 = 5;

/// Builds the `F4` query for one bank, 1 to 5. Read [`LOOP_READ_LEN`] bytes
/// after it.
pub fn loop_query_frame(bank: u8) -> Result<Frame> {
    if !(LOOP_BANK_FIRST..=LOOP_BANK_LAST).contains(&bank) {
        return Err(Error::InvalidArgument(format!(
            "carousel bank must be {LOOP_BANK_FIRST} to {LOOP_BANK_LAST}, not {bank}"
        )));
    }
    Ok(cmd_frame(OP_GET_LOOP, &[bank]))
}

/// Framebuffer target of an upload, from live captures.
/// `docs/PROTOCOL.md` section 6.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum Framebuffer {
    /// Numbered framebuffer of static images.
    Static = 0x0130_0000,
    /// Numbered framebuffer of text.
    Text = 0x0132_0000,
    /// The live framebuffer GIFs stream to, so the panel must be in GIF mode
    /// first.
    Gif = 0x0000_0000,
}

impl Framebuffer {
    /// The address that goes in the `F1` header.
    pub const fn address(self) -> u32 {
        self as u32
    }
}

/// `F1` byte `[9]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ContentFlag {
    /// Image or text.
    Static = 1,
    /// Animated GIF.
    Gif = 2,
}

/// `F1` byte `[17]`. The references do not say what the values mean, only when
/// the automatic choice picks each one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum TransferMode {
    /// Chosen for payloads under [`AUTO_MODE_THRESHOLD`].
    Small = 1,
    /// Chosen for payloads of at least [`AUTO_MODE_THRESHOLD`].
    Large = 2,
}

/// Payload size at which the automatic [`TransferMode`] changes. Keep this
/// exactly: it comes from live captures.
pub const AUTO_MODE_THRESHOLD: usize = 20480;

/// The `F1` fields the caller chooses. The rest follow from the payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadParams {
    /// Where the payload goes.
    pub framebuffer: Framebuffer,
    /// Static or GIF content.
    pub flag: ContentFlag,
    /// Frame count, 0 for image and text.
    pub frame_count: u16,
    /// Per-frame delay in milliseconds, clamped to 255 on the wire.
    pub delay_ms: u32,
    /// Overrides the automatic transfer mode.
    pub mode: Option<TransferMode>,
}

impl UploadParams {
    /// An image or text upload.
    pub const fn still(framebuffer: Framebuffer) -> Self {
        Self {
            framebuffer,
            flag: ContentFlag::Static,
            frame_count: 0,
            delay_ms: 0,
            mode: None,
        }
    }

    /// A GIF upload, which `aorus_lcd.py` always sends with transfer mode 2,
    /// whatever the payload size.
    pub const fn gif(frame_count: u16, delay_ms: u32) -> Self {
        Self {
            framebuffer: Framebuffer::Gif,
            flag: ContentFlag::Gif,
            frame_count,
            delay_ms,
            mode: Some(TransferMode::Large),
        }
    }
}

/// Which upload marker to send.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Marker {
    /// Opens an upload.
    Begin = 1,
    /// Closes an upload.
    End = 2,
}

/// Builds `F2`.
pub fn marker_frame(marker: Marker) -> Frame {
    cmd_frame(OP_UPLOAD_MARKER, &[marker as u8])
}

/// Builds the 19-byte `F1` header, padded to 256 bytes.
/// `docs/PROTOCOL.md` section 6.
pub fn upload_header(params: &UploadParams, chunk_count: u32, payload_len: usize) -> Frame {
    let auto_mode = if payload_len >= AUTO_MODE_THRESHOLD {
        TransferMode::Large
    } else {
        TransferMode::Small
    };

    let mut tail = [0; 14];
    tail[0..4].copy_from_slice(&params.framebuffer.address().to_be_bytes());
    tail[4] = params.flag as u8;
    tail[5..9].copy_from_slice(&chunk_count.to_be_bytes());
    tail[9..11].copy_from_slice(&params.frame_count.to_be_bytes());
    tail[11] = clamp_u8(params.delay_ms);
    tail[12] = params.mode.unwrap_or(auto_mode) as u8;
    cmd_frame(OP_UPLOAD_HEADER, &tail)
}

/// Splits a payload into zero-padded 256-byte chunks.
///
/// The count is always `len / 256 + 1`, so an exact multiple of 256 gets one
/// extra all-zero chunk. That is what the firmware was observed to expect: do
/// not "fix" it.
pub fn chunk_payload(payload: &[u8]) -> Vec<Frame> {
    let mut chunks = vec![[0; FRAME_LEN]; payload.len() / FRAME_LEN + 1];
    for (chunk, part) in chunks.iter_mut().zip(payload.chunks(FRAME_LEN)) {
        chunk[..part.len()].copy_from_slice(part);
    }
    chunks
}

/// The role of a frame in an upload, so pacing never inspects frame bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UploadFrameKind {
    /// The `F2` marker that opens the upload.
    Begin,
    /// The `F1` header.
    Header,
    /// A payload chunk.
    Chunk,
    /// The `F2` marker that closes the upload.
    End,
}

/// One frame of an upload, tagged with its role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadFrame {
    /// What this frame is.
    pub kind: UploadFrameKind,
    /// The bytes to write.
    pub data: Frame,
}

/// Builds a full upload: Begin, Header, chunks, End.
///
/// Image and text payloads must already start with [`DESCRIPTOR`]. Fails only
/// when the chunk count does not fit the header's `u32`.
pub fn build_upload(payload: &[u8], params: &UploadParams) -> Result<Vec<UploadFrame>> {
    let chunks = chunk_payload(payload);
    let chunk_count = u32::try_from(chunks.len()).map_err(|_| {
        Error::InvalidArgument(format!("a payload of {} bytes is too big", payload.len()))
    })?;

    let frame = |kind, data| UploadFrame { kind, data };
    let mut frames = Vec::with_capacity(chunks.len() + 3);
    frames.push(frame(UploadFrameKind::Begin, marker_frame(Marker::Begin)));
    frames.push(frame(
        UploadFrameKind::Header,
        upload_header(params, chunk_count, payload.len()),
    ));
    frames.extend(
        chunks
            .into_iter()
            .map(|chunk| frame(UploadFrameKind::Chunk, chunk)),
    );
    frames.push(frame(UploadFrameKind::End, marker_frame(Marker::End)));
    Ok(frames)
}

/// Bytes to read after `D6`, `DE` or `DF`. `docs/PROTOCOL.md` section 2.
pub const STATUS_READ_LEN: usize = 4;
/// Bytes to read after `F4`. `docs/PROTOCOL.md` section 2.
pub const LOOP_READ_LEN: usize = 8;
/// Bytes to read after `EB 03`. `docs/PROTOCOL.md` section 2.
pub const PROBE_READ_LEN: usize = 8;

/// Checks a read-back's length, so the parsers can index it freely.
fn fixed_reply<'a, const N: usize>(what: &str, reply: &'a [u8]) -> Result<&'a [u8; N]> {
    reply.try_into().map_err(|_| {
        Error::Protocol(format!(
            "{what} read-back must be {N} bytes, not {}",
            reply.len()
        ))
    })
}

/// Panel firmware version from `D6`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FirmwareVersion {
    /// High nibble of byte 1.
    pub major: u8,
    /// Low nibble of byte 1.
    pub minor: u8,
}

impl fmt::Display for FirmwareVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// Parses the `D6` read-back.
pub fn parse_firmware(reply: &[u8]) -> Result<FirmwareVersion> {
    let reply = fixed_reply::<STATUS_READ_LEN>("D6", reply)?;
    Ok(FirmwareVersion {
        major: reply[1] >> 4,
        minor: reply[1] & 0x0F,
    })
}

/// Mode and on-state from `DE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeStatus {
    /// Byte 1 as the panel reported it. Kept because the panel can report
    /// values with no known meaning, and `status` shows them as they are.
    pub raw_mode: u8,
    /// Whether the panel is on.
    pub on: bool,
}

impl ModeStatus {
    /// The mode, or `None` for a raw value with no known meaning.
    ///
    /// The references define this as raw - 1, with internal 9 meaning mode 7.
    /// So both `0x08` and the `0x0A` that `E5` sends read back as the carousel.
    /// Raw - 1 stops at `0x08`: number 8 is `0x0B`, so `0x09` has no meaning.
    pub fn mode(&self) -> Option<Mode> {
        match self.raw_mode {
            raw @ 0x01..=0x08 => Mode::try_from(raw - 1).ok(),
            0x0A => Some(Mode::Carousel),
            0x0B => Some(Mode::PresetRotation),
            _ => None,
        }
    }
}

/// Parses the `DE` read-back. Any mode byte is accepted: see
/// [`ModeStatus::raw_mode`].
pub fn parse_mode(reply: &[u8]) -> Result<ModeStatus> {
    let reply = fixed_reply::<STATUS_READ_LEN>("DE", reply)?;
    Ok(ModeStatus {
        raw_mode: reply[1],
        on: reply[2] == 1,
    })
}

/// Dashboard configuration from `DF`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayStatus {
    /// Enabled widgets.
    pub elements: Elements,
    /// Rotation interval in seconds.
    pub interval_s: u8,
}

/// Parses the `DF` read-back.
pub fn parse_display(reply: &[u8]) -> Result<DisplayStatus> {
    let reply = fixed_reply::<STATUS_READ_LEN>("DF", reply)?;
    Ok(DisplayStatus {
        elements: Elements::from_bits(reply[1]),
        interval_s: reply[2],
    })
}

/// One slot of a carousel bank.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopEntry {
    /// An unused slot.
    Empty,
    /// A mode `F3` can set.
    Mode(Mode),
    /// A raw value `F3` cannot set. Kept rather than refused, because `F4` has
    /// not been seen on real hardware and one odd slot should not fail `status`.
    Unknown(u8),
}

impl LoopEntry {
    fn from_raw(raw: u8) -> Self {
        match raw {
            0 => LoopEntry::Empty,
            1..=7 => Mode::try_from(raw - 1).map_or(LoopEntry::Unknown(raw), LoopEntry::Mode),
            _ => LoopEntry::Unknown(raw),
        }
    }
}

/// One carousel bank from `F4`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoopBank {
    /// The interval, byte 0.
    pub interval: u8,
    /// The slots, bytes 1 to 3.
    pub entries: [LoopEntry; 3],
}

/// Parses the `F4` read-back.
///
/// `docs/PROTOCOL.md` section 2 gives "up to 3 (mode+1) entries per bank +
/// interval byte0" without offsets, and neither reference implements this. That
/// the entries directly follow the interval is an inference.
pub fn parse_loop_bank(reply: &[u8]) -> Result<LoopBank> {
    let reply = fixed_reply::<LOOP_READ_LEN>("F4", reply)?;
    Ok(LoopBank {
        interval: reply[0],
        entries: [reply[1], reply[2], reply[3]].map(LoopEntry::from_raw),
    })
}

/// One reading of the GPU sensors, the input to an `E3` sensor-feed frame.
///
/// Fields are wider than their wire encoding on purpose: sensor code stores
/// what it read, and the `E3` builder clamps each value to its `u8` or `u16`
/// field. Layout reference: `docs/PROTOCOL.md` section 4 and C#
/// `SensorSample.cs`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SensorSample {
    /// GPU temperature in degrees Celsius.
    pub gpu_temp_c: u32,
    /// Graphics clock in MHz.
    pub gpu_clock_mhz: u32,
    /// GPU utilization in percent.
    pub gpu_usage_percent: u32,
    /// Fan speed as displayed by the panel, which prints the raw number: RPM
    /// where available, otherwise percent.
    pub fan_speed: u32,
    /// Memory clock in MHz.
    pub ram_clock_mhz: u32,
    /// VRAM used, as a percentage of the total.
    pub ram_usage_percent: u32,
    /// Frames per second. NVML has no FPS source, so this stays 0.
    pub fps: u32,
    /// Board power in whole watts. GCC sent deci-watts, but the panel prints
    /// the raw number.
    pub tgp_watts: u32,
}
