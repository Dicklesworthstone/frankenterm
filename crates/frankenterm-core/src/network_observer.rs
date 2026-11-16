//! Network attribution observer — bridges FrankenTerm ↔ `rano` CLI.
//!
//! Provides network connection attribution (provider, region, latency)
//! and connectivity checks via the `rano` subprocess. Maps high latency
//! or unreachable state to backpressure tier signals.
//!
//! This subprocess-backed integration is available with the
//! `subprocess-bridge` Cargo feature.

use std::path::Path;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::runtime_async::process::{
    CommandCancellation, CommandCleanupTrigger, CommandOutputStream,
};
use crate::subprocess_bridge::{BridgeError, SubprocessBridge};

/// Largest admitted wall-clock timeout for one `rano` invocation.
pub const MAX_NETWORK_OBSERVER_TIMEOUT_SECS: u64 = 300;
/// Default maximum JSON bytes retained from one `rano` invocation.
pub const DEFAULT_NETWORK_OBSERVER_STDOUT_LIMIT_BYTES: usize = 256 * 1024;
/// Hard admission ceiling for JSON bytes retained from one invocation.
pub const MAX_NETWORK_OBSERVER_STDOUT_LIMIT_BYTES: usize = 4 * 1024 * 1024;
/// Default maximum diagnostic bytes retained while supervising `rano`.
pub const DEFAULT_NETWORK_OBSERVER_STDERR_LIMIT_BYTES: usize = 16 * 1024;
/// Hard admission ceiling for diagnostic bytes retained from one invocation.
pub const MAX_NETWORK_OBSERVER_STDERR_LIMIT_BYTES: usize = 256 * 1024;
/// Maximum positional remote-address bytes admitted into the subprocess argv.
pub const MAX_NETWORK_OBSERVER_REMOTE_ADDRESS_BYTES: usize = 1_024;
/// Maximum bytes admitted for one provider, region, or organization label.
pub const MAX_NETWORK_OBSERVER_LABEL_BYTES: usize = 1_024;

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

#[derive(Debug, Deserialize)]
struct ConnectivityProbe {
    status: String,
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
    /// Maximum JSON bytes retained from stdout for one invocation.
    #[serde(default = "default_stdout_limit_bytes")]
    pub max_stdout_bytes: usize,
    /// Maximum diagnostic bytes retained from stderr for one invocation.
    #[serde(default = "default_stderr_limit_bytes")]
    pub max_stderr_bytes: usize,
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

fn default_stdout_limit_bytes() -> usize {
    DEFAULT_NETWORK_OBSERVER_STDOUT_LIMIT_BYTES
}

fn default_stderr_limit_bytes() -> usize {
    DEFAULT_NETWORK_OBSERVER_STDERR_LIMIT_BYTES
}

impl Default for NetworkObserverConfig {
    fn default() -> Self {
        Self {
            yellow_latency_ms: default_yellow_latency(),
            red_latency_ms: default_red_latency(),
            timeout_secs: default_timeout_secs(),
            max_stdout_bytes: default_stdout_limit_bytes(),
            max_stderr_bytes: default_stderr_limit_bytes(),
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
    BinaryNotFound,
    /// The configured executable did not meet the fixed `rano` path contract.
    InvalidBinary,
    /// A remote address was empty, oversized, or unsafe to pass positionally.
    InvalidRemoteAddress { input_bytes: usize },
    /// Subprocess exited with non-zero code.
    SubprocessFailed { code: i32 },
    /// Subprocess exceeded configured timeout.
    Timeout { timeout_secs: u64 },
    /// Subprocess timeout is outside the finite admitted range.
    InvalidTimeout { timeout_secs: u64 },
    /// A configured capture limit is zero or exceeds its hard ceiling.
    InvalidOutputLimit {
        stream: CommandOutputStream,
        requested: usize,
        maximum: usize,
    },
    /// A child crossed an admitted capture limit.
    OutputTooLarge {
        stream: CommandOutputStream,
        observed: usize,
        limit: usize,
    },
    /// Operation was deliberately cancelled by its caller.
    Cancelled,
    /// The child leader exited but inherited capture descriptors stayed open.
    CaptureIncomplete {
        stdout_open: bool,
        stderr_open: bool,
        drain_timeout_ms: u64,
    },
    /// Bounded child cleanup could not prove complete settlement.
    CleanupIncomplete {
        trigger: CommandCleanupTrigger,
        leader_reaped: bool,
        signal_helper_settled: bool,
        process_tree_signalled: bool,
        stdout_open: bool,
        stderr_open: bool,
        settle_timeout_ms: u64,
    },
    /// Other subprocess I/O failure, classified without path or output content.
    Io { kind: std::io::ErrorKind },
    /// JSON parse failure.
    ParseFailed,
    /// JSON was syntactically valid but violated the finite response contract.
    InvalidResponse { field: &'static str },
}

impl std::fmt::Display for NetworkObserverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BinaryNotFound => f.write_str("rano command unavailable"),
            Self::InvalidBinary => f.write_str("invalid rano executable configuration"),
            Self::InvalidRemoteAddress { input_bytes } => {
                write!(f, "invalid remote address (input_bytes={input_bytes})")
            }
            Self::SubprocessFailed { code } => write!(f, "rano failed (exit {code})"),
            Self::Timeout { timeout_secs } => {
                write!(f, "rano timed out after {timeout_secs}s")
            }
            Self::InvalidTimeout { timeout_secs } => {
                write!(
                    f,
                    "rano timeout is outside 1..={MAX_NETWORK_OBSERVER_TIMEOUT_SECS}s: {timeout_secs}s"
                )
            }
            Self::InvalidOutputLimit {
                stream,
                requested,
                maximum,
            } => write!(
                f,
                "invalid rano {stream} capture limit ({requested}; admitted 1..={maximum})"
            ),
            Self::OutputTooLarge {
                stream,
                observed,
                limit,
            } => write!(
                f,
                "rano {stream} capture limit exceeded (observed_at_least={observed}, limit={limit})"
            ),
            Self::Cancelled => f.write_str("rano operation cancelled"),
            Self::CaptureIncomplete {
                stdout_open,
                stderr_open,
                drain_timeout_ms,
            } => write!(
                f,
                "rano output capture incomplete after {drain_timeout_ms} ms (stdout_open={stdout_open}, stderr_open={stderr_open})"
            ),
            Self::CleanupIncomplete {
                trigger,
                leader_reaped,
                signal_helper_settled,
                process_tree_signalled,
                stdout_open,
                stderr_open,
                settle_timeout_ms,
            } => write!(
                f,
                "rano cleanup incomplete after {settle_timeout_ms} ms (trigger={trigger}, leader_reaped={leader_reaped}, signal_helper_settled={signal_helper_settled}, process_tree_signalled={process_tree_signalled}, stdout_open={stdout_open}, stderr_open={stderr_open})"
            ),
            Self::Io { kind } => write!(f, "rano subprocess I/O failure ({kind:?})"),
            Self::ParseFailed => f.write_str("rano returned invalid JSON"),
            Self::InvalidResponse { field } => {
                write!(f, "rano returned an invalid {field} field")
            }
        }
    }
}

impl NetworkObserverError {
    /// Stable content-free class suitable for structured logs and counters.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::BinaryNotFound => "binary_not_found",
            Self::InvalidBinary => "invalid_binary",
            Self::InvalidRemoteAddress { .. } => "invalid_remote_address",
            Self::SubprocessFailed { .. } => "subprocess_failed",
            Self::Timeout { .. } => "timeout",
            Self::InvalidTimeout { .. } => "invalid_timeout",
            Self::InvalidOutputLimit { .. } => "invalid_output_limit",
            Self::OutputTooLarge { .. } => "output_too_large",
            Self::Cancelled => "cancelled",
            Self::CaptureIncomplete { .. } => "capture_incomplete",
            Self::CleanupIncomplete { .. } => "cleanup_incomplete",
            Self::Io { .. } => "io",
            Self::ParseFailed => "parse_failed",
            Self::InvalidResponse { .. } => "invalid_response",
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
            binary: default_rano_binary().to_string(),
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
        self.rano_bridge::<serde_json::Value>()
            .is_ok_and(|bridge| bridge.is_available())
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
        let cancellation = CommandCancellation::new();
        self.attribute_connection_with_cancellation(remote_addr, &cancellation)
    }

    /// Attribute a remote connection with cooperative, handle-owned child
    /// cancellation. Cancellation is checked before spawn and throughout the
    /// bounded supervisor loop.
    pub fn attribute_connection_with_cancellation(
        &self,
        remote_addr: &str,
        cancellation: &CommandCancellation,
    ) -> Result<NetworkAttribution, NetworkObserverError> {
        validate_remote_address(remote_addr)?;
        debug!(
            bridge = "rano",
            remote_addr_bytes = remote_addr.len(),
            "attributing connection"
        );

        let attr: NetworkAttribution =
            self.invoke_rano(&["attribute", remote_addr, "--json"], cancellation)?;
        validate_attribution_response(&attr, remote_addr)?;

        debug!(
            bridge = "rano",
            remote_addr_bytes = remote_addr.len(),
            provider_bytes = attr.provider.len(),
            "connection attributed"
        );

        Ok(attr)
    }

    /// Check connectivity status.
    pub fn check_connectivity(&self) -> ConnectivityStatus {
        let cancellation = CommandCancellation::new();
        match self.check_connectivity_with_cancellation(&cancellation) {
            Ok(status) => status,
            Err(error) => {
                warn!(
                    bridge = "rano",
                    error_kind = error.kind(),
                    "connectivity check failed"
                );
                ConnectivityStatus::Unknown
            }
        }
    }

    /// Check connectivity with cooperative, handle-owned child cancellation.
    /// Unlike [`Self::check_connectivity`], this variant preserves the typed
    /// cancellation or supervision failure for its caller.
    pub fn check_connectivity_with_cancellation(
        &self,
        cancellation: &CommandCancellation,
    ) -> Result<ConnectivityStatus, NetworkObserverError> {
        let probe = self.invoke_rano::<ConnectivityProbe>(&["check", "--json"], cancellation)?;
        let status = match probe.status.as_str() {
            "connected" => ConnectivityStatus::Connected,
            "degraded" => ConnectivityStatus::Degraded,
            "unreachable" => ConnectivityStatus::Unreachable,
            _ => {
                return Err(NetworkObserverError::InvalidResponse {
                    field: "connectivity_status",
                });
            }
        };
        Ok(status)
    }

    fn invoke_rano<T: DeserializeOwned>(
        &self,
        args: &[&str],
        cancellation: &CommandCancellation,
    ) -> Result<T, NetworkObserverError> {
        if cancellation.is_cancelled() {
            return Err(NetworkObserverError::Cancelled);
        }
        let bridge = self.rano_bridge()?;
        bridge
            .invoke_with_cancellation(args, cancellation)
            .map_err(|error| self.map_bridge_error(error))
    }

    fn rano_bridge<T: DeserializeOwned>(
        &self,
    ) -> Result<SubprocessBridge<T>, NetworkObserverError> {
        self.validate_runtime_limits()?;
        let binary = self.validated_rano_binary()?;
        Ok(SubprocessBridge::new(binary)
            .with_timeout(Duration::from_secs(self.config.timeout_secs))
            .with_search_paths(std::iter::empty::<std::path::PathBuf>())
            .with_stdout_limit(self.config.max_stdout_bytes)
            .with_stderr_limit(self.config.max_stderr_bytes))
    }

    fn validate_runtime_limits(&self) -> Result<(), NetworkObserverError> {
        if self.config.timeout_secs == 0
            || self.config.timeout_secs > MAX_NETWORK_OBSERVER_TIMEOUT_SECS
        {
            return Err(NetworkObserverError::InvalidTimeout {
                timeout_secs: self.config.timeout_secs,
            });
        }
        validate_output_limit(
            CommandOutputStream::Stdout,
            self.config.max_stdout_bytes,
            MAX_NETWORK_OBSERVER_STDOUT_LIMIT_BYTES,
        )?;
        validate_output_limit(
            CommandOutputStream::Stderr,
            self.config.max_stderr_bytes,
            MAX_NETWORK_OBSERVER_STDERR_LIMIT_BYTES,
        )
    }

    fn map_bridge_error(&self, error: BridgeError) -> NetworkObserverError {
        match error {
            BridgeError::BinaryNotFound => NetworkObserverError::BinaryNotFound,
            BridgeError::Timeout(_) => NetworkObserverError::Timeout {
                timeout_secs: self.config.timeout_secs,
            },
            BridgeError::ParseError => NetworkObserverError::ParseFailed,
            BridgeError::ExitCode(code) => NetworkObserverError::SubprocessFailed { code },
            BridgeError::Cancelled => NetworkObserverError::Cancelled,
            BridgeError::CaptureIncomplete {
                stdout_open,
                stderr_open,
                drain_timeout_ms,
            } => NetworkObserverError::CaptureIncomplete {
                stdout_open,
                stderr_open,
                drain_timeout_ms,
            },
            BridgeError::CleanupIncomplete {
                trigger,
                leader_reaped,
                signal_helper_settled,
                process_tree_signalled,
                stdout_open,
                stderr_open,
                settle_timeout_ms,
            } => NetworkObserverError::CleanupIncomplete {
                trigger,
                leader_reaped,
                signal_helper_settled,
                process_tree_signalled,
                stdout_open,
                stderr_open,
                settle_timeout_ms,
            },
            BridgeError::OutputTooLarge {
                stream,
                observed,
                limit,
            } => NetworkObserverError::OutputTooLarge {
                stream,
                observed,
                limit,
            },
            BridgeError::Io(kind) => NetworkObserverError::Io { kind },
        }
    }

    /// Map an attribution to a backpressure tier.
    pub fn classify_pressure(&self, attr: &NetworkAttribution) -> NetworkPressureTier {
        if !attr.latency_ms.is_finite()
            || attr.latency_ms < 0.0
            || !self.config.yellow_latency_ms.is_finite()
            || self.config.yellow_latency_ms < 0.0
            || !self.config.red_latency_ms.is_finite()
            || self.config.red_latency_ms < self.config.yellow_latency_ms
        {
            NetworkPressureTier::Black
        } else if attr.latency_ms >= self.config.red_latency_ms {
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

    fn validated_rano_binary(&self) -> Result<&str, NetworkObserverError> {
        let binary = self.binary.as_str();
        if binary.trim() != binary || binary.is_empty() || binary.contains('\0') {
            return Err(NetworkObserverError::InvalidBinary);
        }
        if binary == "rano" || (cfg!(windows) && binary.eq_ignore_ascii_case("rano.exe")) {
            return Ok(binary);
        }

        let path = Path::new(binary);
        let basename = path.file_name().and_then(|name| name.to_str());
        let basename_is_rano = basename == Some("rano")
            || (cfg!(windows)
                && basename.is_some_and(|name| name.eq_ignore_ascii_case("rano.exe")));
        if path.is_absolute() && basename_is_rano {
            return Ok(binary);
        }

        Err(NetworkObserverError::InvalidBinary)
    }
}

impl Default for NetworkObserver {
    fn default() -> Self {
        Self::new()
    }
}

fn validate_remote_address(remote_addr: &str) -> Result<(), NetworkObserverError> {
    let invalid = remote_addr.is_empty()
        || remote_addr.len() > MAX_NETWORK_OBSERVER_REMOTE_ADDRESS_BYTES
        || remote_addr.starts_with('-')
        || remote_addr
            .chars()
            .any(|character| character.is_control() || character.is_whitespace());
    if invalid {
        return Err(NetworkObserverError::InvalidRemoteAddress {
            input_bytes: remote_addr.len(),
        });
    }
    Ok(())
}

fn validate_attribution_response(
    attribution: &NetworkAttribution,
    requested_remote_addr: &str,
) -> Result<(), NetworkObserverError> {
    validate_response_label("provider", &attribution.provider)?;
    if let Some(region) = attribution.region.as_deref() {
        validate_response_label("region", region)?;
    }
    if let Some(org) = attribution.org.as_deref() {
        validate_response_label("org", org)?;
    }
    if !attribution.latency_ms.is_finite() || attribution.latency_ms < 0.0 {
        return Err(NetworkObserverError::InvalidResponse {
            field: "latency_ms",
        });
    }
    if validate_remote_address(&attribution.remote_addr).is_err()
        || attribution.remote_addr != requested_remote_addr
    {
        return Err(NetworkObserverError::InvalidResponse {
            field: "remote_addr",
        });
    }
    Ok(())
}

fn validate_response_label(field: &'static str, label: &str) -> Result<(), NetworkObserverError> {
    if label.is_empty()
        || label.len() > MAX_NETWORK_OBSERVER_LABEL_BYTES
        || label.trim() != label
        || label.chars().any(char::is_control)
    {
        return Err(NetworkObserverError::InvalidResponse { field });
    }
    Ok(())
}

const fn default_rano_binary() -> &'static str {
    if cfg!(windows) { "rano.exe" } else { "rano" }
}

fn validate_output_limit(
    stream: CommandOutputStream,
    requested: usize,
    maximum: usize,
) -> Result<(), NetworkObserverError> {
    if requested == 0 || requested > maximum {
        return Err(NetworkObserverError::InvalidOutputLimit {
            stream,
            requested,
            maximum,
        });
    }
    Ok(())
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
                remote_addr_bytes = remote_addr.len(),
                error_kind = e.kind(),
                "attribution failed, failing open"
            );
            None
        }
    }
}

/// Classify network pressure from latency, returning Black if rano is unavailable.
pub fn pressure_failclosed(observer: &NetworkObserver, remote_addr: &str) -> NetworkPressureTier {
    match observer.attribute_connection(remote_addr) {
        Ok(attr) => observer.classify_pressure(&attr),
        Err(e) => {
            warn!(
                bridge = "rano",
                remote_addr_bytes = remote_addr.len(),
                error_kind = e.kind(),
                "pressure attribution failed, failing closed"
            );
            NetworkPressureTier::Black
        }
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
        assert_eq!(rt.remote_addr, "10.0.0.1");
        assert_eq!(rt.asn, Some(16509));
        assert_eq!(rt.org, Some("Amazon".into()));
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
        assert_eq!(
            c.max_stdout_bytes,
            DEFAULT_NETWORK_OBSERVER_STDOUT_LIMIT_BYTES
        );
        assert_eq!(
            c.max_stderr_bytes,
            DEFAULT_NETWORK_OBSERVER_STDERR_LIMIT_BYTES
        );
    }

    #[test]
    fn config_serde_roundtrip() {
        let c = NetworkObserverConfig {
            yellow_latency_ms: 50.0,
            red_latency_ms: 200.0,
            timeout_secs: 5,
            max_stdout_bytes: 100_000,
            max_stderr_bytes: 4_000,
        };
        let json_str = serde_json::to_string(&c).unwrap();
        let rt: NetworkObserverConfig = serde_json::from_str(&json_str).unwrap();
        assert!((rt.yellow_latency_ms - 50.0).abs() < f64::EPSILON);
        assert!((rt.red_latency_ms - 200.0).abs() < f64::EPSILON);
        assert_eq!(rt.timeout_secs, 5);
        assert_eq!(rt.max_stdout_bytes, 100_000);
        assert_eq!(rt.max_stderr_bytes, 4_000);
    }

    #[test]
    fn config_serde_defaults() {
        let c: NetworkObserverConfig = serde_json::from_str("{}").unwrap();
        assert!((c.yellow_latency_ms - 100.0).abs() < f64::EPSILON);
        assert!((c.red_latency_ms - 500.0).abs() < f64::EPSILON);
        assert_eq!(
            c.max_stdout_bytes,
            DEFAULT_NETWORK_OBSERVER_STDOUT_LIMIT_BYTES
        );
        assert_eq!(
            c.max_stderr_bytes,
            DEFAULT_NETWORK_OBSERVER_STDERR_LIMIT_BYTES
        );
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
            ..NetworkObserverConfig::default()
        };
        let obs = NetworkObserver::with_config(config);
        assert!((obs.config().yellow_latency_ms - 75.0).abs() < f64::EPSILON);
    }

    #[test]
    fn observer_rano_not_available() {
        let dir = tempfile::tempdir().expect("isolated missing-rano directory");
        let binary = dir.path().join(default_rano_binary());
        let obs = NetworkObserver::with_binary(
            binary.to_string_lossy().into_owned(),
            NetworkObserverConfig::default(),
        );
        assert!(!obs.is_available());
    }

    #[test]
    fn observer_attribute_fails_gracefully() {
        let dir = tempfile::tempdir().expect("isolated missing-rano directory");
        let binary = dir.path().join(default_rano_binary());
        let obs = NetworkObserver::with_binary(
            binary.to_string_lossy().into_owned(),
            NetworkObserverConfig::default(),
        );
        let result = obs.attribute_connection("10.0.0.1");
        assert!(matches!(result, Err(NetworkObserverError::BinaryNotFound)));
    }

    #[test]
    fn observer_check_connectivity_fails_gracefully() {
        let dir = tempfile::tempdir().expect("isolated missing-rano directory");
        let binary = dir.path().join(default_rano_binary());
        let obs = NetworkObserver::with_binary(
            binary.to_string_lossy().into_owned(),
            NetworkObserverConfig::default(),
        );
        let status = obs.check_connectivity();
        assert_eq!(status, ConnectivityStatus::Unknown);
    }

    #[cfg(unix)]
    #[test]
    fn fresh_eyes_observer_subprocess_timeout_is_enforced() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("rano");
        fs::write(&script, "#!/bin/sh\n/bin/sleep 2\n").unwrap();
        let mut perms = fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script, perms).unwrap();

        let obs = NetworkObserver::with_binary(
            script.to_string_lossy().into_owned(),
            NetworkObserverConfig {
                yellow_latency_ms: 100.0,
                red_latency_ms: 500.0,
                timeout_secs: 1,
                ..NetworkObserverConfig::default()
            },
        );

        let started = std::time::Instant::now();
        let err = obs
            .attribute_connection("10.0.0.1")
            .expect_err("hung subprocess should time out");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(err, NetworkObserverError::Timeout { timeout_secs: 1 });
    }

    #[cfg(unix)]
    #[test]
    fn pre_cancelled_observer_never_spawns_child() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("spawned");
        let script = dir.path().join("rano");
        fs::write(
            &script,
            "#!/bin/sh\n: > \"${0%/*}/spawned\"\nprintf '%s\\n' '{}'\n",
        )
        .unwrap();
        let mut perms = fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script, perms).unwrap();

        let observer = NetworkObserver::with_binary(
            script.to_string_lossy().into_owned(),
            NetworkObserverConfig::default(),
        );
        let cancellation = CommandCancellation::new();
        cancellation.cancel();

        let error = observer
            .attribute_connection_with_cancellation("10.0.0.1", &cancellation)
            .expect_err("pre-cancel must surface a typed cancellation");
        assert_eq!(error, NetworkObserverError::Cancelled);
        assert!(
            !marker.exists(),
            "pre-cancelled invocation must be gated before spawn"
        );
    }

    #[test]
    fn observer_rejects_timeout_above_hard_ceiling_before_spawning() {
        let obs = NetworkObserver::with_binary(
            "should-not-be-executed",
            NetworkObserverConfig {
                yellow_latency_ms: 100.0,
                red_latency_ms: 500.0,
                timeout_secs: u64::MAX,
                ..NetworkObserverConfig::default()
            },
        );

        let err = obs
            .attribute_connection("10.0.0.1")
            .expect_err("timeout above hard ceiling should be rejected before spawn");
        assert_eq!(
            err,
            NetworkObserverError::InvalidTimeout {
                timeout_secs: u64::MAX,
            }
        );
    }

    #[test]
    fn observer_rejects_zero_timeout_and_unbounded_capture_limits_before_resolution() {
        let zero_timeout = NetworkObserver::with_binary(
            "not-a-rano-binary",
            NetworkObserverConfig {
                timeout_secs: 0,
                ..NetworkObserverConfig::default()
            },
        );
        assert_eq!(
            zero_timeout
                .attribute_connection("10.0.0.1")
                .expect_err("zero timeout must fail before binary resolution"),
            NetworkObserverError::InvalidTimeout { timeout_secs: 0 }
        );

        let oversized_stdout = NetworkObserver::with_binary(
            "not-a-rano-binary",
            NetworkObserverConfig {
                max_stdout_bytes: usize::MAX,
                ..NetworkObserverConfig::default()
            },
        );
        assert_eq!(
            oversized_stdout
                .attribute_connection("10.0.0.1")
                .expect_err("unbounded stdout capture must fail before binary resolution"),
            NetworkObserverError::InvalidOutputLimit {
                stream: CommandOutputStream::Stdout,
                requested: usize::MAX,
                maximum: MAX_NETWORK_OBSERVER_STDOUT_LIMIT_BYTES,
            }
        );
    }

    #[test]
    fn connectivity_cancellation_is_typed_and_precedes_binary_resolution() {
        let observer =
            NetworkObserver::with_binary("not-a-rano-binary", NetworkObserverConfig::default());
        let cancellation = CommandCancellation::new();
        cancellation.cancel();
        assert_eq!(
            observer
                .check_connectivity_with_cancellation(&cancellation)
                .expect_err("cancellable API must preserve cancellation"),
            NetworkObserverError::Cancelled
        );
    }

    #[test]
    fn remote_address_admission_is_finite_and_content_free() {
        for invalid in ["", "--json", "host name", "host\nname"] {
            let error = validate_remote_address(invalid)
                .expect_err("unsafe positional address must be rejected");
            assert_eq!(error.kind(), "invalid_remote_address");
            if !invalid.is_empty() {
                assert!(!error.to_string().contains(invalid));
            }
        }

        let oversized = "x".repeat(MAX_NETWORK_OBSERVER_REMOTE_ADDRESS_BYTES + 1);
        assert_eq!(
            validate_remote_address(&oversized),
            Err(NetworkObserverError::InvalidRemoteAddress {
                input_bytes: oversized.len(),
            })
        );
        assert!(validate_remote_address("2001:db8::1").is_ok());
    }

    #[test]
    fn attribution_response_rejects_semantically_invalid_or_log_unsafe_data() {
        let valid = NetworkAttribution {
            provider: "Example Network".into(),
            region: Some("us-east-1".into()),
            latency_ms: 4.5,
            is_trusted: false,
            remote_addr: "2001:db8::1".into(),
            asn: None,
            org: Some("Example Org".into()),
        };
        assert!(validate_attribution_response(&valid, "2001:db8::1").is_ok());

        let mut invalid = valid.clone();
        invalid.latency_ms = -1.0;
        assert_eq!(
            validate_attribution_response(&invalid, "2001:db8::1"),
            Err(NetworkObserverError::InvalidResponse {
                field: "latency_ms"
            })
        );

        invalid = valid.clone();
        invalid.provider = "provider\nforged-log-line".into();
        assert_eq!(
            validate_attribution_response(&invalid, "2001:db8::1"),
            Err(NetworkObserverError::InvalidResponse { field: "provider" })
        );

        invalid = valid;
        invalid.remote_addr = "--not-a-positional-address".into();
        assert_eq!(
            validate_attribution_response(&invalid, "2001:db8::1"),
            Err(NetworkObserverError::InvalidResponse {
                field: "remote_addr"
            })
        );

        let mut mismatched = NetworkAttribution {
            provider: "Example Network".into(),
            region: None,
            latency_ms: 1.0,
            is_trusted: false,
            remote_addr: "203.0.113.2".into(),
            asn: None,
            org: None,
        };
        assert_eq!(
            validate_attribution_response(&mismatched, "203.0.113.1"),
            Err(NetworkObserverError::InvalidResponse {
                field: "remote_addr"
            })
        );
        mismatched.remote_addr = "203.0.113.1".into();
        mismatched.region = Some(" ".into());
        assert_eq!(
            validate_attribution_response(&mismatched, "203.0.113.1"),
            Err(NetworkObserverError::InvalidResponse { field: "region" })
        );
    }

    #[test]
    fn network_observer_uses_only_canonical_bounded_subprocess_supervision() {
        let source = include_str!("network_observer.rs");
        let production = source
            .split("// =============================================================================\n// Tests")
            .next()
            .expect("production source prefix");

        for required in [
            "SubprocessBridge",
            "invoke_with_cancellation",
            "with_stdout_limit",
            "with_stderr_limit",
            "MAX_NETWORK_OBSERVER_TIMEOUT_SECS",
        ] {
            assert!(
                production.contains(required),
                "production path must retain {required}"
            );
        }

        for forbidden in [
            ["std::", "process"].concat(),
            ["read_", "to_end"].concat(),
            ["thread::", "spawn"].concat(),
            ["send_unix_", "signal"].concat(),
            [".wa", "it("].concat(),
            [".ki", "ll("].concat(),
            ["error = ", "%e"].concat(),
            ["remote = ", "%remote_addr"].concat(),
        ] {
            assert!(
                !production.contains(&forbidden),
                "production path must not contain {forbidden}"
            );
        }
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
    fn classify_pressure_rejects_invalid_measurements_and_thresholds() {
        let make_attr = |latency_ms| NetworkAttribution {
            provider: "test".into(),
            region: None,
            latency_ms,
            is_trusted: false,
            remote_addr: "1.1.1.1".into(),
            asn: None,
            org: None,
        };
        let observer = NetworkObserver::new();
        for latency_ms in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0] {
            assert_eq!(
                observer.classify_pressure(&make_attr(latency_ms)),
                NetworkPressureTier::Black
            );
        }

        let invalid_thresholds = NetworkObserver::with_config(NetworkObserverConfig {
            yellow_latency_ms: 500.0,
            red_latency_ms: 100.0,
            ..NetworkObserverConfig::default()
        });
        assert_eq!(
            invalid_thresholds.classify_pressure(&make_attr(50.0)),
            NetworkPressureTier::Black
        );
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
            ..NetworkObserverConfig::default()
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

    // -- Unavailable-substrate helpers --

    #[test]
    fn attribute_failopen_returns_none() {
        let dir = tempfile::tempdir().expect("isolated missing-rano directory");
        let binary = dir.path().join(default_rano_binary());
        let obs = NetworkObserver::with_binary(
            binary.to_string_lossy().into_owned(),
            NetworkObserverConfig::default(),
        );
        let result = attribute_failopen(&obs, "10.0.0.1");
        assert!(result.is_none());
    }

    #[test]
    fn pressure_failclosed_returns_black() {
        let dir = tempfile::tempdir().expect("isolated missing-rano directory");
        let binary = dir.path().join(default_rano_binary());
        let obs = NetworkObserver::with_binary(
            binary.to_string_lossy().into_owned(),
            NetworkObserverConfig::default(),
        );
        let tier = pressure_failclosed(&obs, "10.0.0.1");
        assert_eq!(tier, NetworkPressureTier::Black);
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
