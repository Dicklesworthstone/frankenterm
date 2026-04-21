//! Criterion harness for MCP response serialization hot paths.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use serde_json::{Value, json};

fn sample_tool(index: usize) -> Value {
    json!({
        "name": format!("tool_{index}"),
        "description": format!("Bench tool {index} used for MCP manifest-style response serialization"),
        "inputSchema": {
            "type": "object",
            "properties": {
                "pane_id": { "type": "integer" },
                "format": { "type": "string", "enum": ["json", "toon"] },
                "tail": { "type": "integer", "minimum": 0 }
            },
            "required": ["pane_id"],
            "additionalProperties": false
        },
        "annotations": {
            "title": format!("Tool {index}"),
            "destructiveHint": index % 5 == 0,
            "readOnlyHint": index % 2 == 0,
            "idempotentHint": true,
            "openWorldHint": false
        }
    })
}

fn sample_resource(index: usize) -> Value {
    json!({
        "uri": format!("ft://pane/{index}"),
        "name": format!("pane_resource_{index}"),
        "description": format!("Pane {index} resource"),
        "mimeType": "application/json"
    })
}

fn sample_prompt(index: usize) -> Value {
    json!({
        "name": format!("prompt_{index}"),
        "description": format!("Prompt template {index}"),
        "arguments": [
            {
                "name": "pane_id",
                "description": "Pane to inspect",
                "required": true
            },
            {
                "name": "tail",
                "description": "Tail lines to request",
                "required": false
            }
        ]
    })
}

fn sample_mcp_response(size: usize) -> Value {
    json!({
        "ok": true,
        "data": {
            "tools": (0..size).map(sample_tool).collect::<Vec<_>>(),
            "resources": (0..size).map(sample_resource).collect::<Vec<_>>(),
            "prompts": (0..size).map(sample_prompt).collect::<Vec<_>>(),
            "total": size * 3,
            "server": {
                "name": "frankenterm",
                "capabilities": ["tools", "resources", "prompts"]
            }
        },
        "elapsed_ms": 7,
        "version": "bench",
        "now": 1_710_000_000_000u64,
        "mcp_version": "2025-03-26"
    })
}

fn json_text_to_toon(text: &str) -> String {
    let value: Value = serde_json::from_str(text).expect("benchmark fixture must stay valid json");
    toon_rust::encode(value, None)
}

fn bench_mcp_response(c: &mut Criterion) {
    let mut group = c.benchmark_group("mcp_response");
    group.sample_size(20);

    for size in [1usize, 8, 32] {
        let response = sample_mcp_response(size);
        let compact_json =
            serde_json::to_string(&response).expect("benchmark fixture must serialize");

        group.bench_with_input(BenchmarkId::new("json_compact", size), &size, |b, _| {
            b.iter(|| serde_json::to_string(black_box(&response)).expect("serialize json"));
        });

        group.bench_with_input(BenchmarkId::new("json_pretty", size), &size, |b, _| {
            b.iter(|| serde_json::to_string_pretty(black_box(&response)).expect("serialize json"));
        });

        group.bench_with_input(
            BenchmarkId::new("json_text_to_toon", size),
            &size,
            |b, _| {
                b.iter(|| json_text_to_toon(black_box(&compact_json)));
            },
        );
    }

    group.finish();
}

criterion_group!(benches, bench_mcp_response);
criterion_main!(benches);
