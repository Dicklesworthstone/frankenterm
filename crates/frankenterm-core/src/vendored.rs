//! Vendored WezTerm integration helpers.
//!
//! This module provides:
//! - Vendored build metadata (commit/version)
//! - Local WezTerm version parsing
//! - Compatibility classification (matched/compatible/incompatible)

use serde::{Deserialize, Serialize};

const LOCAL_WEZTERM_VERSION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const LOCAL_WEZTERM_VERSION_MAX_STDOUT_BYTES: usize = 512;
const LOCAL_WEZTERM_VERSION_MAX_STDERR_BYTES: usize = 16 * 1024;

#[cfg(all(feature = "vendored", unix))]
mod mux_client;
#[cfg(all(feature = "vendored", unix))]
pub use mux_client::subscribe_pane_output_with_inherited_cx;
#[cfg(all(feature = "vendored", unix))]
pub use mux_client::{
    DirectMuxClient, DirectMuxClientConfig, DirectMuxError, PaneDelta, PaneOutputSubscription,
    SubscriptionConfig, subscribe_pane_output,
};

#[cfg(all(feature = "vendored", unix))]
pub mod mux_pool;
#[cfg(all(feature = "vendored", unix))]
pub use mux_pool::{MuxPool, MuxPoolConfig, MuxPoolError, MuxPoolStats, MuxRecoveryConfig};

#[cfg(all(feature = "vendored", not(unix)))]
#[derive(Debug, thiserror::Error)]
pub enum DirectMuxError {
    #[error("direct mux client is only supported on unix platforms")]
    UnsupportedPlatform,
}

#[cfg(all(feature = "vendored", not(unix)))]
impl DirectMuxError {
    /// Return the canonical recovery decision for the non-Unix stub error.
    #[must_use]
    pub fn recovery_decision(&self) -> crate::protocol_recovery::MuxRecoveryDecision {
        crate::protocol_recovery::mux_recovery_decision(self)
    }

    /// Whether this error represents an explicit capability-context cancellation.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.recovery_decision().cancelled
    }

    /// Project the canonical multi-axis recovery decision to its diagnostic kind.
    #[must_use]
    pub fn protocol_error_kind(&self) -> crate::protocol_recovery::ProtocolErrorKind {
        crate::protocol_recovery::classify_mux_error(self)
    }
}

#[cfg(all(feature = "vendored", not(unix)))]
#[derive(Debug, Clone, Default)]
pub struct DirectMuxClientConfig;

#[cfg(all(feature = "vendored", not(unix)))]
impl DirectMuxClientConfig {
    pub fn from_wa_config(_config: &crate::config::Config) -> Self {
        Self
    }
}

#[cfg(all(feature = "vendored", not(unix)))]
pub struct DirectMuxClient;

#[cfg(all(feature = "vendored", not(unix)))]
impl DirectMuxClient {
    pub async fn connect(_config: DirectMuxClientConfig) -> Result<Self, DirectMuxError> {
        // ft-tr5a0: ergonomic wrapper around `connect_with_cx`.
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        Self::connect_with_cx(&cx, _config).await
    }

    /// ft-tr5a0 Cx-first sibling of [`Self::connect`].
    pub async fn connect_with_cx(
        _cx: &crate::cx::Cx,
        _config: DirectMuxClientConfig,
    ) -> Result<Self, DirectMuxError> {
        Err(DirectMuxError::UnsupportedPlatform)
    }
}

// Windows / non-unix shim for PaneDelta. The real enum lives in the unix-only
// mux_client submodule and is produced exclusively by the Unix-socket
// subscription path, which is unavailable on Windows. The shim mirrors the
// variant shape so crate::vendored-consuming code (e.g. tailer.rs) type-checks;
// no value of this type is ever constructed on Windows.
#[cfg(all(feature = "vendored", not(unix)))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneDelta {
    Output {
        pane_id: u64,
        seqno: u64,
        delta_text: String,
        title: String,
        dirty_range_count: usize,
        dirty_row_count: usize,
    },
    Gap {
        pane_id: u64,
        reason: String,
    },
    Ended {
        pane_id: u64,
        reason: String,
    },
}

// Windows / non-unix shim for the mux_pool module. The real module (and its
// MuxPool/MuxPoolError/etc.) is unix-only because it pools DirectMuxClient
// connections over Unix domain sockets. Only MuxPoolStats is referenced by
// platform-agnostic code (unified_telemetry::MuxPoolPayload), so the shim
// re-creates just that type with identical fields/derives. PoolStats is the
// real cross-platform type from crate::pool, keeping serialization identical.
#[cfg(all(feature = "vendored", not(unix)))]
pub mod mux_pool {
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct MuxPoolStats {
        pub pool: crate::pool::PoolStats,
        pub connections_created: u64,
        pub connections_failed: u64,
        pub health_checks: u64,
        pub health_check_failures: u64,
        pub recovery_attempts: u64,
        pub recovery_successes: u64,
        pub permanent_failures: u64,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeztermVersion {
    pub raw: String,
    pub commit: Option<String>,
}

impl WeztermVersion {
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        let raw = raw.trim().to_string();
        let commit = extract_commit(&raw);
        Self { raw, commit }
    }
}

#[derive(Debug, Clone, Default)]
pub struct VendoredWeztermMetadata {
    pub commit: Option<String>,
    pub version: Option<String>,
    pub source: Option<String>,
    pub enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VendoredCompatibilityStatus {
    Matched,
    Compatible,
    Incompatible,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VendoredCompatibilityReport {
    pub status: VendoredCompatibilityStatus,
    pub vendored_enabled: bool,
    pub allow_vendored: bool,
    pub local_version: Option<String>,
    pub local_commit: Option<String>,
    pub vendored_commit: Option<String>,
    pub vendored_version: Option<String>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recommendation: Option<String>,
}

/// Read vendored commit metadata embedded at build time.
#[must_use]
pub fn vendored_metadata() -> VendoredWeztermMetadata {
    VendoredWeztermMetadata {
        commit: option_env!("FT_WEZTERM_VENDORED_REV").map(|s| s.to_string()),
        version: option_env!("FT_WEZTERM_VENDORED_VERSION").map(|s| s.to_string()),
        source: option_env!("FT_WEZTERM_VENDORED_SOURCE").map(|s| s.to_string()),
        enabled: cfg!(feature = "vendored"),
    }
}

/// Attempt to read the local WezTerm version via a finite `wezterm --version`
/// probe.
///
/// Backend selection runs on interactive paths, so this probe must never wait
/// indefinitely or retain unbounded child output. Any timeout, cleanup
/// failure, oversized output, invalid UTF-8, non-success status, or malformed
/// multi-line/control-bearing response conservatively means that no compatible
/// local version was established.
pub fn read_local_wezterm_version() -> Option<WeztermVersion> {
    let mut command = crate::runtime_async::process::Command::new(crate::wezterm::wezterm_binary());
    command
        .arg("--version")
        .stdout_limit(LOCAL_WEZTERM_VERSION_MAX_STDOUT_BYTES)
        .stderr_limit(LOCAL_WEZTERM_VERSION_MAX_STDERR_BYTES);
    let output = command
        .output_blocking(LOCAL_WEZTERM_VERSION_TIMEOUT)
        .ok()?;
    if !output.status.success() {
        return None;
    }
    admit_local_wezterm_version(&output.stdout)
}

fn admit_local_wezterm_version(stdout: &[u8]) -> Option<WeztermVersion> {
    let version = std::str::from_utf8(stdout).ok()?.trim();
    if version.is_empty() || version.chars().any(char::is_control) {
        return None;
    }
    Some(WeztermVersion::parse(version))
}

/// Compute vendored compatibility classification from local version output.
#[must_use]
pub fn compatibility_report(local: Option<&WeztermVersion>) -> VendoredCompatibilityReport {
    compatibility_report_with(vendored_metadata(), local)
}

#[must_use]
pub fn compatibility_report_with(
    meta: VendoredWeztermMetadata,
    local: Option<&WeztermVersion>,
) -> VendoredCompatibilityReport {
    let vendored_enabled = meta.enabled;
    let vendored_commit = meta.commit.clone();
    let vendored_version = meta.version.clone();
    let local_version = local.map(|v| v.raw.clone());
    let local_commit = local.and_then(|v| v.commit.clone());

    if !vendored_enabled {
        return VendoredCompatibilityReport {
            status: VendoredCompatibilityStatus::Compatible,
            vendored_enabled,
            allow_vendored: false,
            local_version,
            local_commit,
            vendored_commit,
            vendored_version,
            message: "vendored feature not enabled; compatibility check skipped".to_string(),
            recommendation: Some(
                "Rebuild with --features vendored to enable vendored backend".to_string(),
            ),
        };
    }

    if vendored_commit.is_none() {
        return VendoredCompatibilityReport {
            status: VendoredCompatibilityStatus::Incompatible,
            vendored_enabled,
            allow_vendored: false,
            local_version,
            local_commit,
            vendored_commit,
            vendored_version,
            message: "vendored commit not recorded; refusing vendored backend until metadata is refreshed".to_string(),
            recommendation: Some("Rebuild ft to refresh vendored metadata".to_string()),
        };
    }

    // GH #78: an external `wezterm` binary is not what the vendored backend
    // talks to — the in-process client speaks to whatever mux server owns the
    // discovered socket, and real compatibility is enforced by the
    // `GetCodecVersion` handshake at connect time
    // (`DirectMuxClient::verify_codec_version_with_cx`), which fails closed.
    // The external CLI's version is therefore informational only and must
    // never veto the vendored backend. In particular, a stock install with no
    // WezTerm at all (only FrankenTerm.app) is the expected healthy state.
    if local_version.is_none() {
        return VendoredCompatibilityReport {
            status: VendoredCompatibilityStatus::Compatible,
            vendored_enabled,
            allow_vendored: true,
            local_version,
            local_commit,
            vendored_commit,
            vendored_version,
            message: "no external WezTerm CLI found; vendored backend uses its own build identity (codec version verified at connect time)"
                .to_string(),
            recommendation: None,
        };
    }

    let vendored_commit = vendored_commit.unwrap_or_default();

    if local_commit.is_none() {
        return VendoredCompatibilityReport {
            status: VendoredCompatibilityStatus::Compatible,
            vendored_enabled,
            allow_vendored: true,
            local_version,
            local_commit,
            vendored_commit: Some(vendored_commit),
            vendored_version,
            message: "external WezTerm CLI version has no parseable commit; it does not gate the vendored backend (codec version verified at connect time)"
                .to_string(),
            recommendation: None,
        };
    }

    let local_commit = local_commit.unwrap_or_default();
    if commit_matches(&vendored_commit, &local_commit) {
        return VendoredCompatibilityReport {
            status: VendoredCompatibilityStatus::Matched,
            vendored_enabled,
            allow_vendored: true,
            local_version,
            local_commit: Some(local_commit),
            vendored_commit: Some(vendored_commit),
            vendored_version,
            message: "local WezTerm commit matches vendored build".to_string(),
            recommendation: None,
        };
    }

    // Status stays `Incompatible` so `ft doctor` can still report that the
    // external CLI differs from the vendored build, but the difference does
    // not disable the vendored backend (GH #78): the connect-time codec
    // handshake is the authoritative compatibility gate.
    VendoredCompatibilityReport {
        status: VendoredCompatibilityStatus::Incompatible,
        vendored_enabled,
        allow_vendored: true,
        local_version,
        local_commit: Some(local_commit.clone()),
        vendored_commit: Some(vendored_commit.clone()),
        vendored_version,
        message: format!(
            "external WezTerm CLI commit {local_commit} differs from vendored {vendored_commit}; vendored backend selected anyway (codec version verified at connect time)"
        ),
        recommendation: Some(format!(
            "The external `wezterm` CLI is unrelated to the vendored backend; update it to {vendored_commit} only if you drive it directly"
        )),
    }
}

/// Socket path of the first unix domain in the vendored WezTerm configuration,
/// when one is configured without a proxy command.
///
/// Existence is deliberately not checked here; `wezterm::discover_mux_socket_ranked`
/// applies the usability filter so every source is judged the same way.
#[cfg(unix)]
#[must_use]
pub fn configured_unix_domain_socket() -> Option<std::path::PathBuf> {
    use config as wezterm_config;

    let handle = wezterm_config::configuration_result()
        .unwrap_or_else(|_| wezterm_config::ConfigHandle::default_config());
    let domain = handle.unix_domains.first()?;
    domain.proxy_command.is_none().then(|| domain.socket_path())
}

/// Socket path of the default unix domain (`<runtime dir>/sock`), which is
/// where a headless `frankenterm-mux-server` listens. Existence is not checked.
#[cfg(unix)]
#[must_use]
pub fn default_unix_domain_socket() -> Option<std::path::PathBuf> {
    use config as wezterm_config;

    wezterm_config::UnixDomain::default_unix_domains()
        .pop()
        .map(|domain| domain.socket_path())
}

fn commit_matches(vendored: &str, local: &str) -> bool {
    vendored.starts_with(local) || local.starts_with(vendored)
}

fn extract_commit(raw: &str) -> Option<String> {
    let mut candidate: Option<&str> = None;
    for token in raw.split(|c: char| !c.is_ascii_hexdigit()) {
        if token.len() < 7 {
            continue;
        }
        if !token
            .chars()
            .any(|c| c.is_ascii_hexdigit() && !c.is_ascii_digit())
        {
            continue;
        }
        candidate = Some(token);
    }
    candidate.map(|c| c.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta_with(commit: Option<&str>, enabled: bool) -> VendoredWeztermMetadata {
        VendoredWeztermMetadata {
            commit: commit.map(str::to_string),
            version: Some("0.1.0".to_string()),
            source: None,
            enabled,
        }
    }

    #[test]
    fn parse_nightly_wezterm_version() {
        let version = WeztermVersion::parse("wezterm 20240203-110809-5046fc22");
        assert_eq!(version.commit.as_deref(), Some("5046fc22"));
    }

    #[test]
    fn parse_wezterm_version_with_suffix() {
        let version = WeztermVersion::parse("wezterm 20240203-110809-5046fc22 (foo)");
        assert_eq!(version.commit.as_deref(), Some("5046fc22"));
    }

    #[test]
    fn parse_wezterm_version_without_hash() {
        let version = WeztermVersion::parse("wezterm 20240203");
        assert!(version.commit.is_none());
    }

    #[test]
    fn compatibility_matched() {
        let meta = meta_with(Some("abcdef12"), true);
        let local = WeztermVersion::parse("wezterm 20240101-123456-abcdef12");
        let report = compatibility_report_with(meta, Some(&local));
        assert_eq!(report.status, VendoredCompatibilityStatus::Matched);
        assert!(report.allow_vendored);
    }

    /// GH #78 regression: a mismatched external `wezterm` CLI is reported as
    /// `Incompatible` for observability, but must not veto the vendored
    /// backend — codec compatibility is enforced at connect time.
    #[test]
    fn compatibility_mismatched_external_cli_still_allows_vendored() {
        let meta = meta_with(Some("abcdef12"), true);
        let local = WeztermVersion::parse("wezterm 20240101-123456-deadbeef");
        let report = compatibility_report_with(meta, Some(&local));
        assert_eq!(report.status, VendoredCompatibilityStatus::Incompatible);
        assert!(report.allow_vendored);
        assert!(report.message.contains("codec version verified"));
    }

    /// GH #78 regression: a stock install with no external WezTerm binary at
    /// all (only FrankenTerm.app) must be allowed to use the vendored
    /// backend — absence of an irrelevant binary is not incompatibility.
    #[test]
    fn compatibility_missing_local_allows_vendored() {
        let meta = meta_with(Some("abcdef12"), true);
        let report = compatibility_report_with(meta, None);
        assert_eq!(report.status, VendoredCompatibilityStatus::Compatible);
        assert!(report.allow_vendored);
        assert!(report.message.contains("no external WezTerm CLI found"));
    }

    /// GH #78 regression: an external CLI whose version has no parseable
    /// commit hash is informational only and does not gate the backend.
    #[test]
    fn compatibility_unparseable_local_commit_allows_vendored() {
        let meta = meta_with(Some("abcdef12"), true);
        let local = WeztermVersion::parse("wezterm 20240203");
        let report = compatibility_report_with(meta, Some(&local));
        assert_eq!(report.status, VendoredCompatibilityStatus::Compatible);
        assert!(report.allow_vendored);
    }

    #[test]
    fn compatibility_disabled_feature() {
        let meta = meta_with(Some("abcdef12"), false);
        let local = WeztermVersion::parse("wezterm 20240101-123456-abcdef12");
        let report = compatibility_report_with(meta, Some(&local));
        assert_eq!(report.status, VendoredCompatibilityStatus::Compatible);
        assert!(!report.allow_vendored);
    }

    #[test]
    fn vendored_metadata_returns_struct() {
        let meta = vendored_metadata();
        assert!(meta.commit.is_some() || meta.commit.is_none());
        assert_eq!(meta.enabled, cfg!(feature = "vendored"));
    }

    #[test]
    fn commit_prefix_matching_works_both_directions() {
        assert!(commit_matches("abcdef1234567890", "abcdef12"));
        assert!(commit_matches("abcdef12", "abcdef1234567890"));
        assert!(commit_matches("abcdef12", "abcdef12"));
        assert!(!commit_matches("abcdef12", "deadbeef"));
    }

    #[test]
    fn extract_commit_ignores_pure_numeric_tokens() {
        assert!(extract_commit("20240203-110809").is_none());
        assert_eq!(
            extract_commit("20240203-110809-5046fc22").as_deref(),
            Some("5046fc22")
        );
    }

    #[test]
    fn extract_commit_handles_git_source_urls() {
        let source = "git+https://github.com/wez/wezterm#05343b387085842b434d267f91b6b0ec157e4331";
        assert_eq!(
            extract_commit(source).as_deref(),
            Some("05343b387085842b434d267f91b6b0ec157e4331")
        );
    }

    #[test]
    fn extract_commit_returns_none_for_empty_hash() {
        assert!(extract_commit("git+https://github.com/wez/wezterm#").is_none());
        assert!(extract_commit("no-hash-here").is_none());
    }

    #[test]
    fn compatibility_no_vendored_commit_recorded_disables_vendored() {
        let meta = meta_with(None, true);
        let local = WeztermVersion::parse("wezterm 20240101-123456-abcdef12");
        let report = compatibility_report_with(meta, Some(&local));
        assert_eq!(report.status, VendoredCompatibilityStatus::Incompatible);
        assert!(!report.allow_vendored);
        assert!(report.message.contains("not recorded"));
    }

    /// GH #78: an external CLI version without a parseable commit is
    /// informational only; the message says so explicitly.
    #[test]
    fn compatibility_local_version_without_commit_message_is_informational() {
        let meta = meta_with(Some("abcdef12"), true);
        let local = WeztermVersion::parse("wezterm 20240203");
        let report = compatibility_report_with(meta, Some(&local));
        assert_eq!(report.status, VendoredCompatibilityStatus::Compatible);
        assert!(report.allow_vendored);
        assert!(report.message.contains("no parseable commit"));
    }

    #[test]
    fn compatibility_report_json_stable() {
        let meta = meta_with(Some("abcdef12"), true);
        let local = WeztermVersion::parse("wezterm 20240101-123456-abcdef12");
        let report = compatibility_report_with(meta, Some(&local));
        let json = serde_json::to_value(&report).expect("report should serialize");
        assert_eq!(json["status"], "matched");
        assert_eq!(json["vendored_enabled"], true);
        assert_eq!(json["allow_vendored"], true);
        assert!(json["message"].as_str().unwrap().contains("matches"));
    }

    #[test]
    fn incompatible_report_json_includes_recommendation() {
        let meta = meta_with(Some("abcdef12"), true);
        let local = WeztermVersion::parse("wezterm 20240101-123456-deadbeef");
        let report = compatibility_report_with(meta, Some(&local));
        let json = serde_json::to_value(&report).expect("report should serialize");
        assert_eq!(json["status"], "incompatible");
        // GH #78: the mismatch is reported for observability but no longer
        // vetoes the vendored backend.
        assert_eq!(json["allow_vendored"], true);
        assert!(
            json["recommendation"]
                .as_str()
                .unwrap()
                .contains("unrelated to the vendored backend")
        );
        assert_eq!(json["local_commit"], "deadbeef");
        assert_eq!(json["vendored_commit"], "abcdef12");
    }

    #[test]
    fn disabled_feature_report_json() {
        let meta = meta_with(Some("abcdef12"), false);
        let report = compatibility_report_with(meta, None);
        let json = serde_json::to_value(&report).expect("report should serialize");
        assert_eq!(json["status"], "compatible");
        assert_eq!(json["vendored_enabled"], false);
        assert_eq!(json["allow_vendored"], false);
    }

    #[test]
    fn parse_various_wezterm_formats() {
        let v = WeztermVersion::parse("wezterm 20240203-110809-5046fc22");
        assert_eq!(v.commit.as_deref(), Some("5046fc22"));
        let v = WeztermVersion::parse("wezterm 20240203-110809-5046fc22 (Ubuntu 24.04)");
        assert_eq!(v.commit.as_deref(), Some("5046fc22"));
        let v = WeztermVersion::parse("wezterm-gui 0.0.0+05343b387085");
        assert_eq!(v.commit.as_deref(), Some("05343b387085"));
        let v = WeztermVersion::parse("wezterm 20240101");
        assert!(v.commit.is_none());
        let v = WeztermVersion::parse("");
        assert!(v.commit.is_none());
    }

    #[test]
    fn local_version_probe_admission_is_strict_and_single_line() {
        let version = admit_local_wezterm_version(b"wezterm 20240203-110809-5046fc22\n")
            .expect("valid bounded version");
        assert_eq!(version.commit.as_deref(), Some("5046fc22"));
        assert!(admit_local_wezterm_version(b"").is_none());
        assert!(admit_local_wezterm_version(b"wezterm\nforged").is_none());
        assert!(admit_local_wezterm_version(&[0xff]).is_none());
    }

    #[test]
    fn compatibility_all_status_variants_serialize() {
        for status in [
            VendoredCompatibilityStatus::Matched,
            VendoredCompatibilityStatus::Compatible,
            VendoredCompatibilityStatus::Incompatible,
        ] {
            let json = serde_json::to_string(&status).expect("serialize status");
            let back: VendoredCompatibilityStatus =
                serde_json::from_str(&json).expect("deserialize status");
            assert_eq!(back, status);
        }
    }

    #[test]
    fn compatibility_report_full_roundtrip() {
        let meta = meta_with(Some("abcdef12"), true);
        let local = WeztermVersion::parse("wezterm 20240101-123456-abcdef12");
        let report = compatibility_report_with(meta, Some(&local));
        let json_str = serde_json::to_string(&report).expect("serialize report");
        let back: VendoredCompatibilityReport =
            serde_json::from_str(&json_str).expect("deserialize report");
        assert_eq!(back.status, report.status);
        assert_eq!(back.allow_vendored, report.allow_vendored);
        assert_eq!(back.vendored_commit, report.vendored_commit);
        assert_eq!(back.local_commit, report.local_commit);
    }

    #[test]
    fn compatibility_recommendation_absent_on_match() {
        let meta = meta_with(Some("abcdef12"), true);
        let local = WeztermVersion::parse("wezterm 20240101-123456-abcdef12");
        let report = compatibility_report_with(meta, Some(&local));
        assert!(report.recommendation.is_none());
    }

    #[test]
    fn vendored_metadata_enabled_reflects_feature() {
        let meta = vendored_metadata();
        assert_eq!(meta.enabled, cfg!(feature = "vendored"));
    }

    // --- Additional coverage: vendored expanded tests ---

    #[test]
    fn wezterm_version_parse_preserves_raw() {
        let raw = "wezterm 20240203-110809-5046fc22";
        let v = WeztermVersion::parse(raw);
        assert_eq!(v.raw, raw);
    }

    #[test]
    fn wezterm_version_equality() {
        let v1 = WeztermVersion::parse("wezterm 20240203-110809-5046fc22");
        let v2 = WeztermVersion::parse("wezterm 20240203-110809-5046fc22");
        assert_eq!(v1, v2);
    }

    #[test]
    fn wezterm_version_inequality() {
        let v1 = WeztermVersion::parse("wezterm 20240203-110809-5046fc22");
        let v2 = WeztermVersion::parse("wezterm 20240203-110809-deadbeef");
        assert_ne!(v1, v2);
    }

    #[test]
    fn extract_commit_lowercase_normalization() {
        let commit = extract_commit("wezterm 20240203-110809-ABCDEF12");
        assert_eq!(commit.as_deref(), Some("abcdef12"));
    }

    #[test]
    fn extract_commit_short_tokens_ignored() {
        assert!(extract_commit("abc12").is_none());
        assert!(extract_commit("ab1c2d").is_none());
    }

    #[test]
    fn vendored_metadata_default_fields() {
        let meta = VendoredWeztermMetadata::default();
        assert!(!meta.enabled);
        assert!(meta.commit.is_none());
        assert!(meta.version.is_none());
        assert!(meta.source.is_none());
    }

    #[test]
    fn compatibility_incompatible_message_contains_commits() {
        let meta = meta_with(Some("aabbccdd"), true);
        // Local commit must contain at least one a-f hex char for extract_commit
        let local = WeztermVersion::parse("wezterm 20240101-123456-ff223344");
        let report = compatibility_report_with(meta, Some(&local));
        assert_eq!(report.status, VendoredCompatibilityStatus::Incompatible);
        assert!(report.message.contains("ff223344"));
        assert!(report.message.contains("aabbccdd"));
    }

    #[test]
    fn meta_with_helper_sets_version() {
        let meta = meta_with(Some("abc1234d"), true);
        assert_eq!(meta.version.as_deref(), Some("0.1.0"));
        assert_eq!(meta.commit.as_deref(), Some("abc1234d"));
        assert!(meta.enabled);
    }

    #[test]
    fn compatibility_status_clone_and_eq() {
        let s1 = VendoredCompatibilityStatus::Matched;
        let s2 = s1;
        assert_eq!(s1, s2);
    }

    #[test]
    fn wezterm_version_parse_trims_whitespace() {
        let v = WeztermVersion::parse("  wezterm 20240203-110809-5046fc22  ");
        assert_eq!(v.raw, "wezterm 20240203-110809-5046fc22");
        assert_eq!(v.commit.as_deref(), Some("5046fc22"));
    }

    #[cfg(all(feature = "vendored", not(unix)))]
    #[test]
    fn unsupported_platform_uses_canonical_mux_recovery_authority() {
        use crate::protocol_recovery::{MuxConnectionDisposition, ProtocolErrorKind};

        let error = DirectMuxError::UnsupportedPlatform;
        let decision = error.recovery_decision();
        assert_eq!(decision.kind, ProtocolErrorKind::Permanent);
        assert!(!decision.retry);
        assert_eq!(decision.connection, MuxConnectionDisposition::Discard);
        assert!(!decision.cancelled);
        assert!(!error.is_cancelled());
        assert_eq!(error.protocol_error_kind(), ProtocolErrorKind::Permanent);
        assert_eq!(
            crate::protocol_recovery::classify_mux_error(&error),
            ProtocolErrorKind::Permanent
        );
    }
}
