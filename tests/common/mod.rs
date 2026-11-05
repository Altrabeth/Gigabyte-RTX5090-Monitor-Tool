//! Loader for the golden vectors in `tests/vectors`. Format and API are
//! documented in `tests/vectors/README.md`.
//!
//! Everything here panics on malformed data: a vector that cannot be read must
//! fail the test run, and unknown keys are rejected so that a typo cannot
//! quietly drop a check.

// Each integration test compiles its own copy of this module and uses only part
// of it, so what is unused differs from one test binary to the next.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

/// The files holding cases, in the order `load_all` returns them.
pub const CASE_FILES: [&str; 7] = [
    "frames.json",
    "commands.json",
    "sequencing.json",
    "rle.json",
    "pixels.json",
    "discovery.json",
    "parsers.json",
];

/// Cases that record a property or depend on a described input.
pub const PROPERTY_FILE: &str = "properties.json";

/// The reference commit every vector was transcribed from.
pub const REFERENCE_COMMIT: &str = "b7f1fa8";

/// Keys `ByteExpect::parse` accepts. The last ones belong to other checks that
/// share an `expect` object with byte checks.
const BYTE_EXPECT_KEYS: [&str; 15] = [
    "len",
    "exact",
    "at",
    "zero_from",
    "contains",
    "index",
    "reads",
    "returns_canned_reply",
    "count",
    "each_len",
    "items",
    "joined",
    "frame_count",
    "chunk_count_is_len_div_256_plus_1",
    "body_is_payload_then_zero_padding",
];

const SEGMENT_KINDS: [&str; 7] = [
    "hex",
    "u8",
    "u16le",
    "u32le",
    "u16be",
    "u32be",
    "byte_range",
];

const CASE_KEYS: [&str; 9] = [
    "name",
    "source",
    "lines",
    "stated",
    "call",
    "input",
    "expect",
    "note",
    "out_of_scope",
];

/// One transcribed reference assertion.
#[derive(Debug, Clone)]
pub struct Case {
    /// Unique within its file.
    pub name: String,
    /// The vector file this case came from.
    pub file: String,
    /// The reference test, or `aorus_lcd.py::run_selftest::<check>`.
    pub source: String,
    /// Line numbers in the reference file, for audit.
    pub lines: String,
    /// The reference lines this case transcribes, verbatim.
    pub stated: Vec<String>,
    /// The reference function under test, which selects the Rust call.
    pub call: String,
    pub input: Value,
    pub expect: Value,
    pub note: Option<String>,
    /// Why this case has no Rust counterpart, when it has none.
    pub out_of_scope: Option<String>,
}

impl Case {
    fn parse(file: &str, value: &Value) -> Case {
        let map = object(value, file);
        for key in map.keys() {
            assert!(
                CASE_KEYS.contains(&key.as_str()),
                "{file}: unknown case key {key}"
            );
        }
        let text = |key: &str| -> String {
            map.get(key)
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("{file}: case has no string {key}: {value}"))
                .to_owned()
        };
        let optional_text = |key: &str| -> Option<String> { map.get(key).map(|_| text(key)) };
        let stated = map
            .get("stated")
            .and_then(Value::as_array)
            .unwrap_or_else(|| panic!("{file}: case has no stated list: {value}"))
            .iter()
            .map(|line| line.as_str().expect("stated lines are strings").to_owned())
            .collect();

        Case {
            name: text("name"),
            file: file.to_owned(),
            source: text("source"),
            lines: text("lines"),
            stated,
            call: text("call"),
            input: map.get("input").expect("case has input").clone(),
            expect: map.get("expect").expect("case has expect").clone(),
            note: optional_text("note"),
            out_of_scope: optional_text("out_of_scope"),
        }
    }

    /// `file::name`, for assertion messages.
    pub fn label(&self) -> String {
        format!("{}::{}", self.file, self.name)
    }

    fn input_field(&self, key: &str) -> &Value {
        self.input
            .get(key)
            .unwrap_or_else(|| panic!("{}: no input {key}", self.label()))
    }

    /// An integer input. JSON has no hex literals, so `"0xE5"` is also accepted.
    pub fn u64(&self, key: &str) -> u64 {
        number(self.input_field(key))
    }

    /// An input the reference call left at its default.
    pub fn opt_u64(&self, key: &str) -> Option<u64> {
        self.input.get(key).map(number)
    }

    pub fn bool(&self, key: &str) -> bool {
        self.input_field(key)
            .as_bool()
            .unwrap_or_else(|| panic!("{}: input {key} is not a bool", self.label()))
    }

    pub fn opt_bool(&self, key: &str) -> Option<bool> {
        self.input.get(key).map(|_| self.bool(key))
    }

    pub fn str(&self, key: &str) -> &str {
        self.input_field(key)
            .as_str()
            .unwrap_or_else(|| panic!("{}: input {key} is not a string", self.label()))
    }

    /// A byte input, as a hex string or a list of segments.
    pub fn bytes(&self, key: &str) -> Vec<u8> {
        parse_bytes(self.input_field(key))
    }

    pub fn u64_list(&self, key: &str) -> Vec<u64> {
        self.input_field(key)
            .as_array()
            .unwrap_or_else(|| panic!("{}: input {key} is not a list", self.label()))
            .iter()
            .map(number)
            .collect()
    }

    /// The whole `expect` object as byte checks.
    pub fn byte_expect(&self) -> ByteExpect {
        ByteExpect::parse(&self.expect)
    }

    /// The whole `expect` object as checks on a list of frames.
    pub fn frame_list_expect(&self) -> FrameListExpect {
        FrameListExpect::parse(&self.expect)
    }

    /// A named list of byte checks inside `expect`, such as `before_upload`.
    pub fn byte_expect_list(&self, key: &str) -> Vec<ByteExpect> {
        self.expect
            .get(key)
            .and_then(Value::as_array)
            .unwrap_or_else(|| panic!("{}: no expect list {key}", self.label()))
            .iter()
            .map(ByteExpect::parse)
            .collect()
    }

    /// Whether the reference requires this input to be rejected.
    pub fn expects_error(&self) -> bool {
        self.expect.get("error").is_some()
    }
}

/// Only what the reference asserts about a byte string. Bytes it does not
/// mention are not checked, so passing is byte equality only where the checks
/// cover every byte; see the README.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ByteExpect {
    /// Required total length.
    pub len: Option<usize>,
    /// The entire expected output.
    pub exact: Option<Vec<u8>>,
    /// Bytes required at given offsets.
    pub at: Vec<(usize, Vec<u8>)>,
    /// Every byte from this offset to the end must be zero.
    pub zero_from: Option<usize>,
    /// A byte string that must occur somewhere, at any alignment.
    pub contains: Option<Vec<u8>>,
}

impl ByteExpect {
    pub fn parse(value: &Value) -> ByteExpect {
        let map = object(value, "byte expect");
        for key in map.keys() {
            assert!(
                BYTE_EXPECT_KEYS.contains(&key.as_str()),
                "unknown byte expect key {key}"
            );
        }
        let at = map
            .get("at")
            .map(|entries| {
                entries
                    .as_array()
                    .expect("at is a list")
                    .iter()
                    .map(|entry| {
                        let entry = object(entry, "at entry");
                        let offset = entry.get("offset").expect("at entry has an offset");
                        (number(offset) as usize, segment_bytes(entry))
                    })
                    .collect()
            })
            .unwrap_or_default();

        ByteExpect {
            len: map.get("len").map(|v| number(v) as usize),
            exact: map.get("exact").map(parse_bytes),
            at,
            zero_from: map.get("zero_from").map(|v| number(v) as usize),
            contains: map.get("contains").map(parse_bytes),
        }
    }

    /// Whether this says anything at all. A check with no content would pass
    /// on every input.
    pub fn is_empty(&self) -> bool {
        self.len.is_none()
            && self.exact.is_none()
            && self.at.is_empty()
            && self.zero_from.is_none()
            && self.contains.is_none()
    }

    pub fn verify(&self, actual: &[u8]) -> Result<(), String> {
        if let Some(len) = self.len {
            if actual.len() != len {
                return Err(format!("length {} but expected {len}", actual.len()));
            }
        }
        if let Some(exact) = &self.exact {
            if actual != exact.as_slice() {
                return Err(first_difference(actual, exact));
            }
        }
        for (offset, wanted) in &self.at {
            let end = offset + wanted.len();
            match actual.get(*offset..end) {
                Some(found) if found == wanted.as_slice() => {}
                Some(found) => {
                    return Err(format!(
                        "at offset {offset}: found {found:02x?}, expected {wanted:02x?}"
                    ))
                }
                None => return Err(format!("output of {} ends before {end}", actual.len())),
            }
        }
        if let Some(from) = self.zero_from {
            let tail = actual
                .get(from..)
                .ok_or_else(|| format!("output of {} ends before {from}", actual.len()))?;
            if let Some(position) = tail.iter().position(|byte| *byte != 0) {
                return Err(format!(
                    "byte {} is {:#04x}, expected zero padding",
                    from + position,
                    tail[position]
                ));
            }
        }
        if let Some(wanted) = &self.contains {
            if !actual.windows(wanted.len()).any(|window| window == wanted) {
                return Err(format!("output does not contain {wanted:02x?}"));
            }
        }
        Ok(())
    }

    /// Panics with `label` if `actual` breaks a check.
    pub fn check(&self, actual: &[u8], label: &str) {
        if let Err(problem) = self.verify(actual) {
            panic!("{label}: {problem}");
        }
    }
}

/// What the reference asserts about a list of frames or chunks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FrameListExpect {
    pub count: Option<usize>,
    /// Required length of every frame.
    pub each_len: Option<usize>,
    /// Checks on single frames. A negative index counts from the end.
    pub items: Vec<(i64, ByteExpect)>,
    /// Checks on all frames joined together.
    pub joined: Option<ByteExpect>,
}

impl FrameListExpect {
    pub fn parse(value: &Value) -> FrameListExpect {
        let items = value
            .get("items")
            .map(|items| {
                items
                    .as_array()
                    .expect("items is a list")
                    .iter()
                    .map(|item| {
                        let index = item
                            .get("index")
                            .and_then(Value::as_i64)
                            .expect("item has an integer index");
                        (index, ByteExpect::parse(item))
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Validates the keys of the object as a whole.
        ByteExpect::parse(value);

        FrameListExpect {
            count: value.get("count").map(|v| number(v) as usize),
            each_len: value.get("each_len").map(|v| number(v) as usize),
            items,
            joined: value.get("joined").map(ByteExpect::parse),
        }
    }

    pub fn verify<F: AsRef<[u8]>>(&self, frames: &[F]) -> Result<(), String> {
        if let Some(count) = self.count {
            if frames.len() != count {
                return Err(format!("{} frames but expected {count}", frames.len()));
            }
        }
        if let Some(each_len) = self.each_len {
            for (index, frame) in frames.iter().enumerate() {
                if frame.as_ref().len() != each_len {
                    return Err(format!("frame {index} is not {each_len} bytes"));
                }
            }
        }
        for (index, expect) in &self.items {
            let resolved = if *index < 0 {
                frames.len() as i64 + index
            } else {
                *index
            };
            let frame = usize::try_from(resolved)
                .ok()
                .and_then(|i| frames.get(i))
                .ok_or_else(|| format!("no frame {index} among {}", frames.len()))?;
            expect
                .verify(frame.as_ref())
                .map_err(|problem| format!("frame {index}: {problem}"))?;
        }
        if let Some(joined) = &self.joined {
            let all: Vec<u8> = frames.iter().flat_map(|f| f.as_ref().to_vec()).collect();
            joined
                .verify(&all)
                .map_err(|problem| format!("joined frames: {problem}"))?;
        }
        Ok(())
    }

    /// Panics with `label` if `frames` breaks a check.
    pub fn check<F: AsRef<[u8]>>(&self, frames: &[F], label: &str) {
        if let Err(problem) = self.verify(frames) {
            panic!("{label}: {problem}");
        }
    }
}

/// One entry of `coverage.json`: what became of one reference test.
#[derive(Debug, Clone)]
pub struct CoverageEntry {
    pub source: String,
    /// `vectors`, `property` or `skipped`.
    pub status: String,
    /// Where the cases are. Absent for skipped tests.
    pub file: Option<String>,
    /// Why the test was skipped.
    pub reason: Option<String>,
}

pub fn vectors_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/vectors")
}

/// Parses one file of `tests/vectors`.
pub fn load_json(file: &str) -> Value {
    let path = vectors_dir().join(file);
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    let document: Value = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("{} is not valid JSON: {error}", path.display()));
    assert_eq!(
        document["commit"].as_str(),
        Some(REFERENCE_COMMIT),
        "{file} is not pinned to the reference commit"
    );
    document
}

/// All cases in one file.
pub fn load(file: &str) -> Vec<Case> {
    load_json(file)["cases"]
        .as_array()
        .unwrap_or_else(|| panic!("{file} has no cases list"))
        .iter()
        .map(|case| Case::parse(file, case))
        .collect()
}

/// The cases for one reference function. Panics if there are none, so a
/// misspelled name cannot turn a test into a loop over nothing.
pub fn cases_for(file: &str, call: &str) -> Vec<Case> {
    let cases: Vec<Case> = load(file)
        .into_iter()
        .filter(|case| case.call == call)
        .collect();
    assert!(!cases.is_empty(), "{file} has no cases for {call}");
    cases
}

/// One case by name.
pub fn case(file: &str, name: &str) -> Case {
    load(file)
        .into_iter()
        .find(|case| case.name == name)
        .unwrap_or_else(|| panic!("{file} has no case {name}"))
}

/// Every case in every file, property cases included.
pub fn load_all() -> Vec<Case> {
    CASE_FILES
        .iter()
        .chain([&PROPERTY_FILE])
        .flat_map(|file| load(file))
        .collect()
}

pub fn load_coverage() -> Vec<CoverageEntry> {
    load_json("coverage.json")["entries"]
        .as_array()
        .expect("coverage.json has an entries list")
        .iter()
        .map(|entry| {
            let text = |key: &str| entry.get(key).and_then(Value::as_str).map(str::to_owned);
            CoverageEntry {
                source: text("source").expect("coverage entry has a source"),
                status: text("status").expect("coverage entry has a status"),
                file: text("file"),
                reason: text("reason"),
            }
        })
        .collect()
}

/// A reference constant's raw value from `constants.json`.
pub fn constant(name: &str) -> Value {
    load_json("constants.json")["constants"][name]["value"].clone()
}

pub fn constant_u64(name: &str) -> u64 {
    number(&constant(name))
}

pub fn constant_bytes(name: &str) -> Vec<u8> {
    parse_bytes(&constant(name))
}

/// Decodes one RLE blob, following the token grammar the reference test's
/// `_rle_decode` documents as mirroring the firmware (lines 141-156): `u16 LE`
/// head, bit 15 set for a run, low 15 bits the pixel count.
pub fn rle_decode(blob: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < blob.len() {
        let head = u16::from_le_bytes([blob[pos], blob[pos + 1]]);
        let count = usize::from(head & 0x7FFF);
        assert!(count != 0, "zero-count token at {pos}");
        if head & 0x8000 != 0 {
            for _ in 0..count {
                out.extend_from_slice(&blob[pos + 2..pos + 4]);
            }
            pos += 4;
        } else {
            out.extend_from_slice(&blob[pos + 2..pos + 2 + 2 * count]);
            pos += 2 + 2 * count;
        }
    }
    out
}

/// Builds the pseudo-random frame of a case with an `input.generator` object,
/// using the arithmetic the reference test states (lines 189-193).
pub fn lcg_frame(case: &Case) -> Vec<u8> {
    let generator = case.input_field("generator");
    let field = |key: &str| number(&generator[key]);
    let (multiplier, increment) = (field("multiplier"), field("increment"));
    let (state_mask, shift, pixel_mask) = (
        field("state_mask"),
        field("pixel_shift"),
        field("pixel_mask"),
    );

    let mut state = field("seed");
    let mut px = Vec::new();
    for _ in 0..field("pixels") {
        state = (state * multiplier + increment) & state_mask;
        let pixel = (state >> shift) & pixel_mask;
        px.extend_from_slice(&(pixel as u16).to_le_bytes());
    }
    px
}

/// Parses a byte value: a hex string, or a list of segments to concatenate.
pub fn parse_bytes(value: &Value) -> Vec<u8> {
    match value {
        Value::String(text) => parse_hex(text),
        Value::Array(segments) => segments
            .iter()
            .flat_map(|segment| segment_bytes(object(segment, "segment")))
            .collect(),
        other => panic!("not a byte value: {other}"),
    }
}

fn segment_bytes(segment: &Map<String, Value>) -> Vec<u8> {
    let mut kind = None;
    for key in segment.keys() {
        if SEGMENT_KINDS.contains(&key.as_str()) {
            assert!(kind.is_none(), "segment has two kinds: {segment:?}");
            kind = Some(key.as_str());
        } else {
            assert!(
                key == "repeat" || key == "offset",
                "unknown segment key {key}"
            );
        }
    }
    let kind = kind.unwrap_or_else(|| panic!("segment has no kind: {segment:?}"));
    let value = &segment[kind];

    let values = || {
        value
            .as_array()
            .expect("a list of numbers")
            .iter()
            .map(number)
    };
    let unit: Vec<u8> = match kind {
        "hex" => parse_hex(value.as_str().expect("hex is a string")),
        "u8" => values()
            .flat_map(|v| narrow::<u8>(v).to_be_bytes())
            .collect(),
        "u16le" => values()
            .flat_map(|v| narrow::<u16>(v).to_le_bytes())
            .collect(),
        "u16be" => values()
            .flat_map(|v| narrow::<u16>(v).to_be_bytes())
            .collect(),
        "u32le" => values()
            .flat_map(|v| narrow::<u32>(v).to_le_bytes())
            .collect(),
        "u32be" => values()
            .flat_map(|v| narrow::<u32>(v).to_be_bytes())
            .collect(),
        "byte_range" => {
            let bounds: Vec<u64> = values().collect();
            assert_eq!(bounds.len(), 2, "byte_range is [start, end)");
            (bounds[0]..bounds[1]).map(narrow::<u8>).collect()
        }
        _ => unreachable!("kinds are checked above"),
    };

    let repeat = segment.get("repeat").map_or(1, number) as usize;
    unit.repeat(repeat)
}

fn narrow<T: TryFrom<u64>>(value: u64) -> T {
    T::try_from(value).unwrap_or_else(|_| panic!("{value} does not fit its segment"))
}

fn parse_hex(text: &str) -> Vec<u8> {
    text.split_whitespace()
        .map(|pair| {
            assert_eq!(pair.len(), 2, "hex bytes are two digits: {pair}");
            u8::from_str_radix(pair, 16).unwrap_or_else(|_| panic!("bad hex byte {pair}"))
        })
        .collect()
}

/// An integer as a JSON number, or as a `"0x"` string to keep the reference's
/// own spelling of a value.
fn number(value: &Value) -> u64 {
    if let Some(number) = value.as_u64() {
        return number;
    }
    value
        .as_str()
        .and_then(|text| text.strip_prefix("0x"))
        .and_then(|digits| u64::from_str_radix(digits, 16).ok())
        .unwrap_or_else(|| panic!("not an integer: {value}"))
}

fn object<'a>(value: &'a Value, what: &str) -> &'a Map<String, Value> {
    value
        .as_object()
        .unwrap_or_else(|| panic!("{what} is not an object: {value}"))
}

fn first_difference(actual: &[u8], expected: &[u8]) -> String {
    match actual.iter().zip(expected).position(|(a, b)| a != b) {
        Some(i) => format!(
            "byte {i} is {:#04x}, expected {:#04x}",
            actual[i], expected[i]
        ),
        None => format!("length {} but expected {}", actual.len(), expected.len()),
    }
}
