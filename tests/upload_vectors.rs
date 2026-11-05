//! [`Panel::upload`] against what the Python reference states in
//! `tests/vectors/sequencing.json`.

mod common;

use std::time::Duration;

use aorus_lcd::bus::transport::testing::Mock;
use aorus_lcd::panel::{Panel, RecordingSleep, UploadOptions, DEFAULT_CHUNK_DELAY};
use aorus_lcd::protocol::{build_upload, Framebuffer, UploadParams};
use common::{case, cases_for, ByteExpect, Case};

const FILE: &str = "sequencing.json";

/// The Rust form of a case's `fb_addr`, `is_gif` and `set_display_mode`.
///
/// `upload_content_gif_mode_before` reuses `FB_STATIC` frames, because it tests
/// ordering only, and its note says a real gif uses `FB_GIF`. Here the
/// framebuffer decides the order, so that case runs as a real gif.
fn inputs(case: &Case) -> (UploadParams, UploadOptions) {
    assert_eq!(
        case.u64("fb_addr"),
        u64::from(Framebuffer::Static.address())
    );
    let is_gif = case.opt_bool("is_gif").unwrap_or(false);
    let params = if is_gif {
        UploadParams::gif(0, 0)
    } else {
        UploadParams::still(Framebuffer::Static)
    };
    let options = UploadOptions {
        // `send_upload` is the stream alone.
        switch_mode: case.call == "upload_content"
            && case.opt_bool("set_display_mode").unwrap_or(true),
        ..UploadOptions::default()
    };
    (params, options)
}

/// Runs a case, and returns the writes and the delays.
fn run(case: &Case) -> (Vec<Vec<u8>>, Vec<Duration>, Vec<Vec<u8>>) {
    let (params, options) = inputs(case);
    let payload = case.bytes("payload");
    let mock = Mock::new();
    let mut sleep = RecordingSleep::default();

    Panel::new(mock.clone())
        .upload(&payload, &params, options, &mut sleep, |_| {})
        .unwrap();

    let frames = build_upload(&payload, &params).unwrap();
    let frames = frames.iter().map(|frame| frame.data.to_vec()).collect();
    (mock.writes(), sleep.delays, frames)
}

fn seconds(value: &serde_json::Value) -> Duration {
    Duration::from_secs_f64(value.as_f64().unwrap())
}

#[test]
fn send_upload_pacing() {
    let case = case(FILE, "send_upload_pacing");

    let (writes, delays, frames) = run(&case);

    assert_eq!(case.expect["writes_equal_upload_frames"], true);
    assert_eq!(writes, frames);
    let first: Vec<Duration> = case.expect["first_sleeps_s"]
        .as_array()
        .unwrap()
        .iter()
        .map(seconds)
        .collect();
    assert_eq!(delays[..2], first);
    let rest = seconds(&case.expect["remaining_sleeps_each_s"]);
    assert_eq!(rest, DEFAULT_CHUNK_DELAY);
    assert!(delays[2..].iter().all(|delay| *delay == rest));
}

#[test]
fn upload_content_orders_set_mode_as_stated() {
    for case in cases_for(FILE, "upload_content") {
        let (writes, _, frames) = run(&case);

        let before = case.byte_expect_list("before_upload");
        let after = case.byte_expect_list("after_upload");
        assert_eq!(
            writes.len(),
            before.len() + frames.len() + after.len(),
            "{}",
            case.label()
        );
        let (head, rest) = writes.split_at(before.len());
        let (middle, tail) = rest.split_at(frames.len());
        check_all(&before, head, &case);
        assert_eq!(middle, frames, "{}", case.label());
        check_all(&after, tail, &case);
    }
}

fn check_all(expects: &[ByteExpect], writes: &[Vec<u8>], case: &Case) {
    for (expect, write) in expects.iter().zip(writes) {
        expect.check(write, &case.label());
    }
}
