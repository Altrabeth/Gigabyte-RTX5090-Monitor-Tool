# Reference test vectors

Golden values for asserting Rust output against the Python reference without
running it. The source is
[`albancreton/aorus-master-linux`](https://github.com/albancreton/aorus-master-linux)
pinned at commit **`b7f1fa8`**:

- `tests/test_encoders.py`, all 488 lines
- `run_selftest()` in `aorus_lcd.py`, lines 415-448

**These files are the only direct link to the reference**, because no reference
code is ever executed. Treat them as read-only data.

## Rules

- A value is here only if the reference states it literally, or it follows from
  arithmetic the reference states, such as `len(p) // 256 + 1`.
- **Never generate or update a value by running our own code.** A vector computed
  by the code under test proves nothing. If a test disagrees with a vector, check
  the reference; do not edit the vector to pass.
- Nothing was derived to fill a gap. If the reference does not assert a byte, no
  vector does either, even when the value looks obvious.

## Files

| File | Contents | Used by |
| --- | --- | --- |
| `frames.json` | `cmd_frame`, `f2_frame`, `make_f1_header`, `chunk_payload`, `build_upload` | #15 |
| `commands.json` | Wire frames of `E7`, `E5`, `F3`, `E1`, `EB 03`, `FA` | #15, #17 |
| `sequencing.json` | Upload pacing and `SetMode` ordering | #28 |
| `rle.json` | `rle_encode_frame`, `gif_frame_table` | #27 |
| `pixels.json` | RGB565 conversion | #26 |
| `discovery.json` | Adapter selection by name | #12 |
| `parsers.json` | `parse_hex_bytes`, `parse_color` | #31 for the colours; `raw` is not in the README CLI |
| `properties.json` | Cases that state a property, not bytes | #16, #26, #27, #29, #31 |
| `constants.json` | Reference constants the tests use by name, with their source lines | everything |
| `coverage.json` | What became of every reference test | `vectors_load.rs` |

`tests/vectors_load.rs` checks the vectors and needs no encoder.

## Case format

```json
{
  "name": "f1_header_static_image",
  "source": "tests/test_encoders.py::test_f1_header_static_image",
  "lines": "10, 27-37",
  "stated": ["assert h[:5] == F1 and len(h) == 256"],
  "call": "make_f1_header",
  "input": { "fb_addr": "0x01300000", "nchunks": 426, "nframes": 0, "delay": 0, "usize": 108812 },
  "expect": { "len": 256, "at": [{ "offset": 0, "hex": "f1 cb 55 ac 38" }], "zero_from": 19 }
}
```

- `source` is the reference test, or `aorus_lcd.py::run_selftest::<check>`.
- `lines` are line numbers in that file at `b7f1fa8`. `F1` and `F2` in the test
  file are defined on its lines 9-10.
- `stated` quotes the reference lines verbatim, as the audit trail.
- `call` is the **reference** function name, and input keys are its argument names.
  Map them to the Rust API in your test.
- An input key the reference call omits is absent from `input`. It takes the
  reference default, which `make_f1_header` declares as `flag=1, mode=None`
  (auto mode).
- `note` records anything a reader could misjudge.
- `out_of_scope` marks a case with no Rust counterpart. Do not build code for it.

### Byte checks

An `expect` object can hold these keys:

| Key | Meaning |
| --- | --- |
| `len` | Required total length. |
| `exact` | The entire output. |
| `at` | A list of byte values at offsets: an `offset` key plus a segment. |
| `zero_from` | Every byte from this offset to the end is zero. |
| `contains` | This byte string occurs somewhere, at any alignment. |

**A case checks only what the reference asserted.** Passing means byte equality
only where the checks cover the whole output. These cases do:

- every case with `exact`
- `cmd_frame_e5_04`, `f2_begin`, `f2_end`, `f1_header_static_image` and
  `gif_frame_table_offsets`, which `vectors_load.rs` confirms cover every byte

All other cases are partial. For example, the reference never checks the zero
padding of `E7`, `E5`, `F3`, `E1` or `EB` frames, and it checks `FA` only at
offsets 0 to 4. Assert such bytes in your own tests from `PROTOCOL.md`, not here.

### Byte values

A byte value is a hex string (`"e5 04"`) or a list of segments that are joined.
A segment has one kind plus an optional `repeat`:

| Kind | Example | Bytes |
| --- | --- | --- |
| `hex` | `{"hex": "aa", "repeat": 3}` | `aa aa aa` |
| `u8` | `{"u8": [200]}` | `c8` |
| `u16le` | `{"u16le": ["0x1234"]}` | `34 12` |
| `u16be` | `{"u16be": [7]}` | `00 07` |
| `u32le` | `{"u32le": [31]}` | `1f 00 00 00` |
| `u32be` | `{"u32be": [426]}` | `00 00 01 aa` |
| `byte_range` | `{"byte_range": [0, 3]}` | `00 01 02`, the end is excluded |

`u16le` mirrors the reference's `_px` helper. Numbers may be written as `"0x"`
strings, to keep the reference's own spelling. These forms let a vector read like
the assertion it came from, so values are not converted by hand.

### Frame lists

`chunk_payload` and `build_upload` return several frames, so their `expect` has:

| Key | Meaning |
| --- | --- |
| `count` | Required number of frames. |
| `each_len` | Required length of every frame. |
| `items` | Byte checks on one frame, chosen by `index`. A negative index counts from the end. |
| `joined` | Byte checks on all frames concatenated. |

### Other expect keys

| Call | Keys |
| --- | --- |
| `rle_encode_frame` | `error`: the input must be rejected. Check with `case.expects_error()`. |
| `read_cmd` | Byte checks on the write, plus `reads` and `returns_canned_reply`. |
| `send_upload` | `writes_equal_upload_frames`, `first_sleeps_s`, `remaining_sleeps_each_s`. |
| `upload_content` | `before_upload`, `after_upload`: byte checks on the frames around the upload. The writes in between equal the upload frames. |
| `find_nvidia_bus` | `bus`, which is `null` for none. |
| `list_adapters` | `first`, the first `[bus, name]` pair. |
| `parse_color` | `rgb`. |

## Loader API

The loader is in `tests/common/mod.rs`. Add `mod common;` to your integration test:

```rust
mod common;

#[test]
fn header_matches_the_reference() {
    for case in common::cases_for("frames.json", "make_f1_header") {
        let frame = make_f1_header(
            case.u64("fb_addr") as u32,
            case.u64("nchunks") as u32,
            case.u64("nframes") as u16,
            case.u64("delay"),
            case.u64("usize") as usize,
            case.opt_u64("flag"),
            case.opt_u64("mode"),
        );
        case.byte_expect().check(&frame, &case.label());
    }
}

#[test]
fn rle_matches_the_reference() {
    for case in common::cases_for("rle.json", "rle_encode_frame") {
        let result = rle_encode_frame(&case.bytes("px"));
        if case.expects_error() {
            assert!(result.is_err(), "{}", case.label());
        } else {
            case.byte_expect().check(&result.unwrap(), &case.label());
        }
    }
}

#[test]
fn rle_round_trips() {
    let case = common::case(common::PROPERTY_FILE, "rle_round_trip_lcg_frame");
    let frame = common::lcg_frame(&case);
    assert_eq!(common::rle_decode(&rle_encode_frame(&frame).unwrap()), frame);
}
```

| Function | Purpose |
| --- | --- |
| `cases_for(file, call)` | Cases for one reference function. Panics if there are none, so a typo cannot pass by looping over nothing. |
| `case(file, name)` | One case by name. |
| `load(file)`, `load_all()` | All cases of one file, or of every file. |
| `load_json(file)` | The raw document, for example the `generators` in `properties.json`. |
| `load_coverage()` | The entries of `coverage.json`. |
| `constant(name)`, `constant_u64(name)`, `constant_bytes(name)` | A value from `constants.json`. |
| `parse_bytes(value)` | Expands a JSON byte value. |
| `rle_decode(blob)` | Test-only decoder for the token grammar. It panics on a zero-count token. |
| `lcg_frame(&case)` | The pseudo-random frame of a case with `input.generator`. |
| `vectors_dir()` | Path of `tests/vectors`. |

`Case` has the public fields `name`, `file`, `source`, `lines`, `stated`, `call`,
`input`, `expect`, `note` and `out_of_scope`, and these methods:

| Method | Returns |
| --- | --- |
| `u64(key)`, `opt_u64(key)` | An integer input, required or optional. |
| `bool(key)`, `opt_bool(key)`, `str(key)` | Other scalar inputs. |
| `bytes(key)`, `u64_list(key)` | A byte input or an integer list. |
| `byte_expect()` | `expect` as a `ByteExpect`. |
| `frame_list_expect()` | `expect` as a `FrameListExpect`. |
| `byte_expect_list(key)` | A named list of `ByteExpect`, such as `before_upload`. |
| `expects_error()` | Whether the input must be rejected. |
| `label()` | `file::name`, for assertion messages. |

`ByteExpect` and `FrameListExpect` each have `check(actual, label)`, which panics
on a mismatch, and `verify(actual)`, which returns `Result<(), String>`.
`FrameListExpect` takes any slice of `AsRef<[u8]>`.

## Coverage

`coverage.json` lists all 35 reference tests and all 9 `run_selftest` checks, and
`vectors_load.rs` fails if an entry has no cases or a case has no entry. Status is
`vectors`, `property` or `skipped`.

### Skipped

| Test | Reason |
| --- | --- |
| `test_encoder_matches_gcc_ground_truth` | Needs GCC's proprietary `animation.bin`, which is in neither reference. The test skips itself without that file and states no literals. |
| `test_selftest_passes` | Only asserts that the reference's `run_selftest` returns 0. Its nine checks are transcribed one by one. |
| `test_parser_wiring` | Asserts argparse wiring to Python function objects. The README defines the Rust CLI separately. |

### Property cases

These assert behaviour rather than concrete bytes:

| Test | Why no bytes |
| --- | --- |
| `test_rle_round_trip_random_ish` | States the frame's arithmetic and that it decodes back, not the encoded output. |
| `test_render_text_le565` | Pixels depend on font and rasterizer. |
| `test_gif_to_le565_frames`, `test_gif_payload_self_consistent`, `test_build_gif_frames_wraps_payload` | Input is a GIF Pillow writes at test time, which cannot be reproduced without running it. `write_test_gif` in `properties.json` records the recipe. |
| `test_probe_is_a_status_query_not_a_quick_write`, `test_probe_fails_when_readback_fails` | Check behaviour through a fake bus. `detail` is reference CLI text. |

### What the reference does not cover

Each needs a test built from the spec, because no vector pins it:

- the `F1` auto mode boundary at exactly 20480 bytes; the reference tests only 300
  and 108812
- any window split at `0x7FFF` pixels, or a run limited by a window end
- the frames of `E3`, `EA` and `AA`
- read-back parsers for `D6`, `DE`, `DF` and `F4`
- the `SetMode` for text uploads, and the `AA` text effect

### Out-of-scope cases

- `power_off_mode` (`FA`) is transcribed for completeness and marked
  `out_of_scope`, since tracker rule 6 excludes it.
- `set_brightness_mask_and_value` is the reference's experimental brightness
  command, which rule 6 also excludes. Its bytes are the `E1` layout of
  `PROTOCOL.md` section 3, so it still applies to the `E1` builder as element mask
  `0x05` with interval 200.
