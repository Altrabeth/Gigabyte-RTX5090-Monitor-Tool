//! Loads images, resizes them to the panel's 320x170 and converts them to
//! RGB565. Refer to the external crate as `::image` inside this module.
//!
//! Byte parity with the reference (`image_to_le565` in `aorus_lcd.py`) applies
//! to the RGB-to-565 conversion only. Resized images cannot match: the
//! reference resizes with Pillow's `LANCZOS`, and the `image` crate's Lanczos3
//! is a different implementation, so those frames look the same but differ in
//! their bytes. An image that is already 320x170 is copied, not filtered, by
//! both libraries, so it does stay byte-identical.
//!
//! As in the reference, the aspect ratio is not kept, alpha is dropped rather
//! than blended, and EXIF orientation is ignored.

use std::path::Path;

use ::image::imageops::{self, FilterType};
use ::image::{ImageBuffer, ImageReader, Rgb, RgbImage};

use crate::error::{Error, Result};

const WIDTH: u32 = 320;
const HEIGHT: u32 = 170;
const FRAME_PIXELS: usize = WIDTH as usize * HEIGHT as usize;
const RGB_BYTES: usize = FRAME_PIXELS * 3;
/// 108800, `FRAME_BYTES` in `aorus_lcd.py`.
const FRAME_BYTES: usize = FRAME_PIXELS * 2;

/// A decoded RGB image borrowed from the caller's buffer.
type RgbView<'a> = ImageBuffer<Rgb<u8>, &'a [u8]>;

/// Loads an image file and returns it as a panel frame. The format is taken
/// from the file's content, not its name.
///
/// # Errors
///
/// [`Error::Input`] if the file cannot be read or decoded.
pub fn load_file(path: &Path) -> Result<Vec<u8>> {
    let reader = ImageReader::open(path)
        .and_then(ImageReader::with_guessed_format)
        .map_err(|source| Error::Input {
            context: format!("cannot read image {}", path.display()),
            source: source.into(),
        })?;
    let image = reader.decode().map_err(|source| Error::Input {
        context: format!("cannot decode image {}", path.display()),
        source: source.into(),
    })?;
    let image = image.into_rgb8();

    frame_from_rgb(image.as_raw(), image.width(), image.height())
}

/// Turns a decoded RGB image of any size into a panel frame. `rgb` holds three
/// bytes per pixel in row-major order.
///
/// # Errors
///
/// [`Error::InvalidArgument`] if the image is empty or `rgb` is not exactly
/// `width * height * 3` bytes.
pub fn frame_from_rgb(rgb: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let image = view_rgb(rgb, width, height)?;
    rgb_to_le565(resize_to_panel(&image).as_raw())
}

/// Converts a 320x170 row-major RGB buffer to a little-endian RGB565 frame of
/// 108800 bytes.
///
/// # Errors
///
/// [`Error::InvalidArgument`] if `rgb` is not exactly 320x170 pixels. Use
/// [`frame_from_rgb`] for other sizes.
pub fn rgb_to_le565(rgb: &[u8]) -> Result<Vec<u8>> {
    if rgb.len() != RGB_BYTES {
        return Err(Error::InvalidArgument(format!(
            "expected {RGB_BYTES} bytes of RGB for a {WIDTH}x{HEIGHT} frame, got {}",
            rgb.len()
        )));
    }

    let mut frame = Vec::with_capacity(FRAME_BYTES);
    for pixel in rgb.as_chunks::<3>().0 {
        frame.extend_from_slice(&pixel_to_le565(*pixel));
    }
    Ok(frame)
}

/// A frame filled with one colour, given as `[r, g, b]`.
pub fn solid_frame(rgb: [u8; 3]) -> Vec<u8> {
    pixel_to_le565(rgb).repeat(FRAME_PIXELS)
}

/// Keeps the top 5/6/5 bits. Low bits are truncated, not rounded, as in
/// `image_to_le565`.
fn pixel_to_le565([red, green, blue]: [u8; 3]) -> [u8; 2] {
    let red = u16::from(red & 0xF8) << 8;
    let green = u16::from(green & 0xFC) << 3;
    let blue = u16::from(blue >> 3);
    (red | green | blue).to_le_bytes()
}

/// Wraps `rgb` as an image without copying it. The length check is exact
/// because `from_raw` alone lets an overlong buffer through, and an empty image
/// is refused because there is nothing to resample.
fn view_rgb(rgb: &[u8], width: u32, height: u32) -> Result<RgbView<'_>> {
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(3));
    if rgb.is_empty() || expected != Some(rgb.len()) {
        return Err(Error::InvalidArgument(format!(
            "{} bytes of RGB do not form a {width}x{height} image",
            rgb.len()
        )));
    }
    ImageBuffer::from_raw(width, height, rgb)
        .ok_or_else(|| Error::InvalidArgument(format!("cannot wrap a {width}x{height} image")))
}

fn resize_to_panel(image: &RgbView<'_>) -> RgbImage {
    imageops::resize(image, WIDTH, HEIGHT, FilterType::Lanczos3)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Expected values come from `test_image_to_le565_known_pixels` in the
    /// reference's `tests/test_encoders.py`, plus white from
    /// `test_load_image_le565_resizes`.
    const REFERENCE_VECTORS: [([u8; 3], [u8; 2]); 5] = [
        ([255, 0, 0], [0x00, 0xF8]),
        ([0, 255, 0], [0xE0, 0x07]),
        ([0, 0, 255], [0x1F, 0x00]),
        ([7, 3, 7], [0x00, 0x00]),
        ([255, 255, 255], [0xFF, 0xFF]),
    ];

    fn solid_rgb(colour: [u8; 3], width: u32, height: u32) -> Vec<u8> {
        colour.repeat(width as usize * height as usize)
    }

    #[test]
    fn conversion_matches_the_reference_pixels() {
        for (colour, expected) in REFERENCE_VECTORS {
            let frame = rgb_to_le565(&solid_rgb(colour, WIDTH, HEIGHT)).unwrap();

            assert_eq!(frame.len(), 108_800);
            assert_eq!(frame, expected.repeat(54_400), "colour {colour:?}");
        }
    }

    #[test]
    fn conversion_follows_the_reference_bit_formula() {
        // Worked by hand from `image_to_le565`:
        // v = (r & 0xF8) << 8 | (g & 0xFC) << 3 | b >> 3, low byte first.
        assert_eq!(pixel_to_le565([0x12, 0x34, 0x56]), [0xAA, 0x11]);
        // One step above the truncation boundary of each channel.
        assert_eq!(pixel_to_le565([8, 4, 8]), [0x21, 0x08]);
    }

    #[test]
    fn conversion_is_row_major() {
        let mut rgb = vec![0; RGB_BYTES];
        rgb[3] = 255; // red at x=1, y=0
        rgb[WIDTH as usize * 3 + 2] = 255; // blue at x=0, y=1
        rgb[RGB_BYTES - 2] = 255; // green at the last pixel

        let frame = rgb_to_le565(&rgb).unwrap();

        assert_eq!(frame[2..4], [0x00, 0xF8]);
        assert_eq!(frame[640..642], [0x1F, 0x00]);
        assert_eq!(frame[108_798..], [0xE0, 0x07]);
        let lit = frame
            .as_chunks::<2>()
            .0
            .iter()
            .filter(|px| **px != [0, 0])
            .count();
        assert_eq!(lit, 3);
    }

    #[test]
    fn conversion_rejects_a_wrong_length() {
        for length in [0, RGB_BYTES - 1, RGB_BYTES + 1] {
            let result = rgb_to_le565(&vec![0; length]);

            assert!(matches!(result, Err(Error::InvalidArgument(_))));
        }
    }

    #[test]
    fn solid_frame_matches_the_reference_pixels() {
        for (colour, expected) in REFERENCE_VECTORS {
            assert_eq!(solid_frame(colour), expected.repeat(54_400));
        }
    }

    #[test]
    fn resize_gives_panel_dimensions_and_keeps_solid_colours_solid() {
        // Sizes on both sides of the panel, with ratios that are not whole
        // numbers, so the filter kernel is hit at uneven offsets.
        let sizes = [(1, 1), (97, 53), (321, 171), (641, 341), (1000, 3)];
        let colours = [
            [0, 0, 0],
            [255, 255, 255],
            [1, 2, 3],
            [139, 141, 139],
            [254, 0, 127],
        ];

        for (width, height) in sizes {
            for colour in colours {
                let rgb = solid_rgb(colour, width, height);
                let image = view_rgb(&rgb, width, height).unwrap();

                let resized = resize_to_panel(&image);

                assert_eq!(resized.dimensions(), (WIDTH, HEIGHT));
                assert!(
                    resized.pixels().all(|pixel| pixel.0 == colour),
                    "{colour:?} from {width}x{height}"
                );
            }
        }
    }

    #[test]
    fn frame_from_rgb_resizes_to_a_full_frame() {
        let frame = frame_from_rgb(&solid_rgb([255, 0, 0], 64, 64), 64, 64).unwrap();

        assert_eq!(frame, [0x00, 0xF8].repeat(54_400));
    }

    #[test]
    fn frame_from_rgb_does_not_resample_a_panel_sized_image() {
        // Hard edges every pixel, which a filter pass would disturb. Pins
        // `imageops::resize` copying at equal sizes, as Pillow's `resize` does.
        let rgb: Vec<u8> = (0..RGB_BYTES).map(|i| (i % 7 * 40) as u8).collect();

        let frame = frame_from_rgb(&rgb, WIDTH, HEIGHT).unwrap();

        assert_eq!(frame, rgb_to_le565(&rgb).unwrap());
    }

    #[test]
    fn frame_from_rgb_rejects_bad_buffers() {
        let cases: [(&[u8], u32, u32); 5] = [
            (&[], 0, 0),
            (&[], 0, 5),
            (&[0; 3], 2, 2),
            (&[0; 13], 2, 2),
            (&[0; 3], u32::MAX, u32::MAX),
        ];

        for (rgb, width, height) in cases {
            let result = frame_from_rgb(rgb, width, height);

            assert!(
                matches!(result, Err(Error::InvalidArgument(_))),
                "{width}x{height} from {} bytes",
                rgb.len()
            );
        }
    }
}
