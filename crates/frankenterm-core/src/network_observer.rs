//! Network attribution observer — bridges FrankenTerm ↔ `rano` CLI.
//!
//! Provides network connection attribution (provider, region, latency)
//! and connectivity checks via the `rano` subprocess. Maps high latency
//! or unreachable state to backpressure tier signals.

use std::process::Command;

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

// =============================================================================
// Types
// =============================================================================

/// Network connection attribution result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkAttribution {
    /// Cloud provider or ISP name.
    pub provider: String,
    /// Geographic region or data center.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// Round-trip latency in milliseconds.
    pub latency_ms: f64,
    /// Whether the remote is on a trusted/known network.
    #[serde(default)]
    pub is_trusted: bool,
    /// Remote address that was attributed.
    pub remote_addr: String,
    /// ASN number if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asn: Option<u32>,
    /// Organization name if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org: Option<String>,
}

/// Connectivity check result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectivityStatus {
    /// Fully connected with normal latency.
    Connected,
    /// Connected but with degraded performance.
    Degraded,
    /// Unable to reach the target.
    Unreachable,
    /// Check was not performed (tool unavailable).
    Unknown,
}

impl std::fmt::Display for ConnectivityStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connected => write!(f, "connected"),
            Self::Degraded => write!(f, "degraded"),
            Self::Unreachable => write!(f, "unreachable"),
            Self::Unknown => write!(f, "unknown"),
        }
    }
}

/// Backpressure signal derived from network state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkPressureTier {
    /// Normal: latency < threshold, connected.
    Green,
    /// Elevated: latency above warning threshold.
    Yellow,
    /// Critical: latency above critical threshold or degraded.
    Red,
    /// Unreachable or tool unavailable.
    Black,
}

impl std::fmt::Display for NetworkPressureTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Green => write!(f, "green"),
            Self::Yellow => write!(f, "yellow"),
            Self::Red => write!(f, "red"),
            Self::Black => write!(f, "black"),
        }
    }
}

/// Configuration for network observer thresholds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkObserverConfig {
    /// Latency threshold (ms) for Yellow tier.
    #[serde(default = "default_yellow_latency")]
    pub yellow_latency_ms: f64,
    /// Latency threshold (ms) for Red tier.
    #[serde(default = "default_red_latency")]
    pub red_latency_ms: f64,
    /// Subprocess timeout.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_yellow_latency() -> f64 {
    100.0
}

fn default_red_latency() -> f64 {
    500.0
}

fn default_timeout_secs() -> u64 {
    10
}

impl Default for NetworkObserverConfig {
    fn default() -> Self {
        Self {
            yellow_latency_ms: default_yellow_latency(),
            red_latency_ms: default_red_latency(),
            timeout_secs: default_timeout_secs(),
        }
    }
}

// =============================================================================
// Observer
// =============================================================================

/// Network observer that wraps the `rano` CLI for attribution and monitoring.
/// Error type for network observer operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkObserverError {
    /// The rano binary was not found.
    BinaryNotFound(String),
    /// Subprocess exited with non-zero code.
    SubprocessFailed { code: Option<i32>, stderr: String },
    /// JSON parse failure.
    ParseFailed(String),
}

impl std::fmt::Display for NetworkObserverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BinaryNotFound(msg) => write!(f, "rano not found: {}", msg),
            Self::SubprocessFailed { code, stderr } => {
                write!(f, "rano failed (exit {}): {}", code.unwrap_or(-1), stderr)
            }
            Self::ParseFailed(msg) => write!(f, "rano parse error: {}", msg),
        }
    }
}

impl std::error::Error for NetworkObserverError {}

/// Network observer that wraps the `rano` CLI for attribution and monitoring.
#[derive(Debug, Clone)]
pub struct NetworkObserver {
    binary: String,
    config: NetworkObserverConfig,
}

impl NetworkObserver {
    /// Create a new observer with default config.
    pub fn new() -> Self {
        Self::with_config(NetworkObserverConfig::default())
    }

    /// Create with custom config.
    pub fn with_config(config: NetworkObserverConfig) -> Self {
        Self {
            binary: "rano".to_string(),
            config,
        }
    }

    /// Create with a custom binary path.
    pub fn with_binary(binary: impl Into<String>, config: NetworkObserverConfig) -> Self {
        Self {
            binary: binary.into(),
            config,
        }
    }

    /// Check if `rano` is available.
    pub fn is_available(&self) -> bool {
        Command::new(&self.binary)
            .arg("--version")
            .output()
            .is_ok()
    }

    /// Access the config.
    pub fn config(&self) -> &NetworkObserverConfig {
        &self.config
    }

    /// Attribute a remote network connection.
    pub fn attribute_connection(
        &self,
        remote_addr: &str,
    ) -> Result<NetworkAttribution, NetworkObserverError> {
        debug!(bridge = "rano", remote = %remote_addr, "attributing connection");

        let output = self.run_rano(&["attribute", remote_addr, "--json"])?;
        let attr: NetworkAttribution = serde_json::from_str(&output)
            .map_err(|e| NetworkObserverError::ParseFailed(e.to_string()))?;

        debug!(
            bridge = "rano",
            remote = %remote_addr,
            provider = %attr.provider,
            latency_ms = %attr.latency_ms,
            "connection attributed"
        );

        Ok(attr)
    }

    /// Check connectivity status.
    pub fn check_connectivity(&self) -> ConnectivityStatus {
        match self.run_rano(&["check", "--json"]) {
            Ok(output) => {
                let val: serde_json::Value = match serde_json::from_str(&output) {
                    Ok(v) => v,
                    Err(_) => return ConnectivityStatus::Unknown,
                };
                let status_str = val
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                match status_str {
                    "connected" => ConnectivityStatus::Connected,
                    "degraded" => ConnectivityStatus::Degraded,
                    "unreachable" => ConnectivityStatus::Unreachable,
                    _ => ConnectivityStatus::Unknown,
                }
            }
            Err(e) => {
                warn!(bridge = "rano", error = %e, "connectivity check failed");
                ConnectivityStatus::Unknown
            }
        }
    }

    /// Run a rano subprocess and return stdout.
    fn run_rano(&self, args: &[&str]) -> Result<String, NetworkObserverError> {
        let output = Command::new(&self.binary)
            .args(args)
            .output()
            .map_err(|e| {
                NetworkObserverError::BinaryNotFound(format!("{}: {}", self.binary, e))
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            return Err(NetworkObserverError::SubprocessFailed {
                code: output.status.code(),
                stderr,
            });
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Map an attribution to a backpressure tier.
    pub fn classify_pressure(&self, attr: &NetworkAttribution) -> NetworkPressureTier {
        if attr.latency_ms >= self.config.red_latency_ms {
            NetworkPressureTier::Red
        } else if attr.latency_ms >= self.config.yellow_latency_ms {
            NetworkPressureTier::Yellow
        } else {
            NetworkPressureTier::Green
        }
    }

    /// Map a connectivity status to a backpressure tier.
    pub fn classify_connectivity(&self, status: &ConnectivityStatus) -> NetworkPressureTier {
        match status {
            ConnectivityStatus::Connected => NetworkPressureTier::Green,
            ConnectivityStatus::Degraded => NetworkPressureTier::Yellow,
            ConnectivityStatus::Unreachable => NetworkPressureTier::Black,
            ConnectivityStatus::Unknown => NetworkPressureTier::Black,
        }
    }
}

impl Default for NetworkObserver {
    fn default() -> Self {
        Self::new()
    }
}

/// Fail-open: attribute a connection, returning None if rano is unavailable.
pub fn attribute_failopen(
    observer: &NetworkObserver,
    remote_addr: &str,
) -> Option<NetworkAttribution> {
    match observer.attribute_connection(remote_addr) {
        Ok(attr) => Some(attr),
        Err(e) => {
            warn!(
                bridge = "rano",
                remote = %remote_addr,
                error = %e,
                "attribution failed, failing open"
            );
            None
        }
    }
}

/// Classify network pressure from latency, returning Green if rano is unavailable.
pub fn pressure_failopen(
    observer: &NetworkObserver,
    remote_addr: &str,
) -> NetworkPressureTier {
    match observer.attribute_connection(remote_addr) {
        Ok(attr) => observer.classify_pressure(&attr),
        Err(_) => NetworkPressureTier::Green, // fail open
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -- NetworkAttribution --

    #[test]
    fn attribution_serde_roundtrip() {
        let attr = NetworkAttribution {
            provider: "AWS".into(),
            region: Some("us-east-1".into()),
            latency_ms: 42.5,
            is_trusted: true,
            remote_addr: "10.0.0.1".into(),
            asn: Some(16509),
            org: Some("Amazon".into()),
        };
        let json_str = serde_json::to_string(&attr).unwrap();
        let rt: NetworkAttribution = serde_json::from_str(&json_str).unwrap();
        assert_eq!(rt.provider, "AWS");
        assert_eq!(rt.region, Some("us-east-1".into()));
        assert!((rt.latency_ms - 42.5).abs() < f64::EPSILON);
        assert!(rt.is_trusted);
        assert_eq!(rt.asn, Some(16509));
    }

    #[test]
    fn attribution_minimal_deserialize() {
        let json_str = r#"{"provider":"GCP","latency_ms":10.0,"remote_addr":"8.8.8.8"}"#;
        let attr: NetworkAttribution = serde_json::from_str(json_str).unwrap();
        assert_eq!(attr.provider, "GCP");
        assert!(attr.region.is_none());
        assert!(!attr.is_trusted);
        assert!(attr.asn.is_none());
    }

    #[test]
    fn attribution_skip_serializing_none() {
        let attr = NetworkAttribution {
            provider: "X".into(),
            region: None,
            latency_ms: 1.0,
            is_trusted: false,
            remote_addr: "1.1.1.1".into(),
            asn: None,
            org: None,
        };
        let json_str = serde_json::to_string(&attr).unwrap();
        assert!(!json_str.contains("region"));
        assert!(!json_str.contains("asn"));
        assert!(!json_str.contains("org"));
    }

    // -- ConnectivityStatus --

    #[test]
    fn connectivity_status_display() {
        assert_eq!(ConnectivityStatus::Connected.to_string(), "connected");
        assert_eq!(ConnectivityStatus::Degraded.to_string(), "degraded");
        assert_eq!(ConnectivityStatus::Unreachable.to_string(), "unreachable");
        assert_eq!(ConnectivityStatus::Unknown.to_string(), "unknown");
    }

    #[test]
    fn connectivity_status_serde_roundtrip() {
        let statuses = vec![
            ConnectivityStatus::Connected,
            ConnectivityStatus::Degraded,
            ConnectivityStatus::Unreachable,
            ConnectivityStatus::Unknown,
        ];
        for s in statuses {
            let json_str = serde_json::to_string(&s).unwrap();
            let rt: ConnectivityStatus = serde_json::from_str(&json_str).unwrap();
            assert_eq!(s, rt);
        }
    }

    // -- NetworkPressureTier --

    #[test]
    fn pressure_tier_ordering() {
        assert!(NetworkPressureTier::Green < NetworkPressureTier::Yellow);
        assert!(NetworkPressureTier::Yellow < NetworkPressureTier::Red);
        assert!(NetworkPressureTier::Red < NetworkPressureTier::Black);
    }

    #[test]
    fn pressure_tier_display() {
        assert_eq!(NetworkPressureTier::Green.to_string(), "green");
        assert_eq!(NetworkPressureTier::Yellow.to_string(), "yellow");
        assert_eq!(NetworkPressureTier::Red.to_string(), "red");
        assert_eq!(NetworkPressureTier::Black.to_string(), "black");
    }

    #[test]
    fn pressure_tier_serde_roundtrip() {
        let tiers = vec![
            NetworkPressureTier::Green,
            NetworkPressureTier::Yellow,
            NetworkPressureTier::Red,
            NetworkPressureTier::Black,
        ];
        for t in tiers {
            let json_str = serde_json::to_string(&t).unwrap();
            let rt: NetworkPressureTier = serde_json::from_str(&json_str).unwrap();
            assert_eq!(t, rt);
        }
    }

    // -- NetworkObserverConfig --

    #[test]
    fn config_default() {
        let c = NetworkObserverConfig::default();
        assert!((c.yellow_latency_ms - 100.0).abs() < f64::EPSILON);
        assert!((c.red_latency_ms - 500.0).abs() < f64::EPSILON);
        assert_eq!(c.timeout_secs, 10);
    }

    #[test]
    fn config_serde_roundtrip() {
        let c = NetworkObserverConfig {
            yellow_latency_ms: 50.0,
            red_latency_ms: 200.0,
            timeout_secs: 5,
        };
        let json_str = serde_json::to_string(&c).unwrap();
        let rt: NetworkObserverConfig = serde_json::from_str(&json_str).unwrap();
        assert!((rt.yellow_latency_ms - 50.0).abs() < f64::EPSILON);
        assert!((rt.red_latency_ms - 200.0).abs() < f64::EPSILON);
    }

    #[test]
    fn config_serde_defaults() {
        let c: NetworkObserverConfig = serde_json::from_str("{}").unwrap();
        assert!((c.yellow_latency_ms - 100.0).abs() < f64::EPSILON);
        assert!((c.red_latency_ms - 500.0).abs() < f64::EPSILON);
    }

    // -- NetworkObserver --

    #[test]
    fn observer_default() {
        let obs = NetworkObserver::new();
        assert!((obs.config().yellow_latency_ms - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn observer_custom_config() {
        let config = NetworkObserverConfig {
            yellow_latency_ms: 75.0,
            red_latency_ms: 300.0,
            timeout_secs: 15,
        };
        let obs = NetworkObserver::with_config(config);
        assert!((obs.config().yellow_latency_ms - 75.0).abs() < f64::EPSILON);
    }

    #[test]
    fn observer_rano_not_available() {
        // rano is unlikely to be installed in test env
        let obs = NetworkObserver::new();
        // Just ensure it doesn't panic
        let _ = obs.is_available();
    }

    #[test]
    fn observer_attribute_fails_gracefully() {
        let obs = NetworkObserver::new();
        let result = obs.attribute_connection("10.0.0.1");
        // rano not installed → BridgeError
        assert!(result.is_err());
    }

    #[test]
    fn observer_check_connectivity_fails_gracefully() {
        let obs = NetworkObserver::new();
        let status = obs.check_connectivity();
        // rano not installed → Unknown
        assert_eq!(status, ConnectivityStatus::Unknown);
    }

    // -- Backpressure classification --

    #[test]
    fn classify_pressure_green() {
        let obs = NetworkObserver::new();
        let attr = NetworkAttribution {
            provider: "test".into(),
            region: None,
            latency_ms: 10.0,
            is_trusted: false,
            remote_addr: "1.1.1.1".into(),
            asn: None,
            org: None,
        };
        assert_eq!(obs.classify_pressure(&attr), NetworkPressureTier::Green);
    }

    #[test]
    fn classify_pressure_yellow() {
        let obs = NetworkObserver::new();
        let attr = NetworkAttribution {
            provider: "test".into(),
            region: None,
            latency_ms: 150.0,
            is_trusted: false,
            remote_addr: "1.1.1.1".into(),
            asn: None,
            org: None,
        };
        assert_eq!(obs.classify_pressure(&attr), NetworkPressureTier::Yellow);
    }

    #[test]
    fn classify_pressure_red() {
        let obs = NetworkObserver::new();
        let attr = NetworkAttribution {
            provider: "test".into(),
            region: None,
            latency_ms: 600.0,
            is_trusted: false,
            remote_addr: "1.1.1.1".into(),
            asn: None,
            org: None,
        };
        assert_eq!(obs.classify_pressure(&attr), NetworkPressureTier::Red);
    }

    #[test]
    fn classify_pressure_exact_threshold() {
        let obs = NetworkObserver::new();
        let attr = NetworkAttribution {
            provider: "test".into(),
            region: None,
            latency_ms: 100.0, // Exactly yellow threshold
            is_trusted: false,
            remote_addr: "x".into(),
            asn: None,
            org: None,
        };
        assert_eq!(obs.classify_pressure(&attr), NetworkPressureTier::Yellow);
    }

    #[test]
    fn classify_pressure_custom_thresholds() {
        let obs = NetworkObserver::with_config(NetworkObserverConfig {
            yellow_latency_ms: 50.0,
            red_latency_ms: 200.0,
            timeout_secs: 10,
        });
        let attr = NetworkAttribution {
            provider: "test".into(),
            region: None,
            latency_ms: 75.0,
            is_trusted: false,
            remote_addr: "x".into(),
            asn: None,
            org: None,
        };
        assert_eq!(obs.classify_pressure(&attr), NetworkPressureTier::Yellow);
    }

    // -- Connectivity classification --

    #[test]
    fn classify_connectivity_connected() {
        let obs = NetworkObserver::new();
        assert_eq!(
            obs.classify_connectivity(&ConnectivityStatus::Connected),
            NetworkPressureTier::Green
        );
    }

    #[test]
    fn classify_connectivity_degraded() {
        let obs = NetworkObserver::new();
        assert_eq!(
            obs.classify_connectivity(&ConnectivityStatus::Degraded),
            NetworkPressureTier::Yellow
        );
    }

    #[test]
    fn classify_connectivity_unreachable() {
        let obs = NetworkObserver::new();
        assert_eq!(
            obs.classify_connectivity(&ConnectivityStatus::Unreachable),
            NetworkPressureTier::Black
        );
    }

    #[test]
    fn classify_connectivity_unknown() {
        let obs = NetworkObserver::new();
        assert_eq!(
            obs.classify_connectivity(&ConnectivityStatus::Unknown),
            NetworkPressureTier::Black
        );
    }

    // -- Fail-open helpers --

    #[test]
    fn attribute_failopen_returns_none() {
        let obs = NetworkObserver::new();
        let result = attribute_failopen(&obs, "10.0.0.1");
        assert!(result.is_none());
    }

    #[test]
    fn pressure_failopen_returns_green() {
        let obs = NetworkObserver::new();
        let tier = pressure_failopen(&obs, "10.0.0.1");
        assert_eq!(tier, NetworkPressureTier::Green);
    }

    // -- Edge cases --

    #[test]
    fn pressure_tier_all_variants_eq() {
        assert_eq!(NetworkPressureTier::Green, NetworkPressureTier::Green);
        assert_ne!(NetworkPressureTier::Green, NetworkPressureTier::Yellow);
    }

    #[test]
    fn connectivity_status_all_variants_eq() {
        assert_eq!(ConnectivityStatus::Connected, ConnectivityStatus::Connected);
        assert_ne!(ConnectivityStatus::Connected, ConnectivityStatus::Degraded);
    }

    #[test]
    fn attribution_zero_latency() {
        let obs = NetworkObserver::new();
        let attr = NetworkAttribution {
            provider: "local".into(),
            region: None,
            latency_ms: 0.0,
            is_trusted: true,
            remote_addr: "127.0.0.1".into(),
            asn: None,
            org: None,
        };
        assert_eq!(obs.classify_pressure(&attr), NetworkPressureTier::Green);
    }
}
