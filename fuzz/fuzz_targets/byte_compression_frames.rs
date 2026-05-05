#![no_main]
//! Fuzz harness for byte-compression frame decoding.
//!
//! This covers the public scrollback/pane-output compression boundary:
//! `ByteCompressor::decompress` for single frames and
//! `ByteCompressor::decompress_batch` for length-prefixed batches. The target
//! mixes raw adversarial bytes with structured valid frames that can be
//! truncated, extended, or header-corrupted.

use arbitrary::Arbitrary;
use frankenterm_core::byte_compression::{ByteCompressionConfig, ByteCompressor, CompressionLevel};
use libfuzzer_sys::fuzz_target;

const MAX_RAW_BYTES: usize = 64 * 1024;
const MAX_BATCH_RAW_BYTES: usize = 16 * 1024;
const MAX_STRUCTURED_BYTES: usize = 64 * 1024;
const MAX_BATCH_BUFFERS: usize = 8;
const MAX_DICTIONARY_BYTES: usize = 4 * 1024;

#[derive(Arbitrary, Debug)]
enum Input<'a> {
    RawSingle {
        mode: CompressorMode<'a>,
        bytes: &'a [u8],
    },
    RawBatch {
        mode: CompressorMode<'a>,
        bytes: &'a [u8],
    },
    StructuredSingle {
        mode: CompressorMode<'a>,
        payload: &'a [u8],
        mutation: FrameMutation<'a>,
    },
    StructuredBatch {
        mode: CompressorMode<'a>,
        payloads: Vec<&'a [u8]>,
        mutation: FrameMutation<'a>,
    },
}

#[derive(Arbitrary, Debug)]
struct CompressorMode<'a> {
    level_seed: u8,
    include_size_prefix: bool,
    max_input_seed: u8,
    dictionary: &'a [u8],
}

#[derive(Arbitrary, Debug)]
enum FrameMutation<'a> {
    Exact,
    Truncate(u16),
    Append(&'a [u8]),
    FlipByte { offset_seed: u16, xor: u8 },
    OverwriteSizePrefix(u32),
}

fuzz_target!(|input: Input| {
    match input {
        Input::RawSingle { mode, bytes } => {
            let compressor = mode.compressor();
            let bytes = cap_slice(bytes, MAX_RAW_BYTES);
            let _ = compressor.decompress(bytes);
        }
        Input::RawBatch { mode, bytes } => {
            let compressor = mode.compressor();
            let bytes = cap_slice(bytes, MAX_BATCH_RAW_BYTES);
            let _ = compressor.decompress_batch(bytes);
        }
        Input::StructuredSingle {
            mode,
            payload,
            mutation,
        } => {
            let compressor = mode.compressor();
            let payload = cap_slice(payload, MAX_STRUCTURED_BYTES);
            let frame = compressor.compress(payload);
            let exact = matches!(mutation, FrameMutation::Exact);
            let frame = mutation.apply(frame);
            let decoded = compressor.decompress(&frame);

            if exact && payload.len() <= compressor.config().max_input_bytes {
                let decoded = decoded.expect("exact compressed frame must decode");
                assert_eq!(decoded, payload);
            }
        }
        Input::StructuredBatch {
            mode,
            payloads,
            mutation,
        } => {
            let compressor = mode.compressor();
            let capped = payloads
                .into_iter()
                .take(MAX_BATCH_BUFFERS)
                .map(|payload| cap_slice(payload, MAX_STRUCTURED_BYTES).to_vec())
                .collect::<Vec<_>>();
            let refs = capped.iter().map(Vec::as_slice).collect::<Vec<&[u8]>>();
            let (batch, _) = compressor.compress_batch(&refs);
            let exact = matches!(mutation, FrameMutation::Exact);
            let batch = mutation.apply(batch);
            let decoded = compressor.decompress_batch(&batch);

            if exact
                && capped
                    .iter()
                    .all(|payload| payload.len() <= compressor.config().max_input_bytes)
            {
                let decoded = decoded.expect("exact compressed batch must decode");
                assert_eq!(decoded, capped);
            }
        }
    }
});

impl CompressorMode<'_> {
    fn compressor(&self) -> ByteCompressor {
        let config = ByteCompressionConfig {
            level: self.level(),
            max_input_bytes: self.max_input_bytes(),
            include_size_prefix: self.include_size_prefix,
        };
        let compressor = ByteCompressor::with_config(config);
        let dictionary = cap_slice(self.dictionary, MAX_DICTIONARY_BYTES);
        if dictionary.is_empty() {
            compressor
        } else {
            compressor.with_dictionary(dictionary.to_vec())
        }
    }

    fn level(&self) -> CompressionLevel {
        match self.level_seed % 3 {
            0 => CompressionLevel::Fast,
            1 => CompressionLevel::Default,
            _ => CompressionLevel::High,
        }
    }

    fn max_input_bytes(&self) -> usize {
        match self.max_input_seed % 4 {
            0 => 0,
            1 => 1,
            2 => 4096,
            _ => MAX_STRUCTURED_BYTES,
        }
    }
}

impl FrameMutation<'_> {
    fn apply(&self, mut bytes: Vec<u8>) -> Vec<u8> {
        match self {
            Self::Exact => bytes,
            Self::Truncate(seed) => {
                let new_len = if bytes.is_empty() {
                    0
                } else {
                    usize::from(*seed) % (bytes.len() + 1)
                };
                bytes.truncate(new_len);
                bytes
            }
            Self::Append(extra) => {
                bytes.extend_from_slice(cap_slice(extra, 1024));
                bytes
            }
            Self::FlipByte { offset_seed, xor } => {
                if !bytes.is_empty() {
                    let offset = usize::from(*offset_seed) % bytes.len();
                    bytes[offset] ^= *xor;
                }
                bytes
            }
            Self::OverwriteSizePrefix(prefix) => {
                if bytes.len() >= 4 {
                    bytes[..4].copy_from_slice(&prefix.to_le_bytes());
                }
                bytes
            }
        }
    }
}

fn cap_slice(bytes: &[u8], max_len: usize) -> &[u8] {
    &bytes[..bytes.len().min(max_len)]
}
