//! The RLE encoder and GIF payload against the reference.
//!
//! No vector covers the 0x7FFF window split and the reference's ground-truth
//! test needs a proprietary file, so the window quirks are proved here by hand.
//! Each expected value below was traced on paper through `rle_encode_frame` of
//! `aorus_lcd.py` at `b7f1fa8`, never taken from our own encoder. Every head is
//! spelled as literal bytes so that a wrong count cannot hide behind a helper.
//!
//! Notation of the traces: `w` is `wend`. A window is `[i, w)` with
//! `w = i + min(0x7FFF, n - i)`, and a window under 4 pixels is not scanned.
//! Otherwise `j` starts at `i`. At each `j`, first `j + 2 == w` ends the scan
//! with one literal of the whole window, then `px[j..j + 3]` all equal starts a
//! run at `rs = j`. The run sets `j = rs + 2` and grows while `j < w - 1` and
//! `px[j] == px[j + 1]`. It is `j + 1 - rs` pixels, after `rs - i` literal
//! pixels.

mod common;

use aorus_lcd::rle::{encode_frame, frame_table, gif_payload};
use aorus_lcd::Error;

const MAX_WINDOW: usize = 0x7FFF;
const FRAME_PIXELS: usize = 320 * 170;

fn le(words: &[u16]) -> Vec<u8> {
    words.iter().flat_map(|word| word.to_le_bytes()).collect()
}

fn join(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

/// `count` copies of `word`, as pixel bytes.
fn flat(word: u16, count: usize) -> Vec<u8> {
    le(&vec![word; count])
}

/// `1, 2, 1, 2, ...`, as pixel bytes. It never holds three equal pixels in a
/// row, so a window of it has no repeat.
fn alternating(count: usize) -> Vec<u8> {
    let words: Vec<u16> = (0..count).map(|index| 1 + (index % 2) as u16).collect();
    le(&words)
}

fn encoded(px: &[u8]) -> Vec<u8> {
    encode_frame(px).expect("a valid frame")
}

/// Compares without printing megabytes when a window test fails.
#[track_caller]
fn assert_blob(actual: &[u8], expected: &[u8]) {
    assert_eq!(actual.len(), expected.len(), "blob length");
    if let Some(at) = (0..actual.len()).find(|&at| actual[at] != expected[at]) {
        panic!(
            "first difference at byte {at}: {:02x} but expected {:02x}",
            actual[at], expected[at]
        );
    }
}

#[track_caller]
fn assert_invalid_argument<T: std::fmt::Debug>(result: aorus_lcd::Result<T>) {
    match result {
        Err(Error::InvalidArgument(_)) => {}
        other => panic!("expected InvalidArgument, got {other:?}"),
    }
}

// ---- Reference vectors -----------------------------------------------------

#[test]
fn encoder_matches_the_reference_vectors() {
    for case in common::cases_for("rle.json", "rle_encode_frame") {
        let result = encode_frame(&case.bytes("px"));
        if case.expects_error() {
            assert_invalid_argument(result);
        } else {
            case.byte_expect().check(&result.unwrap(), &case.label());
        }
    }
}

#[test]
fn frame_table_matches_the_reference_vectors() {
    for case in common::cases_for("rle.json", "gif_frame_table") {
        let sizes: Vec<usize> = case
            .u64_list("sizes")
            .into_iter()
            .map(|size| size as usize)
            .collect();
        case.byte_expect()
            .check(&frame_table(&sizes).unwrap(), &case.label());
    }
}

// ---- Rejected input --------------------------------------------------------

#[test]
fn a_frame_under_three_pixels_is_an_error() {
    for bytes in [0, 2, 4] {
        assert_invalid_argument(encode_frame(&vec![0; bytes]));
    }
}

#[test]
fn an_odd_byte_length_is_an_error() {
    for bytes in [1, 5, 7, 108_801] {
        assert_invalid_argument(encode_frame(&vec![0; bytes]));
    }
}

// ---- Short frames ----------------------------------------------------------

#[test]
fn three_pixels_are_the_minimum_and_always_a_literal() {
    // wlen 3 is under 4, so the window is not scanned.
    for words in [[7, 7, 7], [1, 2, 3], [1, 1, 2]] {
        let px = le(&words);
        assert_blob(&encoded(&px), &join(&[&[0x03, 0x00], &px]));
    }
}

#[test]
fn four_equal_pixels_are_the_shortest_run() {
    // n=4, w=4. j=0: 2 != 4, equal, rs=0, j=2; 2<3 and equal, j=3; 3<3 stops.
    // Run of 3+1-0 = 4.
    assert_blob(&encoded(&le(&[7, 7, 7, 7])), &[0x04, 0x80, 7, 0]);
}

#[test]
fn only_three_or_more_equal_pixels_are_a_repeat() {
    // Pairs never start a run. j=0..3 find no triple; j=4: 4+2 == 6, literal.
    let px = le(&[1, 1, 2, 2, 3, 3]);
    assert_blob(&encoded(&px), &join(&[&[0x06, 0x00], &px]));
}

#[test]
fn pixels_compare_whole_not_by_low_byte() {
    // All low bytes are 0x07. j=0, j=1: no triple. j=2: 2+2 == 4, literal.
    let px = le(&[0x0107, 0x0207, 0x0307, 0x0407]);
    assert_blob(&encoded(&px), &join(&[&[0x04, 0x00], &px]));
}

#[test]
fn literal_then_run_to_the_end_of_the_frame() {
    // n=5, w=5. j=0 (1,2,9), j=1 (2,9,9): no. j=2: 4 != 5, equal, rs=2, j=4;
    // 4<4 stops. diff=2, same=4+1-2=3.
    assert_blob(
        &encoded(&le(&[1, 2, 9, 9, 9])),
        &[0x02, 0x00, 1, 0, 2, 0, 0x03, 0x80, 9, 0],
    );
}

#[test]
fn run_then_literal_then_run() {
    // n=9. Window at 0: rs=0, j=2; 2<8 but 5 != 1, so same=3. Window at 3, w=9:
    // j=3 (1,2,6), j=4 (2,6,6): no. j=5: 7 != 9, equal, rs=5, j=7; 7<8 and
    // equal, j=8; 8<8 stops. diff=5-3=2, same=8+1-5=4.
    assert_blob(
        &encoded(&le(&[5, 5, 5, 1, 2, 6, 6, 6, 6])),
        &[
            0x03, 0x80, 5, 0, // run
            0x02, 0x00, 1, 0, 2, 0, // literal
            0x04, 0x80, 6, 0, // run
        ],
    );
}

#[test]
fn run_then_a_tail_under_four_pixels_is_a_literal_even_if_equal() {
    // n=7. Window at 0: rs=0, grows to j=3, 7 != 8 stops, same=4. Window at 4,
    // w=7: wlen 3 is under 4, so literal 3 although all three are equal.
    assert_blob(
        &encoded(&le(&[7, 7, 7, 7, 8, 8, 8])),
        &[0x04, 0x80, 7, 0, 0x03, 0x00, 8, 0, 8, 0, 8, 0],
    );

    // n=4. rs=0, j=2; 2<3 but 7 != 8, same=3. Window at 3: wlen 1.
    assert_blob(
        &encoded(&le(&[7, 7, 7, 8])),
        &[0x03, 0x80, 7, 0, 0x01, 0x00, 8, 0],
    );
}

#[test]
fn a_tail_of_exactly_four_pixels_can_run() {
    // The other side of the case above. n=7. Window at 0: rs=0, j=2, 7 != 8,
    // same=3. Window at 3, w=7: wlen 4, j=3: 5 != 7, equal, rs=3, j=5; 5<6 and
    // equal, j=6; 6<6 stops. same=6+1-3=4.
    assert_blob(
        &encoded(&le(&[7, 7, 7, 8, 8, 8, 8])),
        &[0x03, 0x80, 7, 0, 0x04, 0x80, 8, 0],
    );
}

#[test]
fn a_triple_at_the_last_position_of_a_window_is_found() {
    // n=7, w=7. j=4 is the last position tested: 6 != 7, equal, rs=4, j=6;
    // 6<6 stops. diff=4, same=6+1-4=3.
    assert_blob(
        &encoded(&le(&[1, 2, 3, 4, 9, 9, 9])),
        &[
            0x04, 0x00, 1, 0, 2, 0, 3, 0, 4, 0, // literal
            0x03, 0x80, 9, 0, // run
        ],
    );

    // Shortest form. n=4, w=4. j=0 (1,9,9): no. j=1: 3 != 4, equal, rs=1, j=3;
    // 3<3 stops. diff=1, same=3.
    assert_blob(
        &encoded(&le(&[1, 9, 9, 9])),
        &[0x01, 0x00, 1, 0, 0x03, 0x80, 9, 0],
    );
}

#[test]
fn a_pair_at_the_end_of_the_frame_ends_the_scan() {
    // n=5, w=5. j=0..2 find no triple. j=3: 3+2 == 5 ends the scan before
    // px[5], which does not exist, is compared.
    let px = le(&[1, 2, 3, 9, 9]);
    assert_blob(&encoded(&px), &join(&[&[0x05, 0x00], &px]));
}

// ---- Window boundaries, all pixels equal -----------------------------------

#[test]
fn equal_pixels_fill_one_window_with_one_run() {
    // n=0x7FFF, w=0x7FFF. j=0: equal, rs=0, grows to j=0x7FFE; 0x7FFE < 0x7FFE
    // stops. same=0x7FFF. Head 0x7FFF | 0x8000 = 0xFFFF.
    assert_blob(&encoded(&flat(0x1234, 0x7FFF)), &[0xFF, 0xFF, 0x34, 0x12]);

    // One pixel less: same=0x7FFE, head 0xFFFE.
    assert_blob(&encoded(&flat(0x1234, 0x7FFE)), &[0xFE, 0xFF, 0x34, 0x12]);
}

#[test]
fn equal_pixels_past_a_window_end_leave_a_literal_tail() {
    // Each n starts with the run of the test above, which stops at the window
    // end, and i moves to 0x7FFF. What is left is under 4 pixels, so it is one
    // literal of wlen = n - 0x7FFF although every pixel is equal.
    let run: &[u8] = &[0xFF, 0xFF, 0x34, 0x12];

    // n=0x8000: wlen 1.
    let tail = [0x01, 0x00, 0x34, 0x12];
    assert_blob(&encoded(&flat(0x1234, 0x8000)), &join(&[run, &tail]));

    // n=0x8001: wlen 2.
    let tail = [0x02, 0x00, 0x34, 0x12, 0x34, 0x12];
    assert_blob(&encoded(&flat(0x1234, 0x8001)), &join(&[run, &tail]));

    // n=0x8002: wlen 3.
    let tail = [0x03, 0x00, 0x34, 0x12, 0x34, 0x12, 0x34, 0x12];
    assert_blob(&encoded(&flat(0x1234, 0x8002)), &join(&[run, &tail]));
}

#[test]
fn equal_pixels_with_four_past_a_window_end_run_again() {
    // n=0x8003: as above, but wlen 4 is scanned and is a run of 4.
    assert_blob(
        &encoded(&flat(0x1234, 0x8003)),
        &[0xFF, 0xFF, 0x34, 0x12, 0x04, 0x80, 0x34, 0x12],
    );
}

#[test]
fn equal_pixels_filling_two_windows_are_two_full_runs() {
    assert_blob(
        &encoded(&flat(0x1234, 2 * MAX_WINDOW)),
        &[0xFF, 0xFF, 0x34, 0x12, 0xFF, 0xFF, 0x34, 0x12],
    );
}

// ---- Window boundaries, no repeat ------------------------------------------

#[test]
fn a_window_without_a_repeat_is_one_literal() {
    // n=0x7FFF: no triple, so j reaches 0x7FFD where j + 2 == w. Head 0x7FFF.
    let px = alternating(0x7FFF);
    assert_blob(&encoded(&px), &join(&[&[0xFF, 0x7F], &px]));
}

#[test]
fn a_literal_stops_at_the_window_end() {
    // The first window is the case above. The tail is its own literal: wlen 1
    // and 3 are not scanned, and wlen 4 has no triple. Heads 1, 3 and 4.
    for (n, head) in [
        (0x8000, [0x01, 0x00]),
        (0x8002, [0x03, 0x00]),
        (0x8003, [0x04, 0x00]),
    ] {
        let px = alternating(n);
        let (window, tail) = px.split_at(2 * MAX_WINDOW);
        assert_blob(&encoded(&px), &join(&[&[0xFF, 0x7F], window, &head, tail]));
    }
}

// ---- A repeat at a window end ----------------------------------------------

#[test]
fn a_run_stops_at_the_window_end_and_the_rest_starts_a_new_window() {
    // px = 1, 2, then 9 for 0x8007 pixels, n=0x8009.
    // Window at 0, w=0x7FFF: j=0 (1,2,9), j=1 (2,9,9): no. j=2: equal, rs=2,
    // grows while j < 0x7FFE, stops at j=0x7FFE although px[0x7FFF] is also 9.
    // diff=2, same=0x7FFE+1-2=0x7FFD, head 0xFFFD.
    // Window at 0x7FFF, w=0x8009: wlen 10, all 9, a run of 10.
    let px = join(&[&le(&[1, 2]), &flat(9, 0x8007)]);
    assert_blob(
        &encoded(&px),
        &[
            0x02, 0x00, 1, 0, 2, 0, // literal
            0xFD, 0xFF, 9, 0, // run clipped by the window
            0x0A, 0x80, 9, 0, // the same pixels in a new token
        ],
    );
}

#[test]
fn a_run_clipped_to_three_pixels_at_the_window_end() {
    // alternating(0x7FFC), then 9 for 5 pixels, n=0x8001.
    // Window at 0, w=0x7FFF: no triple up to j=0x7FFB. j=0x7FFC: j + 2 = 0x7FFE
    // != w, equal, rs=0x7FFC, j=0x7FFE; 0x7FFE < 0x7FFE stops, although
    // px[0x7FFF] is 9. diff=0x7FFC, same=3.
    // Window at 0x7FFF, w=0x8001: wlen 2, a literal.
    let lead = alternating(0x7FFC);
    let px = join(&[&lead, &flat(9, 5)]);
    let expected = join(&[
        &[0xFC, 0x7F],
        &lead,
        &[0x03, 0x80, 9, 0],
        &[0x02, 0x00, 9, 0, 9, 0],
    ]);
    assert_blob(&encoded(&px), &expected);
}

#[test]
fn a_repeat_reached_only_at_the_window_end_is_not_a_run() {
    // This is why `j + 2 == wend` must be tested before the equality.
    // alternating(0x7FFD), then 9, 9, 9, 3, 4, 5, n=0x8003. Pixels 0x7FFD to
    // 0x7FFF are equal, but 0x7FFF is outside the first window.
    // Window at 0, w=0x7FFF: pixel 0x7FFC is 1, so no triple up to j=0x7FFC.
    // j=0x7FFD: j + 2 == w, the whole window is one literal. Comparing first
    // would see 9, 9, 9 here and start a run across the window end.
    // Window at 0x7FFF, w=0x8003: wlen 4. j=0x7FFF (9,3,4), j=0x8000 (3,4,5):
    // no. j=0x8001: j + 2 == w, literal 4.
    let lead = alternating(0x7FFD);
    let px = join(&[&lead, &le(&[9, 9, 9, 3, 4, 5])]);
    let expected = join(&[
        &[0xFF, 0x7F],
        &lead,
        &[9, 0, 9, 0],
        &[0x04, 0x00, 9, 0, 3, 0, 4, 0, 5, 0],
    ]);
    assert_blob(&encoded(&px), &expected);
}

#[test]
fn a_repeat_split_by_the_window_end_runs_only_in_the_next_window() {
    // alternating(0x7FFD), then 9 for 8 pixels, n=0x8005.
    // Window at 0: the same trace as above, one literal ending in 9, 9.
    // Window at 0x7FFF, w=0x8005: the six remaining 9s are a run.
    let lead = alternating(0x7FFD);
    let px = join(&[&lead, &flat(9, 8)]);
    let expected = join(&[&[0xFF, 0x7F], &lead, &[9, 0, 9, 0], &[0x06, 0x80, 9, 0]]);
    assert_blob(&encoded(&px), &expected);
}

#[test]
fn windows_start_where_the_last_token_ended() {
    // Windows are not on multiples of 0x7FFF. px = 9, 9, 9, 9, then
    // alternating(0x8001), n=0x8005.
    // Window at 0, w=0x7FFF: rs=0, stops at j=3 since 9 != 1, same=4.
    // Window at 4, w=0x8003, one full window: no triple, a literal of 0x7FFF.
    // Window at 0x8003, w=0x8005: wlen 2, a literal.
    let rest = alternating(0x8001);
    let (literal, tail) = rest.split_at(2 * MAX_WINDOW);
    let px = join(&[&flat(9, 4), &rest]);
    let expected = join(&[
        &[0x04, 0x80, 9, 0],
        &[0xFF, 0x7F],
        literal,
        &[0x02, 0x00],
        tail,
    ]);
    assert_blob(&encoded(&px), &expected);
}

// ---- Full-size frames ------------------------------------------------------

/// 54,400 pixels that never repeat.
fn distinct_frame() -> Vec<u8> {
    let words: Vec<u16> = (0..FRAME_PIXELS).map(|index| index as u16).collect();
    le(&words)
}

#[test]
fn a_flat_full_frame_is_two_runs() {
    // 54,400 - 32,767 = 21,633 = 0x5481, head 0xD481.
    let px = flat(0xBEEF, FRAME_PIXELS);
    let blob = encoded(&px);
    assert_blob(&blob, &[0xFF, 0xFF, 0xEF, 0xBE, 0x81, 0xD4, 0xEF, 0xBE]);
    assert!(common::rle_decode(&blob) == px, "round trip");
}

#[test]
fn a_full_frame_without_repeats_is_two_literals() {
    // The same split as above, without the run flag.
    let px = distinct_frame();
    let (first, second) = px.split_at(2 * MAX_WINDOW);
    let blob = encoded(&px);
    assert_blob(&blob, &join(&[&[0xFF, 0x7F], first, &[0x81, 0x54], second]));
    assert!(common::rle_decode(&blob) == px, "round trip");
}

#[test]
fn a_noise_frame_round_trips() {
    let case = common::case(common::PROPERTY_FILE, "rle_round_trip_lcg_frame");
    let px = common::lcg_frame(&case);
    assert_eq!(px.len(), 2 * FRAME_PIXELS);
    assert!(common::rle_decode(&encoded(&px)) == px, "round trip");
}

// ---- Property --------------------------------------------------------------

/// A small deterministic generator, so that a failure reproduces.
struct Lcg(u64);

impl Lcg {
    fn next_below(&mut self, bound: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        ((self.0 >> 33) as usize) % bound
    }
}

/// Stretches of equal pixels of random length, some longer than a window, with
/// few colours so that neighbouring stretches often match by chance.
fn random_frame(random: &mut Lcg, pixels: usize) -> Vec<u8> {
    let mut words = Vec::with_capacity(pixels);
    while words.len() < pixels {
        let colour = random.next_below(4) as u16;
        let length = match random.next_below(100) {
            0 => MAX_WINDOW + random.next_below(8),
            1..=9 => 1 + random.next_below(2000),
            _ => 1 + random.next_below(6),
        };
        let length = length.min(pixels - words.len());
        words.resize(words.len() + length, colour);
    }
    le(&words)
}

/// What the grammar requires of every blob, whatever the input was.
fn assert_tokens_are_legal(blob: &[u8]) {
    let mut pos = 0;
    while pos < blob.len() {
        let head = u16::from_le_bytes([blob[pos], blob[pos + 1]]);
        let count = usize::from(head & 0x7FFF);
        assert_ne!(count, 0, "zero count at {pos}");
        if head & 0x8000 != 0 {
            assert!(count >= 3, "run of {count} at {pos}");
            pos += 4;
        } else {
            pos += 2 + 2 * count;
        }
    }
    assert_eq!(pos, blob.len(), "token overruns the blob");
}

#[test]
fn random_frames_round_trip() {
    let mut random = Lcg(27);
    let around_a_window = 0x7FF8..0x8010;
    let sizes = (3..64)
        .chain(around_a_window.clone())
        .chain(around_a_window.map(|pixels| pixels + MAX_WINDOW))
        .chain([FRAME_PIXELS]);
    for pixels in sizes {
        for _ in 0..8 {
            let px = random_frame(&mut random, pixels);
            let blob = encoded(&px);
            assert_tokens_are_legal(&blob);
            assert!(common::rle_decode(&blob) == px, "{pixels} pixels");
        }
    }
}

// ---- Frame table and GIF payload -------------------------------------------

#[test]
fn an_empty_size_list_is_a_bare_count() {
    // `gif_frame_table([])` is `n.to_bytes(2, "little")` with no entries.
    assert_eq!(frame_table(&[]).unwrap(), [0x00, 0x00]);
}

#[test]
fn frame_table_takes_the_largest_frame_count() {
    // N=65,535, so the table is 2 + 10*65,535 = 655,352 bytes. With sizes of 1,
    // the first end offset is 655,352 and the last 655,352 + 65,534 = 720,886.
    let table = frame_table(&vec![1; 65_535]).unwrap();
    assert_eq!(table.len(), 655_352);
    assert_eq!(table[..2], [0xFF, 0xFF]);
    assert_eq!(table[2..6], 655_352u32.to_le_bytes());
    assert_eq!(table[655_342..655_346], 720_886u32.to_le_bytes());
}

#[test]
fn frame_table_rejects_too_many_frames() {
    assert_invalid_argument(frame_table(&vec![1; 65_536]));
}

#[test]
fn frame_table_takes_the_largest_end_offset() {
    // N=1: the end offset is 12 + size - 1 = size + 11.
    let table = frame_table(&[u32::MAX as usize - 11]).unwrap();
    assert_eq!(table[2..6], [0xFF, 0xFF, 0xFF, 0xFF]);
}

#[test]
fn frame_table_rejects_an_end_offset_past_u32() {
    assert_invalid_argument(frame_table(&[u32::MAX as usize - 10]));
    assert_invalid_argument(frame_table(&[usize::MAX]));
    assert_invalid_argument(frame_table(&[usize::MAX, usize::MAX]));
}

#[test]
fn payload_is_the_table_then_the_blobs() {
    // N=2, table 22 bytes. End offsets 22+3-1 = 24 and 25+2-1 = 26. Each entry
    // ends in 320, 170, 3.
    let blobs = [vec![0xA1, 0xA2, 0xA3], vec![0xB1, 0xB2]];
    let expected = [
        0x02, 0x00, // count
        24, 0, 0, 0, 0x40, 0x01, 0xAA, 0x00, 0x03, 0x00, // entry 0
        26, 0, 0, 0, 0x40, 0x01, 0xAA, 0x00, 0x03, 0x00, // entry 1
        0xA1, 0xA2, 0xA3, 0xB1, 0xB2, // blobs
    ];
    assert_eq!(gif_payload(&blobs).unwrap(), expected);
}

#[test]
fn payload_of_full_frames_has_their_end_offsets() {
    // After test_gif_payload_self_consistent. A flat blob is 8 bytes and a
    // repeat-free blob is 2 + 65,534 + 2 + 43,266 = 108,804. Table 22 bytes, so
    // the end offsets are 22+8-1 = 29 and 30+108,804-1 = 108,833.
    let blobs = [
        encoded(&flat(0xBEEF, FRAME_PIXELS)),
        encoded(&distinct_frame()),
    ];
    let payload = gif_payload(&blobs).unwrap();

    assert_eq!(payload.len(), 108_834);
    assert_eq!(payload[2..6], 29u32.to_le_bytes());
    assert_eq!(payload[12..16], 108_833u32.to_le_bytes());
    assert_eq!(payload[22..30], blobs[0][..]);
    assert_blob(&payload[30..], &blobs[1]);
}

#[test]
fn payload_rejects_no_frames() {
    assert_invalid_argument(gif_payload::<Vec<u8>>(&[]));
}

#[test]
fn payload_rejects_too_many_frames() {
    assert_invalid_argument(gif_payload(&vec![Vec::new(); 65_536]));
}

#[test]
fn payload_rejects_an_end_offset_past_u32_before_copying() {
    // Sixteen references to one zeroed 256 MiB buffer add up to 4 GiB. The
    // pages stay untouched as long as the size check comes before the copy.
    let blob = vec![0u8; 1 << 28];
    assert_invalid_argument(gif_payload(&[blob.as_slice(); 16]));
}
