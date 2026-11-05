//! Byte-level tests of `protocol`.
//!
//! Vector tests assert what the reference states, and most of those cases are
//! partial, so each one also asserts its own zero padding. Everything else
//! writes the expected bytes by hand from the layout tables in
//! `docs/PROTOCOL.md`. No expected value here came from running the builders.

mod common;

use aorus_lcd::error::Error;
use aorus_lcd::protocol::*;

/// The frame `head` describes, zero-padded by hand to the full length.
fn padded(head: &[u8]) -> Vec<u8> {
    let mut frame = head.to_vec();
    frame.resize(256, 0);
    frame
}

fn assert_zero_from(frame: &[u8], from: usize, label: &str) {
    assert_eq!(frame.len(), 256, "{label}: length");
    if let Some(position) = frame[from..].iter().position(|byte| *byte != 0) {
        panic!("{label}: byte {} is not padding", from + position);
    }
}

fn mode(number: u64) -> Mode {
    Mode::try_from(u8::try_from(number).unwrap()).unwrap()
}

fn data(frames: &[UploadFrame]) -> Vec<Frame> {
    frames.iter().map(|frame| frame.data).collect()
}

/// The `UploadParams` of a reference call, with the `make_f1_header` defaults
/// `flag=1, mode=None` for omitted keys.
fn params_of(case: &common::Case) -> UploadParams {
    let framebuffer = match case.u64("fb_addr") {
        0x0130_0000 => Framebuffer::Static,
        0x0132_0000 => Framebuffer::Text,
        0 => Framebuffer::Gif,
        other => panic!("{}: unknown framebuffer {other:#x}", case.label()),
    };
    let flag = match case.opt_u64("flag") {
        None | Some(1) => ContentFlag::Static,
        Some(2) => ContentFlag::Gif,
        Some(other) => panic!("{}: unknown flag {other}", case.label()),
    };
    let transfer_mode = match case.opt_u64("mode") {
        None => None,
        Some(1) => Some(TransferMode::Small),
        Some(2) => Some(TransferMode::Large),
        Some(other) => panic!("{}: unknown mode {other}", case.label()),
    };
    UploadParams {
        framebuffer,
        flag,
        frame_count: case.opt_u64("nframes").unwrap_or(0) as u16,
        delay_ms: case.opt_u64("delay").unwrap_or(0) as u32,
        mode: transfer_mode,
    }
}

// Reference vectors.

#[test]
fn constants_match_the_reference() {
    assert_eq!(WIDTH as u64, common::constant_u64("W"));
    assert_eq!(HEIGHT as u64, common::constant_u64("H"));
    assert_eq!(FRAME_PIXELS as u64, common::constant_u64("FRAME_PIXELS"));
    assert_eq!(FRAME_BYTES as u64, common::constant_u64("FRAME_BYTES"));
    assert_eq!(DESCRIPTOR.to_vec(), common::constant_bytes("DESC"));
    assert_eq!(DESCRIPTOR.len() as u64, common::constant_u64("DESC_LEN"));
    assert_eq!(MAGIC.to_vec(), common::constant_bytes("MAGIC"));
    assert_eq!(
        u64::from(Framebuffer::Static.address()),
        common::constant_u64("FB_STATIC")
    );
    assert_eq!(
        u64::from(Framebuffer::Gif.address()),
        common::constant_u64("FB_GIF")
    );
    assert_eq!(
        u64::from(Mode::Image.number()),
        common::constant_u64("MODE_STATIC")
    );
    assert_eq!(
        u64::from(Mode::Gif.number()),
        common::constant_u64("MODE_GIF")
    );
}

#[test]
fn cmd_frame_vectors_match_through_the_mode_builder() {
    // The primitive is private, so the reference's cmd_frame(0xE5, b"\x04") is
    // reached through the builder that emits that frame.
    for case in common::cases_for("frames.json", "cmd_frame") {
        assert_eq!(case.u64("opcode"), 0xE5, "{}", case.label());
        assert_eq!(case.bytes("tail"), [0x04], "{}", case.label());
        let frame = mode_frame(Mode::Image);
        case.byte_expect().check(&frame, &case.label());
        assert_zero_from(&frame, 6, &case.label());
    }
}

#[test]
fn f2_vectors_match() {
    for case in common::cases_for("frames.json", "f2_frame") {
        let frame = match case.u64("flag") {
            1 => marker_frame(Marker::Begin),
            2 => marker_frame(Marker::End),
            other => panic!("{}: unknown flag {other}", case.label()),
        };
        case.byte_expect().check(&frame, &case.label());
        assert_zero_from(&frame, 6, &case.label());
    }
}

#[test]
fn f1_vectors_match() {
    for case in common::cases_for("frames.json", "make_f1_header") {
        let frame = upload_header(
            &params_of(&case),
            case.u64("nchunks") as u32,
            case.u64("usize") as usize,
        );
        case.byte_expect().check(&frame, &case.label());
        assert_zero_from(&frame, 18, &case.label());
    }
}

#[test]
fn chunk_vectors_match() {
    for case in common::cases_for("frames.json", "chunk_payload") {
        let chunks = chunk_payload(&case.bytes("payload"));
        case.frame_list_expect().check(&chunks, &case.label());
    }
}

#[test]
fn build_upload_vectors_match() {
    for case in common::cases_for("frames.json", "build_upload") {
        let frames = build_upload(&case.bytes("payload"), &params_of(&case)).unwrap();
        case.frame_list_expect()
            .check(&data(&frames), &case.label());
    }
}

#[test]
fn e7_vectors_match() {
    for case in common::cases_for("commands.json", "open_lcd") {
        let frame = power_frame(case.bool("on"));
        case.byte_expect().check(&frame, &case.label());
        assert_zero_from(&frame, 6, &case.label());
    }
}

#[test]
fn e5_vectors_match() {
    for case in common::cases_for("commands.json", "set_mode") {
        let frame = mode_frame(mode(case.u64("mode")));
        case.byte_expect().check(&frame, &case.label());
        assert_zero_from(&frame, 6, &case.label());
    }
}

#[test]
fn f3_vectors_match() {
    for case in common::cases_for("commands.json", "set_carousel") {
        let modes: Vec<Mode> = case.u64_list("modes").into_iter().map(mode).collect();
        let frame = carousel_frame(case.u64("arg") as u8, &modes).unwrap();
        case.byte_expect().check(&frame, &case.label());
        assert_zero_from(&frame, 6 + modes.len(), &case.label());
    }
}

#[test]
fn eb_03_vector_matches() {
    for case in common::cases_for("commands.json", "read_cmd") {
        assert_eq!(case.u64("opcode"), 0xEB, "{}", case.label());
        let frame = probe_frame();
        case.byte_expect().check(&frame, &case.label());
        assert_zero_from(&frame, 6, &case.label());
        // `reads` and the canned reply belong to the panel layer's read path.
        assert_eq!(PROBE_READ_LEN, 8);
    }
}

#[test]
fn e1_vector_matches_as_mask_and_interval() {
    // The reference calls this set_brightness. The bytes are plain E1, so this
    // is mask 0x05 with interval 200: PROTOCOL.md section 3.
    for case in common::cases_for("commands.json", "set_brightness") {
        let frame = display_frame(
            Elements::from_bits(case.u64("mask") as u8),
            case.u64("value") as u8,
        );
        case.byte_expect().check(&frame, &case.label());
        assert_zero_from(&frame, 14, &case.label());
    }
}

#[test]
fn every_command_vector_is_exercised_or_out_of_scope() {
    const COVERED: [&str; 5] = [
        "open_lcd",
        "set_mode",
        "set_carousel",
        "read_cmd",
        "set_brightness",
    ];
    for case in common::load("commands.json") {
        if COVERED.contains(&case.call.as_str()) {
            continue;
        }
        assert_eq!(case.call, "power_off_mode", "{}", case.label());
        assert!(case.out_of_scope.is_some(), "{}", case.label());
    }
}

// E7 and E5, PROTOCOL.md section 2. Mode 7 maps to internal 9.

#[test]
fn power_frames_have_the_exact_layout() {
    assert_eq!(
        power_frame(true).to_vec(),
        padded(&[0xE7, 0xCB, 0x55, 0xAC, 0x38, 0x01])
    );
    assert_eq!(
        power_frame(false).to_vec(),
        padded(&[0xE7, 0xCB, 0x55, 0xAC, 0x38, 0x02])
    );
}

#[test]
fn every_mode_has_its_exact_wire_byte() {
    let expected: [(Mode, u8); 9] = [
        (Mode::Faith1, 0x01),
        (Mode::Faith2, 0x02),
        (Mode::Faith3, 0x03),
        (Mode::Image, 0x04),
        (Mode::Text, 0x05),
        (Mode::Gif, 0x06),
        (Mode::ChibiClock, 0x07),
        (Mode::Carousel, 0x0A),
        (Mode::PresetRotation, 0x0B),
    ];
    for (mode, byte) in expected {
        assert_eq!(
            mode_frame(mode).to_vec(),
            padded(&[0xE5, 0xCB, 0x55, 0xAC, 0x38, byte]),
            "{mode:?}"
        );
    }
}

#[test]
fn e5_preset_rotation_is_0x0b_and_not_number_plus_one() {
    let mut expected = vec![0u8; 256];
    expected[..6].copy_from_slice(&[0xE5, 0xCB, 0x55, 0xAC, 0x38, 0x0B]);

    assert_eq!(mode_frame(Mode::PresetRotation).to_vec(), expected);
    assert_eq!(Mode::try_from(8).unwrap(), Mode::PresetRotation);
}

#[test]
fn only_modes_up_to_8_exist() {
    for number in 0..=8u8 {
        assert_eq!(Mode::try_from(number).unwrap().number(), number);
    }
    for number in 9..=255u8 {
        assert!(
            matches!(Mode::try_from(number), Err(Error::InvalidArgument(_))),
            "{number}"
        );
    }
}

// E1, PROTOCOL.md section 3.

#[test]
fn e1_has_one_flag_per_element_in_table_order() {
    let table: [(Element, usize, u8); 8] = [
        (Element::GpuTemp, 5, 0x01),
        (Element::GpuClock, 6, 0x02),
        (Element::GpuUsage, 7, 0x04),
        (Element::FanSpeed, 8, 0x08),
        (Element::RamClock, 9, 0x10),
        (Element::RamUsage, 10, 0x20),
        (Element::Fps, 11, 0x40),
        (Element::Tgp, 12, 0x80),
    ];
    for (element, offset, bit) in table {
        assert_eq!(element.bit(), bit);
        let mut expected = padded(&[0xE1, 0xCB, 0x55, 0xAC, 0x38]);
        expected[offset] = 1;
        expected[13] = 9;
        let frame = display_frame(Elements::NONE.with(element), 9);
        assert_eq!(frame.to_vec(), expected, "{element:?}");
    }
}

#[test]
fn e1_all_and_none() {
    assert_eq!(
        display_frame(Elements::ALL, 3).to_vec(),
        padded(&[0xE1, 0xCB, 0x55, 0xAC, 0x38, 1, 1, 1, 1, 1, 1, 1, 1, 3])
    );
    assert_eq!(
        display_frame(Elements::NONE, 0).to_vec(),
        padded(&[0xE1, 0xCB, 0x55, 0xAC, 0x38])
    );
}

#[test]
fn widget_names_map_to_their_elements() {
    let table = [
        ("temp", Element::GpuTemp),
        ("clock", Element::GpuClock),
        ("usage", Element::GpuUsage),
        ("fan", Element::FanSpeed),
        ("ramclock", Element::RamClock),
        ("vram", Element::RamUsage),
        ("fps", Element::Fps),
        ("power", Element::Tgp),
    ];

    for (name, element) in table {
        assert_eq!(Element::from_name(name).unwrap(), element);
        assert_eq!(element.name(), name);
    }
}

#[test]
fn an_unknown_widget_is_named_along_with_the_valid_ones() {
    let message = parse_widgets("temp,gpu").unwrap_err().to_string();

    assert_eq!(
        message,
        "unknown widget \"gpu\": valid widgets are \
         temp, clock, usage, fan, ramclock, vram, fps, power"
    );
}

#[test]
fn a_widget_list_is_refused_when_empty_or_repeating() {
    let empty = parse_widgets("").unwrap_err().to_string();
    let twice = parse_widgets("temp,fan,temp").unwrap_err().to_string();

    assert_eq!(empty, "the widget list is empty");
    assert_eq!(twice, "widget \"temp\" is listed twice");
    assert!(parse_widgets("temp,").is_err());
    assert!(parse_widgets(",").is_err());
}

#[test]
fn the_default_widgets_are_the_readme_list() {
    // The bits of temp, clock, usage, fan, vram and power.
    assert_eq!(parse_widgets(DEFAULT_WIDGETS).unwrap().bits(), 0xAF);
}

#[test]
fn element_masks_compose() {
    let set: Elements = [Element::GpuTemp, Element::RamUsage, Element::Tgp]
        .into_iter()
        .collect();
    assert_eq!(set.bits(), 0xA1);
    assert!(set.contains(Element::RamUsage));
    assert!(!set.contains(Element::Fps));
    assert_eq!(Elements::from_bits(0xA1), set);
    assert_eq!(Elements::default(), Elements::NONE);
    assert_eq!(Elements::ALL.bits(), 0xFF);
}

// E3, PROTOCOL.md section 4.

fn sample() -> SensorSample {
    SensorSample {
        gpu_temp_c: 65,
        gpu_clock_mhz: 2520,
        gpu_usage_percent: 97,
        fan_speed: 1800,
        ram_clock_mhz: 14001,
        ram_usage_percent: 42,
        fps: 144,
        tgp_watts: 450,
    }
}

#[rustfmt::skip]
const SAMPLE_E3: [u8; 18] = [
    0xE3, 0xCB, 0x55, 0xAC, 0x38,
    0x41,       // [5]      65 C
    0x09, 0xD8, // [6..7]   2520 MHz
    0x61,       // [8]      97 %
    0x07, 0x08, // [9..10]  1800 RPM
    0x36, 0xB1, // [11..12] 14001 MHz
    0x2A,       // [13]     42 %
    0x00, 0x90, // [14..15] 144 fps
    0x01, 0xC2, // [16..17] 450 W
];

#[test]
fn e3_has_the_exact_layout() {
    assert_eq!(
        sensor_frame(&sample(), FeedFrameLen::default()),
        padded(&SAMPLE_E3)
    );
}

#[test]
fn e3_128_byte_frame_is_the_same_frame_cut_short() {
    let frame = sensor_frame(&sample(), FeedFrameLen::Truncated);
    let mut expected = SAMPLE_E3.to_vec();
    expected.resize(128, 0);
    assert_eq!(frame, expected);
}

#[test]
fn e3_lengths_other_than_128_and_256_are_refused() {
    assert_eq!(FeedFrameLen::try_from(256).unwrap().bytes(), 256);
    assert_eq!(FeedFrameLen::try_from(128).unwrap().bytes(), 128);
    for bytes in [0, 18, 64, 96, 97, 127, 129, 255, 257] {
        assert!(
            matches!(
                FeedFrameLen::try_from(bytes),
                Err(Error::InvalidArgument(_))
            ),
            "{bytes}"
        );
    }
}

fn u8_field_sample(value: u32) -> SensorSample {
    SensorSample {
        gpu_temp_c: value,
        gpu_usage_percent: value,
        ram_usage_percent: value,
        ..SensorSample::default()
    }
}

fn u16_field_sample(value: u32) -> SensorSample {
    SensorSample {
        gpu_clock_mhz: value,
        fan_speed: value,
        ram_clock_mhz: value,
        fps: value,
        tgp_watts: value,
        ..SensorSample::default()
    }
}

#[test]
fn e3_clamps_u8_fields_at_their_edge() {
    let cases: [(u32, u8); 5] = [
        (0, 0x00),
        (254, 0xFE),
        (255, 0xFF),
        (256, 0xFF),
        (u32::MAX, 0xFF),
    ];
    for (value, byte) in cases {
        let frame = sensor_frame(&u8_field_sample(value), FeedFrameLen::Full);
        let mut expected = padded(&[0xE3, 0xCB, 0x55, 0xAC, 0x38]);
        expected[5] = byte;
        expected[8] = byte;
        expected[13] = byte;
        assert_eq!(frame, expected, "{value}");
    }
}

#[test]
fn e3_clamps_u16_fields_at_their_edge() {
    let cases: [(u32, [u8; 2]); 6] = [
        (0, [0x00, 0x00]),
        (65534, [0xFF, 0xFE]),
        (65535, [0xFF, 0xFF]),
        (65536, [0xFF, 0xFF]),
        // Would be 0x11 0x70 if it wrapped.
        (70000, [0xFF, 0xFF]),
        (u32::MAX, [0xFF, 0xFF]),
    ];
    for (value, bytes) in cases {
        let frame = sensor_frame(&u16_field_sample(value), FeedFrameLen::Full);
        let mut expected = padded(&[0xE3, 0xCB, 0x55, 0xAC, 0x38]);
        for offset in [6, 9, 11, 14, 16] {
            expected[offset..offset + 2].copy_from_slice(&bytes);
        }
        assert_eq!(frame, expected, "{value}");
    }
}

// EA, PROTOCOL.md section 5.

#[test]
fn ea_has_the_exact_layout() {
    let template = Template {
        kind: TemplateKind::Image,
        color: [0x12, 0x34, 0x56],
        image_pos: (0x0140, 0x00AA),
        data_pos: (0x0102, 0x0304),
        enabled: true,
    };
    #[rustfmt::skip]
    let expected = padded(&[
        0xEA, 0xCB, 0x55, 0xAC, 0x38,
        0x02,             // [5]      type
        0x12, 0x34, 0x56, // [6..8]   color
        0x01, 0x40,       // [9..10]  imageX
        0x00, 0xAA,       // [11..12] imageY
        0x01, 0x02,       // [13..14] dataX
        0x03, 0x04,       // [15..16] dataY
        0x01,             // [17]     enabled
    ]);
    assert_eq!(template_frame(&template).to_vec(), expected);
}

#[test]
fn ea_type_codes_and_disabled_flag() {
    let cases = [
        (TemplateKind::Gif, 1),
        (TemplateKind::Image, 2),
        (TemplateKind::Pet, 3),
    ];
    for (kind, byte) in cases {
        let frame = template_frame(&Template {
            kind,
            color: [0, 0, 0],
            image_pos: (0, 0),
            data_pos: (0, 0),
            enabled: false,
        });
        assert_eq!(
            frame.to_vec(),
            padded(&[0xEA, 0xCB, 0x55, 0xAC, 0x38, byte])
        );
    }
}

// AA and the read queries, PROTOCOL.md section 2.

#[test]
fn aa_is_opcode_and_magic_only() {
    assert_eq!(
        save_frame().to_vec(),
        padded(&[0xAA, 0xCB, 0x55, 0xAC, 0x38])
    );
}

#[test]
fn query_frames_have_the_exact_layout() {
    assert_eq!(
        firmware_query_frame().to_vec(),
        padded(&[0xD6, 0xCB, 0x55, 0xAC, 0x38])
    );
    assert_eq!(
        mode_query_frame().to_vec(),
        padded(&[0xDE, 0xCB, 0x55, 0xAC, 0x38])
    );
    assert_eq!(
        display_query_frame().to_vec(),
        padded(&[0xDF, 0xCB, 0x55, 0xAC, 0x38])
    );
    assert_eq!(
        probe_frame().to_vec(),
        padded(&[0xEB, 0xCB, 0x55, 0xAC, 0x38, 0x03])
    );
}

#[test]
fn f4_query_takes_banks_1_to_5_only() {
    for bank in 1..=5u8 {
        assert_eq!(
            loop_query_frame(bank).unwrap().to_vec(),
            padded(&[0xF4, 0xCB, 0x55, 0xAC, 0x38, bank])
        );
    }
    for bank in [0, 6, 255] {
        assert!(
            matches!(loop_query_frame(bank), Err(Error::InvalidArgument(_))),
            "{bank}"
        );
    }
}

// F3, PROTOCOL.md section 2.

#[test]
fn f3_has_the_exact_layout_for_all_carousel_modes() {
    let modes = [
        Mode::Faith1,
        Mode::Faith2,
        Mode::Faith3,
        Mode::Image,
        Mode::Text,
        Mode::Gif,
        Mode::ChibiClock,
    ];
    assert_eq!(
        carousel_frame(5, &modes).unwrap().to_vec(),
        padded(&[0xF3, 0xCB, 0x55, 0xAC, 0x38, 5, 1, 2, 3, 4, 5, 6, 7])
    );
}

#[test]
fn f3_refuses_modes_7_and_8_anywhere_in_the_list() {
    for modes in [
        vec![Mode::Carousel],
        vec![Mode::Carousel, Mode::Gif],
        vec![Mode::Gif, Mode::Carousel],
        vec![Mode::PresetRotation],
        vec![Mode::PresetRotation, Mode::Gif],
        vec![Mode::Gif, Mode::PresetRotation],
    ] {
        assert!(
            matches!(carousel_frame(5, &modes), Err(Error::InvalidArgument(_))),
            "{modes:?}"
        );
    }
}

#[test]
fn f3_list_fills_the_frame_and_longer_is_refused() {
    // [5] is the interval, so [6..255] holds 250 entries.
    assert_eq!(CAROUSEL_MAX_ENTRIES, 250);

    let frame = carousel_frame(9, &[Mode::Text; 250]).unwrap();
    assert_eq!(frame[..6], [0xF3, 0xCB, 0x55, 0xAC, 0x38, 9]);
    assert_eq!(frame[6..], [5; 250]);

    assert!(matches!(
        carousel_frame(9, &[Mode::Text; 251]),
        Err(Error::InvalidArgument(_))
    ));
}

#[test]
fn f3_refuses_an_empty_list() {
    assert!(matches!(
        carousel_frame(9, &[]),
        Err(Error::InvalidArgument(_))
    ));
}

// F1 and the upload sequence, PROTOCOL.md section 6.

fn f1(
    framebuffer: [u8; 4],
    flag: u8,
    chunks: [u8; 4],
    frames: [u8; 2],
    delay: u8,
    mode: u8,
) -> Vec<u8> {
    let mut head = vec![0xF1, 0xCB, 0x55, 0xAC, 0x38];
    head.extend(framebuffer);
    head.push(flag);
    head.extend(chunks);
    head.extend(frames);
    head.extend([delay, mode, 0]);
    padded(&head)
}

#[test]
fn f1_below_the_threshold_is_mode_1() {
    // 20479 / 256 + 1 = 80 chunks.
    let frames = build_upload(&[0; 20479], &UploadParams::still(Framebuffer::Static)).unwrap();
    assert_eq!(
        frames[1].data.to_vec(),
        f1([0x01, 0x30, 0, 0], 1, [0, 0, 0, 0x50], [0, 0], 0, 1)
    );
}

#[test]
fn f1_at_the_threshold_is_mode_2() {
    // 20480 / 256 + 1 = 81 chunks: an exact multiple gets a pad chunk.
    let frames = build_upload(&[0; 20480], &UploadParams::still(Framebuffer::Static)).unwrap();
    assert_eq!(
        frames[1].data.to_vec(),
        f1([0x01, 0x30, 0, 0], 1, [0, 0, 0, 0x51], [0, 0], 0, 2)
    );
}

#[test]
fn f1_explicit_mode_overrides_the_threshold_both_ways() {
    let forced = |mode| UploadParams {
        mode: Some(mode),
        ..UploadParams::still(Framebuffer::Static)
    };
    assert_eq!(upload_header(&forced(TransferMode::Large), 1, 300)[17], 2);
    assert_eq!(upload_header(&forced(TransferMode::Small), 1, 20480)[17], 1);
    assert_eq!(upload_header(&forced(TransferMode::Small), 1, 300)[17], 1);
    assert_eq!(upload_header(&forced(TransferMode::Large), 1, 20480)[17], 2);
}

#[test]
fn f1_delay_clamps_at_255() {
    let delayed = |delay_ms| upload_header(&UploadParams::gif(1, delay_ms), 1, 100)[16];
    assert_eq!(delayed(0), 0);
    assert_eq!(delayed(254), 254);
    assert_eq!(delayed(255), 255);
    assert_eq!(delayed(256), 255);
    assert_eq!(delayed(u32::MAX), 255);
}

#[test]
fn f1_gif_header_has_the_exact_layout() {
    // aorus_lcd.py build_gif_frames: FB_GIF, flag 2, mode 2 even when small.
    let frame = upload_header(&UploadParams::gif(0x0102, 999), 0x0304_0506, 2500);
    assert_eq!(
        frame.to_vec(),
        f1([0, 0, 0, 0], 2, [3, 4, 5, 6], [1, 2], 255, 2)
    );
}

#[test]
fn f1_text_upload_targets_the_text_framebuffer() {
    // Descriptor + 108800 pixel bytes = 108812, so 426 chunks (0x1AA) and the
    // auto mode 2. aorus_lcd.py cli_text.
    let mut payload = DESCRIPTOR.to_vec();
    payload.resize(108_812, 0x5A);
    let frames = build_upload(&payload, &UploadParams::still(Framebuffer::Text)).unwrap();

    assert_eq!(frames.len(), 1 + 1 + 426 + 1);
    assert_eq!(
        frames[1].data.to_vec(),
        f1([0x01, 0x32, 0, 0], 1, [0, 0, 0x01, 0xAA], [0, 0], 0, 2)
    );
    assert_eq!(frames[2].data[..12], DESCRIPTOR);
    assert_eq!(frames[2].data[12..], [0x5A; 244]);
}

#[test]
fn f1_small_text_upload_uses_the_text_framebuffer_with_auto_mode_1() {
    let frames = build_upload(&[0x22; 300], &UploadParams::still(Framebuffer::Text)).unwrap();
    assert_eq!(
        frames[1].data.to_vec(),
        f1([0x01, 0x32, 0, 0], 1, [0, 0, 0, 2], [0, 0], 0, 1)
    );
}

#[test]
fn upload_frames_are_tagged_in_order() {
    use UploadFrameKind::{Begin, Chunk, End, Header};

    let frames = build_upload(&[0x22; 300], &UploadParams::still(Framebuffer::Static)).unwrap();
    let kinds: Vec<_> = frames.iter().map(|frame| frame.kind).collect();
    assert_eq!(kinds, [Begin, Header, Chunk, Chunk, End]);

    assert_eq!(
        frames[0].data.to_vec(),
        padded(&[0xF2, 0xCB, 0x55, 0xAC, 0x38, 1])
    );
    assert_eq!(frames[2].data, [0x22; 256]);
    assert_eq!(frames[3].data[..44], [0x22; 44]);
    assert_eq!(frames[3].data[44..], [0; 212]);
    assert_eq!(
        frames[4].data.to_vec(),
        padded(&[0xF2, 0xCB, 0x55, 0xAC, 0x38, 2])
    );
}

#[test]
fn chunks_are_tagged_even_when_they_look_like_commands() {
    // A payload that starts like an F2 END frame is still a chunk: pacing goes
    // by the tag, never by the bytes.
    let payload = padded(&[0xF2, 0xCB, 0x55, 0xAC, 0x38, 2]);
    let frames = build_upload(&payload, &UploadParams::still(Framebuffer::Static)).unwrap();
    assert_eq!(frames[2].kind, UploadFrameKind::Chunk);
    assert_eq!(frames[2].data.to_vec(), payload);
}

#[test]
fn empty_payload_gets_one_pad_chunk() {
    // 0 / 256 + 1 = 1.
    assert_eq!(chunk_payload(&[]), vec![[0u8; 256]]);
}

#[test]
fn chunk_count_at_the_256_boundary() {
    assert_eq!(chunk_payload(&[1; 255]).len(), 1);
    assert_eq!(chunk_payload(&[1; 256]).len(), 2);
    assert_eq!(chunk_payload(&[1; 257]).len(), 2);
}

// Read-back parsers, PROTOCOL.md section 2. All replies are fixtures written
// from that table; none is a capture from a panel.

#[test]
fn d6_is_the_nibbles_of_byte_1() {
    let version = parse_firmware(&[0xFF, 0x12, 0xFF, 0xFF]).unwrap();
    assert_eq!(version, FirmwareVersion { major: 1, minor: 2 });
    assert_eq!(version.to_string(), "1.2");
    assert_eq!(
        parse_firmware(&[0, 0xAF, 0, 0]).unwrap().to_string(),
        "10.15"
    );
}

#[test]
fn de_maps_raw_values_to_modes() {
    let cases = [
        (0x01, Some(Mode::Faith1)),
        (0x04, Some(Mode::Image)),
        (0x07, Some(Mode::ChibiClock)),
        // byte1 - 1 = 7.
        (0x08, Some(Mode::Carousel)),
        // Internal 9, the byte E5 sends for mode 7.
        (0x0A, Some(Mode::Carousel)),
        // Raw - 1 would make this number 8, which is 0x0B.
        (0x09, None),
        (0x0B, Some(Mode::PresetRotation)),
        (0x0C, None),
    ];
    for (raw, expected) in cases {
        let status = parse_mode(&[0, raw, 1, 0]).unwrap();
        assert_eq!(status.raw_mode, raw);
        assert_eq!(status.mode(), expected, "{raw:#x}");
    }
}

#[test]
fn de_raw_0x0b_is_representable() {
    let status = parse_mode(&[0, 0x0B, 1, 0]).unwrap();
    assert_eq!(status.raw_mode, 0x0B);
    assert_eq!(status.mode(), Some(Mode::PresetRotation));
    assert!(status.on);
}

#[test]
fn de_accepts_every_raw_byte_without_inventing_a_mode() {
    for raw in 0..=255u8 {
        let status = parse_mode(&[0, raw, 0, 0]).unwrap();
        assert_eq!(status.raw_mode, raw);
        let known = matches!(raw, 0x01..=0x08 | 0x0A | 0x0B);
        assert_eq!(status.mode().is_some(), known, "{raw:#x}");
    }
}

#[test]
fn de_on_state_is_byte_2_equal_to_1() {
    let on = |byte| parse_mode(&[0, 1, byte, 0]).unwrap().on;
    assert!(on(1));
    assert!(!on(0));
    assert!(!on(2));
    assert!(!on(0xFF));
}

#[test]
fn df_is_mask_in_byte_1_and_interval_in_byte_2() {
    let status = parse_display(&[0xEE, 0x85, 7, 0xEE]).unwrap();
    assert_eq!(status.elements.bits(), 0x85);
    assert!(status.elements.contains(Element::GpuTemp));
    assert!(status.elements.contains(Element::GpuUsage));
    assert!(status.elements.contains(Element::Tgp));
    assert!(!status.elements.contains(Element::Fps));
    assert_eq!(status.interval_s, 7);
}

#[test]
fn f4_is_interval_then_three_entries() {
    let bank = parse_loop_bank(&[5, 1, 2, 7, 0xEE, 0xEE, 0xEE, 0xEE]).unwrap();
    assert_eq!(bank.interval, 5);
    assert_eq!(
        bank.entries,
        [
            LoopEntry::Mode(Mode::Faith1),
            LoopEntry::Mode(Mode::Faith2),
            LoopEntry::Mode(Mode::ChibiClock),
        ]
    );
}

#[test]
fn f4_keeps_empty_and_unknown_slots() {
    let bank = parse_loop_bank(&[0, 4, 0, 0x0A, 0, 0, 0, 0]).unwrap();
    assert_eq!(
        bank.entries,
        [
            LoopEntry::Mode(Mode::Image),
            LoopEntry::Empty,
            LoopEntry::Unknown(0x0A),
        ]
    );

    // Mode 7 cannot be a carousel entry, so its raw value is not one either.
    let bank = parse_loop_bank(&[0, 8, 0xFF, 0, 0, 0, 0, 0]).unwrap();
    assert_eq!(
        bank.entries,
        [
            LoopEntry::Unknown(8),
            LoopEntry::Unknown(0xFF),
            LoopEntry::Empty,
        ]
    );
}

#[test]
fn parsers_refuse_any_other_length_as_a_protocol_error() {
    let reply = [0x55u8; 64];
    for len in (0..=64).filter(|len| *len != 4) {
        for result in [
            parse_firmware(&reply[..len]).map(drop),
            parse_mode(&reply[..len]).map(drop),
            parse_display(&reply[..len]).map(drop),
        ] {
            assert!(matches!(result, Err(Error::Protocol(_))), "length {len}");
        }
    }
    for len in (0..=64).filter(|len| *len != 8) {
        assert!(
            matches!(parse_loop_bank(&reply[..len]), Err(Error::Protocol(_))),
            "length {len}"
        );
    }
}

#[test]
fn parsers_accept_their_read_lengths() {
    assert_eq!(STATUS_READ_LEN, 4);
    assert_eq!(LOOP_READ_LEN, 8);
    assert!(parse_firmware(&[0; STATUS_READ_LEN]).is_ok());
    assert!(parse_mode(&[0; STATUS_READ_LEN]).is_ok());
    assert!(parse_display(&[0; STATUS_READ_LEN]).is_ok());
    assert!(parse_loop_bank(&[0; LOOP_READ_LEN]).is_ok());
}

#[test]
fn opcode_names_follow_the_table() {
    assert_eq!(opcode_name(0xE5), "SetMode");
    assert_eq!(opcode_name(0xAA), "Save");
    assert_eq!(opcode_name(0xFA), "PowerOff");
    assert_eq!(opcode_name(0x42), "unknown opcode");
}
