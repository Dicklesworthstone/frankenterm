#![no_main]

use codec::bounded_varbincode_deserialize_for_fuzz;
use libfuzzer_sys::arbitrary::{Arbitrary, Result as ArbitraryResult, Unstructured};
use libfuzzer_sys::fuzz_target;
use std::io::Cursor;

const MAX_CASE_BYTES: usize = 64 * 1024;
const MAX_SEGMENTS: usize = 64;

#[derive(Clone, Copy, Debug)]
enum TargetKind {
    Bytes,
    String,
    VecBytes,
    VecString,
}

impl<'a> Arbitrary<'a> for TargetKind {
    fn arbitrary(u: &mut Unstructured<'a>) -> ArbitraryResult<Self> {
        Ok(match u.int_in_range(0..=3u8)? {
            0 => Self::Bytes,
            1 => Self::String,
            2 => Self::VecBytes,
            _ => Self::VecString,
        })
    }
}

#[derive(Clone, Debug)]
struct Segment {
    len_prefix: u64,
    bytes: Vec<u8>,
}

impl<'a> Arbitrary<'a> for Segment {
    fn arbitrary(u: &mut Unstructured<'a>) -> ArbitraryResult<Self> {
        let len_prefix = match u.int_in_range(0..=7u8)? {
            0 => 0,
            1 => 1,
            2 => 8,
            3 => 255,
            4 => 4096,
            5 => 1_000_001,
            6 => (256u64 * 1024 * 1024) + 1,
            _ => u64::arbitrary(u)?,
        };
        let bytes = u.bytes(u.int_in_range(0..=1024usize)?)?.to_vec();
        Ok(Self { len_prefix, bytes })
    }
}

#[derive(Clone, Debug)]
struct FuzzCase {
    target: TargetKind,
    container_len: u64,
    segments: Vec<Segment>,
    trailing: Vec<u8>,
}

impl<'a> Arbitrary<'a> for FuzzCase {
    fn arbitrary(u: &mut Unstructured<'a>) -> ArbitraryResult<Self> {
        let target = TargetKind::arbitrary(u)?;
        let container_len = match u.int_in_range(0..=5u8)? {
            0 => 0,
            1 => 1,
            2 => 2,
            3 => 16,
            4 => 1_000_001,
            _ => u64::arbitrary(u)?,
        };
        let segment_count = u.int_in_range(0..=MAX_SEGMENTS)?;
        let mut segments = Vec::with_capacity(segment_count);
        for _ in 0..segment_count {
            segments.push(Segment::arbitrary(u)?);
        }
        let trailing = u.bytes(u.int_in_range(0..=256usize)?)?.to_vec();
        Ok(Self {
            target,
            container_len,
            segments,
            trailing,
        })
    }
}

fn write_leb128_unsigned(buffer: &mut Vec<u8>, value: u64) {
    leb128::write::unsigned(buffer, value).expect("Vec writes cannot fail");
}

fn encode_case(case: &FuzzCase) -> Vec<u8> {
    let mut encoded = Vec::new();

    match case.target {
        TargetKind::Bytes | TargetKind::String => {
            let Some(first) = case.segments.first() else {
                return case.trailing.clone();
            };
            write_leb128_unsigned(&mut encoded, first.len_prefix);
            encoded.extend_from_slice(&first.bytes);
        }
        TargetKind::VecBytes | TargetKind::VecString => {
            write_leb128_unsigned(&mut encoded, case.container_len);
            for segment in &case.segments {
                write_leb128_unsigned(&mut encoded, segment.len_prefix);
                encoded.extend_from_slice(&segment.bytes);
            }
        }
    }

    encoded.extend_from_slice(&case.trailing);
    encoded
}

fn drive_case(case: FuzzCase) {
    let encoded = encode_case(&case);
    if encoded.len() > MAX_CASE_BYTES {
        return;
    }

    let mut cursor = Cursor::new(encoded);
    match case.target {
        TargetKind::Bytes => {
            let _ = bounded_varbincode_deserialize_for_fuzz::<Vec<u8>, _>(&mut cursor);
        }
        TargetKind::String => {
            let _ = bounded_varbincode_deserialize_for_fuzz::<String, _>(&mut cursor);
        }
        TargetKind::VecBytes => {
            let _ = bounded_varbincode_deserialize_for_fuzz::<Vec<Vec<u8>>, _>(&mut cursor);
        }
        TargetKind::VecString => {
            let _ = bounded_varbincode_deserialize_for_fuzz::<Vec<String>, _>(&mut cursor);
        }
    }
}

fuzz_target!(|case: FuzzCase| {
    drive_case(case);
});
