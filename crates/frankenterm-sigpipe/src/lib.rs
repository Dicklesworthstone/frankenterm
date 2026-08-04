//! Broken-pipe (EPIPE) exit handling shared by every FrankenTerm binary.
//!
//! GH#75: Rust's runtime sets `SIGPIPE` to `SIG_IGN` before `main`, so when a
//! downstream reader closes early (`ft list | head`) the failed stdout write
//! surfaces as an `EPIPE` I/O error instead of terminating the process the
//! way traditional Unix tools do. `println!`/`writeln!` panic on that error,
//! and because the release profile sets `panic = "abort"`, the panic becomes
//! a `SIGABRT` process abort — complete with a macOS crash-report dialog and
//! an `.ips` file — for what is a completely ordinary pipeline event.
//!
//! The workspace forbids `unsafe` code, so the conventional
//! `libc::signal(SIGPIPE, SIG_DFL)` reset is unavailable. Instead every
//! binary installs the panic hook below as early as possible in `main`. Panic
//! hooks run *before* the abort under `panic = "abort"`, so the hook can
//! recognize a broken-pipe write panic and turn it into a quiet
//! `exit(141)` — `128 + SIGPIPE`, the exit status shells report for a
//! process killed by `SIGPIPE`, which is what pipeline tooling expects.
//!
//! All other panics are forwarded unchanged to the previously installed hook,
//! so crash reporting and `RUST_BACKTRACE` behavior for real bugs is
//! unaffected.

/// Exit status conventionally reported for a process terminated by
/// `SIGPIPE`: `128 + 13`.
pub const SIGPIPE_EXIT_CODE: i32 = 141;

/// Install a panic hook that converts broken-pipe stdout/stderr write panics
/// into a quiet `exit(141)`.
///
/// Call this as early as possible in `main`, and *after* any other
/// `std::panic::set_hook` installation (e.g. crash notifiers) so that this
/// hook wraps — and delegates non-broken-pipe panics to — the previous hook.
pub fn exit_quietly_on_broken_pipe() {
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if payload_is_broken_pipe(info.payload()) {
            // Flush nothing, print nothing: the reader is gone. 141 is the
            // status `head`-style pipelines conventionally observe.
            std::process::exit(SIGPIPE_EXIT_CODE);
        }
        previous_hook(info);
    }));
}

/// Classify a panic payload as a broken-pipe write failure.
///
/// The std I/O macros panic with a formatted `String` such as
/// `"failed printing to stdout: Broken pipe (os error 32)"`; explicit
/// `expect`/`unwrap` sites on write results can panic with either a `String`
/// or a `&'static str`. Both payload shapes are checked.
#[must_use]
pub fn payload_is_broken_pipe(payload: &dyn std::any::Any) -> bool {
    let message = if let Some(s) = payload.downcast_ref::<&str>() {
        *s
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.as_str()
    } else {
        return false;
    };
    message.contains("Broken pipe") || message.contains("os error 32")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_std_stdout_broken_pipe_string() {
        let payload: Box<dyn std::any::Any> =
            Box::new("failed printing to stdout: Broken pipe (os error 32)".to_string());
        assert!(payload_is_broken_pipe(payload.as_ref()));
    }

    #[test]
    fn classifies_static_str_payload() {
        let payload: Box<dyn std::any::Any> = Box::new("write failed: Broken pipe");
        assert!(payload_is_broken_pipe(payload.as_ref()));
    }

    #[test]
    fn ignores_unrelated_panic_messages() {
        let payload: Box<dyn std::any::Any> =
            Box::new("index out of bounds: the len is 3 but the index is 7".to_string());
        assert!(!payload_is_broken_pipe(payload.as_ref()));
    }

    #[test]
    fn ignores_non_string_payloads() {
        let payload: Box<dyn std::any::Any> = Box::new(42_u64);
        assert!(!payload_is_broken_pipe(payload.as_ref()));
    }
}
