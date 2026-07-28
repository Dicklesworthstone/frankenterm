//! Criterion harness for the ft-6c1t0 opt-in CDC dedup codec.
//!
//! This intentionally benchmarks both sides in one run:
//! - `off_identity_copy_*`: baseline wire path with CDC disabled.
//! - `on_cdc_*`: opt-in [`codec::cdc_dedup`] encoder/decoder path.
//!
//! The ratio line printed during setup is the corpus-level byte ratio that the
//! orchestrator can record alongside throughput.

use std::hint::black_box;

use codec::cdc_dedup::{CdcDedupDecoder, CdcDedupEncoder};
use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};

const FRAME_COUNT: usize = 96;

fn mux_output_corpus() -> Vec<Vec<u8>> {
    let shared_header = b"\x1b[2J\x1b[H[frankenterm] agent output frame\r\n";
    let shared_body = b"\x1b[32mok\x1b[0m compiling crate=frankenterm-core status=running tokens=123456 cwd=/Users/jemanuel/projects/frankenterm\r\n";
    let mirrored_prompt = b"PROMPT> cargo check -p frankenterm-core --all-targets\r\n";

    (0..FRAME_COUNT)
        .map(|idx| {
            let mut frame = Vec::with_capacity(8192);
            frame.extend_from_slice(shared_header);
            for _ in 0..36 {
                frame.extend_from_slice(shared_body);
            }
            frame.extend_from_slice(mirrored_prompt);
            frame.extend_from_slice(format!("pane={:02} seq={:06}\r\n", idx % 8, idx).as_bytes());
            frame
        })
        .collect()
}

fn total_len(frames: &[Vec<u8>]) -> usize {
    frames.iter().map(Vec::len).sum()
}

fn encode_cdc(frames: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let mut encoder = CdcDedupEncoder::new();
    frames
        .iter()
        .map(|frame| encoder.encode(frame))
        .collect::<Vec<_>>()
}

fn assert_cdc_round_trip(frames: &[Vec<u8>], encoded: &[Vec<u8>]) {
    let mut decoder = CdcDedupDecoder::new();
    for (expected, frame) in frames.iter().zip(encoded) {
        let decoded = decoder.decode(frame).expect("cdc decode");
        assert_eq!(&decoded, expected, "CDC bench corpus must be lossless");
    }
}

fn bench_encode(c: &mut Criterion) {
    let frames = mux_output_corpus();
    let input_bytes = total_len(&frames);
    let encoded = encode_cdc(&frames);
    assert_cdc_round_trip(&frames, &encoded);
    let encoded_bytes = total_len(&encoded);

    eprintln!(
        "cdc_dedup bench corpus: input_bytes={input_bytes} encoded_bytes={encoded_bytes} wire_ratio={:.6} dedup_ratio={:.2}x",
        encoded_bytes as f64 / input_bytes as f64,
        input_bytes as f64 / encoded_bytes.max(1) as f64,
    );

    let mut group = c.benchmark_group("cdc_dedup_encode");
    group.throughput(Throughput::Bytes(input_bytes as u64));

    group.bench_function("off_identity_copy_encode", |b| {
        b.iter(|| {
            let copied = frames
                .iter()
                .map(|frame| black_box(frame).clone())
                .collect::<Vec<_>>();
            black_box(copied);
        });
    });

    group.bench_function("on_cdc_encode", |b| {
        b.iter_batched(
            CdcDedupEncoder::new,
            |mut encoder| {
                let encoded = frames
                    .iter()
                    .map(|frame| encoder.encode(black_box(frame)))
                    .collect::<Vec<_>>();
                black_box(encoded);
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

fn bench_decode(c: &mut Criterion) {
    let frames = mux_output_corpus();
    let input_bytes = total_len(&frames);
    let encoded = encode_cdc(&frames);
    assert_cdc_round_trip(&frames, &encoded);

    let mut group = c.benchmark_group("cdc_dedup_decode");
    group.throughput(Throughput::Bytes(input_bytes as u64));

    group.bench_function("off_identity_copy_decode", |b| {
        b.iter(|| {
            let copied = frames
                .iter()
                .map(|frame| black_box(frame).clone())
                .collect::<Vec<_>>();
            black_box(copied);
        });
    });

    group.bench_function("on_cdc_decode", |b| {
        b.iter_batched(
            CdcDedupDecoder::new,
            |mut decoder| {
                let decoded = encoded
                    .iter()
                    .map(|frame| decoder.decode(black_box(frame)).expect("cdc decode"))
                    .collect::<Vec<_>>();
                black_box(decoded);
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

criterion_group!(benches, bench_encode, bench_decode);
criterion_main!(benches);
