//! `image` and `gif` through the real binary, in `--dry-run` only.
//!
//! As in `cli_dry_run.rs`, the child gets a lock path that cannot exist, so a
//! broken `--dry-run` stops at the lock and does not reach the bus. The one
//! helper that starts the child always passes `--dry-run`.
//!
//! GIF fixtures are written byte by byte here, because the delays and blocks are
//! what is under test, and an encoder would choose them itself. They are 320x170
//! wherever the test reads pixels, which `frame_from_rgb` copies unfiltered. The
//! expected bytes are worked out by hand from `docs/PROTOCOL.md` section 6. For
//! the RLE: 54400 pixels make one window of 0x7FFF and one of 0x5481, so a solid
//! frame is two runs, `FF FF <px> 81 D4 <px>`, of 8 bytes.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use aorus_lcd::bus::lock::LOCK_PATH_ENV;
use image::{Rgb, RgbImage};
use tempfile::TempDir;

const WIDTH: u16 = 320;
const HEIGHT: u16 = 170;

const RED: u8 = 0;
const BLUE: u8 = 1;
/// A palette index that fixtures mark as transparent.
const SEE_THROUGH: u8 = 2;

/// A palette of 2^7 entries, so with an LZW minimum code size of 7, codes start
/// at 8 bits. See [`lzw`].
const MIN_CODE_SIZE: u8 = 7;
const LZW_CLEAR: u8 = 128;
const LZW_END: u8 = 129;

/// The rows that `F1` is zero in, after the first two.
const ZERO_ROW: &str = "00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00";

fn dry_run(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_aorus-lcd"))
        .arg("--dry-run")
        .args(arguments)
        .env(LOCK_PATH_ENV, "/nonexistent/aorus-lcd-test/lock")
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

/// Runs a command that has to work, and returns its dump.
fn dump_of(arguments: &[&str]) -> String {
    let output = dry_run(arguments);
    assert!(output.status.success(), "{arguments:?}: {output:?}");
    stdout(&output)
}

/// The first line of every frame, without the first eight pixels of a chunk.
fn frame_sequence(dump: &str) -> Vec<&str> {
    dump.lines()
        .filter(|line| !line.starts_with(' ') && !line.starts_with("dry-run:"))
        .map(|line| line.split_once(':').map_or(line, |(start, _)| start))
        .collect()
}

/// The hex of the first `count` rows after the `index`th line that starts with
/// `header`.
fn rows_after<'a>(dump: &'a str, header: &str, index: usize, count: usize) -> Vec<&'a str> {
    let (start, _) = dump
        .lines()
        .enumerate()
        .filter(|(_, line)| line.starts_with(header))
        .nth(index)
        .unwrap_or_else(|| panic!("no {header} number {index} in {dump}"));
    dump.lines()
        .skip(start + 1)
        .take(count)
        .map(|line| line.trim_start().split_once("  ").unwrap().1)
        .collect()
}

fn f1_rows(dump: &str) -> Vec<&str> {
    rows_after(dump, "F1 ", 0, 3)
}

/// One image of a fixture.
struct Image {
    position: (u16, u16),
    size: (u16, u16),
    pixels: Vec<u8>,
    /// The graphic control extension: delay in hundredths of a second, disposal
    /// method, and whether [`SEE_THROUGH`] is transparent. `None` leaves it out.
    control: Option<(u16, u8, bool)>,
}

impl Image {
    fn solid(colour: u8, control: Option<(u16, u8, bool)>) -> Self {
        Self {
            position: (0, 0),
            size: (WIDTH, HEIGHT),
            pixels: vec![colour; usize::from(WIDTH) * usize::from(HEIGHT)],
            control,
        }
    }

    /// A solid frame that asks for this delay.
    fn delayed(units: u16) -> Self {
        Self::solid(RED, Some((units, 0, false)))
    }
}

/// LZW with no compression: each pixel is its own 8-bit code. A clear code resets
/// the table long before it could grow to 9 bits.
fn lzw(pixels: &[u8]) -> Vec<u8> {
    let mut codes = Vec::new();
    for run in pixels.chunks(100) {
        codes.push(LZW_CLEAR);
        codes.extend_from_slice(run);
    }
    codes.push(LZW_END);
    codes
}

fn push_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// A GIF89a header and global table: red, blue, then black.
fn gif_header(screen: (u16, u16)) -> Vec<u8> {
    let mut gif = b"GIF89a".to_vec();
    push_u16(&mut gif, screen.0);
    push_u16(&mut gif, screen.1);
    gif.extend_from_slice(&[0x86, 0, 0]);
    gif.extend_from_slice(&[255, 0, 0, 0, 0, 255]);
    gif.resize(gif.len() + 126 * 3, 0);
    gif
}

fn push_image(gif: &mut Vec<u8>, image: &Image) {
    if let Some((units, disposal, transparent)) = image.control {
        gif.extend_from_slice(&[0x21, 0xF9, 4, disposal << 2 | u8::from(transparent)]);
        push_u16(gif, units);
        gif.extend_from_slice(&[SEE_THROUGH, 0]);
    }
    gif.push(0x2C);
    for field in [
        image.position.0,
        image.position.1,
        image.size.0,
        image.size.1,
    ] {
        push_u16(gif, field);
    }
    gif.extend_from_slice(&[0, MIN_CODE_SIZE]);
    for block in lzw(&image.pixels).chunks(255) {
        gif.push(block.len() as u8);
        gif.extend_from_slice(block);
    }
    gif.push(0);
}

fn gif_bytes(screen: (u16, u16), images: &[Image]) -> Vec<u8> {
    let mut gif = gif_header(screen);
    for image in images {
        push_image(&mut gif, image);
    }
    gif.push(0x3B);
    gif
}

fn write_file(dir: &TempDir, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.path().join(name);
    fs::write(&path, bytes).unwrap();
    path
}

fn text(path: &Path) -> &str {
    path.to_str().unwrap()
}

/// The dump of a panel-sized GIF of these images.
fn gif_dump(images: &[Image], options: &[&str]) -> String {
    let dir = tempfile::tempdir().unwrap();
    let path = write_file(&dir, "a.gif", &gif_bytes((WIDTH, HEIGHT), images));
    let mut arguments = vec!["gif", text(&path)];
    arguments.extend_from_slice(options);
    dump_of(&arguments)
}

/// Byte `[16]` of `F1`, as a hex row: the delay, then transfer mode 2.
fn delay_row(delay: &str) -> String {
    format!("{delay} 02 00 00 00 00 00 00 00 00 00 00 00 00 00 00")
}

fn png_of(dir: &TempDir, width: u32, height: u32) -> PathBuf {
    let path = dir.path().join("a.png");
    RgbImage::from_pixel(width, height, Rgb([255, 0, 0]))
        .save(&path)
        .unwrap();
    path
}

/// Refused with nothing on stdout, so before the probe and any bus access.
fn assert_refused(arguments: &[&str], message: &str) {
    let output = dry_run(arguments);

    assert!(!output.status.success(), "{arguments:?}: {output:?}");
    assert_eq!(stdout(&output), "", "{arguments:?}");
    let errors = stderr(&output);
    assert!(errors.contains(message), "{arguments:?}: {errors}");
}

#[test]
fn an_image_is_the_probe_then_the_upload_then_set_mode() {
    let dir = tempfile::tempdir().unwrap();
    let png = png_of(&dir, 320, 170);

    let dump = dump_of(&["image", text(&png)]);

    // 12 + 108800 = 108812 bytes, so 425 full chunks and one more. With both
    // markers, the header, `E5` and the probe, that is 431.
    let frames = frame_sequence(&dump);
    assert_eq!(frames.len(), 431, "{frames:?}");
    assert_eq!(
        frames[..3],
        [
            "EB GetImageTpl (256 bytes)",
            "F2 UploadMarker (256 bytes)",
            "F1 UploadHeader (256 bytes)",
        ]
    );
    assert!(
        frames[3..429]
            .iter()
            .all(|frame| *frame == "payload chunk (256 bytes)"),
        "{frames:?}"
    );
    assert_eq!(
        frames[429..],
        ["F2 UploadMarker (256 bytes)", "E5 SetMode (256 bytes)"]
    );
}

#[test]
fn an_image_header_is_framebuffer_flag_and_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let png = png_of(&dir, 320, 170);

    let dump = dump_of(&["image", text(&png)]);

    assert_eq!(
        f1_rows(&dump),
        [
            // Framebuffer 0x01300000, flag 1, 426 chunks (0x1AA), no frames.
            "F1 CB 55 AC 38 01 30 00 00 01 00 00 01 AA 00 00",
            // No delay, then transfer mode 2 for at least 20480 bytes.
            "00 02 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
            ZERO_ROW,
        ]
    );
    assert_eq!(
        // Byte 5 is the mode plus one, so 4 for mode 3.
        rows_after(&dump, "E5 ", 0, 1),
        ["E5 CB 55 AC 38 04 00 00 00 00 00 00 00 00 00 00"]
    );
}

#[test]
fn an_image_payload_is_the_descriptor_then_the_pixels() {
    let dir = tempfile::tempdir().unwrap();
    let png = png_of(&dir, 320, 170);

    let dump = dump_of(&["-v", "image", text(&png)]);

    assert_eq!(
        rows_after(&dump, "payload chunk", 0, 1),
        ["01 00 0B A9 01 00 40 01 AA 00 01 00 00 F8 00 F8"]
    );
    // 108812 - 425 * 256 leaves 12 bytes for the last chunk.
    assert_eq!(
        rows_after(&dump, "payload chunk", 425, 2),
        ["00 F8 00 F8 00 F8 00 F8 00 F8 00 F8 00 00 00 00", ZERO_ROW]
    );
}

#[test]
fn an_image_of_another_size_is_resized_to_a_full_frame() {
    let dir = tempfile::tempdir().unwrap();
    let png = png_of(&dir, 40, 30);

    let dump = dump_of(&["image", text(&png)]);

    assert_eq!(frame_sequence(&dump).len(), 431);
}

#[test]
fn an_image_saves_only_when_asked_and_only_at_the_very_end() {
    let dir = tempfile::tempdir().unwrap();
    let png = png_of(&dir, 320, 170);

    let plain = dump_of(&["image", text(&png)]);
    let saved = dump_of(&["image", text(&png), "--save"]);

    assert!(!plain.contains("AA Save"), "{plain}");
    assert!(!plain.contains("NVRAM"), "{plain}");
    let frames = frame_sequence(&saved);
    assert_eq!(frames.len(), 432);
    assert_eq!(frames[430], "E5 SetMode (256 bytes)");
    assert_eq!(frames[431], "AA Save (256 bytes)");
    assert!(
        saved.contains("dry-run: a real run writes the panel configuration to NVRAM\nAA Save"),
        "{saved}"
    );
    assert_eq!(
        rows_after(&saved, "AA ", 0, 1),
        ["AA CB 55 AC 38 00 00 00 00 00 00 00 00 00 00 00"]
    );
}

#[test]
fn an_image_prints_no_progress() {
    let dir = tempfile::tempdir().unwrap();
    let png = png_of(&dir, 320, 170);

    let output = dry_run(&["image", text(&png)]);

    assert_eq!(stderr(&output), "");
}

#[test]
fn a_gif_sets_the_mode_first_and_streams_after_it() {
    let dump = gif_dump(&[Image::delayed(4), Image::delayed(4)], &[]);

    assert_eq!(
        frame_sequence(&dump),
        [
            "EB GetImageTpl (256 bytes)",
            "E5 SetMode (256 bytes)",
            "F2 UploadMarker (256 bytes)",
            "F1 UploadHeader (256 bytes)",
            "payload chunk (256 bytes)",
            "F2 UploadMarker (256 bytes)",
        ],
        "{dump}"
    );
    assert_eq!(
        // Byte 5 is the mode plus one, so 6 for mode 5.
        rows_after(&dump, "E5 ", 0, 1),
        ["E5 CB 55 AC 38 06 00 00 00 00 00 00 00 00 00 00"]
    );
}

#[test]
fn a_gif_header_is_framebuffer_flag_chunks_frames_delay_and_mode() {
    let dump = gif_dump(&[Image::delayed(4), Image::delayed(4)], &[]);

    assert_eq!(
        f1_rows(&dump),
        [
            // Framebuffer 0, flag 2, one chunk, two frames.
            "F1 CB 55 AC 38 00 00 00 00 02 00 00 00 01 00 02",
            // 40 ms, and mode 2 although the payload is far under 20480 bytes.
            "28 02 00 00 00 00 00 00 00 00 00 00 00 00 00 00",
            ZERO_ROW,
        ]
    );
}

#[test]
fn a_gif_payload_is_the_table_then_the_blobs() {
    let images = [
        Image::solid(RED, None),
        Image::solid(BLUE, Some((4, 0, false))),
    ];

    let dump = gif_dump(&images, &["-v"]);

    assert_eq!(
        rows_after(&dump, "payload chunk", 0, 3),
        [
            // Two frames, then frame 0 ending at 2 + 20 + 8 - 1 = 0x1D, 320, 170, 3.
            // Frame 1 ends at 2 + 20 + 16 - 1 = 0x25.
            "02 00 1D 00 00 00 40 01 AA 00 03 00 25 00 00 00",
            // The rest of that entry, red, then the start of blue.
            "40 01 AA 00 03 00 FF FF 00 F8 81 D4 00 F8 FF FF",
            "1F 00 81 D4 1F 00 00 00 00 00 00 00 00 00 00 00",
        ]
    );
}

#[test]
fn a_gif_frame_is_composited_over_the_last_one() {
    // Three pixels at (0, 0), the first see-through, over a red frame. This gives
    // red, blue, blue, then the red of the frame below.
    let patch = Image {
        position: (0, 0),
        size: (3, 1),
        pixels: vec![SEE_THROUGH, BLUE, BLUE],
        control: Some((4, 0, true)),
    };

    let dump = gif_dump(&[Image::delayed(4), patch], &["-v"]);

    assert_eq!(
        rows_after(&dump, "payload chunk", 0, 3),
        [
            // The second blob is 16 bytes, so frame 1 ends at 2 + 20 + 24 - 1 = 0x2D.
            "02 00 1D 00 00 00 40 01 AA 00 03 00 2D 00 00 00",
            "40 01 AA 00 03 00 FF FF 00 F8 81 D4 00 F8 03 00",
            // A literal of 3, a run of 0x7FFC to the window's end, then 0x5481.
            "00 F8 1F 00 1F 00 FC FF 00 F8 81 D4 00 F8 00 00",
        ]
    );
}

#[test]
fn a_gif_frame_honours_the_disposal_of_the_last_one() {
    // Red is cleared after its frame (disposal 2), so one blue pixel at (1, 0) has
    // nothing around it. Nothing shows through as black.
    let cleared = Image::solid(RED, Some((4, 2, false)));
    let patch = Image {
        position: (1, 0),
        size: (1, 1),
        pixels: vec![BLUE],
        control: Some((4, 0, false)),
    };

    let dump = gif_dump(&[cleared, patch], &["-v"]);

    assert_eq!(
        rows_after(&dump, "payload chunk", 0, 3),
        [
            // The second blob is 14 bytes, so frame 1 ends at 2 + 20 + 22 - 1 = 0x2B.
            "02 00 1D 00 00 00 40 01 AA 00 03 00 2B 00 00 00",
            "40 01 AA 00 03 00 FF FF 00 F8 81 D4 00 F8 02 00",
            // A literal of black and blue, a run of 0x7FFD, then 0x5481.
            "00 00 1F 00 FD FF 00 00 81 D4 00 00 00 00 00 00",
        ]
    );
}

#[test]
fn a_gif_of_any_size_becomes_full_frames() {
    let dir = tempfile::tempdir().unwrap();
    let image = Image {
        position: (0, 0),
        size: (5, 3),
        pixels: vec![RED; 15],
        control: Some((4, 0, false)),
    };
    let path = write_file(&dir, "a.gif", &gif_bytes((5, 3), &[image]));

    let dump = dump_of(&["-v", "gif", text(&path)]);

    assert_eq!(
        rows_after(&dump, "payload chunk", 0, 1),
        ["01 00 13 00 00 00 40 01 AA 00 03 00 FF FF 00 F8"]
    );
}

#[test]
fn the_gif_delay_is_the_mean_rounded_to_even() {
    let cases: [(&[u16], &str); 4] = [
        // Hundredths of a second, so tens of milliseconds.
        (&[1, 1, 1, 2], "0C"), // 12.5 ms goes down to 12
        (&[3, 4, 4, 4], "26"), // 37.5 ms goes up to 38
        (&[2, 3, 3], "1B"),    // 26.67 ms goes to 27
        (&[4], "28"),
    ];
    for (units, delay) in cases {
        let images: Vec<Image> = units.iter().map(|units| Image::delayed(*units)).collect();

        let dump = gif_dump(&images, &[]);

        assert_eq!(f1_rows(&dump)[1], delay_row(delay), "{units:?}");
    }
}

#[test]
fn the_gif_delay_is_clamped_to_one_and_255() {
    let dump = gif_dump(&[Image::delayed(0)], &[]);
    assert_eq!(f1_rows(&dump)[1], delay_row("01"));

    let dump = gif_dump(&[Image::delayed(26)], &[]);
    assert_eq!(f1_rows(&dump)[1], delay_row("FF"));
}

#[test]
fn a_gif_frame_with_no_delay_set_counts_as_100_ms() {
    let dump = gif_dump(&[Image::solid(RED, None), Image::solid(RED, None)], &[]);
    assert_eq!(f1_rows(&dump)[1], delay_row("64"));

    // A delay of zero is a delay, not a missing one: (0 + 100) / 2.
    let images = [Image::delayed(0), Image::solid(RED, None)];
    let dump = gif_dump(&images, &[]);
    assert_eq!(f1_rows(&dump)[1], delay_row("32"));
}

#[test]
fn frame_delay_overrides_the_gif() {
    let dump = gif_dump(&[Image::delayed(4)], &["--frame-delay", "7"]);
    assert_eq!(f1_rows(&dump)[1], delay_row("07"));

    let dump = gif_dump(&[Image::delayed(4)], &["--frame-delay", "300"]);
    assert_eq!(f1_rows(&dump)[1], delay_row("FF"));
}

#[test]
fn a_gif_says_what_the_upload_will_take_and_shows_no_progress() {
    let dir = tempfile::tempdir().unwrap();
    let images = [Image::delayed(4), Image::delayed(4)];
    let path = write_file(&dir, "a.gif", &gif_bytes((WIDTH, HEIGHT), &images));

    let output = dry_run(&["gif", text(&path)]);

    assert!(output.status.success(), "{output:?}");
    // 200 ms to settle, 500 and 1000 for the marker and the header, and 10 each
    // for the chunk and END: 1.72 s.
    assert_eq!(
        stderr(&output),
        "2 frames, 38 bytes: the upload takes at least 2 s\n"
    );
}

#[test]
fn an_unreadable_file_is_refused_before_the_probe() {
    for command in ["image", "gif"] {
        assert_refused(&[command, "/nonexistent/aorus-lcd-test/a"], "cannot read");

        let dir = tempfile::tempdir().unwrap();
        let path = write_file(&dir, "a", b"this is no picture");
        assert_refused(&[command, text(&path)], "cannot ");
    }
}

#[test]
fn a_gif_that_is_no_gif_is_refused_before_the_probe() {
    let dir = tempfile::tempdir().unwrap();
    let png = png_of(&dir, 320, 170);

    assert_refused(&["gif", text(&png)], "cannot decode GIF");
}

#[test]
fn a_truncated_gif_is_refused_before_the_probe() {
    let dir = tempfile::tempdir().unwrap();
    let whole = gif_bytes((WIDTH, HEIGHT), &[Image::delayed(4), Image::delayed(4)]);
    let path = write_file(&dir, "a.gif", &whole[..whole.len() - 100]);

    assert_refused(&["gif", text(&path)], "cannot decode GIF");
}

#[test]
fn a_gif_with_no_frames_is_refused_before_the_probe() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_file(&dir, "a.gif", &gif_bytes((WIDTH, HEIGHT), &[]));

    assert_refused(&["gif", text(&path)], "cannot decode GIF");
}

/// A 1x1 frame with no delay set, for GIFs that need many frames.
fn tiny_frame(pixel: u8) -> Image {
    Image {
        position: (0, 0),
        size: (1, 1),
        pixels: vec![pixel],
        control: None,
    }
}

#[test]
fn a_gif_of_256_frames_is_sent() {
    let dir = tempfile::tempdir().unwrap();
    let images: Vec<Image> = (0..256).map(|_| tiny_frame(RED)).collect();
    let path = write_file(&dir, "a.gif", &gif_bytes((1, 1), &images));

    let dump = dump_of(&["gif", text(&path)]);

    // 2 + 2560 + 256 * 8 = 4610 bytes, so 18 full chunks and one more. With the
    // probe, `E5`, both markers and the header, that is 24.
    assert_eq!(frame_sequence(&dump).len(), 24, "{dump}");
    assert_eq!(
        f1_rows(&dump),
        [
            // 19 chunks (0x13), and 256 frames (0x100).
            "F1 CB 55 AC 38 00 00 00 00 02 00 00 00 13 01 00",
            &delay_row("64"),
            ZERO_ROW,
        ]
    );
}

#[test]
fn a_gif_of_257_frames_is_refused_before_the_probe() {
    let dir = tempfile::tempdir().unwrap();
    let images: Vec<Image> = (0..257).map(|_| tiny_frame(RED)).collect();
    let path = write_file(&dir, "a.gif", &gif_bytes((1, 1), &images));

    assert_refused(&["gif", text(&path)], "has 257 frames, and at most 256");
}

/// An image whose only code is one that the LZW table does not hold yet.
fn undecodable_frame() -> Image {
    tiny_frame(255)
}

#[test]
fn a_gif_with_a_frame_that_will_not_decode_is_refused_before_the_probe() {
    let dir = tempfile::tempdir().unwrap();
    let images = [tiny_frame(RED), undecodable_frame()];
    let path = write_file(&dir, "a.gif", &gif_bytes((1, 1), &images));

    assert_refused(&["gif", text(&path)], "cannot decode GIF");
}

// The bad frame comes early, so that any decoding would end in that error instead
// of the one about the count.
#[test]
fn too_many_frames_are_refused_without_decoding_any() {
    let dir = tempfile::tempdir().unwrap();
    let mut images = vec![tiny_frame(RED), undecodable_frame()];
    images.extend((0..255).map(|_| tiny_frame(RED)));
    let path = write_file(&dir, "a.gif", &gif_bytes((1, 1), &images));

    assert_refused(&["gif", text(&path)], "has 257 frames, and at most 256");
}

#[test]
fn a_frame_delay_of_zero_is_refused_before_the_probe() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_file(
        &dir,
        "a.gif",
        &gif_bytes((WIDTH, HEIGHT), &[Image::delayed(4)]),
    );

    assert_refused(&["gif", text(&path), "--frame-delay", "0"], "--frame-delay");
}
