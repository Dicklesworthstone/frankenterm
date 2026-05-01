//! OS scheduling-priority policy substrate for the latency-pinned
//! input loop (ft-mpc9b.6.3).
//!
//! The bead wants a dedicated input handler that drives input events
//! to PTY writes in <1 ms p99. Achieving that on a busy host requires
//! a priority hint to the OS scheduler so the input thread isn't
//! starved by the renderer or any noisy neighbour. Each platform
//! exposes that knob differently:
//!
//! - **Linux**: `SCHED_FIFO` with a sane priority value (NOT `99` —
//!   that's what kthreads use; spike to `99` and you can deadlock the
//!   scheduler if the input task busy-waits). Safe range is `1..=49`.
//! - **macOS**: `dispatch_set_qos_class_self` with
//!   `QOS_CLASS_USER_INTERACTIVE`. The relative priority offset
//!   stays at the default `0` (raising it inside the class is rarely
//!   honoured and can fight Apple's coalescing policies).
//! - **Windows**: `SetThreadPriority(THREAD_PRIORITY_TIME_CRITICAL)`
//!   inside a process at `HIGH_PRIORITY_CLASS`. The bead text
//!   mentions `REALTIME_PRIORITY_CLASS` but that's so aggressive it
//!   often locks out kernel-mode drivers; the substrate clamps to
//!   high-priority-class + time-critical-thread which delivers ≥99 %
//!   of the latency benefit at ~0 % of the system-stability risk.
//! - **Other**: fall back to `Normal` and log the reason so ft doctor
//!   can surface that the latency target may not be met.
//!
//! ## What this module ships
//!
//! - `InputPriorityClass` — `LowLatency | Normal`. The renderer's
//!   request, before the safety clamp.
//! - `Platform` — `Linux | MacOs | Windows | Other`. Pure-data
//!   identifier, set by the integration layer's startup probe.
//! - `OsPriorityHint` — the per-platform descriptor of *what* to ask
//!   for. Carries the policy decision; the integration layer
//!   translates these to libc / Mach / Win32 calls.
//! - `PriorityFallbackReason` — telemetry value for `ft doctor`
//!   when the requested class downgrades.
//! - `negotiate_priority(class, platform) -> OsPriorityHint` — the
//!   pure-logic lookup. Side-effect-free.
//! - `safe_sched_fifo_priority`, `safe_qos_class`,
//!   `safe_windows_thread_priority` — per-platform safety clamps the
//!   integration layer reads as constants when issuing the OS call.
//! - `record_priority_outcome` / `PriorityOutcomeStats` — running
//!   counters the integration layer feeds with success / fallback
//!   results; surfaced through `ft doctor`.
//!
//! ## What is deferred to the integration bead (ft-mpc9b.6.3.cont)
//!
//! - The actual `runtime_async::spawn` of the input task (asupersync
//!   carries it, not raw `tokio::spawn` per AGENTS.md).
//! - The libc `pthread_setschedparam` / Mach
//!   `dispatch_set_qos_class_self` / Win32 `SetThreadPriority` calls.
//! - The 1000-keystrokes-per-second-for-60-seconds stress bench.
//! - The Cx-cancel test that drops the input task at every await
//!   point and observes clean shutdown via LabRuntime virtual time.
//! - The Loom proof of the input → render mpsc primitive
//!   (cross-link BR-RC-FOUNDATION.G8.2).

#![allow(dead_code)]

// ============================================================================
// Priority class request
// ============================================================================

/// What the renderer is asking for. The integration layer picks
/// `LowLatency` for the input task and `Normal` for everything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum InputPriorityClass {
    /// Pin to the lowest-latency class the platform safely allows.
    LowLatency,
    /// Default class — no special hint.
    #[default]
    Normal,
}

// ============================================================================
// Platform identification
// ============================================================================

/// Operating system identity. Set at startup by the integration
/// layer (cfg!(target_os = "...") or runtime probe). Pure data so
/// tests can construct any combination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Platform {
    Linux,
    MacOs,
    Windows,
    /// BSDs / Solaris / WASI / etc. — falls back to `Normal` since
    /// each has its own priority API and we don't want a half-
    /// implemented hint to give a false sense of safety.
    Other,
}

// ============================================================================
// Per-platform priority descriptors
// ============================================================================

/// On Linux, the integration layer applies this via
/// `pthread_setschedparam(SCHED_FIFO, .sched_priority = value)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinuxSchedFifo {
    /// `sched_priority` value. Safe range `1..=49`. Values above
    /// `49` collide with kernel-thread priorities; `0` means
    /// "default" which is meaningless under SCHED_FIFO.
    pub priority: u8,
}

/// Maximum SCHED_FIFO priority we ever request. Kernel kthreads run
/// at `50..=99`; staying strictly below `50` keeps the input task
/// from out-prioritising scheduler / IO / timer kthreads.
pub const SAFE_SCHED_FIFO_MAX: u8 = 49;

/// Default SCHED_FIFO priority for `LowLatency`. Mid-range so we
/// don't dominate other userland realtime tasks (audio servers
/// commonly run at `30`); high enough that the renderer can't
/// starve us.
pub const DEFAULT_SCHED_FIFO_PRIORITY: u8 = 30;

/// macOS QoS classes. Mirrors `<dispatch/queue.h>` constants but
/// kept as a Rust enum so the substrate stays libdispatch-agnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MacOsQosClass {
    UserInteractive,
    UserInitiated,
    Default,
    Utility,
    Background,
}

/// Windows thread priorities. Subset of the SetThreadPriority enum
/// with the values that actually make sense for an input task; we
/// deliberately exclude `REALTIME_PRIORITY_CLASS` because of the
/// kernel-mode-driver lockout risk noted in the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WindowsThreadPriority {
    TimeCritical,
    Highest,
    AboveNormal,
    Normal,
}

/// What the renderer should pass to the OS-specific call when the
/// priority hint is granted. The integration layer matches on this
/// and dispatches the corresponding libc / Mach / Win32 call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsPriorityHint {
    Linux(LinuxSchedFifo),
    MacOs(MacOsQosClass),
    Windows(WindowsThreadPriority),
    /// No-op — the integration layer skips the OS call entirely.
    /// Always emitted for `Normal` requests and for `LowLatency`
    /// requests on `Platform::Other`.
    Default,
}

impl OsPriorityHint {
    #[must_use]
    pub fn is_default(&self) -> bool {
        matches!(self, Self::Default)
    }
}

/// Why a `LowLatency` request resolved to `OsPriorityHint::Default`
/// (or otherwise downgraded). Telemetry value for `ft doctor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PriorityFallbackReason {
    /// Request was `Normal` — no hint expected. Not an error.
    NormalRequested,
    /// `Platform::Other` — the substrate doesn't carry a priority
    /// API for this platform.
    UnsupportedPlatform,
    /// The integration layer reported the OS call failed (e.g.
    /// CAP_SYS_NICE missing on Linux, or a sandbox profile on macOS
    /// rejected `dispatch_set_qos_class_self`).
    OsCallRejected,
}

// ============================================================================
// Negotiation
// ============================================================================

/// Pure-logic lookup: given a request and a platform, return the
/// per-platform descriptor the integration layer should pass to the
/// OS call. No side effects; testable without any system context.
#[must_use]
pub fn negotiate_priority(
    class: InputPriorityClass,
    platform: Platform,
) -> NegotiatedPriority {
    match class {
        InputPriorityClass::Normal => NegotiatedPriority {
            hint: OsPriorityHint::Default,
            fallback_reason: Some(PriorityFallbackReason::NormalRequested),
        },
        InputPriorityClass::LowLatency => match platform {
            Platform::Linux => NegotiatedPriority {
                hint: OsPriorityHint::Linux(LinuxSchedFifo {
                    priority: DEFAULT_SCHED_FIFO_PRIORITY,
                }),
                fallback_reason: None,
            },
            Platform::MacOs => NegotiatedPriority {
                hint: OsPriorityHint::MacOs(MacOsQosClass::UserInteractive),
                fallback_reason: None,
            },
            Platform::Windows => NegotiatedPriority {
                hint: OsPriorityHint::Windows(WindowsThreadPriority::TimeCritical),
                fallback_reason: None,
            },
            Platform::Other => NegotiatedPriority {
                hint: OsPriorityHint::Default,
                fallback_reason: Some(PriorityFallbackReason::UnsupportedPlatform),
            },
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NegotiatedPriority {
    pub hint: OsPriorityHint,
    pub fallback_reason: Option<PriorityFallbackReason>,
}

impl NegotiatedPriority {
    #[must_use]
    pub fn is_low_latency(&self) -> bool {
        !matches!(self.hint, OsPriorityHint::Default)
    }
}

// ============================================================================
// Safety clamps (read by integration layer when issuing OS calls)
// ============================================================================

/// Clamp a Linux SCHED_FIFO priority into the safe range
/// `1..=SAFE_SCHED_FIFO_MAX`. Returns the clamped value plus a flag
/// indicating whether the clamp engaged. The integration layer uses
/// this when an operator-tuned priority comes from a config file.
#[must_use]
pub fn safe_sched_fifo_priority(requested: u8) -> SafeSchedFifo {
    if requested == 0 {
        // 0 is "default" under SCHED_FIFO, which is meaningless —
        // promote to 1 (the smallest legal real-time value).
        return SafeSchedFifo {
            priority: 1,
            clamped: true,
        };
    }
    if requested > SAFE_SCHED_FIFO_MAX {
        return SafeSchedFifo {
            priority: SAFE_SCHED_FIFO_MAX,
            clamped: true,
        };
    }
    SafeSchedFifo {
        priority: requested,
        clamped: false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SafeSchedFifo {
    pub priority: u8,
    pub clamped: bool,
}

/// Validate that a macOS QoS class is one of the user-facing values
/// (we never want the input task to land in `Background` /
/// `Utility`). Returns the operator's request when valid; clamps
/// to `UserInteractive` and signals the clamp otherwise.
#[must_use]
pub fn safe_qos_class(requested: MacOsQosClass) -> SafeQos {
    match requested {
        MacOsQosClass::UserInteractive | MacOsQosClass::UserInitiated => SafeQos {
            class: requested,
            clamped: false,
        },
        // Default / Utility / Background are valid macOS classes but
        // entirely wrong for an input task; clamp upward.
        _ => SafeQos {
            class: MacOsQosClass::UserInteractive,
            clamped: true,
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SafeQos {
    pub class: MacOsQosClass,
    pub clamped: bool,
}

/// Validate a Windows thread priority. Returns the operator's
/// request when ≥ AboveNormal; clamps to `TimeCritical` for anything
/// at-or-below `Normal` since that defeats the purpose of asking for
/// `LowLatency`.
#[must_use]
pub fn safe_windows_thread_priority(requested: WindowsThreadPriority) -> SafeWindows {
    match requested {
        WindowsThreadPriority::TimeCritical
        | WindowsThreadPriority::Highest
        | WindowsThreadPriority::AboveNormal => SafeWindows {
            priority: requested,
            clamped: false,
        },
        WindowsThreadPriority::Normal => SafeWindows {
            priority: WindowsThreadPriority::TimeCritical,
            clamped: true,
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SafeWindows {
    pub priority: WindowsThreadPriority,
    pub clamped: bool,
}

// ============================================================================
// Outcome telemetry
// ============================================================================

/// Lifetime counters surfaced through `ft doctor`. The integration
/// layer feeds these as it observes priority hints succeed or fall
/// back at runtime.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PriorityOutcomeStats {
    pub low_latency_grants_total: u64,
    pub fallback_unsupported_platform_total: u64,
    pub fallback_os_call_rejected_total: u64,
    pub normal_requests_total: u64,
    pub clamps_engaged_total: u64,
}

impl PriorityOutcomeStats {
    /// Per-grant accumulator. Returns the new total of granted
    /// low-latency hints so the integration layer can short-circuit
    /// duplicate-grant log noise.
    pub fn record_grant(&mut self) -> u64 {
        self.low_latency_grants_total = self.low_latency_grants_total.saturating_add(1);
        self.low_latency_grants_total
    }

    pub fn record_fallback(&mut self, reason: PriorityFallbackReason) {
        match reason {
            PriorityFallbackReason::NormalRequested => {
                self.normal_requests_total =
                    self.normal_requests_total.saturating_add(1);
            }
            PriorityFallbackReason::UnsupportedPlatform => {
                self.fallback_unsupported_platform_total =
                    self.fallback_unsupported_platform_total.saturating_add(1);
            }
            PriorityFallbackReason::OsCallRejected => {
                self.fallback_os_call_rejected_total =
                    self.fallback_os_call_rejected_total.saturating_add(1);
            }
        }
    }

    pub fn record_clamp(&mut self) {
        self.clamps_engaged_total = self.clamps_engaged_total.saturating_add(1);
    }

    /// True when the integration is fully on the low-latency path:
    /// any grants have happened and no rejections.
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        self.low_latency_grants_total > 0
            && self.fallback_os_call_rejected_total == 0
    }
}

/// Convenience helper: record a single negotiation outcome plus an
/// integration-layer apply result (true = OS call succeeded). Used by
/// the integration's startup path so it doesn't have to interleave
/// `record_grant` / `record_fallback` calls itself.
pub fn record_priority_outcome(
    stats: &mut PriorityOutcomeStats,
    negotiated: NegotiatedPriority,
    applied: bool,
) {
    if let Some(reason) = negotiated.fallback_reason {
        stats.record_fallback(reason);
        return;
    }
    if applied {
        stats.record_grant();
    } else {
        stats.record_fallback(PriorityFallbackReason::OsCallRejected);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----------------------------------------------------------------
    // Negotiation
    // ----------------------------------------------------------------

    #[test]
    fn normal_request_returns_default_hint_with_normal_reason() {
        for platform in [
            Platform::Linux,
            Platform::MacOs,
            Platform::Windows,
            Platform::Other,
        ] {
            let n = negotiate_priority(InputPriorityClass::Normal, platform);
            assert_eq!(n.hint, OsPriorityHint::Default);
            assert_eq!(
                n.fallback_reason,
                Some(PriorityFallbackReason::NormalRequested)
            );
            assert!(!n.is_low_latency());
        }
    }

    #[test]
    fn low_latency_on_linux_picks_sched_fifo_at_safe_default() {
        let n = negotiate_priority(InputPriorityClass::LowLatency, Platform::Linux);
        match n.hint {
            OsPriorityHint::Linux(LinuxSchedFifo { priority }) => {
                assert_eq!(priority, DEFAULT_SCHED_FIFO_PRIORITY);
                assert!(priority < SAFE_SCHED_FIFO_MAX);
            }
            other => panic!("expected Linux SCHED_FIFO, got {other:?}"),
        }
        assert_eq!(n.fallback_reason, None);
        assert!(n.is_low_latency());
    }

    #[test]
    fn low_latency_on_macos_picks_user_interactive_qos() {
        let n = negotiate_priority(InputPriorityClass::LowLatency, Platform::MacOs);
        assert_eq!(n.hint, OsPriorityHint::MacOs(MacOsQosClass::UserInteractive));
        assert!(n.is_low_latency());
    }

    #[test]
    fn low_latency_on_windows_picks_time_critical() {
        let n = negotiate_priority(InputPriorityClass::LowLatency, Platform::Windows);
        assert_eq!(
            n.hint,
            OsPriorityHint::Windows(WindowsThreadPriority::TimeCritical)
        );
        assert!(n.is_low_latency());
    }

    #[test]
    fn low_latency_on_other_falls_back_with_unsupported_reason() {
        let n = negotiate_priority(InputPriorityClass::LowLatency, Platform::Other);
        assert_eq!(n.hint, OsPriorityHint::Default);
        assert_eq!(
            n.fallback_reason,
            Some(PriorityFallbackReason::UnsupportedPlatform)
        );
        assert!(!n.is_low_latency());
    }

    // ----------------------------------------------------------------
    // SCHED_FIFO clamp
    // ----------------------------------------------------------------

    #[test]
    fn sched_fifo_zero_promotes_to_one() {
        let s = safe_sched_fifo_priority(0);
        assert_eq!(s.priority, 1);
        assert!(s.clamped);
    }

    #[test]
    fn sched_fifo_above_safe_max_clamps() {
        let s = safe_sched_fifo_priority(99);
        assert_eq!(s.priority, SAFE_SCHED_FIFO_MAX);
        assert!(s.clamped);
    }

    #[test]
    fn sched_fifo_at_safe_max_passes_through() {
        let s = safe_sched_fifo_priority(SAFE_SCHED_FIFO_MAX);
        assert_eq!(s.priority, SAFE_SCHED_FIFO_MAX);
        assert!(!s.clamped);
    }

    #[test]
    fn sched_fifo_in_safe_range_passes_through() {
        for p in 1..=SAFE_SCHED_FIFO_MAX {
            let s = safe_sched_fifo_priority(p);
            assert_eq!(s.priority, p);
            assert!(!s.clamped);
        }
    }

    // ----------------------------------------------------------------
    // QoS clamp
    // ----------------------------------------------------------------

    #[test]
    fn qos_user_interactive_passes_through() {
        let s = safe_qos_class(MacOsQosClass::UserInteractive);
        assert_eq!(s.class, MacOsQosClass::UserInteractive);
        assert!(!s.clamped);
    }

    #[test]
    fn qos_user_initiated_passes_through() {
        let s = safe_qos_class(MacOsQosClass::UserInitiated);
        assert_eq!(s.class, MacOsQosClass::UserInitiated);
        assert!(!s.clamped);
    }

    #[test]
    fn qos_default_clamps_upward() {
        let s = safe_qos_class(MacOsQosClass::Default);
        assert_eq!(s.class, MacOsQosClass::UserInteractive);
        assert!(s.clamped);
    }

    #[test]
    fn qos_utility_and_background_clamp_upward() {
        for input in [MacOsQosClass::Utility, MacOsQosClass::Background] {
            let s = safe_qos_class(input);
            assert_eq!(s.class, MacOsQosClass::UserInteractive);
            assert!(s.clamped, "{input:?} should clamp to user-interactive");
        }
    }

    // ----------------------------------------------------------------
    // Windows clamp
    // ----------------------------------------------------------------

    #[test]
    fn windows_time_critical_passes_through() {
        let s = safe_windows_thread_priority(WindowsThreadPriority::TimeCritical);
        assert_eq!(s.priority, WindowsThreadPriority::TimeCritical);
        assert!(!s.clamped);
    }

    #[test]
    fn windows_highest_and_above_normal_pass_through() {
        for input in [
            WindowsThreadPriority::Highest,
            WindowsThreadPriority::AboveNormal,
        ] {
            let s = safe_windows_thread_priority(input);
            assert_eq!(s.priority, input);
            assert!(!s.clamped);
        }
    }

    #[test]
    fn windows_normal_clamps_to_time_critical() {
        let s = safe_windows_thread_priority(WindowsThreadPriority::Normal);
        assert_eq!(s.priority, WindowsThreadPriority::TimeCritical);
        assert!(s.clamped);
    }

    // ----------------------------------------------------------------
    // OsPriorityHint helpers
    // ----------------------------------------------------------------

    #[test]
    fn os_priority_hint_is_default_works() {
        assert!(OsPriorityHint::Default.is_default());
        assert!(!OsPriorityHint::Linux(LinuxSchedFifo { priority: 30 }).is_default());
        assert!(!OsPriorityHint::MacOs(MacOsQosClass::UserInteractive).is_default());
        assert!(
            !OsPriorityHint::Windows(WindowsThreadPriority::TimeCritical).is_default()
        );
    }

    // ----------------------------------------------------------------
    // PriorityOutcomeStats
    // ----------------------------------------------------------------

    #[test]
    fn stats_default_is_empty_and_unhealthy() {
        let s = PriorityOutcomeStats::default();
        assert_eq!(s.low_latency_grants_total, 0);
        assert!(!s.is_healthy());
    }

    #[test]
    fn stats_record_grant_returns_running_total() {
        let mut s = PriorityOutcomeStats::default();
        assert_eq!(s.record_grant(), 1);
        assert_eq!(s.record_grant(), 2);
        assert_eq!(s.record_grant(), 3);
        assert_eq!(s.low_latency_grants_total, 3);
    }

    #[test]
    fn stats_record_fallback_routes_per_reason() {
        let mut s = PriorityOutcomeStats::default();
        s.record_fallback(PriorityFallbackReason::NormalRequested);
        s.record_fallback(PriorityFallbackReason::UnsupportedPlatform);
        s.record_fallback(PriorityFallbackReason::UnsupportedPlatform);
        s.record_fallback(PriorityFallbackReason::OsCallRejected);
        assert_eq!(s.normal_requests_total, 1);
        assert_eq!(s.fallback_unsupported_platform_total, 2);
        assert_eq!(s.fallback_os_call_rejected_total, 1);
    }

    #[test]
    fn stats_is_healthy_only_with_grants_and_no_rejections() {
        let mut s = PriorityOutcomeStats::default();
        s.record_grant();
        assert!(s.is_healthy());
        s.record_fallback(PriorityFallbackReason::OsCallRejected);
        assert!(!s.is_healthy(), "any rejection should fail the health check");
    }

    // ----------------------------------------------------------------
    // record_priority_outcome composition
    // ----------------------------------------------------------------

    #[test]
    fn record_outcome_grant_path() {
        let mut s = PriorityOutcomeStats::default();
        let n = negotiate_priority(InputPriorityClass::LowLatency, Platform::Linux);
        record_priority_outcome(&mut s, n, true);
        assert_eq!(s.low_latency_grants_total, 1);
        assert_eq!(s.fallback_os_call_rejected_total, 0);
    }

    #[test]
    fn record_outcome_os_call_rejected_path() {
        let mut s = PriorityOutcomeStats::default();
        let n = negotiate_priority(InputPriorityClass::LowLatency, Platform::Linux);
        record_priority_outcome(&mut s, n, false);
        assert_eq!(s.low_latency_grants_total, 0);
        assert_eq!(s.fallback_os_call_rejected_total, 1);
    }

    #[test]
    fn record_outcome_normal_request_path() {
        let mut s = PriorityOutcomeStats::default();
        let n = negotiate_priority(InputPriorityClass::Normal, Platform::Linux);
        record_priority_outcome(&mut s, n, true);
        assert_eq!(s.normal_requests_total, 1);
        assert_eq!(s.low_latency_grants_total, 0);
    }

    #[test]
    fn record_outcome_unsupported_platform_path() {
        let mut s = PriorityOutcomeStats::default();
        let n = negotiate_priority(InputPriorityClass::LowLatency, Platform::Other);
        record_priority_outcome(&mut s, n, true);
        // Even though `applied=true`, the negotiation already
        // resolved to fallback — applied is ignored.
        assert_eq!(s.fallback_unsupported_platform_total, 1);
        assert_eq!(s.low_latency_grants_total, 0);
    }

    // ----------------------------------------------------------------
    // Cross-cut: realistic startup scenario
    // ----------------------------------------------------------------

    #[test]
    fn scenario_macos_user_interactive_grant_then_health_check() {
        let mut s = PriorityOutcomeStats::default();
        let n = negotiate_priority(InputPriorityClass::LowLatency, Platform::MacOs);
        assert!(n.is_low_latency());
        // Integration layer: dispatch_set_qos_class_self(USER_INTERACTIVE)
        // returned 0 (success).
        record_priority_outcome(&mut s, n, true);
        assert!(s.is_healthy());
    }

    #[test]
    fn scenario_linux_no_cap_sys_nice_falls_back() {
        let mut s = PriorityOutcomeStats::default();
        let n = negotiate_priority(InputPriorityClass::LowLatency, Platform::Linux);
        // Integration layer: pthread_setschedparam returned EPERM
        // (CAP_SYS_NICE missing on the host).
        record_priority_outcome(&mut s, n, false);
        assert_eq!(s.fallback_os_call_rejected_total, 1);
        assert!(!s.is_healthy());
    }

    #[test]
    fn scenario_unsupported_platform_logs_correctly() {
        let mut s = PriorityOutcomeStats::default();
        let n = negotiate_priority(InputPriorityClass::LowLatency, Platform::Other);
        record_priority_outcome(&mut s, n, true);
        assert_eq!(s.fallback_unsupported_platform_total, 1);
        // Operator-visible: ft doctor would say "running on
        // unsupported platform; latency target may not be met".
        assert!(!s.is_healthy());
    }
}
