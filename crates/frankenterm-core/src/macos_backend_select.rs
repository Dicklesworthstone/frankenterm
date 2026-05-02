//! macOS rendering-backend selection substrate (ft-mpc9b.3.1).
//!
//! The bead's plan: ship a Metal-direct backend in a separate crate
//! (`frankenterm-renderer-metal`) that uses `CAMetalLayer` +
//! `presentDrawable:afterMinimumDuration:` for sub-vsync latency on
//! Apple Silicon. wgpu remains the only backend on Intel Macs and
//! the rollback path on Apple Silicon.
//!
//! That new crate's unsafe FFI code (Objective-C / metal-rs) is the
//! integration bead's scope. This module ships the pure-logic policy
//! that picks the backend at startup:
//!
//! - `MacosBackend` — `Wgpu | MetalDirect`. The renderer's hot path
//!   matches on this and dispatches.
//! - `MacosArch` — `AppleSilicon | IntelX64`. Probed via `cfg!` or
//!   `sysctl` at startup.
//! - `MacosVersion` — `(major, minor)` tuple + `meets_baseline()`
//!   helper for the bead's macOS 13.0+ requirement.
//! - `BackendSelectionInputs` — operator env override + arch + OS
//!   version. The integration's startup probe fills these in.
//! - `select_macos_backend` — pure-logic selector. Defaults to
//!   `MetalDirect` on AppleSilicon at macOS 13+; falls back to
//!   `Wgpu` for Intel / pre-13 / explicit override.
//! - `BackendSelectionResult` — chosen backend + fallback reason
//!   for `ft doctor` surface.
//! - `SwapChainSlot` policy — triple-buffer tracking matching the
//!   bead's `swap_chain_count = 3` requirement (per
//!   `legacy_ghostty/src/renderer/Metal.zig:37`).
//! - `BackendStats` — lifetime counters for `ft doctor`.
//!
//! ## What is deferred to the integration bead (ft-mpc9b.3.1.cont)
//!
//! - The `frankenterm-renderer-metal` crate (cfg(target_os="macos") +
//!   cfg(target_arch="aarch64")).
//! - `CAMetalLayer` setup + drawable-acquire pattern via metal-rs.
//! - `dispatch_semaphore_t` synchronisation for the triple-buffered
//!   swap chain.
//! - Metal shader compilation (.metal source files); golden-test
//!   parity against the wgpu WGSL shaders.
//! - `FT_MACOS_BACKEND` env-var routing in `frankenterm-gui` startup.
//! - Address-sanitizer + thread-sanitizer CI runs (Loom can't model
//!   FFI per the bead).
//! - Per-release attestation cross-link
//!   (BR-RC-FOUNDATION.G3.1).

#![allow(dead_code)]

// ============================================================================
// Backend identity
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MacosBackend {
    /// The wgpu cross-platform path. The only option on Intel Macs;
    /// the rollback path on Apple Silicon when the operator
    /// requests it via `FT_MACOS_BACKEND=wgpu` or the OS version
    /// doesn't meet the bead's baseline.
    Wgpu,
    /// CAMetalLayer + presentDrawable:afterMinimumDuration: direct
    /// path. Requires Apple Silicon + macOS 13+.
    MetalDirect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum MacosArch {
    /// M1 / M2 / M3 / M4 (any tier).
    AppleSilicon,
    /// x86_64 — wgpu only.
    IntelX64,
    /// Probe didn't run / unknown — conservative fallback to wgpu.
    #[default]
    Unknown,
}

// ============================================================================
// macOS version
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct MacosVersion {
    pub major: u8,
    pub minor: u8,
}

/// Bead-specified baseline: macOS 13.0 (Ventura). Earlier versions
/// fall back to wgpu automatically.
pub const BASELINE_MAJOR: u8 = 13;
pub const BASELINE_MINOR: u8 = 0;

impl MacosVersion {
    #[must_use]
    pub const fn new(major: u8, minor: u8) -> Self {
        Self { major, minor }
    }

    /// Whether this version meets the bead's macOS 13.0+ baseline.
    #[must_use]
    pub const fn meets_baseline(&self) -> bool {
        if self.major > BASELINE_MAJOR {
            return true;
        }
        if self.major == BASELINE_MAJOR && self.minor >= BASELINE_MINOR {
            return true;
        }
        false
    }
}

// ============================================================================
// Operator override
// ============================================================================

/// `FT_MACOS_BACKEND` env-var values. Operator forces a specific
/// backend regardless of automatic selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum BackendOverride {
    /// No override; selector picks per arch + version.
    #[default]
    Auto,
    /// Force wgpu (rollback path).
    Wgpu,
    /// Force Metal-direct (assumes the runtime supports it; selector
    /// downgrades to Wgpu if arch / version disagree).
    MetalDirect,
}

impl BackendOverride {
    /// Parse from an `FT_MACOS_BACKEND` env-var value. Unknown
    /// values fall through to `Auto` (defensive — operator typo
    /// shouldn't break startup).
    #[must_use]
    pub fn from_env_str(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "auto" | "default" => Self::Auto,
            "wgpu" => Self::Wgpu,
            "metal-direct" | "metal_direct" | "metaldirect" | "metal" => Self::MetalDirect,
            _ => Self::Auto,
        }
    }
}

// ============================================================================
// Selection inputs + result
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendSelectionInputs {
    pub arch: MacosArch,
    pub version: MacosVersion,
    pub override_: BackendOverride,
}

impl BackendSelectionInputs {
    #[must_use]
    pub fn new(arch: MacosArch, version: MacosVersion, override_: BackendOverride) -> Self {
        Self {
            arch,
            version,
            override_,
        }
    }
}

/// Why the selector chose this backend (for `ft doctor`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackendFallbackReason {
    /// The Metal-direct happy path: AppleSilicon + macOS 13+ + no
    /// override. Not actually a fallback — variant name preserved
    /// for telemetry symmetry.
    MetalDirectGranted,
    /// Operator explicitly chose wgpu via `FT_MACOS_BACKEND=wgpu`.
    OperatorOverrideWgpu,
    /// Operator explicitly chose Metal-direct via
    /// `FT_MACOS_BACKEND=metal-direct` and the runtime supports it.
    OperatorOverrideMetalDirect,
    /// Operator asked for Metal-direct but arch / version doesn't
    /// support it; selector downgraded to wgpu.
    OperatorOverrideDowngraded,
    /// Intel Mac — Metal-direct is out-of-scope per the bead.
    IntelArch,
    /// macOS version below 13.0 baseline.
    PreBaselineVersion,
    /// Arch couldn't be determined (probe failed).
    UnknownArch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendSelectionResult {
    pub backend: MacosBackend,
    pub reason: BackendFallbackReason,
}

impl BackendSelectionResult {
    #[must_use]
    pub fn is_metal_direct(&self) -> bool {
        matches!(self.backend, MacosBackend::MetalDirect)
    }

    #[must_use]
    pub fn is_fallback(&self) -> bool {
        !matches!(self.reason, BackendFallbackReason::MetalDirectGranted)
    }
}

/// Pure-logic backend selector. The integration's startup probe
/// fills `BackendSelectionInputs` from `cfg!(target_arch = "...")`,
/// `sysctl` (for runtime probe), and `std::env::var
/// ("FT_MACOS_BACKEND")`; this returns the chosen backend + the
/// reason. No I/O.
///
/// Decision tree:
/// 1. Override `Wgpu` → `Wgpu`, reason `OperatorOverrideWgpu`.
/// 2. Override `MetalDirect` + arch supports + version supports →
///    `MetalDirect`, reason `OperatorOverrideMetalDirect`.
/// 3. Override `MetalDirect` + arch / version doesn't support →
///    `Wgpu`, reason `OperatorOverrideDowngraded`.
/// 4. Override `Auto` + Intel → `Wgpu`, reason `IntelArch`.
/// 5. Override `Auto` + Unknown arch → `Wgpu`, reason `UnknownArch`.
/// 6. Override `Auto` + AppleSilicon + pre-13 → `Wgpu`, reason
///    `PreBaselineVersion`.
/// 7. Override `Auto` + AppleSilicon + 13+ → `MetalDirect`, reason
///    `MetalDirectGranted`.
#[must_use]
pub fn select_macos_backend(inputs: BackendSelectionInputs) -> BackendSelectionResult {
    match inputs.override_ {
        BackendOverride::Wgpu => BackendSelectionResult {
            backend: MacosBackend::Wgpu,
            reason: BackendFallbackReason::OperatorOverrideWgpu,
        },
        BackendOverride::MetalDirect => {
            let arch_ok = matches!(inputs.arch, MacosArch::AppleSilicon);
            let version_ok = inputs.version.meets_baseline();
            if arch_ok && version_ok {
                BackendSelectionResult {
                    backend: MacosBackend::MetalDirect,
                    reason: BackendFallbackReason::OperatorOverrideMetalDirect,
                }
            } else {
                BackendSelectionResult {
                    backend: MacosBackend::Wgpu,
                    reason: BackendFallbackReason::OperatorOverrideDowngraded,
                }
            }
        }
        BackendOverride::Auto => match inputs.arch {
            MacosArch::IntelX64 => BackendSelectionResult {
                backend: MacosBackend::Wgpu,
                reason: BackendFallbackReason::IntelArch,
            },
            MacosArch::Unknown => BackendSelectionResult {
                backend: MacosBackend::Wgpu,
                reason: BackendFallbackReason::UnknownArch,
            },
            MacosArch::AppleSilicon => {
                if inputs.version.meets_baseline() {
                    BackendSelectionResult {
                        backend: MacosBackend::MetalDirect,
                        reason: BackendFallbackReason::MetalDirectGranted,
                    }
                } else {
                    BackendSelectionResult {
                        backend: MacosBackend::Wgpu,
                        reason: BackendFallbackReason::PreBaselineVersion,
                    }
                }
            }
        },
    }
}

// ============================================================================
// Triple-buffered swap chain slot
// ============================================================================

/// Per the bead: `swap_chain_count = 3` matches ghostty's
/// `legacy_ghostty/src/renderer/Metal.zig:37`. The integration's
/// `dispatch_semaphore_t` synchronises slot acquire / release; this
/// substrate ships the slot-tracking policy.
pub const SWAP_CHAIN_SLOTS: u8 = 3;

/// Slot identifier in `0..SWAP_CHAIN_SLOTS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SwapChainSlot(pub u8);

impl SwapChainSlot {
    /// Construct, returning `None` if `idx >= SWAP_CHAIN_SLOTS`.
    #[must_use]
    pub const fn try_new(idx: u8) -> Option<Self> {
        if idx < SWAP_CHAIN_SLOTS {
            Some(Self(idx))
        } else {
            None
        }
    }

    /// Next slot in round-robin order. Wraps at `SWAP_CHAIN_SLOTS - 1`.
    #[must_use]
    pub const fn next(self) -> Self {
        Self((self.0 + 1) % SWAP_CHAIN_SLOTS)
    }
}

/// Slot-rotation tracker. The integration's drawable-acquire pattern
/// (acquire next slot → render → present → release) reads
/// `current_slot()` then calls `advance()` after present.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SwapChainRotation {
    current: u8,
}

impl SwapChainRotation {
    #[must_use]
    pub const fn new() -> Self {
        Self { current: 0 }
    }

    #[must_use]
    pub const fn current_slot(&self) -> SwapChainSlot {
        SwapChainSlot(self.current % SWAP_CHAIN_SLOTS)
    }

    pub fn advance(&mut self) {
        self.current = (self.current + 1) % SWAP_CHAIN_SLOTS;
    }
}

// ============================================================================
// Backend stats
// ============================================================================

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BackendStats {
    pub frames_presented: u64,
    pub frames_skipped: u64,
    pub backend_switches: u64,
}

impl BackendStats {
    pub fn record_present(&mut self) {
        self.frames_presented = self.frames_presented.saturating_add(1);
    }

    pub fn record_skip(&mut self) {
        self.frames_skipped = self.frames_skipped.saturating_add(1);
    }

    pub fn record_switch(&mut self) {
        self.backend_switches = self.backend_switches.saturating_add(1);
    }

    /// Present rate as integer percent `[0..=100]`. `0` when no
    /// frames have been observed.
    #[must_use]
    pub fn present_rate_pct(&self) -> u32 {
        let total = self.frames_presented + self.frames_skipped;
        if total == 0 {
            return 0;
        }
        ((self.frames_presented * 100) / total).min(100) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(
        arch: MacosArch,
        major: u8,
        minor: u8,
        ovr: BackendOverride,
    ) -> BackendSelectionInputs {
        BackendSelectionInputs::new(arch, MacosVersion::new(major, minor), ovr)
    }

    // ----------------------------------------------------------------
    // MacosVersion.meets_baseline
    // ----------------------------------------------------------------

    #[test]
    fn version_below_13_does_not_meet_baseline() {
        assert!(!MacosVersion::new(12, 7).meets_baseline());
        assert!(!MacosVersion::new(11, 0).meets_baseline());
        assert!(!MacosVersion::new(10, 15).meets_baseline());
    }

    #[test]
    fn version_13_0_meets_baseline_exactly() {
        assert!(MacosVersion::new(13, 0).meets_baseline());
    }

    #[test]
    fn version_above_13_meets_baseline() {
        assert!(MacosVersion::new(13, 5).meets_baseline());
        assert!(MacosVersion::new(14, 0).meets_baseline());
        assert!(MacosVersion::new(15, 2).meets_baseline());
    }

    // ----------------------------------------------------------------
    // BackendOverride parsing
    // ----------------------------------------------------------------

    #[test]
    fn override_empty_is_auto() {
        assert_eq!(BackendOverride::from_env_str(""), BackendOverride::Auto);
        assert_eq!(BackendOverride::from_env_str("auto"), BackendOverride::Auto);
        assert_eq!(BackendOverride::from_env_str("AUTO"), BackendOverride::Auto);
        assert_eq!(
            BackendOverride::from_env_str("default"),
            BackendOverride::Auto
        );
    }

    #[test]
    fn override_wgpu_parses() {
        assert_eq!(BackendOverride::from_env_str("wgpu"), BackendOverride::Wgpu);
        assert_eq!(BackendOverride::from_env_str("WGPU"), BackendOverride::Wgpu);
        assert_eq!(
            BackendOverride::from_env_str("  wgpu  "),
            BackendOverride::Wgpu
        );
    }

    #[test]
    fn override_metal_direct_accepts_aliases() {
        for s in [
            "metal-direct",
            "metal_direct",
            "metaldirect",
            "metal",
            "Metal",
            "METAL",
        ] {
            assert_eq!(
                BackendOverride::from_env_str(s),
                BackendOverride::MetalDirect,
                "{s:?} should parse as MetalDirect"
            );
        }
    }

    #[test]
    fn override_unknown_value_falls_through_to_auto() {
        // Defensive: typo doesn't break startup.
        assert_eq!(
            BackendOverride::from_env_str("vulkan"),
            BackendOverride::Auto
        );
        assert_eq!(BackendOverride::from_env_str("xyz"), BackendOverride::Auto);
    }

    // ----------------------------------------------------------------
    // select_macos_backend — happy path
    // ----------------------------------------------------------------

    #[test]
    fn select_metal_direct_on_apple_silicon_macos_13_no_override() {
        let r = select_macos_backend(inputs(
            MacosArch::AppleSilicon,
            13,
            0,
            BackendOverride::Auto,
        ));
        assert_eq!(r.backend, MacosBackend::MetalDirect);
        assert_eq!(r.reason, BackendFallbackReason::MetalDirectGranted);
        assert!(r.is_metal_direct());
        assert!(!r.is_fallback());
    }

    #[test]
    fn select_metal_direct_on_apple_silicon_macos_15() {
        let r = select_macos_backend(inputs(
            MacosArch::AppleSilicon,
            15,
            2,
            BackendOverride::Auto,
        ));
        assert_eq!(r.backend, MacosBackend::MetalDirect);
        assert_eq!(r.reason, BackendFallbackReason::MetalDirectGranted);
    }

    // ----------------------------------------------------------------
    // select_macos_backend — automatic fallbacks
    // ----------------------------------------------------------------

    #[test]
    fn select_wgpu_on_intel_arch() {
        let r = select_macos_backend(inputs(MacosArch::IntelX64, 14, 0, BackendOverride::Auto));
        assert_eq!(r.backend, MacosBackend::Wgpu);
        assert_eq!(r.reason, BackendFallbackReason::IntelArch);
        assert!(r.is_fallback());
    }

    #[test]
    fn select_wgpu_on_pre_baseline_version() {
        let r = select_macos_backend(inputs(
            MacosArch::AppleSilicon,
            12,
            7,
            BackendOverride::Auto,
        ));
        assert_eq!(r.backend, MacosBackend::Wgpu);
        assert_eq!(r.reason, BackendFallbackReason::PreBaselineVersion);
    }

    #[test]
    fn select_wgpu_on_unknown_arch() {
        let r = select_macos_backend(inputs(MacosArch::Unknown, 14, 0, BackendOverride::Auto));
        assert_eq!(r.backend, MacosBackend::Wgpu);
        assert_eq!(r.reason, BackendFallbackReason::UnknownArch);
    }

    // ----------------------------------------------------------------
    // select_macos_backend — operator override
    // ----------------------------------------------------------------

    #[test]
    fn override_wgpu_wins_on_apple_silicon() {
        let r = select_macos_backend(inputs(
            MacosArch::AppleSilicon,
            14,
            0,
            BackendOverride::Wgpu,
        ));
        assert_eq!(r.backend, MacosBackend::Wgpu);
        assert_eq!(r.reason, BackendFallbackReason::OperatorOverrideWgpu);
    }

    #[test]
    fn override_metal_direct_granted_on_apple_silicon_13() {
        let r = select_macos_backend(inputs(
            MacosArch::AppleSilicon,
            13,
            0,
            BackendOverride::MetalDirect,
        ));
        assert_eq!(r.backend, MacosBackend::MetalDirect);
        assert_eq!(r.reason, BackendFallbackReason::OperatorOverrideMetalDirect);
    }

    #[test]
    fn override_metal_direct_downgraded_on_intel() {
        let r = select_macos_backend(inputs(
            MacosArch::IntelX64,
            14,
            0,
            BackendOverride::MetalDirect,
        ));
        assert_eq!(r.backend, MacosBackend::Wgpu);
        assert_eq!(r.reason, BackendFallbackReason::OperatorOverrideDowngraded);
    }

    #[test]
    fn override_metal_direct_downgraded_on_pre_baseline() {
        let r = select_macos_backend(inputs(
            MacosArch::AppleSilicon,
            12,
            7,
            BackendOverride::MetalDirect,
        ));
        assert_eq!(r.backend, MacosBackend::Wgpu);
        assert_eq!(r.reason, BackendFallbackReason::OperatorOverrideDowngraded);
    }

    #[test]
    fn override_metal_direct_downgraded_on_unknown_arch() {
        let r = select_macos_backend(inputs(
            MacosArch::Unknown,
            14,
            0,
            BackendOverride::MetalDirect,
        ));
        assert_eq!(r.backend, MacosBackend::Wgpu);
        assert_eq!(r.reason, BackendFallbackReason::OperatorOverrideDowngraded);
    }

    // ----------------------------------------------------------------
    // SwapChainSlot
    // ----------------------------------------------------------------

    #[test]
    fn swap_chain_slot_count_matches_bead() {
        assert_eq!(SWAP_CHAIN_SLOTS, 3);
    }

    #[test]
    fn slot_try_new_in_range() {
        assert!(SwapChainSlot::try_new(0).is_some());
        assert!(SwapChainSlot::try_new(1).is_some());
        assert!(SwapChainSlot::try_new(2).is_some());
        assert!(SwapChainSlot::try_new(3).is_none());
        assert!(SwapChainSlot::try_new(255).is_none());
    }

    #[test]
    fn slot_next_wraps() {
        assert_eq!(SwapChainSlot(0).next(), SwapChainSlot(1));
        assert_eq!(SwapChainSlot(1).next(), SwapChainSlot(2));
        assert_eq!(SwapChainSlot(2).next(), SwapChainSlot(0));
    }

    #[test]
    fn rotation_starts_at_slot_zero() {
        let r = SwapChainRotation::new();
        assert_eq!(r.current_slot(), SwapChainSlot(0));
    }

    #[test]
    fn rotation_advance_cycles_through_three_slots() {
        let mut r = SwapChainRotation::new();
        let mut seen = Vec::new();
        for _ in 0..6 {
            seen.push(r.current_slot().0);
            r.advance();
        }
        assert_eq!(seen, vec![0, 1, 2, 0, 1, 2]);
    }

    // ----------------------------------------------------------------
    // BackendStats
    // ----------------------------------------------------------------

    #[test]
    fn stats_default_is_empty() {
        let s = BackendStats::default();
        assert_eq!(s.frames_presented, 0);
        assert_eq!(s.frames_skipped, 0);
        assert_eq!(s.backend_switches, 0);
        assert_eq!(s.present_rate_pct(), 0);
    }

    #[test]
    fn stats_record_present_skip_switch() {
        let mut s = BackendStats::default();
        s.record_present();
        s.record_present();
        s.record_skip();
        s.record_switch();
        assert_eq!(s.frames_presented, 2);
        assert_eq!(s.frames_skipped, 1);
        assert_eq!(s.backend_switches, 1);
    }

    #[test]
    fn stats_present_rate_full() {
        let mut s = BackendStats::default();
        for _ in 0..10 {
            s.record_present();
        }
        assert_eq!(s.present_rate_pct(), 100);
    }

    #[test]
    fn stats_present_rate_half() {
        let mut s = BackendStats::default();
        for _ in 0..5 {
            s.record_present();
            s.record_skip();
        }
        assert_eq!(s.present_rate_pct(), 50);
    }

    // ----------------------------------------------------------------
    // Cross-cut scenarios
    // ----------------------------------------------------------------

    #[test]
    fn scenario_apple_silicon_m2_ventura_default() {
        // M2 user on macOS 13.0 (Ventura) with no env override — the
        // bead's headline scenario.
        let r = select_macos_backend(inputs(
            MacosArch::AppleSilicon,
            13,
            0,
            BackendOverride::Auto,
        ));
        assert_eq!(r.backend, MacosBackend::MetalDirect);
        assert!(!r.is_fallback());
    }

    #[test]
    fn scenario_intel_imac_falls_back_to_wgpu() {
        let r = select_macos_backend(inputs(MacosArch::IntelX64, 14, 0, BackendOverride::Auto));
        assert_eq!(r.backend, MacosBackend::Wgpu);
        assert_eq!(r.reason, BackendFallbackReason::IntelArch);
    }

    #[test]
    fn scenario_operator_rolls_back_via_env_var() {
        // Operator hits a Metal-direct regression on M3; sets
        // FT_MACOS_BACKEND=wgpu to roll back without rebuild.
        let parsed = BackendOverride::from_env_str("wgpu");
        let r = select_macos_backend(inputs(MacosArch::AppleSilicon, 15, 0, parsed));
        assert_eq!(r.backend, MacosBackend::Wgpu);
        assert_eq!(r.reason, BackendFallbackReason::OperatorOverrideWgpu);
    }

    #[test]
    fn scenario_old_macos_user_gets_wgpu_silently() {
        // M1 user stuck on macOS 12.7 (Monterey, before baseline).
        // Selector picks wgpu without surprise.
        let r = select_macos_backend(inputs(
            MacosArch::AppleSilicon,
            12,
            7,
            BackendOverride::Auto,
        ));
        assert_eq!(r.backend, MacosBackend::Wgpu);
        assert_eq!(r.reason, BackendFallbackReason::PreBaselineVersion);
    }

    #[test]
    fn scenario_full_render_loop_with_swap_chain_rotation() {
        // Render 60 frames on Apple Silicon Metal-direct, advancing
        // the swap-chain rotation each frame. Stats reflect a clean
        // 100% present rate.
        let mut rotation = SwapChainRotation::new();
        let mut stats = BackendStats::default();
        for _ in 0..60 {
            let _slot = rotation.current_slot();
            // (integration would acquire drawable, render, present)
            stats.record_present();
            rotation.advance();
        }
        assert_eq!(stats.frames_presented, 60);
        assert_eq!(stats.present_rate_pct(), 100);
        // Rotation cycled 60 / 3 = 20 times; ends back at slot 0.
        assert_eq!(rotation.current_slot(), SwapChainSlot(0));
    }
}
