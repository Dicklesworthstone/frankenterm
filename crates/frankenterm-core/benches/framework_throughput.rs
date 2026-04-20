//! Benchmarks current FastAPI/FastMCP seam throughput baselines for ft-181uk.
//!
//! This slice records the incumbent framework baselines through the centralized
//! seam modules so later asupersync-native replacements can compare against the
//! same scenarios without leaking direct `fastapi`/`fastmcp` imports back into
//! the codebase. Criterion's report output carries latency percentiles for each
//! benchmark group, covering the p50/p95/p99 evidence requested by the bead.

#![cfg(all(feature = "mcp", feature = "web"))]

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::mcp_framework::{
    FrameworkContent, FrameworkMcpContext, FrameworkMcpResult, FrameworkServer,
    FrameworkTestClient as FrameworkMcpTestClient, FrameworkTool, FrameworkToolHandler,
    framework_create_memory_transport_pair,
};
use frankenterm_core::web_framework::{
    FrameworkApp, FrameworkMethod, FrameworkRequest, FrameworkRequestBody, FrameworkRequestContext,
    FrameworkResponse, FrameworkStatusCode, FrameworkWebTestClient, json_response_with_status,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::hint::black_box;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "http_get_health_fastapi_baseline",
        budget: "Criterion report must capture throughput plus p50/p95/p99 for GET /health baseline",
    },
    bench_common::BenchBudget {
        name: "http_post_json_fastapi_baseline",
        budget: "Criterion report must capture throughput plus p50/p95/p99 for JSON POST body-parse baseline",
    },
    bench_common::BenchBudget {
        name: "mcp_tool_call_fastmcp_baseline",
        budget: "Criterion report must capture throughput plus p50/p95/p99 for single-client MCP tool baseline",
    },
    bench_common::BenchBudget {
        name: "mcp_tool_call_concurrent_fastmcp",
        budget: "Criterion report must capture aggregate throughput for N=2,4,8 parallel in-memory MCP session pairs",
    },
];

#[derive(Clone, Debug, Deserialize, Serialize)]
struct BenchJsonPayload {
    message: String,
    ordinal: u64,
}

fn health_handler(
    _ctx: &FrameworkRequestContext,
    _req: &mut FrameworkRequest,
) -> std::future::Ready<FrameworkResponse> {
    std::future::ready(json_response_with_status(
        FrameworkStatusCode::OK,
        &json!({"status": "ok"}),
    ))
}

fn echo_handler(
    _ctx: &FrameworkRequestContext,
    req: &mut FrameworkRequest,
) -> std::future::Ready<FrameworkResponse> {
    let response = match req.take_body() {
        FrameworkRequestBody::Bytes(body) => {
            match serde_json::from_slice::<BenchJsonPayload>(&body) {
                Ok(payload) => json_response_with_status(FrameworkStatusCode::OK, &payload),
                Err(_) => FrameworkResponse::with_status(FrameworkStatusCode::BAD_REQUEST),
            }
        }
        _ => FrameworkResponse::with_status(FrameworkStatusCode::BAD_REQUEST),
    };
    std::future::ready(response)
}

fn build_http_bench_app() -> FrameworkApp {
    FrameworkApp::builder()
        .route("/health", FrameworkMethod::Get, health_handler)
        .route("/echo", FrameworkMethod::Post, echo_handler)
        .build()
}

fn bench_http_get_health(c: &mut Criterion) {
    let client = FrameworkWebTestClient::new(build_http_bench_app());
    let response = client.get("/health").send();
    assert_eq!(
        response.status_code(),
        200,
        "health smoke check should pass"
    );

    let mut group = c.benchmark_group("framework_throughput/http_get_health");
    group.throughput(Throughput::Elements(1));
    group.bench_function("fastapi_baseline", |b| {
        b.iter(|| {
            let response = client.get("/health").send();
            black_box(response.status_code())
        });
    });
    group.finish();
}

fn bench_http_post_json(c: &mut Criterion) {
    let client = FrameworkWebTestClient::new(build_http_bench_app());
    let request_body = serde_json::to_vec(&BenchJsonPayload {
        message: "framework-baseline".to_string(),
        ordinal: 7,
    })
    .expect("request body should serialize");

    let response = client
        .post("/echo")
        .header_str("content-type", "application/json")
        .body(request_body.clone())
        .send();
    assert_eq!(response.status_code(), 200, "echo smoke check should pass");

    let mut group = c.benchmark_group("framework_throughput/http_post_json");
    group.throughput(Throughput::Elements(1));
    group.bench_function("fastapi_baseline", |b| {
        let request_body = request_body.clone();
        b.iter(|| {
            let response = client
                .post("/echo")
                .header_str("content-type", "application/json")
                .body(request_body.clone())
                .send();
            black_box(response.status_code())
        });
    });
    group.finish();
}

struct BenchEchoTool;

impl FrameworkToolHandler for BenchEchoTool {
    fn definition(&self) -> FrameworkTool {
        FrameworkTool {
            name: "echo".to_string(),
            description: Some("Echo benchmark payload".to_string()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "message": { "type": "string" }
                },
                "required": ["message"]
            }),
            output_schema: None,
            icon: None,
            version: None,
            tags: vec![],
            annotations: None,
        }
    }

    fn call(
        &self,
        _ctx: &FrameworkMcpContext,
        arguments: serde_json::Value,
    ) -> FrameworkMcpResult<Vec<FrameworkContent>> {
        let message = arguments
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        Ok(vec![FrameworkContent::Text { text: message }])
    }
}

struct McpBenchHarness {
    client: FrameworkMcpTestClient,
    server_join: Option<JoinHandle<()>>,
}

impl McpBenchHarness {
    fn new(client_name: &str) -> Self {
        let (client_transport, server_transport) = framework_create_memory_transport_pair();
        let server = FrameworkServer::new("framework-throughput-bench", "1.0.0")
            .tool(BenchEchoTool)
            .build();
        let server_join = thread::spawn(move || {
            server.run_transport_returning(server_transport);
        });

        let mut client =
            FrameworkMcpTestClient::new(client_transport).with_client_info(client_name, "1.0.0");
        client
            .initialize()
            .expect("benchmark client should initialize");

        Self {
            client,
            server_join: Some(server_join),
        }
    }

    fn call_echo(&mut self, message: &str) -> usize {
        let reply = self
            .client
            .call_tool("echo", json!({"message": message}))
            .expect("echo tool should succeed");
        match reply.first() {
            Some(FrameworkContent::Text { text }) => text.len(),
            other => panic!("unexpected echo response: {other:?}"),
        }
    }
}

impl Drop for McpBenchHarness {
    fn drop(&mut self) {
        self.client.close();
        if let Some(join) = self.server_join.take() {
            let _ = join.join();
        }
    }
}

fn bench_mcp_tool_call(c: &mut Criterion) {
    let mut harness = McpBenchHarness::new("framework-bench-single");
    assert_eq!(
        harness.call_echo("smoke-check"),
        "smoke-check".len(),
        "single-client MCP smoke check should pass"
    );

    let mut group = c.benchmark_group("framework_throughput/mcp_tool_call");
    group.throughput(Throughput::Elements(1));
    group.bench_function("fastmcp_baseline", |b| {
        b.iter(|| black_box(harness.call_echo("framework-throughput")));
    });
    group.finish();
}

fn bench_mcp_concurrent_tool_calls(c: &mut Criterion) {
    let mut group = c.benchmark_group("framework_throughput/mcp_concurrent_tool_calls");

    // MemoryTransport is one client/server pair per session, so the current
    // concurrent baseline uses N parallel in-memory session pairs.
    for concurrency in [2usize, 4, 8] {
        let harnesses: Vec<_> = (0..concurrency)
            .map(|idx| {
                Arc::new(Mutex::new(McpBenchHarness::new(&format!(
                    "framework-bench-{concurrency}-{idx}"
                ))))
            })
            .collect();

        group.throughput(Throughput::Elements(concurrency as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(concurrency),
            &concurrency,
            |b, _| {
                let harnesses = harnesses.clone();
                b.iter(|| {
                    thread::scope(|scope| {
                        for (idx, harness) in harnesses.iter().enumerate() {
                            let harness = Arc::clone(harness);
                            scope.spawn(move || {
                                let mut harness = harness.lock().expect("lock harness");
                                black_box(harness.call_echo(&format!("parallel-{idx}")));
                            });
                        }
                    });
                });
            },
        );
    }

    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("framework_throughput", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets =
        bench_http_get_health,
        bench_http_post_json,
        bench_mcp_tool_call,
        bench_mcp_concurrent_tool_calls
);
criterion_main!(benches);
