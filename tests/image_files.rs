//! Loads real files through `load_file`. Nothing here touches a bus.

use std::fs;
use std::path::PathBuf;

use aorus_lcd::image::load_file;
use aorus_lcd::Error;
use image::{ImageFormat, Rgb, RgbImage, Rgba, RgbaImage};

fn temp_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name)
}

#[test]
fn every_enabled_format_loads_as_a_full_frame() {
    let white = RgbImage::from_pixel(64, 64, Rgb([255, 255, 255]));
    let formats = [
        (ImageFormat::Png, "white.png"),
        (ImageFormat::Jpeg, "white.jpg"),
        (ImageFormat::Gif, "white.gif"),
        (ImageFormat::WebP, "white.webp"),
        (ImageFormat::Bmp, "white.bmp"),
    ];

    for (format, name) in formats {
        let path = temp_path(name);
        white.save_with_format(&path, format).unwrap();

        let frame = load_file(&path).unwrap();

        // `test_load_image_le565_resizes` in the reference: 64x64 white.
        assert_eq!(frame, [0xFF, 0xFF].repeat(54_400), "{format:?}");
    }
}

#[test]
fn format_comes_from_the_content_not_the_name() {
    let path = temp_path("really-a-png.jpg");
    RgbImage::from_pixel(8, 8, Rgb([0, 0, 255]))
        .save_with_format(&path, ImageFormat::Png)
        .unwrap();

    let frame = load_file(&path).unwrap();

    assert_eq!(frame, [0x1F, 0x00].repeat(54_400));
}

#[test]
fn alpha_is_dropped_rather_than_blended() {
    // Pillow's `convert("RGB")` discards alpha, so a fully transparent pixel
    // keeps its colour. (10, 200, 30) is 0x0E43 by the `image_to_le565` formula.
    let path = temp_path("transparent.png");
    RgbaImage::from_pixel(320, 170, Rgba([10, 200, 30, 0]))
        .save_with_format(&path, ImageFormat::Png)
        .unwrap();

    let frame = load_file(&path).unwrap();

    assert_eq!(frame, [0x43, 0x0E].repeat(54_400));
}

#[test]
fn a_missing_file_is_an_input_error() {
    let path = temp_path("does-not-exist.png");

    let error = load_file(&path).unwrap_err();

    assert!(matches!(error, Error::Input { .. }), "{error:?}");
    assert!(error.to_string().contains("does-not-exist.png"));
}

#[test]
fn undecodable_content_is_an_input_error() {
    let path = temp_path("not-an-image.png");
    fs::write(&path, b"this is not an image").unwrap();

    let error = load_file(&path).unwrap_err();

    assert!(matches!(error, Error::Input { .. }), "{error:?}");
    assert!(error.to_string().contains("not-an-image.png"));
}

#[test]
fn a_truncated_file_is_an_input_error() {
    let path = temp_path("truncated.png");
    RgbImage::from_pixel(32, 32, Rgb([1, 2, 3]))
        .save_with_format(&path, ImageFormat::Png)
        .unwrap();
    let bytes = fs::read(&path).unwrap();
    fs::write(&path, &bytes[..bytes.len() / 2]).unwrap();

    let error = load_file(&path).unwrap_err();

    assert!(matches!(error, Error::Input { .. }), "{error:?}");
}
