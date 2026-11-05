//! Checks the vectors themselves, never our encoders: that every file loads,
//! that every reference test is accounted for, and that the transcribed values
//! agree with each other and with the arithmetic the reference states.
//!
//! The reference is not in this repository and is never executed. Whether the
//! `stated` lines match it was checked against the pinned commit when they were
//! transcribed.

mod common;

use std::collections::HashSet;

use common::{ByteExpect, Case, FrameListExpect};

/// `def test_*` functions in the reference `tests/test_encoders.py`.
const REFERENCE_TESTS: usize = 35;
/// Entries in the `checks` list of the reference `run_selftest()`.
const SELFTEST_CHECKS: usize = 9;

/// Reference calls whose whole `expect` is a set of byte checks.
const BYTE_CALLS: [&str; 15] = [
    "cmd_frame",
    "f2_frame",
    "make_f1_header",
    "open_lcd",
    "set_mode",
    "set_carousel",
    "power_off_mode",
    "read_cmd",
    "set_brightness",
    "rle_encode_frame",
    "gif_frame_table",
    "image_to_le565",
    "load_image_le565",
    "parse_hex_bytes",
    "render_text_le565",
];

/// Reference calls whose `expect` describes a list of frames.
const FRAME_LIST_CALLS: [&str; 3] = ["chunk_payload", "build_upload", "build_gif_frames"];

#[test]
fn every_case_has_provenance_and_a_unique_name() {
    let cases = common::load_all();
    assert!(!cases.is_empty());

    let mut names = HashSet::new();
    for case in &cases {
        assert!(
            names.insert(case.label()),
            "duplicate case {}",
            case.label()
        );
        assert!(!case.stated.is_empty(), "{} quotes nothing", case.label());
        assert!(!case.lines.is_empty(), "{} has no lines", case.label());
        assert!(
            case.source.starts_with("tests/test_encoders.py::test_")
                || case.source.starts_with("aorus_lcd.py::run_selftest::"),
            "{} has an unknown source {}",
            case.label(),
            case.source
        );
    }
}

#[test]
fn every_expectation_parses_and_checks_something() {
    let mut parsed = 0;
    for case in common::load_all() {
        if case.expects_error() {
            continue;
        }
        if FRAME_LIST_CALLS.contains(&case.call.as_str()) {
            let expect = case.frame_list_expect();
            assert!(
                expect != FrameListExpect::default(),
                "{} checks nothing",
                case.label()
            );
            parsed += 1;
        } else if BYTE_CALLS.contains(&case.call.as_str()) {
            assert!(
                !case.byte_expect().is_empty(),
                "{} checks nothing",
                case.label()
            );
            parsed += 1;
        }
    }
    assert!(parsed > 0);
}

#[test]
fn byte_inputs_all_resolve() {
    for case in common::load_all() {
        for key in ["tail", "payload", "px", "canned_reply"] {
            if case.input.get(key).is_some() {
                case.bytes(key);
            }
        }
    }
}

#[test]
fn checks_stay_inside_the_stated_length() {
    for case in common::load_all() {
        if !BYTE_CALLS.contains(&case.call.as_str()) || case.expects_error() {
            continue;
        }
        let expect = case.byte_expect();
        let Some(len) = expect.len else { continue };
        for (offset, bytes) in &expect.at {
            assert!(offset + bytes.len() <= len, "{} overflows", case.label());
        }
        if let Some(exact) = &expect.exact {
            assert_eq!(exact.len(), len, "{}", case.label());
        }
    }
}

/// Keeps the README's list honest: only these `at` cases may be read as byte
/// equality, because the reference pins every byte of them and of no others.
#[test]
fn only_the_documented_cases_pin_every_byte() {
    let mut complete: Vec<String> = common::load_all()
        .iter()
        .filter(|case| BYTE_CALLS.contains(&case.call.as_str()) && !case.expects_error())
        .filter(|case| pins_every_byte(&case.byte_expect()))
        .map(|case| case.name.clone())
        .collect();
    complete.sort();

    assert_eq!(
        complete,
        [
            "cmd_frame_e5_04",
            "f1_header_static_image",
            "f2_begin",
            "f2_end",
            "gif_frame_table_offsets",
        ]
    );
}

fn pins_every_byte(expect: &ByteExpect) -> bool {
    let Some(len) = expect.len else { return false };
    if expect.exact.is_some() {
        return false;
    }
    let end = expect.zero_from.unwrap_or(len);
    let mut pinned = vec![false; end];
    for (offset, bytes) in &expect.at {
        pinned[*offset..offset + bytes.len()].fill(true);
    }
    pinned.iter().all(|byte| *byte)
}

#[test]
fn coverage_accounts_for_every_reference_test() {
    let coverage = common::load_coverage();
    let tests = coverage
        .iter()
        .filter(|entry| entry.source.starts_with("tests/test_encoders.py::test_"));
    let checks = coverage
        .iter()
        .filter(|entry| entry.source.starts_with("aorus_lcd.py::run_selftest::"));

    assert_eq!(tests.count(), REFERENCE_TESTS);
    assert_eq!(checks.count(), SELFTEST_CHECKS);
    assert_eq!(coverage.len(), REFERENCE_TESTS + SELFTEST_CHECKS);

    let sources: HashSet<&str> = coverage.iter().map(|e| e.source.as_str()).collect();
    assert_eq!(sources.len(), coverage.len(), "duplicate coverage entry");
}

#[test]
fn coverage_status_matches_the_cases() {
    let cases = common::load_all();

    for entry in common::load_coverage() {
        let matching: Vec<&Case> = cases.iter().filter(|c| c.source == entry.source).collect();

        match entry.status.as_str() {
            "skipped" => {
                assert!(matching.is_empty(), "{} has cases", entry.source);
                let reason = entry.reason.as_deref().unwrap_or_default();
                assert!(!reason.is_empty(), "{} has no reason", entry.source);
            }
            "vectors" | "property" => {
                assert!(!matching.is_empty(), "{} has no cases", entry.source);
                let file = entry
                    .file
                    .as_deref()
                    .expect("transcribed entry names a file");
                assert!(
                    matching.iter().all(|case| case.file == file),
                    "{} is not all in {file}",
                    entry.source
                );
                assert_eq!(
                    file == common::PROPERTY_FILE,
                    entry.status == "property",
                    "{}",
                    entry.source
                );
            }
            other => panic!("{}: unknown status {other}", entry.source),
        }
    }

    let covered: HashSet<String> = common::load_coverage()
        .into_iter()
        .map(|entry| entry.source)
        .collect();
    for case in &cases {
        assert!(
            covered.contains(&case.source),
            "{} is unlisted",
            case.label()
        );
    }
}

#[test]
fn constants_agree_with_the_stated_arithmetic() {
    let width = common::constant_u64("W");
    let height = common::constant_u64("H");
    let frame_pixels = common::constant_u64("FRAME_PIXELS");
    let frame_bytes = common::constant_u64("FRAME_BYTES");
    let desc_len = common::constant_u64("DESC_LEN");

    assert_eq!(frame_pixels, width * height);
    assert_eq!(frame_bytes, frame_pixels * 2);
    assert_eq!(frame_bytes, 108800, "stated at aorus_lcd.py:33");
    assert_eq!(common::constant_bytes("DESC").len() as u64, desc_len);
    assert_eq!(common::constant_bytes("MAGIC").len(), 4);

    // The test states "12-byte descriptor + 108800 px bytes = 108812 -> 426 chunks".
    let f1 = common::case("frames.json", "f1_header_static_image");
    assert_eq!(f1.u64("usize"), desc_len + frame_bytes);
    assert_eq!(f1.u64("nchunks"), f1.u64("usize") / 256 + 1);
    assert_eq!(f1.u64("nchunks"), 426);
    assert_eq!(f1.u64("fb_addr"), common::constant_u64("FB_STATIC"));
    assert!(f1
        .byte_expect()
        .at
        .contains(&(10, 426u32.to_be_bytes().to_vec())));
}

/// Tracker correction 4 states this independently of the reference test.
#[test]
fn mode_7_is_wire_byte_0x0a() {
    let expect = common::case("commands.json", "set_mode_7_quirk").byte_expect();
    assert!(expect.at.contains(&(5, vec![0x0A])));
}

#[test]
fn magic_in_vectors_matches_the_constant() {
    let magic = common::constant_bytes("MAGIC");

    let mut seen = 0;
    for case in common::load_all() {
        if !BYTE_CALLS.contains(&case.call.as_str()) || case.expects_error() {
            continue;
        }
        for (offset, bytes) in &case.byte_expect().at {
            // Magic sits at [1..5], alone or inside a longer frame prefix. The
            // other offset 0 and 1 entries are too short to reach it.
            let in_range = match offset {
                0 if bytes.len() >= 5 => &bytes[1..5],
                1 => &bytes[..],
                _ => continue,
            };
            assert_eq!(in_range, magic, "{}", case.label());
            seen += 1;
        }
    }
    assert!(seen > 0);
}

#[test]
fn pixel_counts_match_the_frame_size() {
    let frame_pixels = common::constant_u64("FRAME_PIXELS");

    let property = common::case(common::PROPERTY_FILE, "rle_round_trip_lcg_frame");
    assert_eq!(
        common::lcg_frame(&property).len() as u64,
        frame_pixels * 2,
        "the reference loops FRAME_PIXELS times and appends two bytes"
    );

    let red = common::case("pixels.json", "le565_solid_red").byte_expect();
    assert_eq!(
        red.exact.expect("exact").len() as u64,
        common::constant_u64("FRAME_BYTES")
    );
}

/// Needs no encoder: each expected blob must decode to the case's own input,
/// which catches a slip in either side of the transcription.
#[test]
fn rle_vectors_decode_back_to_their_input() {
    let mut decoded = 0;
    for case in common::cases_for("rle.json", "rle_encode_frame") {
        if case.expects_error() {
            continue;
        }
        let blob = case.byte_expect().exact.expect("rle cases are exact");
        assert_eq!(
            common::rle_decode(&blob),
            case.bytes("px"),
            "{}",
            case.label()
        );
        decoded += 1;
    }
    assert_eq!(decoded, 7);
}

/// The test and the self-check state the table in two different forms.
#[test]
fn both_gif_table_transcriptions_agree() {
    let exact = common::case("rle.json", "selftest_gif_table")
        .byte_expect()
        .exact
        .expect("the selftest states whole bytes");

    common::case("rle.json", "gif_frame_table_offsets")
        .byte_expect()
        .check(&exact, "gif table");
}

/// The test's comment spells this out: `22+10-1 = 31, then 32+20-1 = 51`.
#[test]
fn gif_table_offsets_follow_the_stated_formula() {
    let case = common::case("rle.json", "gif_frame_table_offsets");
    let sizes = case.u64_list("sizes");
    let header = 2 + 10 * sizes.len() as u64;
    assert_eq!(header, 22);

    let first_end = header + sizes[0] - 1;
    let second_end = first_end + 1 + sizes[1] - 1;

    let expect = case.byte_expect();
    assert!(expect
        .at
        .contains(&(2, (first_end as u32).to_le_bytes().to_vec())));
    assert!(expect
        .at
        .contains(&(12, (second_end as u32).to_le_bytes().to_vec())));
}

#[test]
fn segments_expand_as_documented() {
    let bytes = |json: &str| common::parse_bytes(&serde_json::from_str(json).unwrap());

    assert_eq!(bytes(r#""e5 04""#), [0xE5, 0x04]);
    assert_eq!(bytes(r#"[{"u8": [200]}]"#), [200]);
    assert_eq!(bytes(r#"[{"u16le": ["0x1234"]}]"#), [0x34, 0x12]);
    assert_eq!(bytes(r#"[{"u16be": [426]}]"#), [0x01, 0xAA]);
    assert_eq!(bytes(r#"[{"u32le": [31]}]"#), [31, 0, 0, 0]);
    assert_eq!(bytes(r#"[{"u32be": ["0x01300000"]}]"#), [0x01, 0x30, 0, 0]);
    assert_eq!(bytes(r#"[{"byte_range": [1, 4]}]"#), [1, 2, 3]);
    assert_eq!(bytes(r#"[{"hex": "aa", "repeat": 3}]"#), [0xAA; 3]);
    assert_eq!(
        bytes(r#"[{"hex": "01"}, {"u16le": [2], "repeat": 2}]"#),
        [1, 2, 0, 2, 0]
    );
}

#[test]
#[should_panic(expected = "unknown segment key")]
fn unknown_segment_keys_are_rejected() {
    common::parse_bytes(&serde_json::json!([{ "hex": "00", "repeats": 2 }]));
}

#[test]
#[should_panic(expected = "does not fit")]
fn out_of_range_values_are_rejected() {
    common::parse_bytes(&serde_json::json!([{ "u8": [256] }]));
}

#[test]
#[should_panic(expected = "unknown byte expect key")]
fn unknown_expect_keys_are_rejected() {
    ByteExpect::parse(&serde_json::json!({ "zero_form": 6 }));
}

#[test]
fn byte_expect_reports_every_kind_of_mismatch() {
    let expect = ByteExpect {
        len: Some(4),
        at: vec![(0, vec![0xE5])],
        zero_from: Some(2),
        ..ByteExpect::default()
    };

    assert_eq!(expect.verify(&[0xE5, 9, 0, 0]), Ok(()));
    assert!(expect.verify(&[0xE5, 9, 0]).is_err(), "length");
    assert!(expect.verify(&[0xE6, 9, 0, 0]).is_err(), "at");
    assert!(expect.verify(&[0xE5, 9, 0, 1]).is_err(), "padding");

    let short = ByteExpect {
        at: vec![(1, vec![1, 2])],
        ..ByteExpect::default()
    };
    assert!(short.verify(&[0, 1]).is_err(), "at past the end");

    let exact = ByteExpect {
        exact: Some(vec![1, 2]),
        ..ByteExpect::default()
    };
    assert_eq!(exact.verify(&[1, 2]), Ok(()));
    assert!(exact.verify(&[1, 3]).is_err(), "exact");
    assert!(exact.verify(&[1, 2, 3]).is_err(), "exact length");

    let contains = ByteExpect {
        contains: Some(vec![0xFF, 0xFF]),
        ..ByteExpect::default()
    };
    assert_eq!(contains.verify(&[0, 0xFF, 0xFF, 0]), Ok(()));
    assert!(contains.verify(&[0xFF, 0, 0xFF]).is_err(), "contains");
}

#[test]
fn frame_list_expect_handles_negative_indexes_and_joins() {
    let last_is_two = ByteExpect {
        exact: Some(vec![2]),
        ..ByteExpect::default()
    };
    let expect = FrameListExpect {
        count: Some(2),
        each_len: Some(1),
        items: vec![(-1, last_is_two)],
        joined: Some(ByteExpect {
            exact: Some(vec![1, 2]),
            ..ByteExpect::default()
        }),
    };

    assert_eq!(expect.verify(&[vec![1u8], vec![2u8]]), Ok(()));
    assert!(expect.verify(&[vec![2u8], vec![1u8]]).is_err(), "order");
    assert!(expect.verify(&[vec![2u8]]).is_err(), "count");
}

#[test]
#[should_panic(expected = "zero-count token")]
fn the_decoder_rejects_zero_count_tokens() {
    common::rle_decode(&[0x00, 0x00]);
}

#[test]
#[should_panic(expected = "has no cases for")]
fn a_misspelled_call_is_an_error() {
    common::cases_for("frames.json", "cmd_frames");
}
