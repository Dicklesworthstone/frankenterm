//! Vendored WezTerm integration helpers.
//!
//! This module provides:
//! - Vendored build metadata (commit/version)
//! - Local WezTerm version parsing
//! - Compatibility classification (matched/compatible/incompatible)

use serde::{Deserialize, Serialize};
use std::process::Command;

#[cfg(all(feature = "vendored", unix))]
mod mux_client;
#[cfg(all(feature = "vendored", unix))]
pub use mux_client::{
    DirectMuxClient, DirectMuxClientConfig, DirectMuxError, PaneDelta, PaneOutputSubscription,
    SubscriptionConfig, subscribe_pane_output,
};

#[cfg(all(feature = "vendored", not(unix)))]
#[derive(Debug, thiserror::Error)]
pub enum DirectMuxError {
    #[error("direct mux client is only supported on unix platforms")]
    UnsupportedPlatform,
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
        Err(DirectMuxError::UnsupportedPlatform)
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
        commit: option_env!("WA_WEZTERM_VENDORED_REV").map(|s| s.to_string()),
        version: option_env!("WA_WEZTERM_VENDORED_VERSION").map(|s| s.to_string()),
        source: option_env!("WA_WEZTERM_VENDORED_SOURCE").map(|s| s.to_string()),
        enabled: cfg!(feature = "vendored"),
    }
}

/// Attempt to read the local WezTerm version via `wezterm --version`.
pub fn read_local_wezterm_version() -> Option<WeztermVersion> {
    let output = Command::new("wezterm").arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if version.is_empty() {
        return None;
    }
    Some(WeztermVersion::parse(&version))
}

/// Compute vendored compatibility classification from local version output.
#[must_use]
pub fn compatibility_report(local: Option<&WeztermVersion>) -> VendoredCompatibilityReport {
    compatibility_report_with(vendored_metadata(), local)
}

fn compatibility_report_with(
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
            status: VendoredCompatibilityStatus::Compatible,
            vendored_enabled,
            allow_vendored: true,
            local_version,
            local_commit,
            vendored_commit,
            vendored_version,
            message: "vendored commit not recorded; assuming compatible".to_string(),
            recommendation: Some("Rebuild wa to refresh vendored metadata".to_string()),
        };
    }

    if local_version.is_none() {
        return VendoredCompatibilityReport {
            status: VendoredCompatibilityStatus::Compatible,
            vendored_enabled,
            allow_vendored: true,
            local_version,
            local_commit,
            vendored_commit,
            vendored_version,
            message: "local WezTerm version unavailable; assuming compatible".to_string(),
            recommendation: Some(
                "Install WezTerm or ensure the wezterm binary is on PATH".to_string(),
            ),
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
            message: "unable to parse commit from local WezTerm version; assuming compatible"
                .to_string(),
            recommendation: Some(
                "Use a WezTerm build that includes a commit hash in --version".to_string(),
            ),
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

    VendoredCompatibilityReport {
        status: VendoredCompatibilityStatus::Incompatible,
        vendored_enabled,
        allow_vendored: false,
        local_version,
        local_commit: Some(local_commit.clone()),
        vendored_commit: Some(vendored_commit.clone()),
        vendored_version,
        message: format!(
            "local WezTerm commit {local_commit} does not match vendored {vendored_commit}"
        ),
        recommendation: Some(format!(
            "Update WezTerm to {vendored_commit} or rebuild wa with matching vendored commit"
        )),
    }
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

    #[test]
    fn compatibility_incompatible_disables_vendored() {
        let meta = meta_with(Some("abcdef12"), true);
        let local = WeztermVersion::parse("wezterm 20240101-123456-deadbeef");
        let report = compatibility_report_with(meta, Some(&local));
        assert_eq!(report.status, VendoredCompatibilityStatus::Incompatible);
        assert!(!report.allow_vendored);
        assert!(
            report
                .recommendation
                .as_deref()
                .unwrap_or("")
                .contains("Update WezTerm")
        );
    }

    #[test]
    fn compatibility_missing_local_is_warning() {
        let meta = meta_with(Some("abcdef12"), true);
        let report = compatibility_report_with(meta, None);
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

    // --- Build metadata tests (vendored maintenance plan wa-nu4.4.1.7) ---

    #[test]
    fn vendored_metadata_returns_struct() {
        let meta = vendored_metadata();
        // Whether feature is enabled depends on build flags; struct should always construct
        assert!(meta.commit.is_some() || meta.commit.is_none()); // non-panic check
        assert_eq!(meta.enabled, cfg!(feature = "vendored"));
    }

    #[test]
    fn commit_prefix_matching_works_both_directions() {
        // Short local prefix matches long vendored
        assert!(commit_matches("abcdef1234567890", "abcdef12"));
        // Long local matches short vendored prefix
        assert!(commit_matches("abcdef12", "abcdef1234567890"));
        // Exact match
        assert!(commit_matches("abcdef12", "abcdef12"));
        // No match
        assert!(!commit_matches("abcdef12", "deadbeef"));
    }

    #[test]
    fn extract_commit_ignores_pure_numeric_tokens() {
        // Date-like tokens (pure digits) should not be treated as commits
        assert!(extract_commit("20240203-110809").is_none());
        // But hex-containing tokens should work
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
    fn compatibility_no_vendored_commit_recorded() {
        let meta = meta_with(None, true);
        let local = WeztermVersion::parse("wezterm 20240101-123456-abcdef12");
        let report = compatibility_report_with(meta, Some(&local));
        assert_eq!(report.status, VendoredCompatibilityStatus::Compatible);
        assert!(report.allow_vendored);
        assert!(report.message.contains("not recorded"));
    }

    #[test]
    fn compatibility_local_version_without_commit() {
        let meta = meta_with(Some("abcdef12"), true);
        let local = WeztermVersion::parse("wezterm 20240203");
        let report = compatibility_report_with(meta, Some(&local));
        assert_eq!(report.status, VendoredCompatibilityStatus::Compatible);
        assert!(report.allow_vendored);
        assert!(report.message.contains("unable to parse commit"));
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
        assert!(
            json["recommendation"]
                .as_str()
                .unwrap()
                .contains("Update WezTerm")
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
        // Standard nightly
        let v = WeztermVersion::parse("wezterm 20240203-110809-5046fc22");
        assert_eq!(v.commit.as_deref(), Some("5046fc22"));

        // With parenthesized suffix
        let v = WeztermVersion::parse("wezterm 20240203-110809-5046fc22 (Ubuntu 24.04)");
        assert_eq!(v.commit.as_deref(), Some("5046fc22"));

        // Development build with long hash
        let v = WeztermVersion::parse("wezterm-gui 0.0.0+05343b387085");
        assert_eq!(v.commit.as_deref(), Some("05343b387085"));

        // Release with no hash
        let v = WeztermVersion::parse("wezterm 20240101");
        assert!(v.commit.is_none());

        // Empty string
        let v = WeztermVersion::parse("");
        assert!(v.commit.is_none());
    }

    // --- Vendored test consolidation (wa-nu4.4.1.5) ---

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
        // When compiled with vendored feature, enabled should be true; otherwise false.
        // We can't control build features in test, but we verify consistency.
        assert_eq!(meta.enabled, cfg!(feature = "vendored"));
    }
}
