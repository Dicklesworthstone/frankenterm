//! Bootstrap logging via `env_logger`.
//!
//! Initializes a global logger that respects `RUST_LOG` for level filtering.
//! Safe to call multiple times — only the first call has effect.

use std::sync::OnceLock;

/// Whether `setup_logger` has already initialized logging.
static LOGGER_INITIALIZED: OnceLock<bool> = OnceLock::new();

/// Initialize the global logger from the `RUST_LOG` environment variable.
///
/// Uses `env_logger` for stderr output with level filtering. The first call
/// initializes the logger; subsequent calls are harmless no-ops that return
/// immediately.
///
/// # Panics
///
/// Never panics. If `env_logger::try_init()` fails (because another logger
/// was already installed by a binary crate), the failure is silently ignored
/// since the intent — having a working logger — is already satisfied.
pub fn setup_logger() {
    LOGGER_INITIALIZED.get_or_init(|| {
        // try_init returns Err if a logger is already set — that's fine.
        let _ = env_logger::try_init();
        true
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_logger_is_idempotent() {
        // Must not panic on repeated calls.
        setup_logger();
        setup_logger();
        setup_logger();
        assert_eq!(LOGGER_INITIALIZED.get(), Some(&true));
    }

    #[test]
    fn setup_logger_initializes_on_first_call() {
        // After setup, the OnceLock is populated.
        setup_logger();
        assert!(LOGGER_INITIALIZED.get().is_some());
    }
}
