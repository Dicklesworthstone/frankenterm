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
pub use mux_client::{DirectMuxClient, DirectMuxClientConfig, DirectMuxError};

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
}
