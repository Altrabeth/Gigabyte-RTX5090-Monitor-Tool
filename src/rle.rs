//! Pure RLE encoder and GIF payload builder. No I/O.
//!
//! The firmware decoder mirrors Gigabyte's `Compress_RLE`, so [`encode_frame`]
//! reproduces its quirks instead of compressing as well as it could. This is
//! `rle_encode_frame` and `gif_frame_table` of `aorus_lcd.py` at `b7f1fa8`, and
//! `RleEncoder.cs` agrees. The reference validated its output by re-encoding
//! every blob of GCC's `animation.bin`, so `i`, `j`, `wend`, `wlen`, `diff`,
//! `same` and `rs` keep their names here: this code is meant to be read side by
//! side with it. Please do not "improve" the scan.

use crate::{Error, Result};

/// Panel size, from the reference's `W` and `H`. Private until a later issue
/// folds these into `protocol`.
const WIDTH: u16 = 320;
const HEIGHT: u16 = 170;

/// `fmt` of a frame table entry, meaning RLE.
const FORMAT_RLE: u16 = 3;

/// A token's count has 15 bits, so a window holds at most this many pixels.
const MAX_WINDOW: usize = 0x7FFF;

/// Bit 15 of a token head marks a run.
const RUN_FLAG: usize = 0x8000;

/// `Compress_RLE` refuses anything shorter.
const MIN_FRAME_PIXELS: usize = 3;

/// A window under this many pixels is one literal, without being scanned.
const MIN_SCANNED_WINDOW: usize = 4;

/// Size of the frame count that starts a frame table.
const TABLE_HEADER_LEN: usize = 2;

/// Size of one frame table entry.
const TABLE_ENTRY_LEN: usize = 10;

/// RLE-encodes one frame of little-endian RGB565 pixels.
///
/// A token is a `u16 LE` head, whose bit 15 is the run flag and whose low 15
/// bits are a pixel count, followed by one pixel for a run or `count` pixels
/// for a literal. Like the reference, this takes any length from 3 pixels up,
/// not only a full panel.
///
/// # Errors
///
/// [`Error::InvalidArgument`] for an odd byte length, or under 3 pixels.
pub fn encode_frame(px: &[u8]) -> Result<Vec<u8>> {
    let (pixels, odd_byte) = px.as_chunks::<2>();
    if !odd_byte.is_empty() {
        return Err(Error::InvalidArgument(format!(
            "an RGB565 frame has an even byte length, got {}",
            px.len()
        )));
    }
    let n = pixels.len();
    if n < MIN_FRAME_PIXELS {
        return Err(Error::InvalidArgument(format!(
            "an RLE frame has at least {MIN_FRAME_PIXELS} pixels, got {n}"
        )));
    }

    // Only a capacity hint: an incompressible frame is its pixels plus one
    // head per window, so noise needs no reallocation.
    let mut out = Vec::with_capacity(px.len() + 2 * (n / MAX_WINDOW + 1));
    let mut i = 0;
    while i < n {
        // Windows are anchored at `i`, wherever the last token ended, so they
        // do not sit on multiples of 0x7FFF.
        let wend = i + MAX_WINDOW.min(n - i);
        let (diff, same) = scan_window(pixels, i, wend);
        if diff != 0 {
            push_head(&mut out, diff);
            out.extend_from_slice(&px[2 * i..2 * (i + diff)]);
        }
        if same != 0 {
            push_head(&mut out, same | RUN_FLAG);
            out.extend_from_slice(&pixels[i + diff]);
        }
        i += diff + same;
    }
    Ok(out)
}

/// Splits the window `[i, wend)` into `diff` literal pixels followed by a run
/// of `same` pixels. Either can be zero, not both.
fn scan_window(pixels: &[[u8; 2]], i: usize, wend: usize) -> (usize, usize) {
    let wlen = wend - i;
    if wlen < MIN_SCANNED_WINDOW {
        // A tail under 4 pixels is a literal even if all its pixels are equal.
        return (wlen, 0);
    }

    let mut j = i;
    loop {
        // Must come before the equality check. Here `j + 2` is outside the
        // window, so a repeat starting at `wend - 2` is never a run: the whole
        // window goes out as one literal. The last triple that can match is
        // the one at `wend - 3`.
        if j + 2 == wend {
            return (wlen, 0);
        }
        if pixels[j] == pixels[j + 1] && pixels[j + 1] == pixels[j + 2] {
            let rs = j;
            j += 2;
            // The run is maximal, but stops at the window end even if equal
            // pixels follow. They start the next window.
            while j < wend - 1 && pixels[j] == pixels[j + 1] {
                j += 1;
            }
            return (rs - i, j + 1 - rs);
        }
        j += 1;
    }
}

/// Writes a token head. A count is at most 0x7FFF, so with the run flag it
/// still fits in 16 bits.
fn push_head(out: &mut Vec<u8>, head: usize) {
    out.extend_from_slice(&(head as u16).to_le_bytes());
}

/// Builds the table that starts a GIF payload from the size of each RLE blob:
/// the frame count as `u16 LE`, then for each frame its inclusive end offset
/// in the payload as `u32 LE`, and `w`, `h`, `fmt = 3` as `u16 LE`. The end
/// offset of frame `i` is `2 + 10*N + sum(sizes[0..=i]) - 1`.
///
/// # Errors
///
/// [`Error::InvalidArgument`] if the frame count does not fit in `u16`, or an
/// end offset does not fit in `u32`. These would otherwise wrap and describe
/// a different payload than the one sent.
pub fn frame_table(sizes: &[usize]) -> Result<Vec<u8>> {
    let count = u16::try_from(sizes.len()).map_err(|_| {
        Error::InvalidArgument(format!(
            "a GIF payload has at most {} frames, got {}",
            u16::MAX,
            sizes.len()
        ))
    })?;

    let table_len = TABLE_HEADER_LEN + TABLE_ENTRY_LEN * sizes.len();
    let mut table = Vec::with_capacity(table_len);
    table.extend_from_slice(&count.to_le_bytes());

    let mut cum = table_len;
    for size in sizes {
        cum = cum.checked_add(*size).ok_or_else(offset_overflow)?;
        let end = u32::try_from(cum - 1).map_err(|_| offset_overflow())?;
        table.extend_from_slice(&end.to_le_bytes());
        for field in [WIDTH, HEIGHT, FORMAT_RLE] {
            table.extend_from_slice(&field.to_le_bytes());
        }
    }
    Ok(table)
}

fn offset_overflow() -> Error {
    Error::InvalidArgument("a GIF payload does not fit in u32 offsets".to_owned())
}

/// Builds a GIF payload from RLE blobs: the [`frame_table`], then the blobs.
/// There is no checksum.
///
/// # Errors
///
/// [`Error::InvalidArgument`] for no frames, or for the limits of
/// [`frame_table`]. The references have no zero-frame GIF either: both fail on
/// the average delay.
pub fn gif_payload<B: AsRef<[u8]>>(blobs: &[B]) -> Result<Vec<u8>> {
    if blobs.is_empty() {
        return Err(Error::InvalidArgument(
            "a GIF payload has at least one frame".to_owned(),
        ));
    }

    let sizes: Vec<usize> = blobs.iter().map(|blob| blob.as_ref().len()).collect();
    let mut payload = frame_table(&sizes)?;
    // `frame_table` proved that the sum fits.
    payload.reserve(sizes.iter().sum());
    for blob in blobs {
        payload.extend_from_slice(blob.as_ref());
    }
    Ok(payload)
}
