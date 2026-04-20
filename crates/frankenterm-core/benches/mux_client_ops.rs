//! Criterion benchmarks for `DirectMuxClient` operation latency.
//!
//! Bead: ft-p48pw
#![allow(clippy::large_futures)]

use std::collections::HashMap;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use frankenterm_core::runtime_compat::CompatRuntime;

use codec::{
    CODEC_VERSION, GetCodecVersionResponse, GetPaneRenderChangesResponse, ListPanesResponse, Pdu,
    UnitResponse,
};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::runtime_compat::unix::AsyncWriteExt as CompatAsyncWriteExt;
use frankenterm_core::runtime_compat::{
    Mutex as CompatMutex, Runtime, RuntimeBuilder, io, task, timeout, unix,
};
use frankenterm_core::vendored::{
    DirectMuxClient, DirectMuxClientConfig, SubscriptionConfig, subscribe_pane_output,
};
use mux::renderable::{RenderableDimensions, StableCursorPosition};

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "mux_client_ops/pdu_encode_write",
        budget: "write_to_pane request encode+send path remains low-latency across payload sizes",
    },
    bench_common::BenchBudget {
        name: "mux_client_ops/pdu_read_decode",
        budget: "list_panes response read+decode path stays bounded as payload grows",
    },
    bench_common::BenchBudget {
        name: "mux_client_ops/pdu_roundtrip",
        budget: "send_paste request/response round-trip stays stable and monotonic by payload",
    },
    bench_common::BenchBudget {
        name: "mux_client_ops/subscription_setup",
        budget: "pane subscription setup+cancel remains responsive without poller leaks",
    },
    bench_common::BenchBudget {
        name: "mux_client_ops/render_changes_poll",
        budget: "direct client render-change polling stays low-latency across pane ids",
    },
];

#[derive(Clone, Copy)]
struct MockServerConfig {
    list_window_titles: usize,
    list_title_len: usize,
}

fn compat_runtime() -> Runtime {
    RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("create compat runtime")
}

fn socket_path(prefix: &str) -> PathBuf {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("frankenterm-bench-mux-client-{prefix}-{ts}.sock"))
}

fn build_list_panes_response(config: MockServerConfig) -> ListPanesResponse {
    let mut window_titles = HashMap::with_capacity(config.list_window_titles);
    let title_prefix = "x".repeat(config.list_title_len.max(1));
    for window_id in 0..config.list_window_titles {
        window_titles.insert(window_id + 1, format!("{title_prefix}-{window_id:06}"));
    }
    ListPanesResponse {
        tabs: Vec::new(),
        tab_titles: Vec::new(),
        window_titles,
    }
}

fn approx_list_payload_bytes(config: MockServerConfig) -> usize {
    // Rough proxy used only for benchmark throughput labels.
    // Includes title bytes plus a small per-entry overhead for map framing.
    config.list_window_titles * (config.list_title_len + 16)
}

fn build_render_changes_response(pane_id: usize, seqno: usize) -> GetPaneRenderChangesResponse {
    GetPaneRenderChangesResponse {
        pane_id,
        mouse_grabbed: false,
        alt_screen_active: false,
        cursor_position: StableCursorPosition::default(),
        dimensions: RenderableDimensions {
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
        tiered_scrollback_status: None,
        dirty_lines: Vec::new(),
        title: "bench".to_string(),
        working_dir: None,
        bonus_lines: Vec::new().into(),
        input_serial: None,
        seqno,
    }
}

async fn spawn_mock_mux_server(
    socket_path: PathBuf,
    config: MockServerConfig,
) -> task::JoinHandle<()> {
    let _ = std::fs::remove_file(&socket_path);
    let listener = unix::bind(&socket_path)
        .await
        .expect("bind mock mux socket");

    task::spawn(async move {
        loop {
            let (mut stream, _addr) = match listener.accept().await {
                Ok(conn) => conn,
                Err(_) => break,
            };
            let server_config = config;
            std::mem::drop(task::spawn(async move {
                let mut read_buf = Vec::new();
                let mut seqno = 0_usize;
                loop {
                    let mut temp = vec![0_u8; 4096];
                    let read = match io::read(&mut stream, &mut temp).await {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(_) => break,
                    };
                    read_buf.extend_from_slice(&temp[..read]);

                    while let Ok(Some(decoded)) = codec::Pdu::stream_decode(&mut read_buf) {
                        let response = match decoded.pdu {
                            Pdu::GetCodecVersion(_) => {
                                Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                                    codec_vers: CODEC_VERSION,
                                    version_string: "mux-client-bench".to_string(),
                                    executable_path: PathBuf::from("/bin/wezterm"),
                                    config_file_path: None,
                                })
                            }
                            Pdu::SetClientId(_) => Pdu::UnitResponse(UnitResponse {}),
                            Pdu::ListPanes(_) => {
                                Pdu::ListPanesResponse(build_list_panes_response(server_config))
                            }
                            Pdu::WriteToPane(_) | Pdu::SendPaste(_) => {
                                Pdu::UnitResponse(UnitResponse {})
                            }
                            Pdu::GetPaneRenderChanges(request) => {
                                seqno = seqno.saturating_add(1);
                                Pdu::GetPaneRenderChangesResponse(build_render_changes_response(
                                    request.pane_id,
                                    seqno,
                                ))
                            }
                            _ => Pdu::UnitResponse(UnitResponse {}),
                        };

                        let mut out = Vec::new();
                        response
                            .encode(&mut out, decoded.serial)
                            .expect("encode response pdu");
                        if stream.write_all(&out).await.is_err() {
                            break;
                        }
                    }
                }
            }));
        }
    })
}

async fn connect_client(socket_path: &Path) -> DirectMuxClient {
    DirectMuxClient::connect(DirectMuxClientConfig::default().with_socket_path(socket_path))
        .await
        .expect("connect DirectMuxClient")
}

fn bench_pdu_encode_write(c: &mut Criterion) {
    let compat_rt = compat_runtime();
    let mut group = c.benchmark_group("mux_client_ops/pdu_encode_write");

    for &payload_size in &[64usize, 1024, 64 * 1024] {
        let socket = socket_path("encode-write");
        let _server = compat_rt.block_on(spawn_mock_mux_server(
            socket.clone(),
            MockServerConfig {
                list_window_titles: 1,
                list_title_len: 32,
            },
        ));
        let direct_client = Arc::new(CompatMutex::new(
            compat_rt.block_on(connect_client(&socket)),
        ));
        let payload = vec![b'x'; payload_size];

        group.throughput(Throughput::Bytes(payload_size as u64));
        group.bench_with_input(
            BenchmarkId::new("direct_client_write_to_pane", payload_size),
            &payload_size,
            |b, _| {
                let direct_client = Arc::clone(&direct_client);
                b.iter(|| {
                    let direct_client = Arc::clone(&direct_client);
                    let data = payload.clone();
                    compat_rt.block_on(async move {
                        let mut client = direct_client.lock().await;
                        let response = client
                            .write_to_pane(7, data)
                            .await
                            .expect("write_to_pane response");
                        black_box(response);
                    });
                });
            },
        );
    }

    group.finish();
}

fn bench_pdu_read_decode(c: &mut Criterion) {
    let compat_rt = compat_runtime();
    let mut group = c.benchmark_group("mux_client_ops/pdu_read_decode");

    let cases = [
        (
            "approx_64B",
            MockServerConfig {
                list_window_titles: 1,
                list_title_len: 16,
            },
        ),
        (
            "approx_1KB",
            MockServerConfig {
                list_window_titles: 16,
                list_title_len: 32,
            },
        ),
        (
            "approx_64KB",
            MockServerConfig {
                list_window_titles: 512,
                list_title_len: 112,
            },
        ),
    ];

    for &(label, cfg) in &cases {
        let socket = socket_path("read-decode");
        let _server = compat_rt.block_on(spawn_mock_mux_server(socket.clone(), cfg));
        let direct_client = Arc::new(CompatMutex::new(
            compat_rt.block_on(connect_client(&socket)),
        ));

        group.throughput(Throughput::Bytes(approx_list_payload_bytes(cfg) as u64));
        group.bench_with_input(
            BenchmarkId::new("direct_client_list_panes", label),
            &label,
            |b, _| {
                let direct_client = Arc::clone(&direct_client);
                b.iter(|| {
                    let direct_client = Arc::clone(&direct_client);
                    compat_rt.block_on(async move {
                        let mut client = direct_client.lock().await;
                        let response = client.list_panes().await.expect("list_panes response");
                        black_box(response.window_titles.len());
                    });
                });
            },
        );
    }

    group.finish();
}

fn bench_pdu_roundtrip(c: &mut Criterion) {
    let compat_rt = compat_runtime();
    let mut group = c.benchmark_group("mux_client_ops/pdu_roundtrip");

    for &payload_size in &[64usize, 1024, 64 * 1024] {
        let socket = socket_path("roundtrip");
        let _server = compat_rt.block_on(spawn_mock_mux_server(
            socket.clone(),
            MockServerConfig {
                list_window_titles: 1,
                list_title_len: 32,
            },
        ));
        let direct_client = Arc::new(CompatMutex::new(
            compat_rt.block_on(connect_client(&socket)),
        ));
        let payload = "z".repeat(payload_size);

        group.throughput(Throughput::Bytes(payload_size as u64));
        group.bench_with_input(
            BenchmarkId::new("direct_client_send_paste", payload_size),
            &payload_size,
            |b, _| {
                let direct_client = Arc::clone(&direct_client);
                b.iter(|| {
                    let direct_client = Arc::clone(&direct_client);
                    let text = payload.clone();
                    compat_rt.block_on(async move {
                        let mut client = direct_client.lock().await;
                        let response = client.send_paste(9, text).await.expect("send_paste");
                        black_box(response);
                    });
                });
            },
        );
    }

    group.finish();
}

fn bench_subscription_setup(c: &mut Criterion) {
    let compat_rt = compat_runtime();
    let mut group = c.benchmark_group("mux_client_ops/subscription_setup");
    let socket = socket_path("subscription");
    let _server = compat_rt.block_on(spawn_mock_mux_server(
        socket.clone(),
        MockServerConfig {
            list_window_titles: 1,
            list_title_len: 16,
        },
    ));
    let client_config = DirectMuxClientConfig::default().with_socket_path(socket.clone());
    let sub_config = SubscriptionConfig {
        poll_interval: Duration::from_millis(10),
        min_poll_interval: Duration::from_millis(5),
        channel_capacity: 32,
    };

    group.measurement_time(Duration::from_secs(8));
    group.bench_function("direct_client_subscribe_cancel", |b| {
        b.iter(|| {
            let cfg = client_config.clone();
            let subscription_cfg = sub_config.clone();
            compat_rt.block_on(async {
                let client = DirectMuxClient::connect(cfg)
                    .await
                    .expect("connect for subscription");
                let sub = subscribe_pane_output(client, 42, subscription_cfg);
                timeout(Duration::from_millis(250), sub.shutdown())
                    .await
                    .expect("subscription shutdown timeout");
                black_box(());
            });
        });
    });

    group.finish();
}

fn bench_render_changes_poll(c: &mut Criterion) {
    let compat_rt = compat_runtime();
    let mut group = c.benchmark_group("mux_client_ops/render_changes_poll");
    let socket = socket_path("render-changes");
    let _server = compat_rt.block_on(spawn_mock_mux_server(
        socket.clone(),
        MockServerConfig {
            list_window_titles: 1,
            list_title_len: 16,
        },
    ));
    let direct_client = Arc::new(CompatMutex::new(
        compat_rt.block_on(connect_client(&socket)),
    ));

    group.throughput(Throughput::Elements(1));
    for &pane_id in &[1_u64, 42, 4096] {
        group.bench_with_input(
            BenchmarkId::new("direct_client_get_pane_render_changes", pane_id),
            &pane_id,
            |b, &pane_id| {
                let direct_client = Arc::clone(&direct_client);
                b.iter(|| {
                    let direct_client = Arc::clone(&direct_client);
                    compat_rt.block_on(async move {
                        let mut client = direct_client.lock().await;
                        let response = client
                            .get_pane_render_changes(pane_id)
                            .await
                            .expect("get_pane_render_changes");
                        black_box((response.pane_id, response.seqno));
                    });
                });
            },
        );
    }

    group.finish();
}

fn bench_suite(c: &mut Criterion) {
    bench_pdu_encode_write(c);
    bench_pdu_read_decode(c);
    bench_pdu_roundtrip(c);
    bench_subscription_setup(c);
    bench_render_changes_poll(c);
    bench_common::emit_bench_artifacts("mux_client_ops", BUDGETS);
}

criterion_group!(benches, bench_suite);
criterion_main!(benches);
