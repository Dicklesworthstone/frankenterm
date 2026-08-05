//! Release-profile subprocess probe for FrankenTerm panic contracts.
//!
//! This example is not a unit-test substitute. The release gate builds it once
//! with `release-interactive` and once with the explicitly named
//! `release-abort-probe`, then executes the exact artifacts through
//! `scripts/check-release-panic-contract.sh`.

use frankenterm_sigpipe::{
    RecoverablePanicSite, catch_recoverable, catch_recoverable_future,
};
use std::path::PathBuf;

const SECRET_SENTINEL: &str = "FT_PANIC_SECRET_SENTINEL_DO_NOT_REFLECT";

fn main() {
    frankenterm_sigpipe::exit_quietly_on_broken_pipe();

    let mut args = std::env::args().skip(1);
    let scenario = args.next();
    // Model the shipped ordering without starting a GUI:
    // crash bundle -> GUI notification -> sanitized base hook. The probe hook
    // uses the same shared claim primitive as `notify_on_panic`, so the
    // subprocess contract can prove that successful bundle persistence does
    // not silence the GUI layer or duplicate the base report.
    if matches!(
        scenario.as_deref(),
        Some(
            "gui-uncaught"
                | "gui-uncaught-epipe-spoof"
                | "gui-caught-epipe-spoof"
                | "gui-epipe"
        )
    ) {
        install_probe_gui_hook();
    }

    let crash_dir = std::env::var_os("FT_PANIC_PROBE_CRASH_DIR").map(PathBuf::from);
    // The caught spoof isolates the GUI marker decision. Real GUI EPIPE must
    // retain the full production crash->GUI->base chain so the subprocess
    // proof catches an ordering regression in any installed layer.
    if !matches!(scenario.as_deref(), Some("gui-caught-epipe-spoof")) {
        frankenterm_core::crash::install_panic_hook(&frankenterm_core::crash::CrashConfig {
            crash_dir,
            include_backtrace: false,
        });
    }

    match scenario.as_deref() {
        Some("caught") => run_caught(args.next().as_deref()),
        Some("uncaught") => std::panic::panic_any(String::from(SECRET_SENTINEL)),
        Some("uncaught-epipe-spoof") => std::panic::panic_any(String::from(
            "failed printing to stdout: Broken pipe (os error 32)",
        )),
        Some("gui-uncaught") => std::panic::panic_any(String::from(SECRET_SENTINEL)),
        Some("gui-uncaught-epipe-spoof") => std::panic::panic_any(String::from(
            "failed printing to stdout: Broken pipe (os error 32)",
        )),
        Some("gui-caught-epipe-spoof") => run_caught(Some("epipe-spoof")),
        Some("payload-drop-once") => run_pathological_payload_drop(false),
        Some("payload-drop-twice") => run_pathological_payload_drop(true),
        Some("nested-drop-panic") => run_nested_drop_panic_during_unwind(),
        Some("suite") => run_release_contract_suite(),
        // The checker supplies stdout as the write end of a pipe with no read
        // end. This must be a real std-printing EPIPE, not a forged payload.
        Some("epipe" | "gui-epipe") => println!("panic-contract EPIPE probe"),
        Some("marker") => {
            println!(
                "marker={}",
                frankenterm_sigpipe::panic_recovery_is_executable()
            );
        }
        _ => {
            eprintln!(
                "usage: panic_contract_probe <caught SITE|uncaught|uncaught-epipe-spoof|gui-uncaught|gui-uncaught-epipe-spoof|gui-caught-epipe-spoof|payload-drop-once|payload-drop-twice|nested-drop-panic|epipe|gui-epipe|marker|suite>"
            );
            std::process::exit(2);
        }
    }
}

fn run_release_contract_suite() {
    let interactive_probe = std::env::current_exe().expect("resolve interactive probe path");
    let profile_dir = interactive_probe
        .parent()
        .and_then(std::path::Path::parent)
        .expect("probe path has profile directory");
    assert_eq!(
        profile_dir.file_name().and_then(std::ffi::OsStr::to_str),
        Some("release-interactive"),
        "suite must execute the release-interactive artifact"
    );
    let target_dir = profile_dir.parent().expect("profile directory has target root");
    let abort_probe = target_dir
        .join("release-abort-probe")
        .join("examples")
        .join("panic_contract_probe");
    assert!(
        abort_probe.is_file(),
        "abort-profile probe is missing: {}",
        abort_probe.display()
    );

    let checker = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("scripts/check-release-panic-contract.sh");
    let status = std::process::Command::new("bash")
        .arg(&checker)
        .arg(&interactive_probe)
        .arg(&abort_probe)
        .status()
        .unwrap_or_else(|error| {
            panic!(
                "execute release panic-contract checker at {}: {error}",
                checker.display()
            )
        });
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
}

fn run_pathological_payload_drop(repeat_recovery: bool) {
    use std::io::Write;

    struct PayloadWithPanickingDrop;

    impl Drop for PayloadWithPanickingDrop {
        fn drop(&mut self) {
            panic!("{SECRET_SENTINEL}:payload-drop");
        }
    }

    let before = frankenterm_sigpipe::recovered_panics_total();
    let first = catch_recoverable(
        RecoverablePanicSite::MuxPaneCallback,
        std::panic::AssertUnwindSafe(|| {
            std::panic::panic_any(PayloadWithPanickingDrop);
        }),
    );
    assert!(first.is_err());
    let recovered_delta = frankenterm_sigpipe::recovered_panics_total().saturating_sub(before);
    println!(
        "first-payload-drop-contained marker={} recovered_delta={recovered_delta}",
        frankenterm_sigpipe::is_recoverable_panic()
    );
    let _ = std::io::stdout().lock().flush();

    if repeat_recovery {
        // The first double-panic consumes the one process-wide quarantine.
        // A later recovery attempt must terminate before executing caller code
        // so aggregate opaque-payload retention cannot grow without bound.
        let _ = catch_recoverable(
            RecoverablePanicSite::MuxPaneCallback,
            std::panic::AssertUnwindSafe(|| ()),
        );
        unreachable!("poisoned recovery must fail closed");
    }
}

fn run_nested_drop_panic_during_unwind() {
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    struct PendingFutureWithPanickingDrop;

    impl Future for PendingFutureWithPanickingDrop {
        type Output = ();

        fn poll(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Self::Output> {
            Poll::Pending
        }
    }

    impl Drop for PendingFutureWithPanickingDrop {
        fn drop(&mut self) {
            std::panic::panic_any(String::from(SECRET_SENTINEL));
        }
    }

    let _ = catch_recoverable(
        RecoverablePanicSite::CoreAsyncTaskJoin,
        std::panic::AssertUnwindSafe(|| {
            let _future = catch_recoverable_future(
                RecoverablePanicSite::CoreAsyncTaskJoin,
                PendingFutureWithPanickingDrop,
            );
            std::panic::panic_any(String::from("outer-recoverable-panic"));
        }),
    );
    unreachable!("a destructor panic during unwinding must abort fail-closed");
}

fn install_probe_gui_hook() {
    use std::io::Write;

    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if frankenterm_sigpipe::is_recoverable_panic() {
            return;
        }
        if frankenterm_sigpipe::panic_is_broken_pipe(info) {
            previous_hook(info);
            return;
        }
        let report_claim = frankenterm_sigpipe::FatalReportClaim::enter();
        if report_claim.is_owner() {
            // Mirror the real hook's best-effort, non-panicking notification
            // surface. A probe hook must not manufacture a recursive panic if
            // stderr is already closed.
            let mut stderr = std::io::stderr().lock();
            let _ = stderr.write_all(b"PROBE_GUI_GENERIC_FATAL_REPORT\n");
        }
        previous_hook(info);
    }));
}

fn run_caught(site_name: Option<&str>) {
    let (site, payload) = match site_name {
        Some("mux-pane-callback") => (
            RecoverablePanicSite::MuxPaneCallback,
            SECRET_SENTINEL,
        ),
        Some("mux-subscriber") => (RecoverablePanicSite::MuxSubscriber, SECRET_SENTINEL),
        Some("mux-pane-retirement") => (
            RecoverablePanicSite::MuxPaneRetirement,
            SECRET_SENTINEL,
        ),
        Some("storage-writer") => (RecoverablePanicSite::StorageWriter, SECRET_SENTINEL),
        Some("epipe-spoof") => (
            RecoverablePanicSite::MuxPaneCallback,
            "failed printing to stdout: Broken pipe (os error 32)",
        ),
        _ => {
            eprintln!("unknown finite panic-contract site");
            std::process::exit(2);
        }
    };

    let before = frankenterm_sigpipe::recovered_panics_total();
    let outcome = catch_recoverable(
        site,
        std::panic::AssertUnwindSafe(|| {
            std::panic::panic_any(String::from(payload));
        }),
    );
    match outcome {
        Err(error) => {
            let recovered_delta = frankenterm_sigpipe::recovered_panics_total()
                .saturating_sub(before);
            println!(
                "recovered site={} alive=true recovered_delta={recovered_delta}",
                error.site().as_str()
            );
        }
        Ok(_) => {
            eprintln!("panic-contract probe unexpectedly returned normally");
            std::process::exit(3);
        }
    }
}
