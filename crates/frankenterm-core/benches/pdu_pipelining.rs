//! Benchmarks for vendored mux PDU pipelining.
//!
//! Required scenarios:
//! - pipeline_vs_sequential_throughput
//! - pipeline_batch_latency
//! - pipeline_depth_saturation
//! - sequence_number_matching

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use codec::{
    CODEC_VERSION, GetCodecVersionResponse, GetPaneRenderChangesResponse, Pdu, UnitResponse,
};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::vendored::{DirectMuxClient, DirectMuxClientConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "pipeline_vs_sequential_throughput",
        budget: "p50 pipelined throughput > 2x sequential for 50 requests",
    },
    bench_common::BenchBudget {
        name: "pipeline_batch_latency",
        budget: "p50 50-request pipelined batch latency < 20ms on local unix socket",
    },
    bench_common::BenchBudget {
        name: "pipeline_depth_saturation",
        budget: "throughput saturates between depth 16-64 depending on host",
    },
    bench_common::BenchBudget {
        name: "sequence_number_matching",
        budget: "dispatch overhead < 1us/response (VecDeque serial matching)",
    },
];

fn make_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

fn render_changes_response(pane_id: usize) -> GetPaneRenderChangesResponse {
    GetPaneRenderChangesResponse {
        pane_id,
        mouse_grabbed: false,
        cursor_position: mux::renderable::StableCursorPosition::default(),
        dimensions: mux::renderable::RenderableDimensions {
            cols: 80,
            viewport_rows: 24,
            scrollback_rows: 0,
            physical_top: 0,
            scrollback_top: 0,
            dpi: 96,
            pixel_width: 0,
            pixel_height: 0,
            reverse_video: false,
        },
        dirty_lines: Vec::new(),
        title: format!("pane-{pane_id}"),
        working_dir: None,
        bonus_lines: Vec::new().into(),
        input_serial: None,
        seqno: pane_id,
    }
}

async fn setup_client(
    response_batch_delay: Duration,
    reverse_batch_responses: bool,
) -> (DirectMuxClient, tempfile::TempDir) {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let socket_path = temp_dir.path().join("pdu-pipeline-bench.sock");
    let listener = tokio::net::UnixListener::bind(&socket_path).expect("bind");

    tokio::spawn(async move {
        let (mut stream, _) = match listener.accept().await {
            Ok(v) => v,
            Err(_) => return,
        };
        let mut read_buf = Vec::new();
        loop {
            let mut temp = vec![0u8; 8192];
            let read = match stream.read(&mut temp).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            read_buf.extend_from_slice(&temp[..read]);

            let mut control_responses: Vec<(u64, Pdu)> = Vec::new();
            let mut render_responses: Vec<(u64, usize)> = Vec::new();

            while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                match decoded.pdu {
                    Pdu::GetCodecVersion(_) => {
                        control_responses.push((
                            decoded.serial,
                            Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                codec_vers: CODEC_VERSION,
                                version_string: "pdu-pipeline-bench".to_string(),
                                executable_path: PathBuf::from("/bin/wezterm"),
                                config_file_path: None,
                            }),
                        ));
                    }
                    Pdu::SetClientId(_) => {
                        control_responses
                            .push((decoded.serial, Pdu::UnitResponse(UnitResponse {})));
                    }
                    Pdu::GetPaneRenderChanges(req) => {
                        render_responses.push((decoded.serial, req.pane_id));
                    }
                    _ => {}
                }
            }

            for (serial, pdu) in control_responses {
                let mut out = Vec::new();
                pdu.encode(&mut out, serial)
                    .expect("encode control response");
                if stream.write_all(&out).await.is_err() {
                    return;
                }
            }

            if !render_responses.is_empty() {
                tokio::time::sleep(response_batch_delay).await;
                if reverse_batch_responses {
                    render_responses.reverse();
                }
                for (serial, pane_id) in render_responses {
                    let response =
                        Pdu::GetPaneRenderChangesResponse(render_changes_response(pane_id));
                    let mut out = Vec::new();
                    response
                        .encode(&mut out, serial)
                        .expect("encode render response");
                    if stream.write_all(&out).await.is_err() {
                        return;
                    }
                }
            }
        }
    });

    let config = DirectMuxClientConfig::default().with_socket_path(socket_path);
    let client = DirectMuxClient::connect(config)
        .await
        .expect("connect client");
    (client, temp_dir)
}

fn bench_pipeline_vs_sequential_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("pdu_pipelining/pipeline_vs_sequential_throughput");
    group.throughput(Throughput::Elements(50));
    let rt = make_runtime();
    let pane_ids: Arc<Vec<u64>> = Arc::new((0u64..50).collect());

    let (sequential_client, _sequential_temp_dir) =
        rt.block_on(setup_client(Duration::from_millis(1), false));
    let sequential_client = Arc::new(tokio::sync::Mutex::new(sequential_client));
    group.bench_function("sequential_50", |b| {
        let pane_ids = Arc::clone(&pane_ids);
        let sequential_client = Arc::clone(&sequential_client);
        b.to_async(&rt).iter(|| {
            let pane_ids = Arc::clone(&pane_ids);
            let sequential_client = Arc::clone(&sequential_client);
            async move {
                let mut client = sequential_client.lock().await;
                for pane_id in pane_ids.iter() {
                    client
                        .get_pane_render_changes(*pane_id)
                        .await
                        .expect("sequential request");
                }
            }
        });
    });

    let (pipelined_client, _pipelined_temp_dir) =
        rt.block_on(setup_client(Duration::from_millis(1), true));
    let pipelined_client = Arc::new(tokio::sync::Mutex::new(pipelined_client));
    group.bench_function("pipelined_50_depth32", |b| {
        let pane_ids = Arc::clone(&pane_ids);
        let pipelined_client = Arc::clone(&pipelined_client);
        b.to_async(&rt).iter(|| {
            let pane_ids = Arc::clone(&pane_ids);
            let pipelined_client = Arc::clone(&pipelined_client);
            async move {
                let mut client = pipelined_client.lock().await;
                client
                    .get_pane_render_changes_batch(&pane_ids, 32, Duration::from_secs(5))
                    .await
                    .expect("pipelined batch");
            }
        });
    });

    group.finish();
}

fn bench_pipeline_batch_latency(c: &mut Criterion) {
    let mut group = c.benchmark_group("pdu_pipelining/pipeline_batch_latency");
    let rt = make_runtime();

    for &batch_size in &[10usize, 25, 50] {
        group.throughput(Throughput::Elements(batch_size as u64));
        let pane_ids: Arc<Vec<u64>> = Arc::new((0..batch_size as u64).collect());
        let (client, _temp_dir) = rt.block_on(setup_client(Duration::from_millis(1), true));
        let client = Arc::new(tokio::sync::Mutex::new(client));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("batch_{batch_size}")),
            &batch_size,
            |b, _| {
                let pane_ids = Arc::clone(&pane_ids);
                let client = Arc::clone(&client);
                b.to_async(&rt).iter(|| {
                    let pane_ids = Arc::clone(&pane_ids);
                    let client = Arc::clone(&client);
                    async move {
                        let mut client = client.lock().await;
                        client
                            .get_pane_render_changes_batch(&pane_ids, 32, Duration::from_secs(5))
                            .await
                            .expect("pipelined batch");
                    }
                });
            },
        );
    }

    group.finish();
}

fn bench_pipeline_depth_saturation(c: &mut Criterion) {
    let mut group = c.benchmark_group("pdu_pipelining/pipeline_depth_saturation");
    let rt = make_runtime();
    let pane_ids: Arc<Vec<u64>> = Arc::new((0u64..50).collect());

    for &depth in &[1usize, 2, 4, 8, 16, 32, 64] {
        group.throughput(Throughput::Elements(50));
        let (client, _temp_dir) = rt.block_on(setup_client(Duration::from_millis(1), true));
        let client = Arc::new(tokio::sync::Mutex::new(client));
        group.bench_with_input(BenchmarkId::from_parameter(depth), &depth, |b, &depth| {
            let pane_ids = Arc::clone(&pane_ids);
            let client = Arc::clone(&client);
            b.to_async(&rt).iter(|| {
                let pane_ids = Arc::clone(&pane_ids);
                let client = Arc::clone(&client);
                async move {
                    let mut client = client.lock().await;
                    client
                        .get_pane_render_changes_batch(&pane_ids, depth, Duration::from_secs(5))
                        .await
                        .expect("batch");
                }
            });
        });
    }

    group.finish();
}

fn take_in_flight_slot(in_flight: &mut VecDeque<(usize, u64)>, serial: u64) -> Option<usize> {
    let pos = in_flight
        .iter()
        .position(|(_, expected)| *expected == serial)?;
    in_flight.remove(pos).map(|(idx, _)| idx)
}

fn bench_sequence_number_matching(c: &mut Criterion) {
    let mut group = c.benchmark_group("pdu_pipelining/sequence_number_matching");

    for &depth in &[8usize, 16, 32, 64] {
        group.throughput(Throughput::Elements(depth as u64));
        group.bench_with_input(BenchmarkId::from_parameter(depth), &depth, |b, &depth| {
            b.iter(|| {
                let mut pending: VecDeque<(usize, u64)> =
                    (0..depth).map(|idx| (idx, (idx + 1) as u64)).collect();
                for serial in (1..=depth as u64).rev() {
                    let idx = take_in_flight_slot(&mut pending, serial).expect("serial must match");
                    std::hint::black_box(idx);
                }
            });
        });
    }

    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("pdu_pipelining", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_pipeline_vs_sequential_throughput,
        bench_pipeline_batch_latency,
        bench_pipeline_depth_saturation,
        bench_sequence_number_matching
);
criterion_main!(benches);
