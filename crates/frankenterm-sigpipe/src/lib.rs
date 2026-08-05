//! Process panic policy shared by every FrankenTerm binary.
//!
//! GH#75: Rust's runtime sets `SIGPIPE` to `SIG_IGN` before `main`, so when a
//! downstream reader closes early (`ft list | head`) the failed stdout write
//! surfaces as an `EPIPE` I/O error instead of terminating the process the
//! way traditional Unix tools do. `println!`/`writeln!` panic on that error,
//! which must become a quiet shell-compatible exit rather than a crash report.
//!
//! The workspace forbids `unsafe` code, so the conventional
//! `libc::signal(SIGPIPE, SIG_DFL)` reset is unavailable. Instead every
//! binary installs the panic hook below as early as possible in `main`. The
//! hook recognizes a broken-pipe write panic and turns it into a quiet
//! `exit(141)` — `128 + SIGPIPE`, the exit status shells report for a
//! process killed by `SIGPIPE`, which is what pipeline tooling expects.
//!
//! This crate also owns FrankenTerm's recoverable-panic boundary. Rust invokes
//! panic hooks *before* [`std::panic::catch_unwind`] returns, so a hook cannot
//! infer from the stack that a panic will be contained. [`catch_recoverable`]
//! places one nested-safe, thread-local RAII marker around explicitly audited
//! recovery boundaries. Project hooks consult that marker and suppress fatal
//! reporting while the unwind is in flight. The marker is deliberately inert
//! when the artifact was compiled with `panic = "abort"`: an aborting process
//! must never hide a panic under the false assumption that recovery can run.

#[cfg(panic = "unwind")]
use std::cell::Cell;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::panic::{AssertUnwindSafe, UnwindSafe};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll};

#[cfg(panic = "unwind")]
thread_local! {
    static RECOVERABLE_PANIC_DEPTH: Cell<usize> = const { Cell::new(0) };
}

static RECOVERED_PANICS_TOTAL: AtomicU64 = AtomicU64::new(0);
// Safe Rust cannot recursively destroy an arbitrary panic payload whose Drop
// implementation manufactures another panicking payload. Permit one opaque
// secondary payload object to be quarantined process-wide; after that sticky
// poison, every later recovery attempt exits fail-closed so aggregate leaked
// object count cannot grow beyond one.
static PAYLOAD_DISPOSAL_POISONED: AtomicBool = AtomicBool::new(false);
static PAYLOAD_DISPOSAL_FATAL_REPORTED: AtomicBool = AtomicBool::new(false);
static FATAL_REPORT_PROCESS_CLAIMED: AtomicBool = AtomicBool::new(false);

thread_local! {
    static FATAL_REPORT_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Audited production boundary that intentionally converts a panic into a
/// bounded error or conservative fallback.
///
/// Keeping this as a closed enum prevents plugin- or caller-controlled text
/// from reaching post-catch telemetry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RecoverablePanicSite {
    /// Scheduling the bounded mux activity-pruning pass.
    MuxActivityScheduler,
    /// A callback through the mux `Pane` trait.
    MuxPaneCallback,
    /// Exact-registration rollback while abandoning a prepared pane split.
    MuxRegistrationRollback,
    /// Pane kill/finalization during mux retirement.
    MuxPaneRetirement,
    /// A mux notification subscriber callback.
    MuxSubscriber,
    /// A tmux-domain callback or deferred tmux cleanup.
    MuxTmuxCallback,
    /// A mux window callback.
    MuxWindowCallback,
    /// A storage-writer command or transaction boundary.
    StorageWriter,
    /// An MCP post-response event-delivery completion callback.
    McpEventDeliveryCompletion,
    /// An isolated MCP await-event request task.
    McpAwaitEventRequest,
    /// A project dataflow subscriber callback.
    CoreDataflowCallback,
    /// Best-effort recording finalization during `Drop`.
    CoreRecordingFinalize,
    /// A search bridge whose failure falls back to another search path.
    CoreSearchBridge,
    /// A structured asynchronous task join that converts panic to task error.
    CoreAsyncTaskJoin,
    /// A shard compensation/rollback future or join.
    ShardingRollback,
    /// A promise waker isolated from the promise state machine.
    PromiseWaker,
    /// A native-window callback isolated at an operating-system boundary.
    PlatformWindowCallback,
    /// An OpenSSL callback isolated from async connection cleanup.
    AsyncOpenSslCallback,
    /// A client callback or client-owned task isolated from the mux process.
    ClientCallback,
    /// A scripting callback isolated from the host runtime.
    ScriptingCallback,
}

impl RecoverablePanicSite {
    /// Bounded, content-free label suitable for logs and metrics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MuxActivityScheduler => "mux.activity_scheduler",
            Self::MuxPaneCallback => "mux.pane_callback",
            Self::MuxRegistrationRollback => "mux.registration_rollback",
            Self::MuxPaneRetirement => "mux.pane_retirement",
            Self::MuxSubscriber => "mux.subscriber",
            Self::MuxTmuxCallback => "mux.tmux_callback",
            Self::MuxWindowCallback => "mux.window_callback",
            Self::StorageWriter => "storage.writer",
            Self::McpEventDeliveryCompletion => "mcp.event_delivery_completion",
            Self::McpAwaitEventRequest => "mcp.await_event_request",
            Self::CoreDataflowCallback => "core.dataflow_callback",
            Self::CoreRecordingFinalize => "core.recording_finalize",
            Self::CoreSearchBridge => "core.search_bridge",
            Self::CoreAsyncTaskJoin => "core.async_task_join",
            Self::ShardingRollback => "sharding.rollback",
            Self::PromiseWaker => "promise.waker",
            Self::PlatformWindowCallback => "window.platform_callback",
            Self::AsyncOpenSslCallback => "async_ossl.callback",
            Self::ClientCallback => "client.callback",
            Self::ScriptingCallback => "scripting.callback",
        }
    }
}

/// Sanitized evidence that an audited boundary contained a panic.
///
/// The original payload is disposed inside [`catch_recoverable`] and is never
/// exposed through this type. A pathological payload destructor is contained
/// by the process-wide quarantine/poison policy rather than reflected through
/// caller errors or logs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecoveredPanic {
    site: RecoverablePanicSite,
}

impl RecoveredPanic {
    /// Return the finite, content-free boundary classification.
    #[must_use]
    pub const fn site(self) -> RecoverablePanicSite {
        self.site
    }
}

impl fmt::Display for RecoveredPanic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "recoverable panic contained at {}",
            self.site.as_str()
        )
    }
}

impl std::error::Error for RecoveredPanic {}

/// Nested-safe RAII marker for one audited synchronous recovery boundary.
///
/// The guard is intentionally `!Send`: thread-local hook state must be
/// restored on the same thread on which it was entered. Do not hold this guard
/// across an `.await`; use [`catch_recoverable_future`] so each poll and future
/// teardown is marked on the executor thread that performs it.
#[must_use = "the guard must remain live for the complete catch_unwind boundary"]
struct RecoverablePanicBoundary {
    #[cfg(panic = "unwind")]
    previous_depth: Option<usize>,
    not_send: PhantomData<Rc<()>>,
}

/// Nested hook-chain and process-overlap claim for one fatal visible report.
///
/// The outer project hook that can produce the most useful privacy-bounded
/// report claims ownership, then delegates while this guard remains live.
/// Inner hooks and simultaneously panicking threads observe
/// `is_owner() == false` and do not duplicate the report. The process claim is
/// released with the outer guard so a later independent panic can report.
#[must_use = "the claim must remain live while delegating to inner panic hooks"]
pub struct FatalReportClaim {
    owner: bool,
    previous_depth: Option<usize>,
    not_send: PhantomData<Rc<()>>,
}

impl FatalReportClaim {
    /// Attempt to own the one fatal report for the current hook invocation.
    #[must_use]
    pub fn enter() -> Self {
        let previous_depth = FATAL_REPORT_DEPTH
            .try_with(|depth| {
                let previous = depth.get();
                depth.set(previous.saturating_add(1));
                previous
            })
            .ok();
        let owner = previous_depth == Some(0)
            && FATAL_REPORT_PROCESS_CLAIMED
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok();
        Self {
            owner,
            previous_depth,
            not_send: PhantomData,
        }
    }

    /// Whether this hook owns operator-visible fatal reporting.
    #[must_use]
    pub const fn is_owner(&self) -> bool {
        self.owner
    }
}

impl Drop for FatalReportClaim {
    fn drop(&mut self) {
        if let Some(previous_depth) = self.previous_depth {
            let _ = FATAL_REPORT_DEPTH.try_with(|depth| depth.set(previous_depth));
        }
        if self.owner {
            FATAL_REPORT_PROCESS_CLAIMED.store(false, Ordering::Release);
        }
    }
}

impl RecoverablePanicBoundary {
    /// Enter a recoverable boundary when unwinding is executable.
    #[must_use]
    fn enter() -> Self {
        #[cfg(panic = "unwind")]
        let previous_depth = RECOVERABLE_PANIC_DEPTH
            .try_with(|depth| {
                let previous = depth.get();
                depth.set(previous.saturating_add(1));
                previous
            })
            .ok();

        Self {
            #[cfg(panic = "unwind")]
            previous_depth,
            not_send: PhantomData,
        }
    }
}

impl Drop for RecoverablePanicBoundary {
    fn drop(&mut self) {
        #[cfg(panic = "unwind")]
        if let Some(previous_depth) = self.previous_depth {
            let _ = RECOVERABLE_PANIC_DEPTH.try_with(|depth| depth.set(previous_depth));
        }
    }
}

/// Whether the current thread is unwinding through an explicitly audited
/// recoverable boundary.
///
/// This is compile-time fail-closed under `panic = "abort"`.
#[must_use]
pub fn is_recoverable_panic() -> bool {
    #[cfg(panic = "unwind")]
    {
        return RECOVERABLE_PANIC_DEPTH
            .try_with(|depth| depth.get())
            .unwrap_or(0)
            > 0;
    }

    #[cfg(not(panic = "unwind"))]
    {
        false
    }
}

/// Whether this artifact can execute audited panic-recovery branches.
///
/// This compile-time fact is safe to expose for release-profile identity
/// probes. The marker guard itself remains private so arbitrary callers cannot
/// suppress fatal hooks without entering the canonical catch implementation.
#[must_use]
pub const fn panic_recovery_is_executable() -> bool {
    cfg!(panic = "unwind")
}

/// Execute one audited synchronous panic-recovery boundary.
///
/// On recovery, the original payload is disposed under a second marked
/// boundary, a bounded content-free counter is incremented, and only
/// [`RecoveredPanic`] is returned. If payload disposal itself panics, one
/// secondary object may be quarantined process-wide and future recovery fails
/// closed instead of accumulating opaque leaks.
///
/// This boundary remains effective when entered from a destructor while an
/// outer unwind is already active. `catch_unwind` stops the nested unwind
/// before it can escape the destructor, allowing the original unwind to
/// continue without triggering Rust's double-panic abort path.
pub fn catch_recoverable<F, R>(
    site: RecoverablePanicSite,
    operation: F,
) -> Result<R, RecoveredPanic>
where
    F: FnOnce() -> R + UnwindSafe,
{
    fail_closed_if_payload_disposal_poisoned();
    catch_recoverable_internal(site, operation)
}

fn catch_recoverable_internal<F, R>(
    site: RecoverablePanicSite,
    operation: F,
) -> Result<R, RecoveredPanic>
where
    F: FnOnce() -> R + UnwindSafe,
{
    let boundary = RecoverablePanicBoundary::enter();
    let result = std::panic::catch_unwind(AssertUnwindSafe(operation));
    drop(boundary);

    match result {
        Ok(value) => Ok(value),
        Err(payload) => Err(record_recovered_panic(site, payload)),
    }
}

/// Future wrapper that marks and catches each individual poll on the thread
/// that performs it.
///
/// This is the async counterpart to [`catch_recoverable`]. It never stores a
/// thread-local guard across `.await`, so a `Send` future may migrate between
/// executor threads without leaking or losing marker state.
#[must_use = "futures do nothing unless polled or awaited"]
pub struct RecoverableFuture<F> {
    future: Option<Pin<Box<F>>>,
    site: RecoverablePanicSite,
}

impl<F> Future for RecoverableFuture<F>
where
    F: Future,
{
    type Output = Result<F::Output, RecoveredPanic>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        fail_closed_if_payload_disposal_poisoned();
        let boundary = RecoverablePanicBoundary::enter();
        let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
            this.future
                .as_mut()
                .expect("recoverable future polled after completion")
                .as_mut()
                .poll(context)
        }));
        drop(boundary);
        match outcome {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(value)) => {
                let future = this.future.take();
                match catch_recoverable_internal(
                    this.site,
                    AssertUnwindSafe(|| drop(future)),
                ) {
                    Ok(()) => Poll::Ready(Ok(value)),
                    Err(error) => {
                        // The completed future failed during teardown, so its
                        // output cannot be delivered. Destroy that output
                        // under an independent recovery boundary as well: its
                        // destructor is just as untrusted as the future's.
                        // Preserve the first error because it identifies the
                        // operation that made the output unusable.
                        let _ = catch_recoverable_internal(
                            this.site,
                            AssertUnwindSafe(|| drop(value)),
                        );
                        Poll::Ready(Err(error))
                    }
                }
            }
            Err(payload) => {
                let error = record_recovered_panic(this.site, payload);
                let future = this.future.take();
                // A panicking poll may leave arbitrary state behind. Destroy
                // it before returning, under a second marked boundary, so a
                // panicking Drop cannot escape later during cancellation or
                // an unrelated outer unwind.
                let _ = catch_recoverable_internal(
                    this.site,
                    AssertUnwindSafe(|| drop(future)),
                );
                Poll::Ready(Err(error))
            }
        }
    }
}

impl<F> Drop for RecoverableFuture<F> {
    fn drop(&mut self) {
        let future = self.future.take();
        if future.is_some() {
            // Cancellation owns teardown just as completion does. Under abort
            // this remains fail-closed and cannot pretend to contain a panic.
            let _ = catch_recoverable(self.site, AssertUnwindSafe(|| drop(future)));
        }
    }
}

/// Wrap an asynchronous recovery contract without holding thread-local state
/// across executor suspension.
pub fn catch_recoverable_future<F>(
    site: RecoverablePanicSite,
    future: F,
) -> RecoverableFuture<F>
where
    F: Future,
{
    RecoverableFuture {
        future: Some(Box::pin(future)),
        site,
    }
}

fn record_recovered_panic(
    site: RecoverablePanicSite,
    payload: Box<dyn std::any::Any + Send>,
) -> RecoveredPanic {
    saturating_increment(&RECOVERED_PANICS_TOTAL);
    dispose_caught_payload(payload);
    RecoveredPanic { site }
}

/// Destroy a caught payload without allowing an adversarial payload destructor
/// to escape the recovery contract.
///
/// If that destructor itself panics, the runtime gives us a *second* opaque
/// panic payload. Recursively dropping arbitrary opaque payloads cannot be made
/// total in safe Rust: each destructor can manufacture another panicking
/// payload. We therefore count the secondary panic and allow exactly one such
/// object to be deliberately quarantined process-wide. The sticky poison makes
/// every later recovery attempt terminate without unwinding, so aggregate
/// quarantined object count cannot exceed one even under hostile repetition or
/// races. The one object's owned heap size remains opaque and caller-controlled.
fn dispose_caught_payload(payload: Box<dyn std::any::Any + Send>) {
    let boundary = RecoverablePanicBoundary::enter();
    let disposal = std::panic::catch_unwind(AssertUnwindSafe(|| drop(payload)));
    drop(boundary);
    if let Err(secondary_payload) = disposal {
        saturating_increment(&RECOVERED_PANICS_TOTAL);
        if claim_secondary_payload_quarantine(&PAYLOAD_DISPOSAL_POISONED) {
            std::mem::forget(secondary_payload);
        } else {
            // Do not drop this racing/repeated secondary payload. `exit`
            // performs no unwinding and the OS reclaims it with the process.
            fail_closed_after_payload_disposal_poison();
        }
    }
}

fn claim_secondary_payload_quarantine(poisoned: &AtomicBool) -> bool {
    poisoned
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

fn fail_closed_if_payload_disposal_poisoned() {
    if PAYLOAD_DISPOSAL_POISONED.load(Ordering::Acquire) {
        fail_closed_after_payload_disposal_poison();
    }
}

fn fail_closed_after_payload_disposal_poison() -> ! {
    if PAYLOAD_DISPOSAL_FATAL_REPORTED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        write_generic_fatal_report();
    }
    // Conventional abort status, but without invoking another panic hook,
    // reflecting a payload, unwinding, or running potentially poisoned Drop.
    std::process::exit(134)
}

fn saturating_increment(counter: &AtomicU64) {
    let _ = counter.try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(1))
    });
}

/// Process-local count of panics contained by [`catch_recoverable`].
#[must_use]
pub fn recovered_panics_total() -> u64 {
    RECOVERED_PANICS_TOTAL.load(Ordering::Relaxed)
}

/// Exit status conventionally reported for a process terminated by
/// `SIGPIPE`: `128 + 13`.
pub const SIGPIPE_EXIT_CODE: i32 = 141;

/// Generic fatal report emitted by the base project hook.
///
/// It intentionally contains no payload, location, thread name, or path.
pub const GENERIC_FATAL_REPORT: &str =
    "FrankenTerm: fatal internal error; diagnostic details were suppressed";

fn write_generic_fatal_report() {
    use std::io::Write;

    // Panic hooks must not call `eprintln!`: std's formatting macro panics on
    // EPIPE and would turn an otherwise controlled fatal path into a recursive
    // panic inside the hook. Best-effort direct I/O keeps reporting fallible.
    let mut stderr = std::io::stderr().lock();
    let _ = stderr.write_all(GENERIC_FATAL_REPORT.as_bytes());
    let _ = stderr.write_all(b"\n");
}

/// Install a panic hook that converts broken-pipe stdout/stderr write panics
/// into a quiet `exit(141)`.
///
/// Call this as early as possible in `main`, before project notification or
/// crash-bundle hooks. It installs a sanitized terminal hook, then wraps it
/// with EPIPE and recoverable-boundary handling. Later project hooks may use
/// `take_hook` and delegate to this safe chain.
pub fn exit_quietly_on_broken_pipe() {
    // The standard hook reflects arbitrary payloads, source paths, and
    // backtraces. PanicHookInfo cannot be reconstructed with sanitized fields,
    // so forwarding to it cannot satisfy the privacy contract. Replace it with
    // one bounded project-owned terminal hook before installing composable
    // project layers above it.
    drop(std::panic::take_hook());
    std::panic::set_hook(Box::new(|_info| {
        if is_recoverable_panic() {
            return;
        }
        let claim = FatalReportClaim::enter();
        if claim.is_owner() {
            write_generic_fatal_report();
        }
    }));

    let sanitized_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // The audited boundary is the stronger authority. Panic payloads are
        // caller/plugin controlled, so an EPIPE-looking string inside a catch
        // must not spoof the top-level pipeline policy and terminate a process
        // that promised containment.
        if is_recoverable_panic() {
            return;
        }
        if panic_is_broken_pipe(info) {
            // Flush nothing, print nothing: the reader is gone. 141 is the
            // status `head`-style pipelines conventionally observe.
            std::process::exit(SIGPIPE_EXIT_CODE);
        }
        sanitized_hook(info);
    }));
}

/// Classify a hook invocation as the standard library's stdout/stderr
/// broken-pipe panic.
///
/// Payload text alone is never authority: arbitrary application or plugin
/// code can panic with the same string. Rust's printing macros originate the
/// panic in `library/std/src/io/stdio.rs`, so the policy requires both that
/// source location and the exact std printing-message shape. This is not a
/// sandbox boundary against malicious native code (which can exit directly),
/// but it prevents ordinary caller-controlled panic strings from being
/// mistaken for a closed pipeline and silently suppressing a fatal report.
#[must_use]
pub fn panic_is_broken_pipe(info: &std::panic::PanicHookInfo<'_>) -> bool {
    let Some(location) = info.location() else {
        return false;
    };
    if !is_std_stdio_location(location.file()) {
        return false;
    }
    payload_is_std_broken_pipe_print(info.payload())
}

fn is_std_stdio_location(file: &str) -> bool {
    const STDIO_SOURCE: &str = "library/std/src/io/stdio.rs";
    file == STDIO_SOURCE
}

fn payload_is_std_broken_pipe_print(payload: &dyn std::any::Any) -> bool {
    let message = if let Some(s) = payload.downcast_ref::<&str>() {
        *s
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.as_str()
    } else {
        return false;
    };
    let Some(error) = message
        .strip_prefix("failed printing to stdout: ")
        .or_else(|| message.strip_prefix("failed printing to stderr: "))
    else {
        return false;
    };
    error == "Broken pipe (os error 32)"
}

#[cfg(test)]
mod tests {
    use super::*;

    static FATAL_REPORT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn recoverable_boundary_is_nested_and_restores_exact_prior_depth() {
        assert_eq!(panic_recovery_is_executable(), cfg!(panic = "unwind"));
        assert!(!is_recoverable_panic());
        let outer = RecoverablePanicBoundary::enter();
        assert_eq!(is_recoverable_panic(), cfg!(panic = "unwind"));
        {
            let inner = RecoverablePanicBoundary::enter();
            assert_eq!(is_recoverable_panic(), cfg!(panic = "unwind"));
            drop(inner);
        }
        assert_eq!(is_recoverable_panic(), cfg!(panic = "unwind"));
        drop(outer);
        assert!(!is_recoverable_panic());
    }

    #[cfg(panic = "unwind")]
    #[test]
    fn synchronous_cleanup_contains_nested_panic_during_outer_unwind() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;

        struct CleanupProbe {
            ran: Arc<AtomicBool>,
            saw_recoverable_marker: Arc<AtomicBool>,
            nested_panic_was_contained: Arc<AtomicBool>,
        }

        impl Drop for CleanupProbe {
            fn drop(&mut self) {
                let result = catch_recoverable(
                    RecoverablePanicSite::CoreRecordingFinalize,
                    AssertUnwindSafe(|| {
                        self.ran.store(true, Ordering::Relaxed);
                        self.saw_recoverable_marker
                            .store(is_recoverable_panic(), Ordering::Relaxed);
                        panic!("nested-cleanup-test-sentinel");
                    }),
                );
                self.nested_panic_was_contained
                    .store(result.is_err(), Ordering::Relaxed);
            }
        }

        let ran = Arc::new(AtomicBool::new(false));
        let saw_recoverable_marker = Arc::new(AtomicBool::new(true));
        let nested_panic_was_contained = Arc::new(AtomicBool::new(false));
        let outer = catch_recoverable(RecoverablePanicSite::CoreAsyncTaskJoin, AssertUnwindSafe({
            let ran = Arc::clone(&ran);
            let saw_recoverable_marker = Arc::clone(&saw_recoverable_marker);
            let nested_panic_was_contained = Arc::clone(&nested_panic_was_contained);
            move || {
                let _probe = CleanupProbe {
                    ran,
                    saw_recoverable_marker,
                    nested_panic_was_contained,
                };
                panic!("outer-unwind-test-sentinel");
            }
        }));

        assert!(outer.is_err());
        assert!(ran.load(Ordering::Relaxed));
        assert!(saw_recoverable_marker.load(Ordering::Relaxed));
        assert!(nested_panic_was_contained.load(Ordering::Relaxed));
        assert!(!is_recoverable_panic());
    }

    #[test]
    fn recoverable_boundary_never_leaks_to_another_thread() {
        let boundary = RecoverablePanicBoundary::enter();
        let child_observed = std::thread::spawn(is_recoverable_panic)
            .join()
            .expect("marker observation thread");
        assert!(!child_observed);
        drop(boundary);
    }

    #[cfg(panic = "unwind")]
    #[test]
    fn catch_recoverable_drops_payload_and_restores_marker() {
        let before = recovered_panics_total();
        let result = catch_recoverable(RecoverablePanicSite::MuxPaneCallback, || {
            assert!(is_recoverable_panic());
            std::panic::panic_any(String::from("sentinel-must-not-escape"));
        });
        assert_eq!(
            result,
            Err(RecoveredPanic {
                site: RecoverablePanicSite::MuxPaneCallback,
            })
        );
        assert!(!is_recoverable_panic());
        // Other panic-containment tests may run in parallel and increment the
        // process-wide counter between these reads. Assert the monotonic lower
        // bound here; the release subprocess probe owns the isolated exact
        // delta assertion.
        assert!(recovered_panics_total() >= before.saturating_add(1));
        assert!(!result.unwrap_err().to_string().contains("sentinel"));
    }

    #[test]
    fn secondary_payload_quarantine_claim_is_sticky_and_single_owner() {
        let poisoned = AtomicBool::new(false);
        assert!(claim_secondary_payload_quarantine(&poisoned));
        assert!(poisoned.load(Ordering::Acquire));
        assert!(!claim_secondary_payload_quarantine(&poisoned));
        assert!(poisoned.load(Ordering::Acquire));
    }

    #[test]
    fn secondary_payload_quarantine_has_one_owner_under_race() {
        use std::sync::{Arc, Barrier};

        const CONTENDERS: usize = 16;
        let poisoned = Arc::new(AtomicBool::new(false));
        let barrier = Arc::new(Barrier::new(CONTENDERS));
        let mut threads = Vec::with_capacity(CONTENDERS);
        for _ in 0..CONTENDERS {
            let poisoned = Arc::clone(&poisoned);
            let barrier = Arc::clone(&barrier);
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                usize::from(claim_secondary_payload_quarantine(&poisoned))
            }));
        }
        let owners: usize = threads
            .into_iter()
            .map(|thread| thread.join().expect("quarantine contender"))
            .sum();
        assert_eq!(owners, 1);
        assert!(poisoned.load(Ordering::Acquire));
    }

    #[test]
    fn every_telemetry_site_label_is_bounded_and_content_free() {
        let sites = [
            RecoverablePanicSite::MuxActivityScheduler,
            RecoverablePanicSite::MuxPaneCallback,
            RecoverablePanicSite::MuxRegistrationRollback,
            RecoverablePanicSite::MuxPaneRetirement,
            RecoverablePanicSite::MuxSubscriber,
            RecoverablePanicSite::MuxTmuxCallback,
            RecoverablePanicSite::MuxWindowCallback,
            RecoverablePanicSite::StorageWriter,
            RecoverablePanicSite::McpEventDeliveryCompletion,
            RecoverablePanicSite::McpAwaitEventRequest,
            RecoverablePanicSite::CoreDataflowCallback,
            RecoverablePanicSite::CoreRecordingFinalize,
            RecoverablePanicSite::CoreSearchBridge,
            RecoverablePanicSite::CoreAsyncTaskJoin,
            RecoverablePanicSite::ShardingRollback,
            RecoverablePanicSite::PromiseWaker,
            RecoverablePanicSite::PlatformWindowCallback,
            RecoverablePanicSite::AsyncOpenSslCallback,
            RecoverablePanicSite::ClientCallback,
            RecoverablePanicSite::ScriptingCallback,
        ];
        for site in sites {
            assert!(site.as_str().len() <= 32, "{}", site.as_str());
            assert!(
                site.as_str()
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || matches!(byte, b'.' | b'_')),
                "{}",
                site.as_str()
            );
        }
    }

    #[test]
    fn recovered_panic_counter_saturates_instead_of_wrapping() {
        let counter = AtomicU64::new(u64::MAX - 1);
        saturating_increment(&counter);
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
        saturating_increment(&counter);
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
    }

    #[test]
    fn fatal_report_claim_is_nested_and_reusable_across_hook_invocations() {
        let _test_guard = FATAL_REPORT_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let outer = FatalReportClaim::enter();
        assert!(outer.is_owner());
        let inner = FatalReportClaim::enter();
        assert!(!inner.is_owner());
        drop(inner);
        drop(outer);

        let next_hook_invocation = FatalReportClaim::enter();
        assert!(next_hook_invocation.is_owner());
    }

    #[test]
    fn fatal_report_claim_has_one_owner_across_simultaneous_threads() {
        use std::sync::{Arc, Barrier};

        let _test_guard = FATAL_REPORT_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        const CONTENDERS: usize = 16;
        let entered = Arc::new(Barrier::new(CONTENDERS));
        let mut threads = Vec::with_capacity(CONTENDERS);
        for _ in 0..CONTENDERS {
            let entered = Arc::clone(&entered);
            threads.push(std::thread::spawn(move || {
                let claim = FatalReportClaim::enter();
                entered.wait();
                claim.is_owner()
            }));
        }

        let owners: usize = threads
            .into_iter()
            .map(|thread| usize::from(thread.join().expect("fatal-report contender")))
            .sum();
        assert_eq!(owners, 1);

        let later = FatalReportClaim::enter();
        assert!(later.is_owner());
    }

    #[cfg(panic = "unwind")]
    #[test]
    fn recoverable_future_marks_only_its_poll_and_drops_payload() {
        let mut future = Box::pin(catch_recoverable_future(
            RecoverablePanicSite::CoreAsyncTaskJoin,
            async {
                assert!(is_recoverable_panic());
                std::panic::panic_any(String::from("async-secret-sentinel"));
            },
        ));
        let waker = std::task::Waker::noop();
        let mut context = Context::from_waker(waker);
        let result = future.as_mut().poll(&mut context);
        let Poll::Ready(Err(error)) = result else {
            panic!("panicking future must resolve to a sanitized error");
        };
        assert_eq!(error.site(), RecoverablePanicSite::CoreAsyncTaskJoin);
        assert!(!error.to_string().contains("sentinel"));
        assert!(!is_recoverable_panic());
    }

    #[cfg(panic = "unwind")]
    #[test]
    fn recoverable_future_contains_panicking_teardown_after_panicking_poll() {
        struct PollAndDropPanic;

        impl Future for PollAndDropPanic {
            type Output = ();

            fn poll(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Self::Output> {
                panic!("poll-secret-sentinel");
            }
        }

        impl Drop for PollAndDropPanic {
            fn drop(&mut self) {
                panic!("drop-secret-sentinel");
            }
        }

        let mut future = Box::pin(catch_recoverable_future(
            RecoverablePanicSite::CoreAsyncTaskJoin,
            PollAndDropPanic,
        ));
        let waker = std::task::Waker::noop();
        let mut context = Context::from_waker(waker);
        assert!(matches!(
            future.as_mut().poll(&mut context),
            Poll::Ready(Err(_))
        ));
        // The poisoned inner future was already destroyed inside poll; wrapper
        // cancellation cannot trigger its Drop a second time.
        drop(future);
    }

    #[cfg(panic = "unwind")]
    #[test]
    fn recoverable_future_contains_panicking_drop_on_cancellation_and_ready() {
        struct DropPanic {
            ready: bool,
        }

        impl Future for DropPanic {
            type Output = ();

            fn poll(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Self::Output> {
                if self.ready {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            }
        }

        impl Drop for DropPanic {
            fn drop(&mut self) {
                panic!("future-drop-secret-sentinel");
            }
        }

        let pending = catch_recoverable_future(
            RecoverablePanicSite::CoreAsyncTaskJoin,
            DropPanic { ready: false },
        );
        drop(pending);

        let mut ready = Box::pin(catch_recoverable_future(
            RecoverablePanicSite::CoreAsyncTaskJoin,
            DropPanic { ready: true },
        ));
        let waker = std::task::Waker::noop();
        let mut context = Context::from_waker(waker);
        assert!(matches!(
            ready.as_mut().poll(&mut context),
            Poll::Ready(Err(_))
        ));
        drop(ready);
    }

    #[cfg(panic = "unwind")]
    #[test]
    fn recoverable_future_contains_completed_future_and_output_drop_panics() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;

        struct OutputWithPanickingDrop(Arc<AtomicBool>);

        impl Drop for OutputWithPanickingDrop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
                panic!("output-drop-secret-sentinel");
            }
        }

        struct ReadyFutureWithPanickingDrop {
            future_drop_ran: Arc<AtomicBool>,
            output_drop_ran: Arc<AtomicBool>,
        }

        impl Future for ReadyFutureWithPanickingDrop {
            type Output = OutputWithPanickingDrop;

            fn poll(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Self::Output> {
                Poll::Ready(OutputWithPanickingDrop(Arc::clone(
                    &self.output_drop_ran,
                )))
            }
        }

        impl Drop for ReadyFutureWithPanickingDrop {
            fn drop(&mut self) {
                self.future_drop_ran.store(true, Ordering::Relaxed);
                panic!("future-drop-secret-sentinel");
            }
        }

        let future_drop_ran = Arc::new(AtomicBool::new(false));
        let output_drop_ran = Arc::new(AtomicBool::new(false));
        let mut future = Box::pin(catch_recoverable_future(
            RecoverablePanicSite::CoreAsyncTaskJoin,
            ReadyFutureWithPanickingDrop {
                future_drop_ran: Arc::clone(&future_drop_ran),
                output_drop_ran: Arc::clone(&output_drop_ran),
            },
        ));
        let waker = std::task::Waker::noop();
        let mut context = Context::from_waker(waker);

        assert!(matches!(
            future.as_mut().poll(&mut context),
            Poll::Ready(Err(_))
        ));
        assert!(future_drop_ran.load(Ordering::Relaxed));
        assert!(output_drop_ran.load(Ordering::Relaxed));
        assert!(!is_recoverable_panic());
        drop(future);
    }

    #[cfg(panic = "unwind")]
    #[test]
    fn recoverable_future_contains_drop_panic_during_outer_unwind() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;

        struct PendingFutureWithObservedDrop {
            drop_ran: Arc<AtomicBool>,
            drop_was_marked_recoverable: Arc<AtomicBool>,
        }

        impl Future for PendingFutureWithObservedDrop {
            type Output = ();

            fn poll(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Self::Output> {
                Poll::Pending
            }
        }

        impl Drop for PendingFutureWithObservedDrop {
            fn drop(&mut self) {
                self.drop_ran.store(true, Ordering::Relaxed);
                self.drop_was_marked_recoverable
                    .store(is_recoverable_panic(), Ordering::Relaxed);
                panic!("nested-future-drop-test-sentinel");
            }
        }

        let inner_drop_ran = Arc::new(AtomicBool::new(false));
        let drop_was_marked_recoverable = Arc::new(AtomicBool::new(true));
        let outer = catch_recoverable(
            RecoverablePanicSite::CoreAsyncTaskJoin,
            AssertUnwindSafe(|| {
                let _future = catch_recoverable_future(
                    RecoverablePanicSite::CoreAsyncTaskJoin,
                    PendingFutureWithObservedDrop {
                        drop_ran: Arc::clone(&inner_drop_ran),
                        drop_was_marked_recoverable: Arc::clone(
                            &drop_was_marked_recoverable,
                        ),
                    },
                );
                panic!("unrelated-outer-unwind");
            }),
        );

        assert!(outer.is_err());
        assert!(inner_drop_ran.load(Ordering::Relaxed));
        assert!(drop_was_marked_recoverable.load(Ordering::Relaxed));
        assert!(!is_recoverable_panic());
    }

    #[cfg(panic = "unwind")]
    #[test]
    fn recoverable_future_repoll_is_sanitized_and_holds_no_inner_state() {
        let mut future = Box::pin(catch_recoverable_future(
            RecoverablePanicSite::CoreAsyncTaskJoin,
            std::future::ready(7_u8),
        ));
        let waker = std::task::Waker::noop();
        let mut context = Context::from_waker(waker);
        assert_eq!(future.as_mut().poll(&mut context), Poll::Ready(Ok(7)));
        assert!(matches!(
            future.as_mut().poll(&mut context),
            Poll::Ready(Err(_))
        ));
    }

    #[test]
    fn classifies_std_stdout_broken_pipe_message_shape() {
        let payload: Box<dyn std::any::Any> =
            Box::new("failed printing to stdout: Broken pipe (os error 32)".to_string());
        assert!(payload_is_std_broken_pipe_print(payload.as_ref()));
    }

    #[test]
    fn classifies_std_stderr_broken_pipe_message_shape() {
        let payload: Box<dyn std::any::Any> =
            Box::new("failed printing to stderr: Broken pipe (os error 32)");
        assert!(payload_is_std_broken_pipe_print(payload.as_ref()));
    }

    #[test]
    fn rejects_broad_or_embedded_broken_pipe_messages() {
        for message in [
            "write failed: Broken pipe",
            "failed printing to stdout: prefix Broken pipe (os error 32)",
            "called Result::unwrap() on an Err value: Broken pipe (os error 32)",
            "index out of bounds: the len is 3 but the index is 7",
        ] {
            let payload: Box<dyn std::any::Any> = Box::new(message.to_string());
            assert!(!payload_is_std_broken_pipe_print(payload.as_ref()));
        }
    }

    #[test]
    fn ignores_non_string_payloads() {
        let payload: Box<dyn std::any::Any> = Box::new(42_u64);
        assert!(!payload_is_std_broken_pipe_print(payload.as_ref()));
    }

    #[test]
    fn recognizes_only_rust_standard_stdio_source_locations() {
        assert!(is_std_stdio_location("library/std/src/io/stdio.rs"));
        assert!(!is_std_stdio_location(
            "/rustc/toolchain/library/std/src/io/stdio.rs"
        ));
        assert!(!is_std_stdio_location(
            r"C:\rustc\toolchain\library/std/src/io/stdio.rs"
        ));
        assert!(!is_std_stdio_location("plugin/src/io/stdio.rs"));
        assert!(!is_std_stdio_location(
            "plugin/library/std/src/io/stdio.rs"
        ));
    }
}
