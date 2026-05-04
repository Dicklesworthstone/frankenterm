//! Operator-tunable constants for ft deployment configuration.
//!
//! This module collects hard-coded constants from across the codebase into a
//! single, serde-deserializable structure that operators can override via the
//! `[tuning]` section of `ft.toml`.
//!
//! # Design Principles
//!
//! 1. **Zero-config by default**: Every field has a `Default` impl that matches
//!    the original hard-coded constant exactly. An empty `[tuning]` section (or
//!    no section at all) produces identical behavior to the pre-migration code.
//!
//! 2. **Additive schema**: New fields can be added without breaking existing
//!    config files thanks to `#[serde(default)]` on every struct.
//!
//! 3. **Validated at load time**: The `validate()` method catches out-of-range
//!    values before the runtime starts, producing clear error messages.
//!
//! # Origin
//!
//! Created as part of the hardcoded-constants audit (epic ft-8gmtq, track T1).
//! Each field documents the source file and line of the original constant.

use serde::{Deserialize, Serialize};

// =============================================================================
// Top-level TuningConfig
// =============================================================================

/// Operator-tunable constants loaded from `ft.toml` `[tuning]` section.
///
/// All fields use `#[serde(default)]` so missing keys produce the original
/// hard-coded values. This struct is immutable after loading — hot-reload is
/// a future enhancement.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct TuningConfig {
    /// Runtime observation loop parameters.
    pub runtime: RuntimeTuning,
    /// Backpressure detection thresholds.
    pub backpressure: BackpressureTuning,
    /// Session snapshot maintenance timings.
    pub snapshot: SnapshotTuning,
    /// Ingest pipeline parameters.
    pub ingest: IngestTuning,
    /// Pattern detection engine limits.
    pub patterns: PatternsTuning,
    /// Policy engine and rate limiting.
    pub policy: PolicyTuning,
    /// Audit trail retention and limits.
    pub audit: AuditTuning,
    /// Web API server parameters.
    pub web: WebTuning,
    /// Workflow execution limits.
    pub workflows: WorkflowsTuning,
    /// Search and query limits.
    pub search: SearchTuning,
    /// Wire protocol (distributed mode) limits.
    pub wire_protocol: WireProtocolTuning,
    /// IPC (local Unix socket RPC) parameters.
    pub ipc: IpcTuning,
    /// WezTerm backend adapter timeouts.
    pub wezterm: WeztermTuning,
}

impl Default for TuningConfig {
    fn default() -> Self {
        Self {
            runtime: RuntimeTuning::default(),
            backpressure: BackpressureTuning::default(),
            snapshot: SnapshotTuning::default(),
            ingest: IngestTuning::default(),
            patterns: PatternsTuning::default(),
            policy: PolicyTuning::default(),
            audit: AuditTuning::default(),
            web: WebTuning::default(),
            workflows: WorkflowsTuning::default(),
            search: SearchTuning::default(),
            wire_protocol: WireProtocolTuning::default(),
            ipc: IpcTuning::default(),
            wezterm: WeztermTuning::default(),
        }
    }
}

// =============================================================================
// RuntimeTuning
// =============================================================================

/// Runtime observation loop parameters.
///
/// Controls output buffering, telemetry, resize watchdog, and storage lock
/// thresholds in `runtime.rs`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RuntimeTuning {
    /// Output coalesce window (ms). Batches pane output bursts before processing.
    /// Source: runtime.rs NATIVE_OUTPUT_COALESCE_WINDOW_MS = 50
    pub output_coalesce_window_ms: u64,

    /// Max delay guard for output coalescing (ms). Prevents indefinite buffering.
    /// Source: runtime.rs NATIVE_OUTPUT_COALESCE_MAX_DELAY_MS = 200
    pub output_coalesce_max_delay_ms: u64,

    /// Max bytes in a coalesced output batch.
    /// Source: runtime.rs NATIVE_OUTPUT_COALESCE_MAX_BYTES = 256 * 1024
    pub output_coalesce_max_bytes: usize,

    /// Recent samples retained for lock/memory percentile telemetry.
    /// Source: runtime.rs TELEMETRY_PERCENTILE_WINDOW_CAPACITY = 1024
    pub telemetry_percentile_window: usize,

    /// Warning threshold (ms) for stalled resize transactions.
    /// Source: runtime.rs RESIZE_WATCHDOG_WARNING_THRESHOLD_MS = 2000
    pub resize_watchdog_warning_ms: u64,

    /// Critical threshold (ms) for stalled resize transactions.
    /// Source: runtime.rs RESIZE_WATCHDOG_CRITICAL_THRESHOLD_MS = 8000
    pub resize_watchdog_critical_ms: u64,

    /// Consecutive critical stalls before safe-mode recommendation.
    /// Source: runtime.rs RESIZE_WATCHDOG_CRITICAL_STALLED_LIMIT = 2
    pub resize_watchdog_stalled_limit: usize,

    /// Max stalled-transaction samples retained in watchdog payloads.
    /// Source: runtime.rs RESIZE_WATCHDOG_SAMPLE_LIMIT = 8
    pub resize_watchdog_sample_limit: usize,

    /// Storage lock wait warning threshold (ms). Warns when SQLite lock
    /// acquisition exceeds this duration.
    /// Source: runtime.rs STORAGE_LOCK_WAIT_WARN_MS = 15.0
    pub storage_lock_wait_warn_ms: f64,

    /// Storage lock hold warning threshold (ms). Warns when SQLite lock
    /// is held longer than this.
    /// Source: runtime.rs STORAGE_LOCK_HOLD_WARN_MS = 75.0
    pub storage_lock_hold_warn_ms: f64,

    /// Memory warning threshold (bytes) for retained pane cursor snapshots.
    /// Source: runtime.rs CURSOR_SNAPSHOT_MEMORY_WARN_BYTES = 64MB
    pub cursor_snapshot_memory_warn_bytes: u64,

    /// Max age (seconds) for agent state detection data. Detections older than
    /// this are considered stale and excluded from state inference.
    /// Source: agent_correlator.rs + snapshot_engine.rs STATE_DETECTION_MAX_AGE = 300
    pub state_detection_max_age_secs: u64,
}

impl RuntimeTuning {
    /// Default output coalesce window (ms).
    pub const DEFAULT_OUTPUT_COALESCE_WINDOW_MS: u64 = 50;
    /// Default output coalesce max delay (ms).
    pub const DEFAULT_OUTPUT_COALESCE_MAX_DELAY_MS: u64 = 200;
    /// Default output coalesce max bytes.
    pub const DEFAULT_OUTPUT_COALESCE_MAX_BYTES: usize = 256 * 1024;
    /// Default telemetry percentile window capacity.
    pub const DEFAULT_TELEMETRY_PERCENTILE_WINDOW: usize = 1024;
    /// Default resize watchdog warning threshold (ms).
    pub const DEFAULT_RESIZE_WATCHDOG_WARNING_MS: u64 = 2_000;
    /// Default resize watchdog critical threshold (ms).
    pub const DEFAULT_RESIZE_WATCHDOG_CRITICAL_MS: u64 = 8_000;
    /// Default resize watchdog stalled limit.
    pub const DEFAULT_RESIZE_WATCHDOG_STALLED_LIMIT: usize = 2;
    /// Default resize watchdog sample limit.
    pub const DEFAULT_RESIZE_WATCHDOG_SAMPLE_LIMIT: usize = 8;
    /// Default storage lock wait warning (ms).
    pub const DEFAULT_STORAGE_LOCK_WAIT_WARN_MS: f64 = 15.0;
    /// Default storage lock hold warning (ms).
    pub const DEFAULT_STORAGE_LOCK_HOLD_WARN_MS: f64 = 75.0;
    /// Default cursor snapshot memory warning (bytes).
    pub const DEFAULT_CURSOR_SNAPSHOT_MEMORY_WARN_BYTES: u64 = 64 * 1024 * 1024;
    /// Default state detection max age (seconds).
    pub const DEFAULT_STATE_DETECTION_MAX_AGE_SECS: u64 = 300;
}

impl Default for RuntimeTuning {
    fn default() -> Self {
        Self {
            output_coalesce_window_ms: Self::DEFAULT_OUTPUT_COALESCE_WINDOW_MS,
            output_coalesce_max_delay_ms: Self::DEFAULT_OUTPUT_COALESCE_MAX_DELAY_MS,
            output_coalesce_max_bytes: Self::DEFAULT_OUTPUT_COALESCE_MAX_BYTES,
            telemetry_percentile_window: Self::DEFAULT_TELEMETRY_PERCENTILE_WINDOW,
            resize_watchdog_warning_ms: Self::DEFAULT_RESIZE_WATCHDOG_WARNING_MS,
            resize_watchdog_critical_ms: Self::DEFAULT_RESIZE_WATCHDOG_CRITICAL_MS,
            resize_watchdog_stalled_limit: Self::DEFAULT_RESIZE_WATCHDOG_STALLED_LIMIT,
            resize_watchdog_sample_limit: Self::DEFAULT_RESIZE_WATCHDOG_SAMPLE_LIMIT,
            storage_lock_wait_warn_ms: Self::DEFAULT_STORAGE_LOCK_WAIT_WARN_MS,
            storage_lock_hold_warn_ms: Self::DEFAULT_STORAGE_LOCK_HOLD_WARN_MS,
            cursor_snapshot_memory_warn_bytes: Self::DEFAULT_CURSOR_SNAPSHOT_MEMORY_WARN_BYTES,
            state_detection_max_age_secs: Self::DEFAULT_STATE_DETECTION_MAX_AGE_SECS,
        }
    }
}

// =============================================================================
// BackpressureTuning
// =============================================================================

/// Backpressure detection thresholds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct BackpressureTuning {
    /// Queue fullness ratio that triggers a warning in health snapshots.
    /// 0.75 = warn at 75% full.
    /// Source: runtime.rs BACKPRESSURE_WARN_RATIO = 0.75
    pub warn_ratio: f64,
}

impl BackpressureTuning {
    /// Default backpressure warn ratio.
    pub const DEFAULT_WARN_RATIO: f64 = 0.75;
}

impl Default for BackpressureTuning {
    fn default() -> Self {
        Self {
            warn_ratio: Self::DEFAULT_WARN_RATIO,
        }
    }
}

// =============================================================================
// SnapshotTuning
// =============================================================================

/// Session snapshot maintenance timings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct SnapshotTuning {
    /// Interval (seconds) for snapshot trigger bridge maintenance checks.
    /// Source: runtime.rs SNAPSHOT_TRIGGER_BRIDGE_TICK_SECS = 30
    pub trigger_bridge_tick_secs: u64,

    /// Idle duration (seconds) before emitting `IdleWindow` trigger.
    /// Source: runtime.rs SNAPSHOT_IDLE_WINDOW_SECS = 300
    pub idle_window_secs: u64,

    /// Minimum interval (seconds) between memory-pressure-triggered snapshots.
    /// Source: runtime.rs SNAPSHOT_MEMORY_TRIGGER_COOLDOWN_SECS = 120
    pub memory_trigger_cooldown_secs: u64,
}

impl SnapshotTuning {
    /// Default snapshot trigger bridge tick (seconds).
    pub const DEFAULT_TRIGGER_BRIDGE_TICK_SECS: u64 = 30;
    /// Default idle window (seconds).
    pub const DEFAULT_IDLE_WINDOW_SECS: u64 = 300;
    /// Default memory trigger cooldown (seconds).
    pub const DEFAULT_MEMORY_TRIGGER_COOLDOWN_SECS: u64 = 120;
}

impl Default for SnapshotTuning {
    fn default() -> Self {
        Self {
            trigger_bridge_tick_secs: Self::DEFAULT_TRIGGER_BRIDGE_TICK_SECS,
            idle_window_secs: Self::DEFAULT_IDLE_WINDOW_SECS,
            memory_trigger_cooldown_secs: Self::DEFAULT_MEMORY_TRIGGER_COOLDOWN_SECS,
        }
    }
}

// =============================================================================
// IngestTuning
// =============================================================================

/// Ingest pipeline parameters.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct IngestTuning {
    /// Max bytes per persisted output segment. Controls SQLite write granularity.
    /// Source: ingest.rs persistence segment size guard = 64KB
    pub max_persist_segment_bytes: usize,

    /// Max single record payload for tantivy ingestion (bytes).
    /// Source: tantivy_ingest.rs append-log reader payload guard = 64MB
    pub max_record_payload_bytes: usize,
}

impl IngestTuning {
    /// Default max persist segment bytes (64 KB).
    pub const DEFAULT_MAX_PERSIST_SEGMENT_BYTES: usize = 64 * 1024;
    /// Default max record payload bytes (64 MB).
    pub const DEFAULT_MAX_RECORD_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;
}

impl Default for IngestTuning {
    fn default() -> Self {
        Self {
            max_persist_segment_bytes: Self::DEFAULT_MAX_PERSIST_SEGMENT_BYTES,
            max_record_payload_bytes: Self::DEFAULT_MAX_RECORD_PAYLOAD_BYTES,
        }
    }
}

// =============================================================================
// PatternsTuning
// =============================================================================

/// Pattern detection engine limits.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PatternsTuning {
    /// Max entries in the pattern dedup cache. Each unique
    /// (rule_id, pane_id, fingerprint) tuple occupies one slot.
    /// Source: patterns.rs MAX_SEEN_KEYS = 1000
    pub max_seen_keys: usize,

    /// Tail buffer size (bytes) for anchor matching. Retains the last N bytes
    /// of each pane's output for pattern context.
    /// Source: patterns.rs MAX_TAIL_SIZE = 2048
    pub max_tail_size_bytes: usize,

    /// Bloom filter false-positive rate for pattern pre-filtering.
    /// Lower = more memory, fewer false regex evaluations.
    /// Source: patterns.rs BLOOM_FALSE_POSITIVE_RATE = 0.01
    pub bloom_false_positive_rate: f64,
}

impl PatternsTuning {
    /// Default max seen keys in dedup cache.
    pub const DEFAULT_MAX_SEEN_KEYS: usize = 1000;
    /// Default tail buffer size (bytes).
    pub const DEFAULT_MAX_TAIL_SIZE_BYTES: usize = 2048;
    /// Default Bloom filter false-positive rate.
    pub const DEFAULT_BLOOM_FALSE_POSITIVE_RATE: f64 = 0.01;
}

impl Default for PatternsTuning {
    fn default() -> Self {
        Self {
            max_seen_keys: Self::DEFAULT_MAX_SEEN_KEYS,
            max_tail_size_bytes: Self::DEFAULT_MAX_TAIL_SIZE_BYTES,
            bloom_false_positive_rate: Self::DEFAULT_BLOOM_FALSE_POSITIVE_RATE,
        }
    }
}

// =============================================================================
// PolicyTuning
// =============================================================================

/// Policy engine and rate limiting parameters.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct PolicyTuning {
    /// Sliding window (seconds) for send_text rate limiting.
    /// Source: policy.rs RATE_LIMIT_WINDOW = 60
    pub rate_limit_window_secs: u64,

    /// Max panes tracked by the rate limiter. When exceeded, oldest pane data
    /// is evicted (causing rate limit amnesia for that pane).
    /// Source: rate_limit_tracker.rs MAX_TRACKED_PANES = 256
    pub max_tracked_panes: usize,

    /// Max events retained per pane in the rate limiter.
    /// Source: rate_limit_tracker.rs MAX_EVENTS_PER_PANE = 64
    pub max_events_per_pane: usize,

    /// Max panes tracked by the cost tracker.
    /// Source: cost_tracker.rs MAX_TRACKED_PANES = 512
    pub cost_tracker_max_panes: usize,
}

impl PolicyTuning {
    pub const DEFAULT_RATE_LIMIT_WINDOW_SECS: u64 = 60;
    pub const DEFAULT_MAX_TRACKED_PANES: usize = 256;
    pub const DEFAULT_MAX_EVENTS_PER_PANE: usize = 64;
    pub const DEFAULT_COST_TRACKER_MAX_PANES: usize = 512;
}

impl Default for PolicyTuning {
    fn default() -> Self {
        Self {
            rate_limit_window_secs: Self::DEFAULT_RATE_LIMIT_WINDOW_SECS,
            max_tracked_panes: Self::DEFAULT_MAX_TRACKED_PANES,
            max_events_per_pane: Self::DEFAULT_MAX_EVENTS_PER_PANE,
            cost_tracker_max_panes: Self::DEFAULT_COST_TRACKER_MAX_PANES,
        }
    }
}

// =============================================================================
// AuditTuning
// =============================================================================

/// Audit trail retention and limits.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AuditTuning {
    /// Days to retain audit log entries.
    /// Source: recorder_audit.rs DEFAULT_AUDIT_RETENTION_DAYS = 90
    pub retention_days: u32,

    /// Approval token time-to-live (seconds). After this, one-time approvals expire.
    /// Source: recorder_audit.rs DEFAULT_APPROVAL_TTL_SECONDS = 900
    pub approval_ttl_secs: u64,

    /// Max rows returned by raw audit queries.
    /// Source: recorder_audit.rs DEFAULT_MAX_RAW_QUERY_ROWS = 100
    pub max_raw_query_rows: usize,

    /// Days to retain replay artifacts.
    /// Source: replay_artifact_registry.rs DEFAULT_RETENTION_DAYS = 30
    pub artifact_retention_days: u32,

    /// Duration (days) for shadow rollout evaluation.
    /// Source: replay_shadow_rollout.rs DEFAULT_SHADOW_DAYS = 14
    pub shadow_rollout_days: u32,
}

impl AuditTuning {
    pub const DEFAULT_RETENTION_DAYS: u32 = 90;
    pub const DEFAULT_APPROVAL_TTL_SECS: u64 = 900;
    pub const DEFAULT_MAX_RAW_QUERY_ROWS: usize = 100;
    pub const DEFAULT_ARTIFACT_RETENTION_DAYS: u32 = 30;
    pub const DEFAULT_SHADOW_ROLLOUT_DAYS: u32 = 14;
}

impl Default for AuditTuning {
    fn default() -> Self {
        Self {
            retention_days: Self::DEFAULT_RETENTION_DAYS,
            approval_ttl_secs: Self::DEFAULT_APPROVAL_TTL_SECS,
            max_raw_query_rows: Self::DEFAULT_MAX_RAW_QUERY_ROWS,
            artifact_retention_days: Self::DEFAULT_ARTIFACT_RETENTION_DAYS,
            shadow_rollout_days: Self::DEFAULT_SHADOW_ROLLOUT_DAYS,
        }
    }
}

// =============================================================================
// WebTuning
// =============================================================================

/// Web API server parameters.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct WebTuning {
    /// Bind address for the web server.
    /// Source: web.rs DEFAULT_HOST = "127.0.0.1"
    pub default_host: String,

    /// Port for the web server.
    /// Source: web.rs DEFAULT_PORT = 8000
    pub default_port: u16,

    /// Hard ceiling on list endpoint results.
    /// Source: web.rs MAX_LIMIT = 500
    pub max_list_limit: usize,

    /// Default page size for list endpoints.
    /// Source: web.rs DEFAULT_LIMIT = 50
    pub default_list_limit: usize,

    /// Max HTTP request body size (bytes).
    /// Source: web.rs MAX_REQUEST_BODY_BYTES = 64KB
    pub max_request_body_bytes: usize,

    /// Default max SSE event rate (Hz).
    /// Source: web.rs STREAM_DEFAULT_MAX_HZ = 50
    pub stream_default_max_hz: u32,

    /// Hard ceiling on SSE event rate (Hz).
    /// Source: web.rs STREAM_MAX_MAX_HZ = 500
    pub stream_max_max_hz: u32,

    /// SSE keep-alive ping interval (seconds).
    /// Source: web.rs STREAM_KEEPALIVE_SECS = 15
    pub stream_keepalive_secs: u64,

    /// Scan page size for streaming queries.
    /// Source: web.rs STREAM_SCAN_LIMIT = 256
    pub stream_scan_limit: usize,

    /// Max pages scanned per streaming query.
    /// Source: web.rs STREAM_SCAN_MAX_PAGES = 8
    pub stream_scan_max_pages: usize,
}

impl WebTuning {
    pub const DEFAULT_HOST: &'static str = "127.0.0.1";
    pub const DEFAULT_PORT: u16 = 8000;
    pub const DEFAULT_MAX_LIST_LIMIT: usize = 500;
    pub const DEFAULT_LIST_LIMIT: usize = 50;
    pub const DEFAULT_MAX_REQUEST_BODY_BYTES: usize = 64 * 1024;
    pub const DEFAULT_STREAM_DEFAULT_MAX_HZ: u32 = 50;
    pub const DEFAULT_STREAM_MAX_MAX_HZ: u32 = 500;
    pub const DEFAULT_STREAM_KEEPALIVE_SECS: u64 = 15;
    pub const DEFAULT_STREAM_SCAN_LIMIT: usize = 256;
    pub const DEFAULT_STREAM_SCAN_MAX_PAGES: usize = 8;
}

impl Default for WebTuning {
    fn default() -> Self {
        Self {
            default_host: Self::DEFAULT_HOST.to_string(),
            default_port: Self::DEFAULT_PORT,
            max_list_limit: Self::DEFAULT_MAX_LIST_LIMIT,
            default_list_limit: Self::DEFAULT_LIST_LIMIT,
            max_request_body_bytes: Self::DEFAULT_MAX_REQUEST_BODY_BYTES,
            stream_default_max_hz: Self::DEFAULT_STREAM_DEFAULT_MAX_HZ,
            stream_max_max_hz: Self::DEFAULT_STREAM_MAX_MAX_HZ,
            stream_keepalive_secs: Self::DEFAULT_STREAM_KEEPALIVE_SECS,
            stream_scan_limit: Self::DEFAULT_STREAM_SCAN_LIMIT,
            stream_scan_max_pages: Self::DEFAULT_STREAM_SCAN_MAX_PAGES,
        }
    }
}

// =============================================================================
// WorkflowsTuning
// =============================================================================

/// Configuration for a single CASS (Cross-Agent Session Search) query handler.
///
/// Three handlers use this pattern with slightly different defaults:
/// session_start, on_error, and auth. This struct unifies the duplicated
/// constant blocks from workflows/handlers.rs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct CassQueryConfig {
    /// Max hints to include in context injection.
    pub hint_limit: usize,
    /// CASS query timeout (seconds).
    pub timeout_secs: u64,
    /// How far back (days) to search session history.
    pub lookback_days: u32,
    /// Max characters in the query string sent to CASS.
    pub query_max_chars: usize,
    /// Max characters per hint in the response.
    pub hint_max_chars: usize,
}

impl CassQueryConfig {
    // Session-start handler defaults (workflows/handlers.rs lines 1015-1020)
    pub const SESSION_START_HINT_LIMIT: usize = 3;
    pub const SESSION_START_TIMEOUT_SECS: u64 = 8;
    pub const SESSION_START_LOOKBACK_DAYS: u32 = 30;
    pub const SESSION_START_QUERY_MAX_CHARS: usize = 180;
    pub const SESSION_START_HINT_MAX_CHARS: usize = 160;

    // On-error handler defaults (workflows/handlers.rs lines 1572-1576)
    // Note: timeout is 6s (not 8s) — intentionally faster for error paths.
    pub const ON_ERROR_HINT_LIMIT: usize = 3;
    pub const ON_ERROR_TIMEOUT_SECS: u64 = 6;
    pub const ON_ERROR_LOOKBACK_DAYS: u32 = 30;
    pub const ON_ERROR_QUERY_MAX_CHARS: usize = 200;
    pub const ON_ERROR_HINT_MAX_CHARS: usize = 180;

    // Auth handler defaults (workflows/handlers.rs lines 2301-2305)
    pub const AUTH_HINT_LIMIT: usize = 3;
    pub const AUTH_TIMEOUT_SECS: u64 = 8;
    pub const AUTH_LOOKBACK_DAYS: u32 = 30;
    pub const AUTH_QUERY_MAX_CHARS: usize = 160;
    pub const AUTH_HINT_MAX_CHARS: usize = 140;

    /// Defaults for the session-start handler.
    pub fn session_start() -> Self {
        Self {
            hint_limit: Self::SESSION_START_HINT_LIMIT,
            timeout_secs: Self::SESSION_START_TIMEOUT_SECS,
            lookback_days: Self::SESSION_START_LOOKBACK_DAYS,
            query_max_chars: Self::SESSION_START_QUERY_MAX_CHARS,
            hint_max_chars: Self::SESSION_START_HINT_MAX_CHARS,
        }
    }

    /// Defaults for the on-error handler.
    pub fn on_error() -> Self {
        Self {
            hint_limit: Self::ON_ERROR_HINT_LIMIT,
            timeout_secs: Self::ON_ERROR_TIMEOUT_SECS,
            lookback_days: Self::ON_ERROR_LOOKBACK_DAYS,
            query_max_chars: Self::ON_ERROR_QUERY_MAX_CHARS,
            hint_max_chars: Self::ON_ERROR_HINT_MAX_CHARS,
        }
    }

    /// Defaults for the auth handler.
    pub fn auth() -> Self {
        Self {
            hint_limit: Self::AUTH_HINT_LIMIT,
            timeout_secs: Self::AUTH_TIMEOUT_SECS,
            lookback_days: Self::AUTH_LOOKBACK_DAYS,
            query_max_chars: Self::AUTH_QUERY_MAX_CHARS,
            hint_max_chars: Self::AUTH_HINT_MAX_CHARS,
        }
    }
}

impl Default for CassQueryConfig {
    fn default() -> Self {
        Self::session_start()
    }
}

/// Workflow execution limits and CASS handler configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct WorkflowsTuning {
    /// Max steps per workflow descriptor.
    /// Source: workflows/descriptors.rs DESCRIPTOR_MAX_STEPS = 32
    pub max_steps: usize,

    /// Max wait-for timeout per step (ms).
    /// Source: workflows/descriptors.rs DESCRIPTOR_MAX_WAIT_TIMEOUT_MS = 120000
    pub max_wait_timeout_ms: u64,

    /// Max sleep duration per step (ms).
    /// Source: workflows/descriptors.rs DESCRIPTOR_MAX_SLEEP_MS = 30000
    pub max_sleep_ms: u64,

    /// Max text payload per step (bytes).
    /// Source: workflows/descriptors.rs DESCRIPTOR_MAX_TEXT_LEN = 8192
    pub max_text_len: usize,

    /// Max match pattern length (bytes).
    /// Source: workflows/descriptors.rs DESCRIPTOR_MAX_MATCH_LEN = 1024
    pub max_match_len: usize,

    /// CASS config for session-start handler.
    pub cass_session_start: CassQueryConfig,

    /// CASS config for on-error handler.
    pub cass_on_error: CassQueryConfig,

    /// CASS config for auth handler.
    pub cass_auth: CassQueryConfig,

    /// Swarm learning index timeout (seconds).
    /// Source: workflows/handlers.rs SWARM_LEARNING_INDEX_TIMEOUT_SECS = 30
    pub swarm_learning_index_timeout_secs: u64,

    /// Claude Code rate limit cooldown (ms).
    /// Source: workflows/handlers.rs CLAUDE_CODE_LIMITS_COOLDOWN_MS = 600000
    pub claude_code_limits_cooldown_ms: u64,

    /// Context injection cooldown (ms) for session-start handler.
    /// Source: workflows/handlers.rs SESSION_START_CONTEXT_COOLDOWN_MS = 600000
    pub session_start_context_cooldown_ms: u64,

    /// Context injection cooldown (ms) for on-error handler.
    /// Source: workflows/handlers.rs ON_ERROR_CASS_COOLDOWN_MS = 180000
    pub on_error_cooldown_ms: u64,

    /// Cooldown (ms) for swarm learning index handler.
    /// Source: workflows/handlers.rs SWARM_LEARNING_INDEX_COOLDOWN_MS = 900000
    pub swarm_learning_index_cooldown_ms: u64,

    /// Cooldown (ms) for auth handler.
    /// Source: workflows/handlers.rs AUTH_COOLDOWN_MS = 300000
    pub auth_cooldown_ms: u64,
}

impl WorkflowsTuning {
    // Descriptor limits
    pub const DEFAULT_MAX_STEPS: usize = 32;
    pub const DEFAULT_MAX_WAIT_TIMEOUT_MS: u64 = 120_000;
    pub const DEFAULT_MAX_SLEEP_MS: u64 = 30_000;
    pub const DEFAULT_MAX_TEXT_LEN: usize = 8192;
    pub const DEFAULT_MAX_MATCH_LEN: usize = 1024;
    /// Default session-start context cooldown (ms).
    pub const DEFAULT_SESSION_START_CONTEXT_COOLDOWN_MS: u64 = 600_000;
    /// Default Claude Code limits cooldown (ms).
    pub const DEFAULT_CLAUDE_CODE_LIMITS_COOLDOWN_MS: u64 = 600_000;
    /// Default swarm learning index timeout (seconds).
    pub const DEFAULT_SWARM_LEARNING_INDEX_TIMEOUT_SECS: u64 = 30;
    /// Default on-error cooldown (ms).
    pub const DEFAULT_ON_ERROR_COOLDOWN_MS: u64 = 3 * 60 * 1000;
    /// Default swarm learning index cooldown (ms).
    pub const DEFAULT_SWARM_LEARNING_INDEX_COOLDOWN_MS: u64 = 15 * 60 * 1000;
    /// Default auth cooldown (ms).
    pub const DEFAULT_AUTH_COOLDOWN_MS: u64 = 5 * 60 * 1000;
}

impl Default for WorkflowsTuning {
    fn default() -> Self {
        Self {
            max_steps: Self::DEFAULT_MAX_STEPS,
            max_wait_timeout_ms: Self::DEFAULT_MAX_WAIT_TIMEOUT_MS,
            max_sleep_ms: Self::DEFAULT_MAX_SLEEP_MS,
            max_text_len: Self::DEFAULT_MAX_TEXT_LEN,
            max_match_len: Self::DEFAULT_MAX_MATCH_LEN,
            cass_session_start: CassQueryConfig::session_start(),
            cass_on_error: CassQueryConfig::on_error(),
            cass_auth: CassQueryConfig::auth(),
            swarm_learning_index_timeout_secs: Self::DEFAULT_SWARM_LEARNING_INDEX_TIMEOUT_SECS,
            claude_code_limits_cooldown_ms: Self::DEFAULT_CLAUDE_CODE_LIMITS_COOLDOWN_MS,
            session_start_context_cooldown_ms: Self::DEFAULT_SESSION_START_CONTEXT_COOLDOWN_MS,
            on_error_cooldown_ms: Self::DEFAULT_ON_ERROR_COOLDOWN_MS,
            swarm_learning_index_cooldown_ms: Self::DEFAULT_SWARM_LEARNING_INDEX_COOLDOWN_MS,
            auth_cooldown_ms: Self::DEFAULT_AUTH_COOLDOWN_MS,
        }
    }
}

// =============================================================================
// SearchTuning
// =============================================================================

/// Search and query parameters.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct SearchTuning {
    /// Default result limit for search queries.
    /// Source: query_contract.rs SEARCH_LIMIT_DEFAULT = 20
    pub default_limit: usize,

    /// Hard ceiling on search result count.
    /// Source: query_contract.rs SEARCH_LIMIT_MAX = 1000
    pub max_limit: usize,

    /// Default limit for saved search list queries.
    /// Source: storage.rs SAVED_SEARCH_DEFAULT_LIMIT = 50
    pub saved_search_limit: usize,

    /// Max records exported via CASS export.
    /// Source: cass.rs DEFAULT_CASS_EXPORT_LIMIT = 1000
    pub cass_export_limit: usize,

    /// Tantivy index writer memory budget (bytes).
    /// Source: recorder_lexical_ingest.rs DEFAULT_WRITER_MEMORY_BYTES = 50MB
    pub tantivy_writer_memory_bytes: usize,
}

impl SearchTuning {
    /// Default search limit.
    pub const DEFAULT_LIMIT: usize = 20;
    /// Default max search limit.
    pub const DEFAULT_MAX_LIMIT: usize = 1000;
    /// Default saved search limit.
    pub const DEFAULT_SAVED_SEARCH_LIMIT: usize = 50;
    /// Default CASS export limit.
    pub const DEFAULT_CASS_EXPORT_LIMIT: usize = 1000;
    /// Default tantivy writer memory (50 MB, exactly 50_000_000 bytes).
    pub const DEFAULT_TANTIVY_WRITER_MEMORY_BYTES: usize = 50_000_000;
}

impl Default for SearchTuning {
    fn default() -> Self {
        Self {
            default_limit: Self::DEFAULT_LIMIT,
            max_limit: Self::DEFAULT_MAX_LIMIT,
            saved_search_limit: Self::DEFAULT_SAVED_SEARCH_LIMIT,
            cass_export_limit: Self::DEFAULT_CASS_EXPORT_LIMIT,
            tantivy_writer_memory_bytes: Self::DEFAULT_TANTIVY_WRITER_MEMORY_BYTES,
        }
    }
}

// =============================================================================
// WireProtocolTuning
// =============================================================================

/// Wire protocol (distributed mode) limits.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct WireProtocolTuning {
    /// Max message size for the distributed wire protocol (bytes).
    /// Both sender and receiver must agree on this value.
    /// Source: wire_protocol.rs MAX_MESSAGE_SIZE = 1MB
    pub max_message_size: usize,

    /// Max sender ID length (bytes).
    /// Source: wire_protocol.rs MAX_SENDER_ID_LEN = 128
    pub max_sender_id_len: usize,
}

impl WireProtocolTuning {
    /// Canonical default wire protocol message size limit (1 MiB).
    pub const DEFAULT_MAX_MESSAGE_SIZE: usize = 1024 * 1024;
    /// Canonical default max sender ID length (128 bytes).
    pub const DEFAULT_MAX_SENDER_ID_LEN: usize = 128;
    /// Lowest practical sender ID limit. Distributed session scoping appends
    /// replay-disambiguation suffixes, so values below this become unusable.
    pub const MIN_MAX_SENDER_ID_LEN: usize = 32;
    /// Generous upper bound that prevents pathological per-message identities.
    pub const MAX_MAX_SENDER_ID_LEN: usize = 4096;
}

impl Default for WireProtocolTuning {
    fn default() -> Self {
        Self {
            max_message_size: Self::DEFAULT_MAX_MESSAGE_SIZE,
            max_sender_id_len: Self::DEFAULT_MAX_SENDER_ID_LEN,
        }
    }
}

// =============================================================================
// IpcTuning
// =============================================================================

/// IPC (local Unix socket RPC) parameters.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct IpcTuning {
    /// Max IPC message size (bytes).
    /// Source: ipc.rs MAX_MESSAGE_SIZE = 131072
    pub max_message_size: usize,

    /// IPC connection accept poll interval (ms).
    /// Source: ipc.rs IPC_ACCEPT_POLL_INTERVAL = 100
    pub accept_poll_interval_ms: u64,
}

impl IpcTuning {
    /// Canonical default IPC message size limit (128 KB).
    ///
    /// Exposed as a const so callers that cannot access a TuningConfig instance
    /// (e.g., CLI validation in main.rs) can reference the single source of truth.
    pub const DEFAULT_MAX_MESSAGE_SIZE: usize = 128 * 1024;
    /// Default IPC accept poll interval (ms).
    pub const DEFAULT_ACCEPT_POLL_INTERVAL_MS: u64 = 100;
}

impl Default for IpcTuning {
    fn default() -> Self {
        Self {
            max_message_size: Self::DEFAULT_MAX_MESSAGE_SIZE,
            accept_poll_interval_ms: Self::DEFAULT_ACCEPT_POLL_INTERVAL_MS,
        }
    }
}

// =============================================================================
// WeztermTuning
// =============================================================================

/// WezTerm backend adapter timeouts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct WeztermTuning {
    /// CLI command timeout (seconds).
    /// Source: wezterm.rs DEFAULT_TIMEOUT_SECS = 30
    pub timeout_secs: u64,

    /// Retry delay between CLI attempts (ms).
    /// Source: wezterm.rs DEFAULT_RETRY_DELAY_MS = 200
    pub retry_delay_ms: u64,

    /// Max error output retained from CLI (bytes).
    /// Source: wezterm.rs MAX_ERROR_BYTES = 8192
    pub max_error_bytes: usize,

    /// Mux socket connect timeout (ms).
    /// Source: vendored mux_client.rs DEFAULT_CONNECT_TIMEOUT_MS = 5000
    pub connect_timeout_ms: u64,

    /// Mux socket read timeout (ms).
    /// Source: vendored mux_client.rs DEFAULT_READ_TIMEOUT_MS = 5000
    pub read_timeout_ms: u64,

    /// Mux socket write timeout (ms).
    /// Source: vendored mux_client.rs DEFAULT_WRITE_TIMEOUT_MS = 5000
    pub write_timeout_ms: u64,
}

impl WeztermTuning {
    /// Default CLI timeout (seconds).
    pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
    /// Default retry delay (ms).
    pub const DEFAULT_RETRY_DELAY_MS: u64 = 200;
    /// Default max error bytes.
    pub const DEFAULT_MAX_ERROR_BYTES: usize = 8192;
    /// Default mux connect timeout (ms).
    pub const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 5000;
    /// Default mux read timeout (ms).
    pub const DEFAULT_READ_TIMEOUT_MS: u64 = 5000;
    /// Default mux write timeout (ms).
    pub const DEFAULT_WRITE_TIMEOUT_MS: u64 = 5000;
}

impl Default for WeztermTuning {
    fn default() -> Self {
        Self {
            timeout_secs: Self::DEFAULT_TIMEOUT_SECS,
            retry_delay_ms: Self::DEFAULT_RETRY_DELAY_MS,
            max_error_bytes: Self::DEFAULT_MAX_ERROR_BYTES,
            connect_timeout_ms: Self::DEFAULT_CONNECT_TIMEOUT_MS,
            read_timeout_ms: Self::DEFAULT_READ_TIMEOUT_MS,
            write_timeout_ms: Self::DEFAULT_WRITE_TIMEOUT_MS,
        }
    }
}

// =============================================================================
// Validation
// =============================================================================

impl TuningConfig {
    /// Validate all fields are within acceptable ranges.
    ///
    /// Returns a list of validation errors. Empty list means valid.
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();

        // Runtime
        if self.runtime.output_coalesce_window_ms < 5 {
            errors.push("tuning.runtime.output_coalesce_window_ms must be >= 5".into());
        }
        if self.runtime.output_coalesce_max_delay_ms < self.runtime.output_coalesce_window_ms {
            errors.push(
                "tuning.runtime.output_coalesce_max_delay_ms must be >= output_coalesce_window_ms"
                    .into(),
            );
        }
        if self.runtime.output_coalesce_max_bytes < 4096 {
            errors.push("tuning.runtime.output_coalesce_max_bytes must be >= 4096".into());
        }
        if self.runtime.resize_watchdog_warning_ms >= self.runtime.resize_watchdog_critical_ms {
            errors.push(
                "tuning.runtime.resize_watchdog_warning_ms must be < resize_watchdog_critical_ms"
                    .into(),
            );
        }
        if self.runtime.resize_watchdog_stalled_limit < 1 {
            errors.push("tuning.runtime.resize_watchdog_stalled_limit must be >= 1".into());
        }

        // Backpressure
        if !(0.1..=0.99).contains(&self.backpressure.warn_ratio) {
            errors.push("tuning.backpressure.warn_ratio must be in [0.1, 0.99]".into());
        }

        // Snapshot
        if self.snapshot.trigger_bridge_tick_secs < 5 {
            errors.push("tuning.snapshot.trigger_bridge_tick_secs must be >= 5".into());
        }

        // Ingest
        if self.ingest.max_persist_segment_bytes < 1024 {
            errors.push("tuning.ingest.max_persist_segment_bytes must be >= 1024".into());
        }
        if self.ingest.max_record_payload_bytes < 1024 * 1024 {
            errors.push("tuning.ingest.max_record_payload_bytes must be >= 1MB".into());
        }

        // Patterns
        if self.patterns.max_seen_keys < 100 {
            errors.push("tuning.patterns.max_seen_keys must be >= 100".into());
        }
        if self.patterns.max_tail_size_bytes < 256 {
            errors.push("tuning.patterns.max_tail_size_bytes must be >= 256".into());
        }
        if !(0.001..=0.2).contains(&self.patterns.bloom_false_positive_rate) {
            errors.push("tuning.patterns.bloom_false_positive_rate must be in [0.001, 0.2]".into());
        }

        // Policy
        if self.policy.rate_limit_window_secs < 10 {
            errors.push("tuning.policy.rate_limit_window_secs must be >= 10".into());
        }
        if self.policy.max_tracked_panes < 32 {
            errors.push("tuning.policy.max_tracked_panes must be >= 32".into());
        }
        if self.policy.max_events_per_pane < 8 {
            errors.push("tuning.policy.max_events_per_pane must be >= 8".into());
        }

        // Audit
        if self.audit.retention_days < 1 {
            errors.push("tuning.audit.retention_days must be >= 1".into());
        }
        if self.audit.approval_ttl_secs < 60 {
            errors.push("tuning.audit.approval_ttl_secs must be >= 60".into());
        }

        // Web
        if self.web.default_list_limit > self.web.max_list_limit {
            errors
                .push("tuning.web.default_list_limit must be <= tuning.web.max_list_limit".into());
        }
        if self.web.max_list_limit < 10 {
            errors.push("tuning.web.max_list_limit must be >= 10".into());
        }
        if self.web.stream_keepalive_secs < 1 {
            errors.push("tuning.web.stream_keepalive_secs must be >= 1".into());
        }
        // br-ft-g681u: SSE rate and body-size invariants. The
        // runtime path forces `max_hz.max(1)` and divides by
        // `max_request_body_bytes` for body-too-large rejection,
        // so zero values produce contradictory behavior:
        // advertised max_hz=0 while the scheduler runs at 1Hz, or
        // Content-Length accidentally turning into deny-all. Catch
        // at validate() so the operator sees a config error
        // instead of a confusing runtime mismatch.
        if self.web.max_request_body_bytes < 1 {
            errors.push("br-ft-g681u: tuning.web.max_request_body_bytes must be >= 1".into());
        }
        if self.web.stream_default_max_hz < 1 {
            errors.push("br-ft-g681u: tuning.web.stream_default_max_hz must be >= 1".into());
        }
        if self.web.stream_max_max_hz < self.web.stream_default_max_hz {
            errors.push(
                "br-ft-g681u: tuning.web.stream_max_max_hz must be >= tuning.web.stream_default_max_hz".into(),
            );
        }
        if self.web.stream_scan_limit < 1 {
            errors.push("br-ft-g681u: tuning.web.stream_scan_limit must be >= 1".into());
        }
        if self.web.stream_scan_max_pages < 1 {
            errors.push("br-ft-g681u: tuning.web.stream_scan_max_pages must be >= 1".into());
        }

        // Workflows
        if self.workflows.max_steps < 4 {
            errors.push("tuning.workflows.max_steps must be >= 4".into());
        }
        if self.workflows.max_wait_timeout_ms < 1000 {
            errors.push("tuning.workflows.max_wait_timeout_ms must be >= 1000".into());
        }

        // Search
        if self.search.default_limit > self.search.max_limit {
            errors.push("tuning.search.default_limit must be <= tuning.search.max_limit".into());
        }
        if self.search.max_limit < 10 {
            errors.push("tuning.search.max_limit must be >= 10".into());
        }
        if self.search.tantivy_writer_memory_bytes < 10 * 1024 * 1024 {
            errors.push("tuning.search.tantivy_writer_memory_bytes must be >= 10MB".into());
        }

        // Wire protocol
        if self.wire_protocol.max_message_size < 64 * 1024 {
            errors.push("tuning.wire_protocol.max_message_size must be >= 64KB".into());
        }
        if self.wire_protocol.max_message_size > 64 * 1024 * 1024 {
            errors.push("tuning.wire_protocol.max_message_size must be <= 64MB".into());
        }
        if self.wire_protocol.max_sender_id_len < WireProtocolTuning::MIN_MAX_SENDER_ID_LEN {
            errors.push("tuning.wire_protocol.max_sender_id_len must be >= 32".into());
        }
        if self.wire_protocol.max_sender_id_len > WireProtocolTuning::MAX_MAX_SENDER_ID_LEN {
            errors.push("tuning.wire_protocol.max_sender_id_len must be <= 4096".into());
        }

        // IPC
        if self.ipc.max_message_size < 16 * 1024 {
            errors.push("tuning.ipc.max_message_size must be >= 16KB".into());
        }
        if self.ipc.accept_poll_interval_ms < 10 {
            errors.push("tuning.ipc.accept_poll_interval_ms must be >= 10".into());
        }

        errors
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_f64_eq(left: f64, right: f64) {
        assert!((left - right).abs() < f64::EPSILON);
    }

    #[test]
    fn default_validates_clean() {
        let cfg = TuningConfig::default();
        let errors = cfg.validate();
        assert!(
            errors.is_empty(),
            "default config should validate: {errors:?}"
        );
    }

    #[test]
    fn serde_roundtrip_preserves_defaults() {
        let original = TuningConfig::default();
        let toml_str = toml::to_string_pretty(&original).expect("serialize");
        let deserialized: TuningConfig = toml::from_str(&toml_str).expect("deserialize");
        assert_eq!(original, deserialized);
    }

    #[test]
    fn partial_override_preserves_other_defaults() {
        let toml_str = r"
[runtime]
output_coalesce_window_ms = 100

[web]
default_port = 9000
";
        let cfg: TuningConfig = toml::from_str(toml_str).expect("parse");
        assert_eq!(cfg.runtime.output_coalesce_window_ms, 100);
        assert_eq!(cfg.web.default_port, 9000);
        // Other fields retain defaults
        assert_eq!(cfg.runtime.output_coalesce_max_delay_ms, 200);
        assert_f64_eq(cfg.backpressure.warn_ratio, 0.75);
        assert_eq!(cfg.policy.rate_limit_window_secs, 60);
    }

    #[test]
    fn defaults_match_current_hardcoded_constants() {
        let cfg = TuningConfig::default();

        // Runtime (runtime.rs)
        assert_eq!(cfg.runtime.output_coalesce_window_ms, 50);
        assert_eq!(cfg.runtime.output_coalesce_max_delay_ms, 200);
        assert_eq!(cfg.runtime.output_coalesce_max_bytes, 256 * 1024);
        assert_eq!(cfg.runtime.telemetry_percentile_window, 1024);
        assert_eq!(cfg.runtime.resize_watchdog_warning_ms, 2_000);
        assert_eq!(cfg.runtime.resize_watchdog_critical_ms, 8_000);
        assert_eq!(cfg.runtime.resize_watchdog_stalled_limit, 2);
        assert_eq!(cfg.runtime.resize_watchdog_sample_limit, 8);
        assert_f64_eq(cfg.runtime.storage_lock_wait_warn_ms, 15.0);
        assert_f64_eq(cfg.runtime.storage_lock_hold_warn_ms, 75.0);
        assert_eq!(
            cfg.runtime.cursor_snapshot_memory_warn_bytes,
            64 * 1024 * 1024
        );
        assert_eq!(cfg.runtime.state_detection_max_age_secs, 300);

        // Backpressure
        assert_f64_eq(cfg.backpressure.warn_ratio, 0.75);

        // Snapshot
        assert_eq!(cfg.snapshot.trigger_bridge_tick_secs, 30);
        assert_eq!(cfg.snapshot.idle_window_secs, 300);
        assert_eq!(cfg.snapshot.memory_trigger_cooldown_secs, 120);

        // Ingest
        assert_eq!(cfg.ingest.max_persist_segment_bytes, 64 * 1024);
        assert_eq!(cfg.ingest.max_record_payload_bytes, 64 * 1024 * 1024);

        // Patterns
        assert_eq!(cfg.patterns.max_seen_keys, 1000);
        assert_eq!(cfg.patterns.max_tail_size_bytes, 2048);
        assert_f64_eq(cfg.patterns.bloom_false_positive_rate, 0.01);

        // Policy
        assert_eq!(cfg.policy.rate_limit_window_secs, 60);
        assert_eq!(cfg.policy.max_tracked_panes, 256);
        assert_eq!(cfg.policy.max_events_per_pane, 64);
        assert_eq!(cfg.policy.cost_tracker_max_panes, 512);

        // Audit
        assert_eq!(cfg.audit.retention_days, 90);
        assert_eq!(cfg.audit.approval_ttl_secs, 900);
        assert_eq!(cfg.audit.max_raw_query_rows, 100);
        assert_eq!(cfg.audit.artifact_retention_days, 30);
        assert_eq!(cfg.audit.shadow_rollout_days, 14);

        // Web
        assert_eq!(cfg.web.default_host, "127.0.0.1");
        assert_eq!(cfg.web.default_port, 8000);
        assert_eq!(cfg.web.max_list_limit, 500);
        assert_eq!(cfg.web.default_list_limit, 50);
        assert_eq!(cfg.web.max_request_body_bytes, 64 * 1024);
        assert_eq!(cfg.web.stream_default_max_hz, 50);
        assert_eq!(cfg.web.stream_max_max_hz, 500);
        assert_eq!(cfg.web.stream_keepalive_secs, 15);
        assert_eq!(cfg.web.stream_scan_limit, 256);
        assert_eq!(cfg.web.stream_scan_max_pages, 8);

        // Workflows
        assert_eq!(cfg.workflows.max_steps, 32);
        assert_eq!(cfg.workflows.max_wait_timeout_ms, 120_000);
        assert_eq!(cfg.workflows.max_sleep_ms, 30_000);
        assert_eq!(cfg.workflows.max_text_len, 8192);
        assert_eq!(cfg.workflows.max_match_len, 1024);
        assert_eq!(cfg.workflows.swarm_learning_index_timeout_secs, 30);
        assert_eq!(cfg.workflows.claude_code_limits_cooldown_ms, 600_000);
        assert_eq!(cfg.workflows.session_start_context_cooldown_ms, 600_000);
        assert_eq!(cfg.workflows.on_error_cooldown_ms, 180_000);
        assert_eq!(cfg.workflows.swarm_learning_index_cooldown_ms, 900_000);
        assert_eq!(cfg.workflows.auth_cooldown_ms, 300_000);

        // CASS handler configs
        let ss = &cfg.workflows.cass_session_start;
        assert_eq!(ss.hint_limit, 3);
        assert_eq!(ss.timeout_secs, 8);
        assert_eq!(ss.lookback_days, 30);
        assert_eq!(ss.query_max_chars, 180);
        assert_eq!(ss.hint_max_chars, 160);

        let oe = &cfg.workflows.cass_on_error;
        assert_eq!(oe.timeout_secs, 6); // intentionally different
        assert_eq!(oe.query_max_chars, 200);
        assert_eq!(oe.hint_max_chars, 180);

        let au = &cfg.workflows.cass_auth;
        assert_eq!(au.timeout_secs, 8);
        assert_eq!(au.query_max_chars, 160);
        assert_eq!(au.hint_max_chars, 140);

        // Search
        assert_eq!(cfg.search.default_limit, 20);
        assert_eq!(cfg.search.max_limit, 1000);
        assert_eq!(cfg.search.saved_search_limit, 50);
        assert_eq!(cfg.search.cass_export_limit, 1000);
        assert_eq!(cfg.search.tantivy_writer_memory_bytes, 50_000_000);

        // Wire protocol
        assert_eq!(cfg.wire_protocol.max_message_size, 1024 * 1024);
        assert_eq!(cfg.wire_protocol.max_sender_id_len, 128);

        // IPC
        assert_eq!(cfg.ipc.max_message_size, 128 * 1024);
        assert_eq!(cfg.ipc.accept_poll_interval_ms, 100);

        // Wezterm
        assert_eq!(cfg.wezterm.timeout_secs, 30);
        assert_eq!(cfg.wezterm.retry_delay_ms, 200);
        assert_eq!(cfg.wezterm.max_error_bytes, 8192);
        assert_eq!(cfg.wezterm.connect_timeout_ms, 5000);
        assert_eq!(cfg.wezterm.read_timeout_ms, 5000);
        assert_eq!(cfg.wezterm.write_timeout_ms, 5000);
    }

    #[test]
    fn validation_catches_bad_values() {
        let mut cfg = TuningConfig::default();
        cfg.ingest.max_persist_segment_bytes = 512;
        cfg.ingest.max_record_payload_bytes = 512 * 1024;
        cfg.backpressure.warn_ratio = 1.5;
        cfg.patterns.bloom_false_positive_rate = 0.0;
        cfg.search.tantivy_writer_memory_bytes = 5 * 1024 * 1024;
        cfg.web.default_list_limit = 1000;
        cfg.web.max_list_limit = 5;
        cfg.wire_protocol.max_sender_id_len = 0;

        let errors = cfg.validate();
        assert!(
            errors
                .iter()
                .any(|err| err == "tuning.ingest.max_persist_segment_bytes must be >= 1024")
        );
        assert!(
            errors
                .iter()
                .any(|err| err == "tuning.ingest.max_record_payload_bytes must be >= 1MB")
        );
        assert!(
            errors
                .iter()
                .any(|err| err == "tuning.search.tantivy_writer_memory_bytes must be >= 10MB")
        );
        assert!(
            errors
                .iter()
                .any(|err| err == "tuning.wire_protocol.max_sender_id_len must be >= 32")
        );
        assert!(
            errors.len() >= 6,
            "expected multiple errors including ingest/search bounds: {errors:?}"
        );
    }

    // ── br-ft-g681u: web SSE rate + body-size invariants ──

    #[test]
    fn validation_catches_zero_max_request_body_bytes_ft_g681u() {
        // br-ft-g681u: max_request_body_bytes=0 turns Content-
        // Length rejection into accidental deny-all.
        let mut cfg = TuningConfig::default();
        cfg.web.max_request_body_bytes = 0;
        let errors = cfg.validate();
        assert!(
            errors
                .iter()
                .any(|e| e.contains("br-ft-g681u")
                    && e.contains("max_request_body_bytes must be >= 1")),
            "expected max_request_body_bytes error; got {errors:?}"
        );
    }

    #[test]
    fn validation_catches_zero_stream_default_max_hz_ft_g681u() {
        // br-ft-g681u: stream_default_max_hz=0 produces an
        // advertised max_hz of 0 in SSE open frames while the
        // scheduler runs at 1Hz.
        let mut cfg = TuningConfig::default();
        cfg.web.stream_default_max_hz = 0;
        let errors = cfg.validate();
        assert!(
            errors
                .iter()
                .any(|e| e.contains("br-ft-g681u")
                    && e.contains("stream_default_max_hz must be >= 1")),
            "expected stream_default_max_hz error; got {errors:?}"
        );
    }

    #[test]
    fn validation_catches_misordered_stream_max_hz_ft_g681u() {
        // br-ft-g681u: stream_max_max_hz < stream_default_max_hz
        // would let the actual max go below the documented default.
        let mut cfg = TuningConfig::default();
        cfg.web.stream_default_max_hz = 30;
        cfg.web.stream_max_max_hz = 10;
        let errors = cfg.validate();
        assert!(
            errors.iter().any(|e| e.contains("br-ft-g681u")
                && e.contains("stream_max_max_hz must be >= tuning.web.stream_default_max_hz")),
            "expected stream_max_max_hz ordering error; got {errors:?}"
        );
    }

    #[test]
    fn validation_catches_zero_stream_scan_limits_ft_g681u() {
        let mut cfg = TuningConfig::default();
        cfg.web.stream_scan_limit = 0;
        cfg.web.stream_scan_max_pages = 0;
        let errors = cfg.validate();
        assert!(
            errors
                .iter()
                .any(|e| e.contains("br-ft-g681u")
                    && e.contains("stream_scan_limit must be >= 1")),
            "expected stream_scan_limit error; got {errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|e| e.contains("br-ft-g681u")
                    && e.contains("stream_scan_max_pages must be >= 1")),
            "expected stream_scan_max_pages error; got {errors:?}"
        );
    }

    #[test]
    fn validation_default_passes_web_invariants_ft_g681u() {
        // Sanity: defaults must not trip any of the new checks.
        let cfg = TuningConfig::default();
        let errors = cfg.validate();
        // No web errors at all.
        let web_errors: Vec<&String> = errors.iter().filter(|e| e.contains("tuning.web.")).collect();
        assert!(
            web_errors.is_empty(),
            "default web tuning must validate; got {web_errors:?}"
        );
    }

    #[test]
    fn validation_catches_wire_protocol_sender_id_upper_bound() {
        let mut cfg = TuningConfig::default();
        cfg.wire_protocol.max_sender_id_len = WireProtocolTuning::MAX_MAX_SENDER_ID_LEN + 1;

        let errors = cfg.validate();
        assert!(
            errors
                .iter()
                .any(|err| err == "tuning.wire_protocol.max_sender_id_len must be <= 4096")
        );
    }

    #[test]
    fn cass_query_config_constructors_differ() {
        let ss = CassQueryConfig::session_start();
        let oe = CassQueryConfig::on_error();
        let au = CassQueryConfig::auth();

        assert_ne!(ss.timeout_secs, oe.timeout_secs);
        assert_ne!(ss.query_max_chars, oe.query_max_chars);
        assert_ne!(ss.hint_max_chars, au.hint_max_chars);
    }

    #[test]
    fn empty_toml_produces_default() {
        let cfg: TuningConfig = toml::from_str("").expect("parse empty");
        assert_eq!(cfg, TuningConfig::default());
    }

    /// Verify all DEFAULT_ associated consts match the Default impl values.
    /// This catches accidental divergence between the const and the Default.
    #[test]
    fn associated_consts_match_default_impl() {
        let runtime = RuntimeTuning::default();
        assert_eq!(
            runtime.output_coalesce_window_ms,
            RuntimeTuning::DEFAULT_OUTPUT_COALESCE_WINDOW_MS
        );
        assert_eq!(
            runtime.resize_watchdog_warning_ms,
            RuntimeTuning::DEFAULT_RESIZE_WATCHDOG_WARNING_MS
        );
        assert_eq!(
            runtime.state_detection_max_age_secs,
            RuntimeTuning::DEFAULT_STATE_DETECTION_MAX_AGE_SECS
        );

        let backpressure = BackpressureTuning::default();
        assert_f64_eq(
            backpressure.warn_ratio,
            BackpressureTuning::DEFAULT_WARN_RATIO,
        );

        let snapshot = SnapshotTuning::default();
        assert_eq!(
            snapshot.idle_window_secs,
            SnapshotTuning::DEFAULT_IDLE_WINDOW_SECS
        );

        let ingest = IngestTuning::default();
        assert_eq!(
            ingest.max_persist_segment_bytes,
            IngestTuning::DEFAULT_MAX_PERSIST_SEGMENT_BYTES
        );

        let patterns = PatternsTuning::default();
        assert_eq!(
            patterns.max_seen_keys,
            PatternsTuning::DEFAULT_MAX_SEEN_KEYS
        );
        assert_f64_eq(
            patterns.bloom_false_positive_rate,
            PatternsTuning::DEFAULT_BLOOM_FALSE_POSITIVE_RATE,
        );

        let pol = PolicyTuning::default();
        assert_eq!(
            pol.rate_limit_window_secs,
            PolicyTuning::DEFAULT_RATE_LIMIT_WINDOW_SECS
        );
        assert_eq!(
            pol.max_tracked_panes,
            PolicyTuning::DEFAULT_MAX_TRACKED_PANES
        );
        assert_eq!(
            pol.cost_tracker_max_panes,
            PolicyTuning::DEFAULT_COST_TRACKER_MAX_PANES
        );

        let audit = AuditTuning::default();
        assert_eq!(audit.retention_days, AuditTuning::DEFAULT_RETENTION_DAYS);
        assert_eq!(
            audit.approval_ttl_secs,
            AuditTuning::DEFAULT_APPROVAL_TTL_SECS
        );

        let web = WebTuning::default();
        assert_eq!(web.default_port, WebTuning::DEFAULT_PORT);
        assert_eq!(web.max_list_limit, WebTuning::DEFAULT_MAX_LIST_LIMIT);
        assert_eq!(web.stream_scan_limit, WebTuning::DEFAULT_STREAM_SCAN_LIMIT);

        let wf = WorkflowsTuning::default();
        assert_eq!(wf.max_steps, WorkflowsTuning::DEFAULT_MAX_STEPS);
        assert_eq!(
            wf.claude_code_limits_cooldown_ms,
            WorkflowsTuning::DEFAULT_CLAUDE_CODE_LIMITS_COOLDOWN_MS
        );

        let sr = SearchTuning::default();
        assert_eq!(sr.default_limit, SearchTuning::DEFAULT_LIMIT);
        assert_eq!(sr.max_limit, SearchTuning::DEFAULT_MAX_LIMIT);
        assert_eq!(
            sr.tantivy_writer_memory_bytes,
            SearchTuning::DEFAULT_TANTIVY_WRITER_MEMORY_BYTES
        );

        let wp = WireProtocolTuning::default();
        assert_eq!(
            wp.max_message_size,
            WireProtocolTuning::DEFAULT_MAX_MESSAGE_SIZE
        );
        assert_eq!(
            wp.max_sender_id_len,
            WireProtocolTuning::DEFAULT_MAX_SENDER_ID_LEN
        );
        assert_eq!(WireProtocolTuning::MIN_MAX_SENDER_ID_LEN, 32);
        assert_eq!(WireProtocolTuning::MAX_MAX_SENDER_ID_LEN, 4096);

        let ip = IpcTuning::default();
        assert_eq!(ip.max_message_size, IpcTuning::DEFAULT_MAX_MESSAGE_SIZE);

        let wz = WeztermTuning::default();
        assert_eq!(wz.timeout_secs, WeztermTuning::DEFAULT_TIMEOUT_SECS);
        assert_eq!(
            wz.connect_timeout_ms,
            WeztermTuning::DEFAULT_CONNECT_TIMEOUT_MS
        );

        // CassQueryConfig associated consts
        assert_eq!(
            CassQueryConfig::SESSION_START_TIMEOUT_SECS,
            CassQueryConfig::session_start().timeout_secs
        );
        assert_eq!(
            CassQueryConfig::ON_ERROR_TIMEOUT_SECS,
            CassQueryConfig::on_error().timeout_secs
        );
        assert_eq!(
            CassQueryConfig::AUTH_HINT_MAX_CHARS,
            CassQueryConfig::auth().hint_max_chars
        );
    }
}
