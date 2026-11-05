//! `aorus-lcd gif`: plays an animated GIF on the panel.
//!
//! This follows `build_gif_payload` and `gif_to_le565_frames` in `aorus_lcd.py`,
//! with two limits on that parity. Frames that are resized cannot match byte for
//! byte, see [`aorus_lcd::image`]. And pixels no frame has drawn are black here,
//! because the `image` crate leaves them fully transparent and alpha is dropped,
//! where Pillow takes the colour from the palette.
//!
//! One difference is deliberate: the first frame is sent twice, see
//! [`with_first_frame_repeated`].

use std::cmp::Ordering;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ::image::codecs::gif::GifDecoder;
use ::image::{AnimationDecoder, Frame, ImageDecoder, Limits};
use aorus_lcd::image::frame_from_rgb;
use aorus_lcd::panel::{estimated_upload_pauses, UploadOptions};
use aorus_lcd::protocol::UploadParams;
use aorus_lcd::{rle, Error, Result};

use super::upload;

/// The most frames this command sends, the repeated first frame included. This is
/// a conservative cap, not a known firmware limit: `MaxFrames` in `GifDecoder.cs`
/// has the same value, its comment says it guards against exhausting memory, and
/// only its error message calls it a panel limit. Nothing above it has been shown
/// to work, and uploads land in panel memory that is not understood.
const MAX_FRAMES: usize = 256;

/// The most frames a GIF may have: one less than [`MAX_FRAMES`], because
/// [`with_first_frame_repeated`] adds a frame.
const MAX_SOURCE_FRAMES: usize = MAX_FRAMES - 1;

/// The delay of a frame with no graphic control extension. The default of
/// `im.info.get("duration", 100)` in `gif_to_le565_frames`.
const MISSING_DELAY_MS: u32 = 100;

/// A GIF stores delays in hundredths of a second.
const MS_PER_DELAY_UNIT: u32 = 10;

/// The clamp `build_gif_payload` applies to the mean delay.
const MIN_MEAN_DELAY_MS: u32 = 1;
const MAX_MEAN_DELAY_MS: u32 = 255;

/// Signature and logical screen descriptor.
const HEADER_LEN: usize = 13;
/// Separator, position, size and flags.
const IMAGE_DESCRIPTOR_LEN: usize = 10;
const EXTENSION_BLOCK: u8 = 0x21;
const IMAGE_BLOCK: u8 = 0x2C;
const GRAPHIC_CONTROL_LABEL: u8 = 0xF9;

/// Arguments of `aorus-lcd gif`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// GIF file; each frame is resized to 320x170 without keeping its aspect ratio
    #[arg(value_name = "FILE")]
    pub file: PathBuf,

    /// Milliseconds per frame, at least 1; the panel's header holds up to 255, and
    /// more is sent as 255 [default: the mean delay of the GIF's frames]
    // Above 255, `upload_header` clamps as `aorus_lcd.py` does. The reference would
    // also send 0, but what the firmware does with that is unknown.
    #[arg(
        long,
        value_name = "MS",
        value_parser = clap::value_parser!(u32).range(1..),
    )]
    pub frame_delay: Option<u32>,

    #[arg(long, help = upload::NO_OVERLAY_HELP)]
    pub no_overlay: bool,
}

/// A GIF, ready to send.
#[derive(Debug)]
struct Prepared {
    payload: Vec<u8>,
    /// How many frames `payload` holds, which is one more than the GIF has.
    sent_frames: u16,
    /// The delay of each frame of the GIF in milliseconds, without the repeat.
    /// Never empty.
    delays_ms: Vec<u32>,
}

/// Decodes the file, says what the upload will take, then uploads it. All of the
/// refusing happens first, because connecting already puts the probe on the bus,
/// or in a dry run prints it.
pub fn run(args: Args, globals: &crate::Globals) -> Result<()> {
    let gif = prepare(&args.file)?;
    let delay_ms = args
        .frame_delay
        .unwrap_or_else(|| header_delay(&gif.delays_ms));
    let params = UploadParams::gif(gif.sent_frames, delay_ms);

    let pauses = estimated_upload_pauses(&gif.payload, &params, UploadOptions::default())?;
    // On stderr, which leaves the dry-run dump on stdout to the frames alone.
    eprintln!(
        "{}",
        summary(gif.delays_ms.len(), gif.payload.len(), pauses)
    );
    let finish = upload::Finish {
        clear_overlay: args.no_overlay,
        persist: false,
    };
    upload::send(&gif.payload, &params, finish, globals)
}

fn prepare(path: &Path) -> Result<Prepared> {
    let data = fs::read(path).map_err(|source| Error::Input {
        context: format!("cannot read GIF {}", path.display()),
        source: source.into(),
    })?;
    let decode_error = |source: ::image::ImageError| Error::Input {
        context: format!("cannot decode GIF {}", path.display()),
        source: source.into(),
    };

    let mut decoder = GifDecoder::new(Cursor::new(&data)).map_err(decode_error)?;
    // Unlike `ImageReader` in `load_file`, this decoder starts without limits, and
    // a GIF's header alone can ask for gigabytes per frame.
    decoder
        .set_limits(Limits::default())
        .map_err(decode_error)?;

    let delays_ms = frame_delays_ms(&data);
    // Before any frame is decoded, so that an oversized GIF is refused at once.
    if delays_ms.len() > MAX_SOURCE_FRAMES {
        return Err(Error::InvalidArgument(format!(
            "{} has {} frames, and at most {MAX_SOURCE_FRAMES} are accepted: the panel \
             gets at most {MAX_FRAMES}, and the first frame is sent twice",
            path.display(),
            delays_ms.len()
        )));
    }

    let mut blobs = Vec::with_capacity(delays_ms.len());
    for frame in decoder.into_frames() {
        blobs.push(encode(frame.map_err(decode_error)?)?);
    }
    if blobs.len() != delays_ms.len() {
        return Err(Error::InvalidArgument(format!(
            "{} has {} frames but {} frame delays, so it looks damaged",
            path.display(),
            blobs.len(),
            delays_ms.len()
        )));
    }

    let sent = with_first_frame_repeated(&blobs);
    Ok(Prepared {
        // This also refuses a GIF with no frames, which keeps `delays_ms` from
        // being empty.
        payload: rle::gif_payload(&sent)?,
        // `gif_payload` proved that the count fits.
        sent_frames: sent.len() as u16,
        delays_ms,
    })
}

/// The blobs to send: all of them, with the first one twice.
///
/// Firmware 1.3 on the RTX 5090 Master never shows the first frame of a GIF. Two
/// uploads with unrelated content lost it: four labelled colour blocks showed 2 to
/// 4, and five digits showed 2 to 5. So this goes by position and not by content,
/// and decoding what we sent found frame 1 correct. The copy absorbs the skip.
///
/// This is a workaround for observed behaviour, not documented protocol, and
/// neither reference mentions it. It lives here because it is a choice of what to
/// send: [`rle::gif_payload`] stays byte-identical to `build_gif_payload`.
fn with_first_frame_repeated(blobs: &[Vec<u8>]) -> Vec<&[u8]> {
    blobs
        .first()
        .into_iter()
        .chain(blobs)
        .map(Vec::as_slice)
        .collect()
}

/// Turns one composited frame into its RLE blob. `into_frames` has already
/// applied each frame's position, transparency and disposal.
fn encode(frame: Frame) -> Result<Vec<u8>> {
    let rgba = frame.into_buffer();
    let (width, height) = rgba.dimensions();
    let rgb: Vec<u8> = rgba
        .as_raw()
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|[red, green, blue, _alpha]| [*red, *green, *blue])
        .collect();
    rle::encode_frame(&frame_from_rgb(&rgb, width, height)?)
}

/// The delay of each frame, by walking the file's blocks.
///
/// The decoder cannot give this: it reports 0 both for a frame that asks for no
/// delay and for one with no graphic control extension, and `aorus_lcd.py` counts
/// the first as 0 and the second as 100. As in Pillow, an extension applies only
/// to the next image, and the last one before it wins.
///
/// This runs on a file the decoder has accepted. It stops quietly at anything it
/// does not know, and the caller compares the count with the decoded frames.
fn frame_delays_ms(gif: &[u8]) -> Vec<u32> {
    let mut delays = Vec::new();
    let mut pending = None;
    let mut at = HEADER_LEN + colour_table_len(gif.get(HEADER_LEN - 3));
    while let Some(&block) = gif.get(at) {
        match block {
            EXTENSION_BLOCK => {
                if let Some(delay) = control_delay_ms(&gif[at..]) {
                    pending = Some(delay);
                }
                at = skip_sub_blocks(gif, at + 2);
            }
            IMAGE_BLOCK => {
                delays.push(pending.take().unwrap_or(MISSING_DELAY_MS));
                let table = colour_table_len(gif.get(at + IMAGE_DESCRIPTOR_LEN - 1));
                // One more byte holds the LZW minimum code size.
                at = skip_sub_blocks(gif, at + IMAGE_DESCRIPTOR_LEN + table + 1);
            }
            _ => break,
        }
    }
    delays
}

/// The delay in `extension`, if it is a graphic control extension.
fn control_delay_ms(extension: &[u8]) -> Option<u32> {
    match *extension {
        [EXTENSION_BLOCK, GRAPHIC_CONTROL_LABEL, size, _flags, low, high, ..] if size >= 3 => {
            Some(u32::from(u16::from_le_bytes([low, high])) * MS_PER_DELAY_UNIT)
        }
        _ => None,
    }
}

/// The size of the colour table that a descriptor's `flags` announce.
fn colour_table_len(flags: Option<&u8>) -> usize {
    match flags {
        Some(flags) if flags & 0x80 != 0 => 3 << ((flags & 0x07) + 1),
        _ => 0,
    }
}

/// The position after a run of sub-blocks, which a zero length ends.
fn skip_sub_blocks(gif: &[u8], mut at: usize) -> usize {
    while let Some(&len) = gif.get(at) {
        at += 1 + usize::from(len);
        if len == 0 {
            break;
        }
    }
    at
}

/// The `F1` delay of a GIF, as `min(255, max(1, round(sum / len)))` in
/// `build_gif_payload`.
fn header_delay(delays_ms: &[u32]) -> u32 {
    let sum: u64 = delays_ms.iter().copied().map(u64::from).sum();
    let mean = round_half_even(sum, delays_ms.len() as u64);
    u32::try_from(mean)
        .unwrap_or(MAX_MEAN_DELAY_MS)
        .clamp(MIN_MEAN_DELAY_MS, MAX_MEAN_DELAY_MS)
}

/// `numerator / denominator` rounded as Python's `round` does: to the nearest
/// whole number, and from exactly halfway to the even one. With at most
/// [`MAX_FRAMES`] frames of at most 655350 ms, the reference's float division is
/// exact enough to land on the same side of every tie.
fn round_half_even(numerator: u64, denominator: u64) -> u64 {
    let (quotient, remainder) = (numerator / denominator, numerator % denominator);
    match (2 * remainder).cmp(&denominator) {
        Ordering::Less => quotient,
        Ordering::Greater => quotient + 1,
        Ordering::Equal => quotient + (quotient & 1),
    }
}

/// `frames` are those of the GIF, while `payload_bytes` is what is really sent, the
/// repeated first frame included. `pauses` leaves out the time on the bus, which is
/// why this says "at least".
fn summary(frames: usize, payload_bytes: usize, pauses: Duration) -> String {
    let seconds = pauses.as_secs_f64().ceil() as u64;
    let time = match (seconds / 60, seconds % 60) {
        (0, seconds) => format!("{seconds} s"),
        (minutes, seconds) => format!("{minutes} min {seconds} s"),
    };
    format!("{frames} frames, {payload_bytes} bytes: the upload takes at least {time}")
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[derive(Parser)]
    struct Harness {
        #[command(flatten)]
        args: Args,
    }

    fn frame_delay_parsed_from(arguments: &[&str]) -> Option<Option<u32>> {
        Some(Harness::try_parse_from(arguments).ok()?.args.frame_delay)
    }

    /// A GIF of blocks with no pixels in them, for the walker alone.
    fn gif_of(blocks: &[&[u8]]) -> Vec<u8> {
        let mut gif = b"GIF89a\x01\x00\x01\x00\x00\x00\x00".to_vec();
        for block in blocks {
            gif.extend_from_slice(block);
        }
        gif.push(0x3B);
        gif
    }

    /// A graphic control extension with this delay, in hundredths of a second.
    fn control(units: u16) -> Vec<u8> {
        let [low, high] = units.to_le_bytes();
        vec![0x21, 0xF9, 4, 0, low, high, 0, 0]
    }

    const IMAGE: &[u8] = &[0x2C, 0, 0, 0, 0, 1, 0, 1, 0, 0, 2, 2, 0x44, 0x01, 0];

    #[test]
    fn ties_go_to_the_even_neighbour() {
        // Worked by hand with Python's `round`.
        assert_eq!(header_delay(&[10, 11]), 10); // 10.5
        assert_eq!(header_delay(&[11, 12]), 12); // 11.5
        assert_eq!(header_delay(&[12, 13]), 12); // 12.5
        assert_eq!(header_delay(&[10, 10, 10, 20]), 12); // 12.5
    }

    #[test]
    fn a_mean_that_is_no_tie_goes_to_the_nearest() {
        assert_eq!(header_delay(&[10, 10, 11]), 10); // 10.33
        assert_eq!(header_delay(&[10, 11, 11]), 11); // 10.67
        assert_eq!(header_delay(&[40]), 40);
    }

    #[test]
    fn the_mean_is_clamped_to_a_byte_and_never_to_zero() {
        assert_eq!(header_delay(&[0, 0]), 1);
        assert_eq!(header_delay(&[0, 1]), 1); // 0.5 rounds to 0, then clamps
        assert_eq!(header_delay(&[254, 255]), 254); // 254.5
        assert_eq!(header_delay(&[255, 256]), 255); // 255.5 rounds to 256, then clamps
        assert_eq!(header_delay(&[655_350, 655_350]), 255);
    }

    #[test]
    fn delays_come_in_hundredths_of_a_second() {
        let gif = gif_of(&[&control(4), IMAGE, &control(7), IMAGE]);

        assert_eq!(frame_delays_ms(&gif), [40, 70]);
    }

    #[test]
    fn a_frame_with_no_extension_counts_as_100_and_one_of_zero_as_0() {
        let gif = gif_of(&[IMAGE, &control(0), IMAGE]);

        assert_eq!(frame_delays_ms(&gif), [100, 0]);
    }

    #[test]
    fn an_extension_applies_to_the_next_frame_only() {
        let gif = gif_of(&[&control(4), IMAGE, IMAGE]);

        assert_eq!(frame_delays_ms(&gif), [40, 100]);
    }

    #[test]
    fn the_walker_steps_over_colour_tables_and_other_extensions() {
        let mut gif = b"GIF89a\x01\x00\x01\x00\x80\x00\x00".to_vec();
        gif.extend_from_slice(&[0x2C; 6]); // a global table, full of block tags
        gif.extend_from_slice(&[0x21, 0xFE, 2, 0x2C, 0x2C, 0]); // a comment
        gif.extend_from_slice(&control(3));
        gif.extend_from_slice(&[0x2C, 0, 0, 0, 0, 1, 0, 1, 0, 0x80]);
        gif.extend_from_slice(&[0x2C; 6]); // a local table
        gif.extend_from_slice(&[2, 2, 0x44, 0x01, 0, 0x3B]);

        assert_eq!(frame_delays_ms(&gif), [30]);
    }

    #[test]
    fn a_truncated_file_ends_the_walk() {
        let gif = gif_of(&[&control(4), IMAGE]);

        for length in 0..gif.len() {
            assert!(frame_delays_ms(&gif[..length]).len() <= 1, "{length}");
        }
    }

    #[test]
    fn frame_delay_is_optional_and_must_be_positive() {
        assert_eq!(frame_delay_parsed_from(&["gif", "a.gif"]), Some(None));
        assert_eq!(
            frame_delay_parsed_from(&["gif", "a.gif", "--frame-delay", "300"]),
            Some(Some(300))
        );
        for delay in ["0", "-1", "soon"] {
            assert_eq!(
                frame_delay_parsed_from(&["gif", "a.gif", "--frame-delay", delay]),
                None,
                "{delay}"
            );
        }
    }

    #[test]
    fn the_summary_gives_frames_bytes_and_a_time_rounded_up() {
        assert_eq!(
            summary(30, 1234, Duration::from_millis(2300)),
            "30 frames, 1234 bytes: the upload takes at least 3 s"
        );
        assert_eq!(
            summary(600, 99, Duration::from_secs(125)),
            "600 frames, 99 bytes: the upload takes at least 2 min 5 s"
        );
    }
}
