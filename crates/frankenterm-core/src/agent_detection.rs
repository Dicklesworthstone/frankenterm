//! Feature-gated adapter for filesystem-based coding agent detection.
//!
//! This re-exports `franken-agent-detection` through `frankenterm-core` so
//! internal callers can use a stable module path as higher-level inventory
//! integration evolves.

pub use franken_agent_detection::*;
