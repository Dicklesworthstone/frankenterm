#![no_main]
//! Fuzz harness for the public `FtviIndex::from_bytes` vector-index loader.
//!
//! Coverage has two lanes:
//! - raw arbitrary bytes for crash/no-panic parser robustness
//! - structured valid FTVI buffers from `write_ftvi_vec`, optionally corrupted
//!   at the wire level so header and loop invariants get exercised too

use arbitrary::{Arbitrary, Unstructured};
use frankenterm_core::search::{FtviIndex, write_ftvi_vec};
use libfuzzer_sys::fuzz_target;

const MAX_RAW_BYTES: usize = 256 * 1024;
const MAX_DIMENSION: usize = 32;
const MAX_RECORDS: usize = 32;
const MAX_GARBAGE_BYTES: usize = 128;

#[derive(Arbitrary, Debug)]
enum Input {
    Raw(Vec<u8>),
    Structured(StructuredInput),
}

#[derive(Arbitrary, Debug)]
struct StructuredInput {
    dimension_seed: u8,
    records: Vec<Record>,
    query: Vec<f32>,
    k: u8,
    wire_mode: WireMode,
}

#[derive(Arbitrary, Debug)]
struct Record {
    id: u64,
    values: Vec<f32>,
}

#[derive(Arbitrary, Debug)]
enum WireMode {
    Exact,
    Truncate(u16),
    AppendGarbage(Vec<u8>),
    BadMagic([u8; 4]),
    BadVersion(u16),
    InflatedCount(u8),
    InflatedDimension(u8),
}

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_RAW_BYTES {
        return;
    }

    let mut input = Unstructured::new(data);
    let input = match Input::arbitrary(&mut input) {
        Ok(input) => input,
        Err(_) => return,
    };

    match input {
        Input::Raw(bytes) => {
            let raw = cap_bytes(bytes, MAX_RAW_BYTES);
            let _ = FtviIndex::from_bytes(&raw);
        }
        Input::Structured(input) => {
            let Some((bytes, expected)) = input.to_bytes() else {
                return;
            };

            let parsed = FtviIndex::from_bytes(&bytes);
            if let Some(expected) = expected {
                let index = parsed.expect("exact structured FTVI bytes must parse");
                assert_eq!(index.dimension(), expected.dimension);
                assert_eq!(index.len(), expected.record_count);
                assert_eq!(index.is_empty(), expected.record_count == 0);

                if expected.dimension != 0 && expected.record_count != 0 {
                    let results = index.search(&expected.query, expected.k);
                    assert!(results.len() <= expected.k);
                    assert!(results.len() <= expected.record_count);
                }
            }
        }
    }
});

struct ExpectedIndex {
    dimension: usize,
    record_count: usize,
    query: Vec<f32>,
    k: usize,
}

impl StructuredInput {
    fn to_bytes(self) -> Option<(Vec<u8>, Option<ExpectedIndex>)> {
        let dimension = normalized_dimension(self.dimension_seed, &self.records);
        let normalized_records = self
            .records
            .into_iter()
            .take(MAX_RECORDS)
            .map(|record| normalize_record(record, dimension))
            .collect::<Vec<_>>();

        let record_refs = normalized_records
            .iter()
            .map(|record| (record.id, record.values.as_slice()))
            .collect::<Vec<_>>();

        let mut bytes = write_ftvi_vec(u16::try_from(dimension).ok()?, &record_refs).ok()?;
        let query = normalize_query(self.query, dimension);
        let k = usize::from(self.k).max(1);

        let expected = matches!(self.wire_mode, WireMode::Exact).then_some(ExpectedIndex {
            dimension,
            record_count: normalized_records.len(),
            query,
            k,
        });

        apply_wire_mode(&mut bytes, &self.wire_mode);
        Some((bytes, expected))
    }
}

fn normalized_dimension(seed: u8, records: &[Record]) -> usize {
    let seeded = usize::from(seed) % (MAX_DIMENSION + 1);
    let inferred = records
        .iter()
        .find_map(|record| (!record.values.is_empty()).then_some(record.values.len()))
        .unwrap_or(0);
    seeded.max(inferred.min(MAX_DIMENSION))
}

fn normalize_record(record: Record, dimension: usize) -> Record {
    let mut values = record.values.into_iter().take(dimension).collect::<Vec<_>>();
    values.resize(dimension, 0.0);
    Record {
        id: record.id,
        values,
    }
}

fn normalize_query(query: Vec<f32>, dimension: usize) -> Vec<f32> {
    let mut query = query.into_iter().take(dimension).collect::<Vec<_>>();
    query.resize(dimension, 0.0);
    query
}

fn cap_bytes(mut bytes: Vec<u8>, max_len: usize) -> Vec<u8> {
    if bytes.len() > max_len {
        bytes.truncate(max_len);
    }
    bytes
}

fn apply_wire_mode(bytes: &mut Vec<u8>, wire_mode: &WireMode) {
    match wire_mode {
        WireMode::Exact => {}
        WireMode::Truncate(seed) => {
            let keep = usize::from(*seed) % (bytes.len().saturating_add(1));
            bytes.truncate(keep);
        }
        WireMode::AppendGarbage(garbage) => {
            let garbage = garbage
                .iter()
                .copied()
                .take(MAX_GARBAGE_BYTES)
                .collect::<Vec<_>>();
            bytes.extend_from_slice(&garbage);
        }
        WireMode::BadMagic(magic) => {
            if bytes.len() >= 4 {
                bytes[..4].copy_from_slice(magic);
            }
        }
        WireMode::BadVersion(version) => {
            if bytes.len() >= 6 {
                bytes[4..6].copy_from_slice(&version.to_le_bytes());
            }
        }
        WireMode::InflatedCount(extra) => {
            if bytes.len() >= 12 {
                let mut count_bytes = [0u8; 4];
                count_bytes.copy_from_slice(&bytes[8..12]);
                let count = u32::from_le_bytes(count_bytes);
                let inflated = count.saturating_add(u32::from(*extra).saturating_add(1));
                bytes[8..12].copy_from_slice(&inflated.to_le_bytes());
            }
        }
        WireMode::InflatedDimension(seed) => {
            if bytes.len() >= 8 {
                let dimension = u16::from((*seed).max(1)) % ((MAX_DIMENSION as u16) + 1);
                bytes[6..8].copy_from_slice(&dimension.to_le_bytes());
            }
        }
    }
}
