#![no_main]
//! MCP request params parser fuzz target.
//!
//! Feeds arbitrary request bytes through the MCP tool-call decode seam:
//!
//! ```text
//! raw bytes (bounded to 256 KiB)
//!   -> serde_json::from_slice::<Value>
//!   -> extract tool name + arguments
//!   -> serde_json::from_value::<ToolSpecificParams>
//! ```
//!
//! The oracle is:
//! - no panic / stack overflow / OOM on malformed or adversarial bytes
//! - deterministic status token for the same input bytes

use frankenterm_core::mcp::__fuzz_parse_tool_call_request;
use libfuzzer_sys::fuzz_target;

const MAX_INPUT_BYTES: usize = 256 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }

    let first = __fuzz_parse_tool_call_request(data);
    let second = __fuzz_parse_tool_call_request(data);
    assert_eq!(
        first, second,
        "MCP request parse seam must be deterministic"
    );
});
