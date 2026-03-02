//! Logging setup stub.

/// Initialize the logger. The full WezTerm implementation uses env_logger
/// with a ring buffer. We delegate to the caller's tracing setup.
pub fn setup_logger() {
    // In FrankenTerm, logging is set up by the binary crate (tracing-subscriber).
    // This is a no-op stub so that code calling setup_logger() still compiles.
}
