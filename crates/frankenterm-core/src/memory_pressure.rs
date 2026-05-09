//! Memory pressure monitoring for adaptive pane management.
//!
//! Samples system memory utilization and classifies it into pressure tiers
//! that drive scrollback compression, eviction, and pane cleanup decisions.
//!
//! - **Linux**: reads `/proc/pressure/memory` (PSI avg10) and `/proc/meminfo`
//! - **macOS**: reads memory stats via `vm_stat` and `sysctl`
//! - **Other**: returns `Green` (no monitoring available)

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

// =============================================================================
// Pressure tiers
// =============================================================================

/// Memory pressure severity tier.
///
/// Aligned with [`CpuPressureTier`](crate::cpu_pressure::CpuPressureTier) and
/// [`BackpressureTier`](crate::backpressure::BackpressureTier).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryPressureTier {
    /// Memory utilization below warning threshold.
    Green,
    /// Moderate pressure — compress idle pane scrollback.
    Yellow,
    /// High pressure — evict scrollback to disk, pause captures.
    Orange,
    /// Critical — kill largest idle pane, emergency eviction.
    Red,
}

impl std::fmt::Display for MemoryPressureTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Green => write!(f, "GREEN"),
            Self::Yellow => write!(f, "YELLOW"),
            Self::Orange => write!(f, "ORANGE"),
            Self::Red => write!(f, "RED"),
        }
    }
}

impl MemoryPressureTier {
    /// Numeric value for gauge metrics (0-3).
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Green => 0,
            Self::Yellow => 1,
            Self::Orange => 2,
            Self::Red => 3,
        }
    }

    /// Suggested action for this pressure level.
    #[must_use]
    pub const fn suggested_action(self) -> MemoryAction {
        match self {
            Self::Green => MemoryAction::None,
            Self::Yellow => MemoryAction::CompressIdle,
            Self::Orange => MemoryAction::EvictToDisk,
            Self::Red => MemoryAction::EmergencyCleanup,
        }
    }
}

/// Suggested action based on memory pressure tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryAction {
    /// No action needed.
    None,
    /// Compress scrollback for idle panes.
    CompressIdle,
    /// Evict scrollback to disk for old idle panes.
    EvictToDisk,
    /// Emergency: kill largest idle pane, evict all scrollback.
    EmergencyCleanup,
}

// =============================================================================
// Configuration
// =============================================================================

/// Memory pressure monitoring configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryPressureConfig {
    /// Enable memory pressure monitoring.
    pub enabled: bool,
    /// Sample interval in milliseconds.
    pub sample_interval_ms: u64,
    /// Threshold for Yellow (percentage of total RAM used).
    pub yellow_threshold: f64,
    /// Threshold for Orange.
    pub orange_threshold: f64,
    /// Threshold for Red.
    pub red_threshold: f64,
    /// Idle time before scrollback compression (seconds).
    pub compress_idle_secs: u64,
    /// Idle time before scrollback eviction to disk (seconds).
    pub evict_idle_secs: u64,
}

impl Default for MemoryPressureConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sample_interval_ms: 10_000,
            yellow_threshold: 70.0,
            orange_threshold: 85.0,
            red_threshold: 95.0,
            compress_idle_secs: 300,
            evict_idle_secs: 1800,
        }
    }
}

// =============================================================================
// Memory sample
// =============================================================================

/// A single memory pressure sample.
#[derive(Debug, Clone)]
pub struct MemorySample {
    /// Memory utilization percentage (0-100).
    pub used_percent: f64,
    /// Total system memory in KB.
    pub total_kb: u64,
    /// Available memory in KB.
    pub available_kb: u64,
    /// Classified tier.
    pub tier: MemoryPressureTier,
    /// Timestamp of the sample.
    pub sampled_at: Instant,
}

// =============================================================================
// Per-pane memory info
// =============================================================================

/// Per-pane memory tracking record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneMemoryInfo {
    /// Pane ID.
    pub pane_id: u64,
    /// Resident set size in KB for the pane's process tree.
    pub rss_kb: u64,
    /// Whether scrollback is compressed.
    pub scrollback_compressed: bool,
    /// Whether scrollback is evicted to disk.
    pub scrollback_evicted: bool,
    /// Time since last pane activity (seconds).
    pub idle_secs: u64,
}

// =============================================================================
// macOS heap-vs-residency classifier
// =============================================================================

/// macOS residency bucket used by the resource-pressure cockpit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MacosResidencyBucket {
    /// Allocator-owned heap and long-lived Rust structures.
    RustHeap,
    /// File-backed mappings, dylibs, mmap-backed indexes, and mapped data.
    MmapFileBacked,
    /// SQLite page cache, WAL, or SQLite-owned mappings.
    SqlitePageCache,
    /// GPU, image, font, video, or render/media residency.
    GraphicsMedia,
    /// Hot/warm terminal scrollback and terminal cache residency.
    ScrollbackCache,
    /// Child process RSS attributed to the same run.
    ChildProcesses,
    /// Resident bytes that could not be attributed.
    Unknown,
}

impl MacosResidencyBucket {
    const ALL: [Self; 7] = [
        Self::RustHeap,
        Self::MmapFileBacked,
        Self::SqlitePageCache,
        Self::GraphicsMedia,
        Self::ScrollbackCache,
        Self::ChildProcesses,
        Self::Unknown,
    ];

    const fn index(self) -> usize {
        match self {
            Self::RustHeap => 0,
            Self::MmapFileBacked => 1,
            Self::SqlitePageCache => 2,
            Self::GraphicsMedia => 3,
            Self::ScrollbackCache => 4,
            Self::ChildProcesses => 5,
            Self::Unknown => 6,
        }
    }
}

/// Trust state for one macOS residency evidence source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MacosResidencyEvidenceState {
    /// Parsed from the live host/process run represented by the caller.
    Measured,
    /// Generated from a fixture, replay, or dry-run.
    Simulated,
    /// The source was missing, unreadable, or not wired.
    Unavailable,
    /// The source is older than the caller's freshness budget.
    Stale,
    /// The final report combines sources with different states.
    Mixed,
}

/// Apple-native evidence source used by the classifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MacosResidencyEvidenceTool {
    /// `ps -o rss=` or equivalent process RSS.
    ProcessRss,
    /// `vmmap <pid>` output.
    Vmmap,
    /// `heap <pid>` output.
    Heap,
    /// `/usr/bin/sample <pid> ...` output.
    Sample,
    /// Child-process RSS attribution.
    ChildProcesses,
}

/// Status for one evidence source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MacosResidencyEvidenceStatus {
    /// Evidence source.
    pub tool: MacosResidencyEvidenceTool,
    /// Evidence trust state.
    pub state: MacosResidencyEvidenceState,
    /// Stable reason code for the source status.
    pub reason_code: String,
    /// Short operator detail.
    pub detail: String,
}

/// Classified residency row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MacosResidencyBucketSummary {
    /// Residency bucket.
    pub bucket: MacosResidencyBucket,
    /// Attributed bytes, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    /// Confidence from 0 to 100.
    pub confidence: u8,
    /// Stable reason codes that contributed to this row.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reason_codes: Vec<String>,
}

impl MacosResidencyBucketSummary {
    fn new(bucket: MacosResidencyBucket) -> Self {
        Self {
            bucket,
            bytes: None,
            confidence: 0,
            reason_codes: Vec::new(),
        }
    }
}

/// Borrowed inputs for the pure macOS residency classifier.
///
/// The classifier intentionally does not invoke `vmmap`, `heap`, `sample`, or
/// `ps`. Live collection belongs in a bounded caller that can enforce timeout,
/// authorization, and artifact retention. Tests pass fixture strings here.
#[derive(Debug, Clone, Default)]
pub struct MacosResidencyClassifierInput<'a> {
    /// Process RSS in bytes from `ps -o rss=` or an equivalent source.
    pub process_rss_bytes: Option<u64>,
    /// Raw `vmmap <pid>` output.
    pub vmmap_output: Option<&'a str>,
    /// Raw `heap <pid>` output.
    pub heap_output: Option<&'a str>,
    /// Raw `/usr/bin/sample <pid> ...` output.
    pub sample_output: Option<&'a str>,
    /// Sum of child-process RSS bytes attributed to this run.
    pub child_process_rss_bytes: Option<u64>,
}

/// macOS heap-vs-residency classification for resource cockpit diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MacosResidencyClassification {
    /// Top-level evidence state synthesized from all sources.
    pub evidence_state: MacosResidencyEvidenceState,
    /// Process RSS in bytes, if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process_rss_bytes: Option<u64>,
    /// Sum of non-unknown attributed bytes.
    pub known_bytes: u64,
    /// RSS bytes that remain unattributed after known buckets.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unknown_bytes: Option<u64>,
    /// Largest bucket by attributed bytes, falling back to `unknown`.
    pub dominant_bucket: MacosResidencyBucket,
    /// Bucket summaries in the resource cockpit order.
    pub buckets: Vec<MacosResidencyBucketSummary>,
    /// Per-tool evidence status.
    pub evidence: Vec<MacosResidencyEvidenceStatus>,
    /// Report-level reason codes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reason_codes: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
enum MacosResidencyBucketMerge {
    Sum,
    Max,
}

/// Classify macOS heap, file-backed, graphics/media, scrollback, child-process,
/// and unknown residency from Apple-native evidence strings.
#[must_use]
pub fn classify_macos_residency(
    input: &MacosResidencyClassifierInput<'_>,
) -> MacosResidencyClassification {
    let mut buckets = MacosResidencyBucket::ALL
        .iter()
        .copied()
        .map(MacosResidencyBucketSummary::new)
        .collect::<Vec<_>>();
    let mut evidence = Vec::new();

    record_rss_evidence(input.process_rss_bytes, &mut evidence);
    classify_vmmap_output(input.vmmap_output, &mut buckets, &mut evidence);
    classify_heap_output(input.heap_output, &mut buckets, &mut evidence);
    classify_sample_output(input.sample_output, &mut buckets, &mut evidence);
    classify_child_processes(input.child_process_rss_bytes, &mut buckets, &mut evidence);

    let known_bytes = buckets
        .iter()
        .filter(|row| row.bucket != MacosResidencyBucket::Unknown)
        .filter_map(|row| row.bytes)
        .fold(0_u64, u64::saturating_add);

    let unknown_bytes = input.process_rss_bytes.map(|rss| {
        let unattributed = rss.saturating_sub(known_bytes);
        if unattributed > 0 {
            add_bucket_bytes(
                &mut buckets,
                MacosResidencyBucket::Unknown,
                unattributed,
                60,
                "resource.memory.unknown_residency",
                MacosResidencyBucketMerge::Max,
            );
        }
        unattributed
    });

    let dominant_bucket = buckets
        .iter()
        .filter_map(|row| row.bytes.map(|bytes| (row.bucket, bytes)))
        .max_by_key(|(_, bytes)| *bytes)
        .map_or(MacosResidencyBucket::Unknown, |(bucket, _)| bucket);

    let evidence_state = synthesize_macos_residency_state(&evidence);
    let mut reason_codes = Vec::new();
    for status in &evidence {
        push_reason_code(&mut reason_codes, &status.reason_code);
    }
    match evidence_state {
        MacosResidencyEvidenceState::Unavailable => {
            push_reason_code(&mut reason_codes, "resource.telemetry.unavailable");
        }
        MacosResidencyEvidenceState::Mixed => {
            push_reason_code(&mut reason_codes, "resource.telemetry.mixed");
        }
        MacosResidencyEvidenceState::Measured
        | MacosResidencyEvidenceState::Simulated
        | MacosResidencyEvidenceState::Stale => {}
    }

    MacosResidencyClassification {
        evidence_state,
        process_rss_bytes: input.process_rss_bytes,
        known_bytes,
        unknown_bytes,
        dominant_bucket,
        buckets,
        evidence,
        reason_codes,
    }
}

fn record_rss_evidence(
    process_rss_bytes: Option<u64>,
    evidence: &mut Vec<MacosResidencyEvidenceStatus>,
) {
    match process_rss_bytes {
        Some(bytes) => evidence.push(MacosResidencyEvidenceStatus {
            tool: MacosResidencyEvidenceTool::ProcessRss,
            state: MacosResidencyEvidenceState::Measured,
            reason_code: "resource.memory.rss_measured".to_string(),
            detail: format!("process rss bytes={bytes}"),
        }),
        None => evidence.push(MacosResidencyEvidenceStatus {
            tool: MacosResidencyEvidenceTool::ProcessRss,
            state: MacosResidencyEvidenceState::Unavailable,
            reason_code: "resource.telemetry.unavailable".to_string(),
            detail: "process RSS was not provided".to_string(),
        }),
    }
}

fn classify_vmmap_output(
    output: Option<&str>,
    buckets: &mut [MacosResidencyBucketSummary],
    evidence: &mut Vec<MacosResidencyEvidenceStatus>,
) {
    let Some(output) = output else {
        evidence.push(MacosResidencyEvidenceStatus {
            tool: MacosResidencyEvidenceTool::Vmmap,
            state: MacosResidencyEvidenceState::Unavailable,
            reason_code: "resource.telemetry.unavailable".to_string(),
            detail: "vmmap output was not provided".to_string(),
        });
        return;
    };

    if output.trim().is_empty() {
        evidence.push(MacosResidencyEvidenceStatus {
            tool: MacosResidencyEvidenceTool::Vmmap,
            state: MacosResidencyEvidenceState::Unavailable,
            reason_code: "resource.memory.vmmap_malformed".to_string(),
            detail: "vmmap output was empty".to_string(),
        });
        return;
    }

    let mut classified_lines = 0_u64;
    for line in output.lines() {
        let Some(bytes) = parse_first_size_bytes(line) else {
            continue;
        };
        if bytes == 0 {
            continue;
        }
        let bucket = classify_vmmap_line(line);
        let reason = reason_code_for_macos_bucket(bucket);
        add_bucket_bytes(
            buckets,
            bucket,
            bytes,
            80,
            reason,
            MacosResidencyBucketMerge::Sum,
        );
        classified_lines = classified_lines.saturating_add(1);
    }

    if classified_lines == 0 {
        evidence.push(MacosResidencyEvidenceStatus {
            tool: MacosResidencyEvidenceTool::Vmmap,
            state: MacosResidencyEvidenceState::Unavailable,
            reason_code: "resource.memory.vmmap_malformed".to_string(),
            detail: "vmmap output did not contain parseable region sizes".to_string(),
        });
    } else {
        evidence.push(MacosResidencyEvidenceStatus {
            tool: MacosResidencyEvidenceTool::Vmmap,
            state: MacosResidencyEvidenceState::Measured,
            reason_code: "resource.memory.vmmap_residency".to_string(),
            detail: format!("classified vmmap region lines={classified_lines}"),
        });
    }
}

fn classify_heap_output(
    output: Option<&str>,
    buckets: &mut [MacosResidencyBucketSummary],
    evidence: &mut Vec<MacosResidencyEvidenceStatus>,
) {
    let Some(output) = output else {
        evidence.push(MacosResidencyEvidenceStatus {
            tool: MacosResidencyEvidenceTool::Heap,
            state: MacosResidencyEvidenceState::Unavailable,
            reason_code: "resource.telemetry.unavailable".to_string(),
            detail: "heap output was not provided".to_string(),
        });
        return;
    };

    if output.trim().is_empty() {
        evidence.push(MacosResidencyEvidenceStatus {
            tool: MacosResidencyEvidenceTool::Heap,
            state: MacosResidencyEvidenceState::Unavailable,
            reason_code: "resource.memory.heap_malformed".to_string(),
            detail: "heap output was empty".to_string(),
        });
        return;
    }

    let heap_bytes = output
        .lines()
        .filter(|line| line_mentions_any(line, &["all zones", "total", "malloc", "heap"]))
        .filter_map(parse_first_size_bytes)
        .max();

    if let Some(bytes) = heap_bytes {
        add_bucket_bytes(
            buckets,
            MacosResidencyBucket::RustHeap,
            bytes,
            90,
            "resource.memory.heap_growth",
            MacosResidencyBucketMerge::Max,
        );
        evidence.push(MacosResidencyEvidenceStatus {
            tool: MacosResidencyEvidenceTool::Heap,
            state: MacosResidencyEvidenceState::Measured,
            reason_code: "resource.memory.heap_growth".to_string(),
            detail: format!("heap attributed bytes={bytes}"),
        });
    } else {
        evidence.push(MacosResidencyEvidenceStatus {
            tool: MacosResidencyEvidenceTool::Heap,
            state: MacosResidencyEvidenceState::Unavailable,
            reason_code: "resource.memory.heap_malformed".to_string(),
            detail: "heap output did not contain a parseable heap total".to_string(),
        });
    }
}

fn classify_sample_output(
    output: Option<&str>,
    buckets: &mut [MacosResidencyBucketSummary],
    evidence: &mut Vec<MacosResidencyEvidenceStatus>,
) {
    let Some(output) = output else {
        evidence.push(MacosResidencyEvidenceStatus {
            tool: MacosResidencyEvidenceTool::Sample,
            state: MacosResidencyEvidenceState::Unavailable,
            reason_code: "resource.telemetry.unavailable".to_string(),
            detail: "sample output was not provided".to_string(),
        });
        return;
    };

    let trimmed = output.trim();
    if trimmed.is_empty() {
        evidence.push(MacosResidencyEvidenceStatus {
            tool: MacosResidencyEvidenceTool::Sample,
            state: MacosResidencyEvidenceState::Unavailable,
            reason_code: "resource.memory.sample_malformed".to_string(),
            detail: "sample output was empty".to_string(),
        });
        return;
    }

    let mut signals = 0_u64;
    for line in trimmed.lines() {
        if line_mentions_any(line, &["malloc", "rust_alloc", "alloc::", "heap"]) {
            add_bucket_signal(
                buckets,
                MacosResidencyBucket::RustHeap,
                55,
                "resource.memory.heap_stack_signal",
            );
            signals = signals.saturating_add(1);
        } else if line_mentions_any(line, &["sqlite", "wal", "page cache"]) {
            add_bucket_signal(
                buckets,
                MacosResidencyBucket::SqlitePageCache,
                55,
                "resource.memory.sqlite_stack_signal",
            );
            signals = signals.saturating_add(1);
        } else if line_mentions_any(
            line,
            &[
                "iosurface",
                "metal",
                "coregraphics",
                "cg raster",
                "imageio",
                "font",
                "gpu",
                "video",
                "media",
            ],
        ) {
            add_bucket_signal(
                buckets,
                MacosResidencyBucket::GraphicsMedia,
                55,
                "resource.memory.graphics_stack_signal",
            );
            signals = signals.saturating_add(1);
        } else if line_mentions_any(line, &["scrollback", "terminal history"]) {
            add_bucket_signal(
                buckets,
                MacosResidencyBucket::ScrollbackCache,
                55,
                "resource.memory.scrollback_stack_signal",
            );
            signals = signals.saturating_add(1);
        } else if line_mentions_any(line, &["mmap", "mapped file", "mmap_file"]) {
            add_bucket_signal(
                buckets,
                MacosResidencyBucket::MmapFileBacked,
                50,
                "resource.memory.mmap_stack_signal",
            );
            signals = signals.saturating_add(1);
        }
    }

    evidence.push(MacosResidencyEvidenceStatus {
        tool: MacosResidencyEvidenceTool::Sample,
        state: MacosResidencyEvidenceState::Measured,
        reason_code: if signals == 0 {
            "resource.memory.sample_no_residency_signal".to_string()
        } else {
            "resource.memory.sample_residency_signal".to_string()
        },
        detail: format!("sample residency signals={signals}"),
    });
}

fn classify_child_processes(
    child_process_rss_bytes: Option<u64>,
    buckets: &mut [MacosResidencyBucketSummary],
    evidence: &mut Vec<MacosResidencyEvidenceStatus>,
) {
    match child_process_rss_bytes {
        Some(bytes) => {
            if bytes > 0 {
                add_bucket_bytes(
                    buckets,
                    MacosResidencyBucket::ChildProcesses,
                    bytes,
                    85,
                    "resource.memory.child_process_rss",
                    MacosResidencyBucketMerge::Max,
                );
            }
            evidence.push(MacosResidencyEvidenceStatus {
                tool: MacosResidencyEvidenceTool::ChildProcesses,
                state: MacosResidencyEvidenceState::Measured,
                reason_code: "resource.memory.child_process_rss".to_string(),
                detail: format!("child process rss bytes={bytes}"),
            });
        }
        None => evidence.push(MacosResidencyEvidenceStatus {
            tool: MacosResidencyEvidenceTool::ChildProcesses,
            state: MacosResidencyEvidenceState::Unavailable,
            reason_code: "resource.telemetry.unavailable".to_string(),
            detail: "child-process RSS was not provided".to_string(),
        }),
    }
}

fn classify_vmmap_line(line: &str) -> MacosResidencyBucket {
    if line_mentions_any(line, &["scrollback", "terminal history"]) {
        MacosResidencyBucket::ScrollbackCache
    } else if line_mentions_any(line, &["sqlite", "wal", "page cache"]) {
        MacosResidencyBucket::SqlitePageCache
    } else if line_mentions_any(
        line,
        &[
            "iosurface",
            "metal",
            "coregraphics",
            "cg raster",
            "imageio",
            "font",
            "gpu",
            "opengl",
            "video",
            "media",
        ],
    ) {
        MacosResidencyBucket::GraphicsMedia
    } else if line_mentions_any(line, &["malloc", "heap", "nanov2", "default_malloc_zone"]) {
        MacosResidencyBucket::RustHeap
    } else if line_mentions_any(
        line,
        &[
            "mapped file",
            "mapped_file",
            "mmap",
            "__text",
            "__data",
            "shared memory",
            "file-backed",
        ],
    ) {
        MacosResidencyBucket::MmapFileBacked
    } else {
        MacosResidencyBucket::Unknown
    }
}

fn reason_code_for_macos_bucket(bucket: MacosResidencyBucket) -> &'static str {
    match bucket {
        MacosResidencyBucket::RustHeap => "resource.memory.heap_growth",
        MacosResidencyBucket::MmapFileBacked => "resource.memory.mmap_residency",
        MacosResidencyBucket::SqlitePageCache => "resource.memory.sqlite_page_cache",
        MacosResidencyBucket::GraphicsMedia => "resource.memory.graphics_media",
        MacosResidencyBucket::ScrollbackCache => "resource.memory.scrollback_cache",
        MacosResidencyBucket::ChildProcesses => "resource.memory.child_process_rss",
        MacosResidencyBucket::Unknown => "resource.memory.unknown_residency",
    }
}

fn add_bucket_bytes(
    buckets: &mut [MacosResidencyBucketSummary],
    bucket: MacosResidencyBucket,
    bytes: u64,
    confidence: u8,
    reason_code: &str,
    merge: MacosResidencyBucketMerge,
) {
    let row = &mut buckets[bucket.index()];
    row.bytes = Some(match (row.bytes, merge) {
        (Some(existing), MacosResidencyBucketMerge::Sum) => existing.saturating_add(bytes),
        (Some(existing), MacosResidencyBucketMerge::Max) => existing.max(bytes),
        (None, _) => bytes,
    });
    row.confidence = row.confidence.max(confidence.min(100));
    push_reason_code(&mut row.reason_codes, reason_code);
}

fn add_bucket_signal(
    buckets: &mut [MacosResidencyBucketSummary],
    bucket: MacosResidencyBucket,
    confidence: u8,
    reason_code: &str,
) {
    let row = &mut buckets[bucket.index()];
    row.confidence = row.confidence.max(confidence.min(100));
    push_reason_code(&mut row.reason_codes, reason_code);
}

fn push_reason_code(reason_codes: &mut Vec<String>, reason_code: &str) {
    if !reason_codes.iter().any(|existing| existing == reason_code) {
        reason_codes.push(reason_code.to_string());
    }
}

fn synthesize_macos_residency_state(
    evidence: &[MacosResidencyEvidenceStatus],
) -> MacosResidencyEvidenceState {
    let mut states = evidence.iter().map(|status| status.state);
    let Some(first) = states.next() else {
        return MacosResidencyEvidenceState::Unavailable;
    };
    if states.all(|state| state == first) {
        first
    } else {
        MacosResidencyEvidenceState::Mixed
    }
}

fn line_mentions_any(line: &str, needles: &[&str]) -> bool {
    let lower = line.to_ascii_lowercase();
    needles.iter().any(|needle| lower.contains(needle))
}

fn parse_first_size_bytes(line: &str) -> Option<u64> {
    let tokens = line
        .split_whitespace()
        .map(clean_size_token)
        .collect::<Vec<_>>();

    for (idx, token) in tokens.iter().enumerate() {
        if let Some(bytes) = parse_size_token_with_suffix(token) {
            return Some(bytes);
        }
        if let Some(unit) = tokens.get(idx.saturating_add(1)) {
            if let Some(bytes) = parse_size_token_with_separate_unit(token, unit) {
                return Some(bytes);
            }
        }
    }

    None
}

fn clean_size_token(token: &str) -> String {
    token
        .trim_matches(|c: char| {
            matches!(c, ',' | ':' | ';' | '(' | ')' | '[' | ']' | '{' | '}' | '=')
        })
        .replace(',', "")
}

fn parse_size_token_with_suffix(token: &str) -> Option<u64> {
    let lower = token.to_ascii_lowercase();
    for (suffix, multiplier) in [
        ("bytes", 1_u64),
        ("byte", 1),
        ("gb", 1024_u64 * 1024 * 1024),
        ("g", 1024_u64 * 1024 * 1024),
        ("mb", 1024_u64 * 1024),
        ("m", 1024_u64 * 1024),
        ("kb", 1024),
        ("k", 1024),
    ] {
        if let Some(number) = lower.strip_suffix(suffix) {
            return parse_decimal_size(number, multiplier);
        }
    }
    None
}

fn parse_size_token_with_separate_unit(number: &str, unit: &str) -> Option<u64> {
    let multiplier = match unit.to_ascii_lowercase().as_str() {
        "bytes" | "byte" => 1,
        "gb" | "g" => 1024_u64 * 1024 * 1024,
        "mb" | "m" => 1024_u64 * 1024,
        "kb" | "k" => 1024,
        _ => return None,
    };
    parse_decimal_size(number, multiplier)
}

fn parse_decimal_size(number: &str, multiplier: u64) -> Option<u64> {
    let value = number.parse::<f64>().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    let bytes = value * multiplier as f64;
    if bytes > u64::MAX as f64 {
        Some(u64::MAX)
    } else {
        Some(bytes.round() as u64)
    }
}

// =============================================================================
// Resource-pressure action receipts and attribution
// =============================================================================

/// Schema version for compact resource-pressure action receipts.
pub const RESOURCE_PRESSURE_ACTION_RECEIPT_SCHEMA_VERSION: u32 = 1;

/// Cockpit domain an action receipt is intended to relieve or audit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourcePressureDomain {
    /// Host or process memory pressure.
    Memory,
    /// RSS residency classification and leak triage.
    RssResidency,
    /// Per-pane memory budget pressure.
    PaneBudget,
    /// Capture/write/persistence/search queue pressure.
    QueueBackpressure,
    /// SQLite, cold-tier, or target-dir IO pressure.
    StorageIo,
    /// RCH/worker-pool pressure.
    WorkerPool,
    /// Capacity-level admission decisions.
    CapacityAdmission,
    /// Resource-level admission decisions.
    ResourceAdmission,
    /// Health of the receipts themselves.
    ActionReceipts,
}

impl ResourcePressureDomain {
    const ALL: [Self; 9] = [
        Self::Memory,
        Self::RssResidency,
        Self::PaneBudget,
        Self::QueueBackpressure,
        Self::StorageIo,
        Self::WorkerPool,
        Self::CapacityAdmission,
        Self::ResourceAdmission,
        Self::ActionReceipts,
    ];

    const fn index(self) -> usize {
        match self {
            Self::Memory => 0,
            Self::RssResidency => 1,
            Self::PaneBudget => 2,
            Self::QueueBackpressure => 3,
            Self::StorageIo => 4,
            Self::WorkerPool => 5,
            Self::CapacityAdmission => 6,
            Self::ResourceAdmission => 7,
            Self::ActionReceipts => 8,
        }
    }
}

/// Mitigation or audit action represented by a resource-pressure receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourcePressureAction {
    /// Observe and retain evidence only.
    Observe,
    /// Delay a non-critical admission.
    DelayAdmission,
    /// Degrade capture/search fidelity with an explicit receipt.
    DegradeCapture,
    /// Shed optional work.
    ShedOptionalWork,
    /// Compress scrollback.
    CompressScrollback,
    /// Evict scrollback or cold data to disk.
    EvictScrollback,
    /// Throttle a queue or worker lane.
    ThrottleQueue,
    /// Block admission because policy or telemetry requires fail-closed behavior.
    BlockAdmission,
    /// Roll back a prior pressure action.
    Rollback,
    /// Compensate after a failed or partial action.
    Compensate,
}

/// Receipt lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourcePressureReceiptStatus {
    /// Action is only planned.
    Planned,
    /// Caller intentionally performed no side effect.
    DryRun,
    /// Side effect was applied but not independently confirmed.
    Applied,
    /// Side effect was applied and confirmed.
    Succeeded,
    /// Action was blocked before side effects.
    Blocked,
    /// Action attempted and failed.
    Failed,
    /// Compensation ran after a partial or failed action.
    Compensated,
    /// Compensation failed.
    CompensationFailed,
    /// Rollback is required before more actions are safe.
    RollbackRequired,
}

impl ResourcePressureReceiptStatus {
    const fn is_failed(self) -> bool {
        matches!(
            self,
            Self::Failed | Self::CompensationFailed | Self::RollbackRequired
        )
    }

    const fn is_blocked(self) -> bool {
        matches!(self, Self::Blocked)
    }

    const fn reason_code(self) -> &'static str {
        match self {
            Self::Planned => "action_receipt.planned",
            Self::DryRun => "action_receipt.dry_run",
            Self::Applied | Self::Succeeded => "action_receipt.applied",
            Self::Blocked => "action_receipt.blocked",
            Self::Failed | Self::CompensationFailed => "action_receipt.failed",
            Self::Compensated => "action_receipt.compensated",
            Self::RollbackRequired => "action_receipt.rollback_required",
        }
    }
}

/// Policy outcome attached to an action receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourcePressurePolicyDecision {
    /// Policy allowed the action.
    Allow,
    /// Policy denied the action.
    Deny,
    /// Policy requires operator approval.
    RequireApproval,
    /// No policy gate was checked.
    NotChecked,
}

/// Trust state for resource-pressure action evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourcePressureEvidenceState {
    /// Evidence came from a live measured run.
    Measured,
    /// Evidence came from a fixture, replay, dry-run, or simulator.
    Simulated,
    /// Telemetry was missing, unreadable, or intentionally not wired.
    Unavailable,
    /// Telemetry is older than the caller's freshness budget.
    Stale,
    /// The receipt combines sources with different freshness states.
    Mixed,
}

/// Known subject attribution for one pressure action.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcePressureAttribution {
    /// Pane id affected by this action, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<u64>,
    /// Agent name affected by this action, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    /// Target directory or build/output root affected by this action, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_dir: Option<String>,
    /// Queue or lane affected by this action, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue_name: Option<String>,
    /// Bytes affected, refused, evicted, compressed, or delayed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub affected_bytes: Option<u64>,
}

impl ResourcePressureAttribution {
    fn is_unknown(&self) -> bool {
        self.pane_id.is_none()
            && self.agent_name.is_none()
            && self.target_dir.is_none()
            && self.queue_name.is_none()
            && self.affected_bytes.is_none()
    }
}

/// Caller-supplied input for a pure resource-pressure receipt normalization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourcePressureActionReceiptInput {
    /// Stable idempotency or audit id.
    pub receipt_id: String,
    /// Optional cross-surface correlation id.
    pub correlation_id: Option<String>,
    /// Requested action.
    pub action: ResourcePressureAction,
    /// Domain the action is meant to relieve.
    pub target_domain: ResourcePressureDomain,
    /// Request timestamp in milliseconds since the caller's chosen epoch.
    pub requested_at_ms: u64,
    /// Completion timestamp when known.
    pub completed_at_ms: Option<u64>,
    /// Caller-observed status.
    pub status: ResourcePressureReceiptStatus,
    /// Whether the caller intentionally avoided side effects.
    pub dry_run: bool,
    /// Policy gate decision.
    pub policy_decision: ResourcePressurePolicyDecision,
    /// Evidence freshness for the receipt.
    pub evidence_state: ResourcePressureEvidenceState,
    /// Known attribution.
    pub attribution: ResourcePressureAttribution,
    /// Caller-supplied reason codes.
    pub reason_codes: Vec<String>,
    /// Proof, audit, or replay artifacts.
    pub artifact_paths: Vec<String>,
}

/// Normalized compact resource-pressure action receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcePressureActionReceipt {
    /// Receipt schema version.
    pub schema_version: u32,
    /// Stable idempotency or audit id.
    pub receipt_id: String,
    /// Optional cross-surface correlation id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    /// Requested action.
    pub action: ResourcePressureAction,
    /// Domain the action is meant to relieve.
    pub target_domain: ResourcePressureDomain,
    /// Request timestamp in milliseconds.
    pub requested_at_ms: u64,
    /// Completion timestamp when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at_ms: Option<u64>,
    /// Normalized status.
    pub status: ResourcePressureReceiptStatus,
    /// Whether side effects were intentionally avoided.
    pub dry_run: bool,
    /// Policy decision after fail-closed normalization.
    pub policy_decision: ResourcePressurePolicyDecision,
    /// Evidence freshness for the receipt.
    pub evidence_state: ResourcePressureEvidenceState,
    /// Known subject attribution.
    pub attribution: ResourcePressureAttribution,
    /// Stable reason codes.
    pub reason_codes: Vec<String>,
    /// Proof, audit, or replay artifacts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifact_paths: Vec<String>,
}

/// Per-domain receipt rollup for cockpit summaries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcePressureDomainReceiptSummary {
    /// Domain represented by this row.
    pub target_domain: ResourcePressureDomain,
    /// Total receipts in the domain.
    pub total: u64,
    /// Applied or succeeded receipts.
    pub applied: u64,
    /// Dry-run receipts.
    pub dry_run: u64,
    /// Blocked receipts.
    pub blocked: u64,
    /// Failed, compensation-failed, or rollback-required receipts.
    pub failed: u64,
    /// Receipts with unavailable evidence.
    pub unavailable: u64,
    /// Receipts with stale evidence.
    pub stale: u64,
    /// Stable reason codes observed in this domain.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reason_codes: Vec<String>,
}

impl ResourcePressureDomainReceiptSummary {
    const fn empty(target_domain: ResourcePressureDomain) -> Self {
        Self {
            target_domain,
            total: 0,
            applied: 0,
            dry_run: 0,
            blocked: 0,
            failed: 0,
            unavailable: 0,
            stale: 0,
            reason_codes: Vec::new(),
        }
    }
}

/// Complete receipt report for Robot/MCP/operator cockpit consumers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcePressureActionReceiptReport {
    /// Receipt schema version.
    pub schema_version: u32,
    /// Normalized receipts.
    pub receipts: Vec<ResourcePressureActionReceipt>,
    /// Per-domain rollups.
    pub domain_summaries: Vec<ResourcePressureDomainReceiptSummary>,
    /// Count of blocked receipts.
    pub blocked_receipts: u64,
    /// Count of failed receipts.
    pub failed_receipts: u64,
    /// Count of receipts with unavailable evidence.
    pub unavailable_receipts: u64,
    /// Count of receipts with stale evidence.
    pub stale_receipts: u64,
    /// Report-level reason codes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reason_codes: Vec<String>,
}

/// Normalize resource-pressure action receipts without performing side effects.
///
/// Missing telemetry is fail-closed for non-dry-run actions that would otherwise
/// report progress. The receipt is rewritten to `blocked`, policy is escalated
/// to `require_approval`, and the reason codes preserve the original cause.
#[must_use]
pub fn evaluate_resource_pressure_action_receipts(
    inputs: &[ResourcePressureActionReceiptInput],
) -> ResourcePressureActionReceiptReport {
    let receipts = inputs
        .iter()
        .map(normalize_resource_pressure_action_receipt)
        .collect::<Vec<_>>();
    let domain_summaries = summarize_resource_pressure_receipt_domains(&receipts);

    let blocked_receipts = receipts
        .iter()
        .filter(|receipt| receipt.status.is_blocked())
        .count() as u64;
    let failed_receipts = receipts
        .iter()
        .filter(|receipt| receipt.status.is_failed())
        .count() as u64;
    let unavailable_receipts = receipts
        .iter()
        .filter(|receipt| receipt.evidence_state == ResourcePressureEvidenceState::Unavailable)
        .count() as u64;
    let stale_receipts = receipts
        .iter()
        .filter(|receipt| receipt.evidence_state == ResourcePressureEvidenceState::Stale)
        .count() as u64;

    let mut reason_codes = Vec::new();
    for receipt in &receipts {
        for reason_code in &receipt.reason_codes {
            push_reason_code(&mut reason_codes, reason_code);
        }
    }

    ResourcePressureActionReceiptReport {
        schema_version: RESOURCE_PRESSURE_ACTION_RECEIPT_SCHEMA_VERSION,
        receipts,
        domain_summaries,
        blocked_receipts,
        failed_receipts,
        unavailable_receipts,
        stale_receipts,
        reason_codes,
    }
}

fn normalize_resource_pressure_action_receipt(
    input: &ResourcePressureActionReceiptInput,
) -> ResourcePressureActionReceipt {
    let mut status = if input.dry_run {
        ResourcePressureReceiptStatus::DryRun
    } else {
        input.status
    };
    let mut policy_decision = input.policy_decision;
    let mut reason_codes = input.reason_codes.clone();

    match input.evidence_state {
        ResourcePressureEvidenceState::Measured => {}
        ResourcePressureEvidenceState::Simulated => {
            push_reason_code(&mut reason_codes, "resource.telemetry.simulated");
        }
        ResourcePressureEvidenceState::Unavailable => {
            push_reason_code(&mut reason_codes, "resource.telemetry.unavailable");
            if !input.dry_run
                && matches!(
                    status,
                    ResourcePressureReceiptStatus::Planned
                        | ResourcePressureReceiptStatus::Applied
                        | ResourcePressureReceiptStatus::Succeeded
                )
            {
                status = ResourcePressureReceiptStatus::Blocked;
                push_reason_code(&mut reason_codes, "admission.fail_closed.missing_telemetry");
                policy_decision = ResourcePressurePolicyDecision::RequireApproval;
            }
        }
        ResourcePressureEvidenceState::Stale => {
            push_reason_code(&mut reason_codes, "resource.telemetry.stale");
        }
        ResourcePressureEvidenceState::Mixed => {
            push_reason_code(&mut reason_codes, "resource.telemetry.mixed");
        }
    }

    if input.attribution.is_unknown() {
        push_reason_code(&mut reason_codes, "resource.attribution.unknown");
    }
    push_reason_code(&mut reason_codes, status.reason_code());

    ResourcePressureActionReceipt {
        schema_version: RESOURCE_PRESSURE_ACTION_RECEIPT_SCHEMA_VERSION,
        receipt_id: input.receipt_id.clone(),
        correlation_id: input.correlation_id.clone(),
        action: input.action,
        target_domain: input.target_domain,
        requested_at_ms: input.requested_at_ms,
        completed_at_ms: input.completed_at_ms,
        status,
        dry_run: input.dry_run,
        policy_decision,
        evidence_state: input.evidence_state,
        attribution: input.attribution.clone(),
        reason_codes,
        artifact_paths: input.artifact_paths.clone(),
    }
}

fn summarize_resource_pressure_receipt_domains(
    receipts: &[ResourcePressureActionReceipt],
) -> Vec<ResourcePressureDomainReceiptSummary> {
    let mut summaries = ResourcePressureDomain::ALL
        .iter()
        .copied()
        .map(ResourcePressureDomainReceiptSummary::empty)
        .collect::<Vec<_>>();

    for receipt in receipts {
        let summary = &mut summaries[receipt.target_domain.index()];
        summary.total = summary.total.saturating_add(1);
        match receipt.status {
            ResourcePressureReceiptStatus::Applied | ResourcePressureReceiptStatus::Succeeded => {
                summary.applied = summary.applied.saturating_add(1);
            }
            ResourcePressureReceiptStatus::DryRun => {
                summary.dry_run = summary.dry_run.saturating_add(1);
            }
            ResourcePressureReceiptStatus::Blocked => {
                summary.blocked = summary.blocked.saturating_add(1);
            }
            ResourcePressureReceiptStatus::Failed
            | ResourcePressureReceiptStatus::CompensationFailed
            | ResourcePressureReceiptStatus::RollbackRequired => {
                summary.failed = summary.failed.saturating_add(1);
            }
            ResourcePressureReceiptStatus::Planned | ResourcePressureReceiptStatus::Compensated => {
            }
        }
        match receipt.evidence_state {
            ResourcePressureEvidenceState::Unavailable => {
                summary.unavailable = summary.unavailable.saturating_add(1);
            }
            ResourcePressureEvidenceState::Stale => {
                summary.stale = summary.stale.saturating_add(1);
            }
            ResourcePressureEvidenceState::Measured
            | ResourcePressureEvidenceState::Simulated
            | ResourcePressureEvidenceState::Mixed => {}
        }
        for reason_code in &receipt.reason_codes {
            push_reason_code(&mut summary.reason_codes, reason_code);
        }
    }

    summaries
        .into_iter()
        .filter(|summary| summary.total > 0)
        .collect()
}

// =============================================================================
// Monitor
// =============================================================================

/// Memory pressure monitor that samples system memory utilization.
///
/// Thread-safe. Uses atomic operations for the latest tier.
pub struct MemoryPressureMonitor {
    config: MemoryPressureConfig,
    /// Latest tier as atomic u8 (0-3).
    latest_tier: Arc<AtomicU64>,
}

impl MemoryPressureMonitor {
    /// Create a new monitor with the given configuration.
    pub fn new(config: MemoryPressureConfig) -> Self {
        Self {
            config,
            latest_tier: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Get the latest pressure tier (lock-free read).
    #[must_use]
    pub fn current_tier(&self) -> MemoryPressureTier {
        match self.latest_tier.load(Ordering::Relaxed) {
            1 => MemoryPressureTier::Yellow,
            2 => MemoryPressureTier::Orange,
            3 => MemoryPressureTier::Red,
            _ => MemoryPressureTier::Green,
        }
    }

    /// Get an Arc to the tier atomic for sharing with other tasks.
    #[must_use]
    pub fn tier_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.latest_tier)
    }

    /// Take a single memory pressure sample.
    pub fn sample(&self) -> MemorySample {
        let (total_kb, available_kb) = read_memory_info();
        let used_percent = if total_kb > 0 {
            (total_kb.saturating_sub(available_kb) as f64 / total_kb as f64) * 100.0
        } else {
            0.0
        };
        let tier = self.classify(used_percent);
        self.latest_tier
            .store(tier.as_u8() as u64, Ordering::Relaxed);

        MemorySample {
            used_percent,
            total_kb,
            available_kb,
            tier,
            sampled_at: Instant::now(),
        }
    }

    /// Run the monitoring loop until the shutdown flag is set.
    pub async fn run(&self, shutdown: Arc<std::sync::atomic::AtomicBool>) {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.run_with_cx(&cx, shutdown).await;
    }

    /// Explicit quarantine for legacy non-asupersync memory-pressure sampling.
    ///
    /// Owner: `ft-xbnl0.2.5`.
    /// Removal path: drop this helper once the workspace no longer supports
    /// non-`asupersync-runtime` sampling loops.
    /// Run the monitoring loop against the caller's asupersync capability
    /// context (ft-xbnl0.2.x Cx-first entry point).
    ///
    /// Short-circuits before the first sample if `cx` is already cancelled.
    /// Otherwise each inter-sample sleep is bound via
    /// [`crate::runtime_async::sleep_with_cx`], so budget-driven
    /// cancellation from the outer scope cuts the sleep deterministically
    /// under `LabRuntime` virtual time. Both the `shutdown` flag and
    /// `cx.is_cancel_requested()` are checked each iteration so either
    /// cancellation path terminates the loop promptly without waiting on
    /// the full sample interval.
    ///
    /// Mirrors `CpuPressureMonitor::run_with_cx` and
    /// `MemoryBudgetManager::run_with_cx` — these three sampling loops
    /// share the same lifecycle shape.
    ///
    /// [`run`](Self::run) now prefers the ambient current Cx under
    /// `asupersync-runtime`; this remains the explicit inherited-Cx sibling.
    pub async fn run_with_cx(
        &self,
        cx: &crate::cx::Cx,
        shutdown: Arc<std::sync::atomic::AtomicBool>,
    ) {
        if cx.is_cancel_requested() {
            return;
        }

        let interval = Duration::from_millis(self.config.sample_interval_ms.max(1000));
        let mut first_tick = true;

        loop {
            if !first_tick {
                // `sleep_with_cx` returns Err on cancellation; treat as
                // "time to exit" so the loop terminates cleanly without
                // a spurious extra sample after cancellation.
                if crate::runtime_async::sleep_with_cx(cx, interval)
                    .await
                    .is_err()
                {
                    break;
                }
            }
            first_tick = false;

            if shutdown.load(Ordering::SeqCst) || cx.is_cancel_requested() {
                break;
            }

            let sample = self.sample();
            if sample.tier >= MemoryPressureTier::Yellow {
                tracing::info!(
                    used_percent = format!("{:.1}", sample.used_percent),
                    available_mb = sample.available_kb / 1024,
                    tier = %sample.tier,
                    action = %sample.tier.suggested_action(),
                    "Memory pressure elevated"
                );
            }
        }
    }

    /// Classify memory utilization into a tier.
    fn classify(&self, used_percent: f64) -> MemoryPressureTier {
        if used_percent >= self.config.red_threshold {
            MemoryPressureTier::Red
        } else if used_percent >= self.config.orange_threshold {
            MemoryPressureTier::Orange
        } else if used_percent >= self.config.yellow_threshold {
            MemoryPressureTier::Yellow
        } else {
            MemoryPressureTier::Green
        }
    }
}

impl std::fmt::Display for MemoryAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => write!(f, "none"),
            Self::CompressIdle => write!(f, "compress_idle"),
            Self::EvictToDisk => write!(f, "evict_to_disk"),
            Self::EmergencyCleanup => write!(f, "emergency_cleanup"),
        }
    }
}

// =============================================================================
// Platform-specific memory reading
// =============================================================================

/// Read total and available memory in KB.
fn read_memory_info() -> (u64, u64) {
    #[cfg(target_os = "linux")]
    {
        read_linux_meminfo()
    }
    #[cfg(target_os = "macos")]
    {
        read_macos_memory()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        (0, 0)
    }
}

// =============================================================================
// Linux: /proc/meminfo
// =============================================================================

#[cfg(target_os = "linux")]
fn read_linux_meminfo() -> (u64, u64) {
    let Ok(contents) = std::fs::read_to_string("/proc/meminfo") else {
        return (0, 0);
    };

    let mut total_kb = 0u64;
    let mut available_kb = 0u64;

    for line in contents.lines() {
        if let Some(val) = line.strip_prefix("MemTotal:") {
            total_kb = parse_meminfo_value(val);
        } else if let Some(val) = line.strip_prefix("MemAvailable:") {
            available_kb = parse_meminfo_value(val);
        }
    }

    (total_kb, available_kb)
}

#[cfg(target_os = "linux")]
fn parse_meminfo_value(s: &str) -> u64 {
    s.trim()
        .trim_end_matches("kB")
        .trim()
        .parse::<u64>()
        .unwrap_or(0)
}

// =============================================================================
// macOS: sysctl + vm_stat (safe, no FFI)
// =============================================================================

#[cfg(target_os = "macos")]
fn read_macos_memory() -> (u64, u64) {
    let total_kb = read_macos_total_memory();
    let available_kb = read_macos_available_memory();
    (total_kb, available_kb)
}

/// Read total physical memory via `sysctl -n hw.memsize` (returns bytes).
#[cfg(target_os = "macos")]
fn read_macos_total_memory() -> u64 {
    std::process::Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|bytes| bytes / 1024)
        .unwrap_or(0)
}

/// Read available memory by parsing `vm_stat` output.
///
/// vm_stat reports pages; we compute available = (free + inactive) pages × page_size.
#[cfg(target_os = "macos")]
fn read_macos_available_memory() -> u64 {
    let output = std::process::Command::new("vm_stat")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok());

    let Some(output) = output else {
        return 0;
    };

    // Parse page size from first line: "Mach Virtual Memory Statistics: (page size of 16384 bytes)"
    let page_size = output
        .lines()
        .next()
        .and_then(|line| {
            let start = line.find("page size of ")? + 13;
            let end = line[start..].find(' ')? + start;
            line[start..end].parse::<u64>().ok()
        })
        .unwrap_or(16384);

    let mut free_pages = 0u64;
    let mut inactive_pages = 0u64;
    let mut purgeable_pages = 0u64;

    for line in output.lines() {
        if let Some(val) = line.strip_prefix("Pages free:") {
            free_pages = parse_vmstat_value(val);
        } else if let Some(val) = line.strip_prefix("Pages inactive:") {
            inactive_pages = parse_vmstat_value(val);
        } else if let Some(val) = line.strip_prefix("Pages purgeable:") {
            purgeable_pages = parse_vmstat_value(val);
        }
    }

    let available_pages = free_pages + inactive_pages + purgeable_pages;
    (available_pages * page_size) / 1024
}

/// Parse a vm_stat line value like "  12345.\n" → 12345
#[cfg(target_os = "macos")]
fn parse_vmstat_value(s: &str) -> u64 {
    s.trim().trim_end_matches('.').parse::<u64>().unwrap_or(0)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> MemoryPressureConfig {
        MemoryPressureConfig {
            enabled: true,
            sample_interval_ms: 1000,
            yellow_threshold: 70.0,
            orange_threshold: 85.0,
            red_threshold: 95.0,
            compress_idle_secs: 300,
            evict_idle_secs: 1800,
        }
    }

    #[test]
    fn tier_ordering() {
        assert!(MemoryPressureTier::Green < MemoryPressureTier::Yellow);
        assert!(MemoryPressureTier::Yellow < MemoryPressureTier::Orange);
        assert!(MemoryPressureTier::Orange < MemoryPressureTier::Red);
    }

    #[test]
    fn tier_display() {
        assert_eq!(format!("{}", MemoryPressureTier::Green), "GREEN");
        assert_eq!(format!("{}", MemoryPressureTier::Red), "RED");
    }

    #[test]
    fn tier_numeric() {
        assert_eq!(MemoryPressureTier::Green.as_u8(), 0);
        assert_eq!(MemoryPressureTier::Yellow.as_u8(), 1);
        assert_eq!(MemoryPressureTier::Orange.as_u8(), 2);
        assert_eq!(MemoryPressureTier::Red.as_u8(), 3);
    }

    #[test]
    fn tier_suggested_actions() {
        assert_eq!(
            MemoryPressureTier::Green.suggested_action(),
            MemoryAction::None
        );
        assert_eq!(
            MemoryPressureTier::Yellow.suggested_action(),
            MemoryAction::CompressIdle
        );
        assert_eq!(
            MemoryPressureTier::Orange.suggested_action(),
            MemoryAction::EvictToDisk
        );
        assert_eq!(
            MemoryPressureTier::Red.suggested_action(),
            MemoryAction::EmergencyCleanup
        );
    }

    #[test]
    fn classify_green() {
        let monitor = MemoryPressureMonitor::new(test_config());
        assert_eq!(monitor.classify(0.0), MemoryPressureTier::Green);
        assert_eq!(monitor.classify(69.9), MemoryPressureTier::Green);
    }

    #[test]
    fn classify_yellow() {
        let monitor = MemoryPressureMonitor::new(test_config());
        assert_eq!(monitor.classify(70.0), MemoryPressureTier::Yellow);
        assert_eq!(monitor.classify(84.9), MemoryPressureTier::Yellow);
    }

    #[test]
    fn classify_orange() {
        let monitor = MemoryPressureMonitor::new(test_config());
        assert_eq!(monitor.classify(85.0), MemoryPressureTier::Orange);
        assert_eq!(monitor.classify(94.9), MemoryPressureTier::Orange);
    }

    #[test]
    fn classify_red() {
        let monitor = MemoryPressureMonitor::new(test_config());
        assert_eq!(monitor.classify(95.0), MemoryPressureTier::Red);
        assert_eq!(monitor.classify(100.0), MemoryPressureTier::Red);
    }

    #[test]
    fn current_tier_default_is_green() {
        let monitor = MemoryPressureMonitor::new(test_config());
        assert_eq!(monitor.current_tier(), MemoryPressureTier::Green);
    }

    #[test]
    fn sample_returns_valid_data() {
        let monitor = MemoryPressureMonitor::new(test_config());
        let sample = monitor.sample();
        assert!(sample.used_percent >= 0.0);
        assert_eq!(sample.tier, monitor.current_tier());
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            assert!(sample.total_kb > 0, "total memory should be > 0");
        }
    }

    #[test]
    fn tier_handle_shares_state() {
        let monitor = MemoryPressureMonitor::new(test_config());
        let handle = monitor.tier_handle();
        assert_eq!(handle.load(Ordering::Relaxed), 0);

        handle.store(3, Ordering::Relaxed);
        assert_eq!(monitor.current_tier(), MemoryPressureTier::Red);
    }

    #[test]
    fn default_config_values() {
        let cfg = MemoryPressureConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.sample_interval_ms, 10_000);
        assert!((cfg.yellow_threshold - 70.0).abs() < f64::EPSILON);
        assert!((cfg.orange_threshold - 85.0).abs() < f64::EPSILON);
        assert!((cfg.red_threshold - 95.0).abs() < f64::EPSILON);
        assert_eq!(cfg.compress_idle_secs, 300);
        assert_eq!(cfg.evict_idle_secs, 1800);
    }

    #[test]
    fn config_serde_roundtrip() {
        let cfg = MemoryPressureConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: MemoryPressureConfig = serde_json::from_str(&json).unwrap();
        assert!((parsed.yellow_threshold - cfg.yellow_threshold).abs() < f64::EPSILON);
        assert!((parsed.red_threshold - cfg.red_threshold).abs() < f64::EPSILON);
    }

    #[test]
    fn tier_serde_roundtrip() {
        let tier = MemoryPressureTier::Orange;
        let json = serde_json::to_string(&tier).unwrap();
        assert_eq!(json, "\"orange\"");
        let parsed: MemoryPressureTier = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, tier);
    }

    #[test]
    fn action_display() {
        assert_eq!(format!("{}", MemoryAction::None), "none");
        assert_eq!(format!("{}", MemoryAction::CompressIdle), "compress_idle");
        assert_eq!(format!("{}", MemoryAction::EvictToDisk), "evict_to_disk");
        assert_eq!(
            format!("{}", MemoryAction::EmergencyCleanup),
            "emergency_cleanup"
        );
    }

    #[test]
    fn action_serde_roundtrip() {
        for action in [
            MemoryAction::None,
            MemoryAction::CompressIdle,
            MemoryAction::EvictToDisk,
            MemoryAction::EmergencyCleanup,
        ] {
            let json = serde_json::to_string(&action).unwrap();
            let parsed: MemoryAction = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, action);
        }
    }

    #[test]
    fn pane_memory_info_serde() {
        let info = PaneMemoryInfo {
            pane_id: 42,
            rss_kb: 500_000,
            scrollback_compressed: false,
            scrollback_evicted: false,
            idle_secs: 120,
        };
        let json = serde_json::to_string(&info).unwrap();
        let parsed: PaneMemoryInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.pane_id, 42);
        assert_eq!(parsed.rss_kb, 500_000);
    }

    fn residency_bucket(
        report: &MacosResidencyClassification,
        bucket: MacosResidencyBucket,
    ) -> &MacosResidencyBucketSummary {
        report
            .buckets
            .iter()
            .find(|row| row.bucket == bucket)
            .expect("bucket row should be present")
    }

    #[test]
    fn macos_residency_classifier_separates_heap_mmap_graphics_scrollback_child_and_unknown() {
        let mib = 1024_u64 * 1024;
        let input = MacosResidencyClassifierInput {
            process_rss_bytes: Some(900 * mib),
            vmmap_output: Some(
                "\
MALLOC_SMALL                      128M
MAPPED_FILE /tmp/frankenterm.tantivy 256M
SQLite page cache                 64M
IOSurface CoreGraphics atlas      32M
frankenterm scrollback cache      96M
",
            ),
            heap_output: Some("Process 42: All zones: 192M total allocated\n"),
            sample_output: Some(
                "\
Call graph:
  rust_alloc::alloc
  CoreGraphics render atlas
  terminal history scrollback lookup
",
            ),
            child_process_rss_bytes: Some(48 * mib),
        };

        let report = classify_macos_residency(&input);

        assert_eq!(report.evidence_state, MacosResidencyEvidenceState::Measured);
        assert_eq!(
            residency_bucket(&report, MacosResidencyBucket::RustHeap).bytes,
            Some(192 * mib)
        );
        assert_eq!(
            residency_bucket(&report, MacosResidencyBucket::MmapFileBacked).bytes,
            Some(256 * mib)
        );
        assert_eq!(
            residency_bucket(&report, MacosResidencyBucket::SqlitePageCache).bytes,
            Some(64 * mib)
        );
        assert_eq!(
            residency_bucket(&report, MacosResidencyBucket::GraphicsMedia).bytes,
            Some(32 * mib)
        );
        assert_eq!(
            residency_bucket(&report, MacosResidencyBucket::ScrollbackCache).bytes,
            Some(96 * mib)
        );
        assert_eq!(
            residency_bucket(&report, MacosResidencyBucket::ChildProcesses).bytes,
            Some(48 * mib)
        );
        assert_eq!(report.unknown_bytes, Some(212 * mib));
        assert_eq!(report.dominant_bucket, MacosResidencyBucket::MmapFileBacked);
    }

    #[test]
    fn macos_residency_classifier_missing_sources_are_unavailable_not_green() {
        let report = classify_macos_residency(&MacosResidencyClassifierInput::default());

        assert_eq!(
            report.evidence_state,
            MacosResidencyEvidenceState::Unavailable
        );
        assert!(report.process_rss_bytes.is_none());
        assert!(report.unknown_bytes.is_none());
        assert_eq!(report.dominant_bucket, MacosResidencyBucket::Unknown);
        assert!(
            report
                .evidence
                .iter()
                .all(|status| status.state == MacosResidencyEvidenceState::Unavailable)
        );
        assert!(
            report
                .reason_codes
                .iter()
                .any(|code| code == "resource.telemetry.unavailable")
        );
    }

    #[test]
    fn macos_residency_classifier_malformed_tool_outputs_fail_visible() {
        let report = classify_macos_residency(&MacosResidencyClassifierInput {
            process_rss_bytes: Some(100),
            vmmap_output: Some("VM Map of process 42 with no region totals"),
            heap_output: Some("heap report truncated before totals"),
            sample_output: Some(""),
            child_process_rss_bytes: Some(0),
        });

        assert_eq!(report.evidence_state, MacosResidencyEvidenceState::Mixed);
        assert_eq!(report.unknown_bytes, Some(100));
        assert!(
            report
                .evidence
                .iter()
                .any(|status| status.tool == MacosResidencyEvidenceTool::Vmmap
                    && status.state == MacosResidencyEvidenceState::Unavailable
                    && status.reason_code == "resource.memory.vmmap_malformed")
        );
        assert!(
            report
                .evidence
                .iter()
                .any(|status| status.tool == MacosResidencyEvidenceTool::Heap
                    && status.state == MacosResidencyEvidenceState::Unavailable
                    && status.reason_code == "resource.memory.heap_malformed")
        );
        assert!(
            report
                .evidence
                .iter()
                .any(|status| status.tool == MacosResidencyEvidenceTool::Sample
                    && status.state == MacosResidencyEvidenceState::Unavailable
                    && status.reason_code == "resource.memory.sample_malformed")
        );
    }

    #[test]
    fn macos_residency_classifier_parses_unknown_and_decimal_region_sizes() {
        let mib = 1024_u64 * 1024;
        let gib = 1024_u64 * mib;
        let report = classify_macos_residency(&MacosResidencyClassifierInput {
            process_rss_bytes: Some(2 * gib),
            vmmap_output: Some(
                "\
MALLOC_LARGE       1.5GB
MYSTERY_REGION     512 MB
",
            ),
            heap_output: None,
            sample_output: None,
            child_process_rss_bytes: None,
        });

        assert_eq!(
            residency_bucket(&report, MacosResidencyBucket::RustHeap).bytes,
            Some(gib + (512 * mib))
        );
        assert_eq!(report.unknown_bytes, Some(512 * mib));
        assert_eq!(
            residency_bucket(&report, MacosResidencyBucket::Unknown).bytes,
            Some(512 * mib)
        );
    }

    #[test]
    fn macos_residency_classifier_sample_signals_do_not_forge_bytes() {
        let report = classify_macos_residency(&MacosResidencyClassifierInput {
            process_rss_bytes: None,
            vmmap_output: None,
            heap_output: None,
            sample_output: Some(
                "\
Call graph:
  malloc_zone_malloc
  mmap_file_read
  IOSurfaceAccelerator
  frankenterm scrollback lookup
",
            ),
            child_process_rss_bytes: None,
        });

        assert_eq!(report.evidence_state, MacosResidencyEvidenceState::Mixed);
        assert_eq!(
            residency_bucket(&report, MacosResidencyBucket::RustHeap).bytes,
            None
        );
        assert!(
            residency_bucket(&report, MacosResidencyBucket::RustHeap)
                .reason_codes
                .iter()
                .any(|code| code == "resource.memory.heap_stack_signal")
        );
        assert!(
            residency_bucket(&report, MacosResidencyBucket::GraphicsMedia)
                .reason_codes
                .iter()
                .any(|code| code == "resource.memory.graphics_stack_signal")
        );
        assert!(
            residency_bucket(&report, MacosResidencyBucket::ScrollbackCache)
                .reason_codes
                .iter()
                .any(|code| code == "resource.memory.scrollback_stack_signal")
        );
    }

    fn receipt_input(
        receipt_id: &str,
        action: ResourcePressureAction,
        target_domain: ResourcePressureDomain,
        status: ResourcePressureReceiptStatus,
        evidence_state: ResourcePressureEvidenceState,
    ) -> ResourcePressureActionReceiptInput {
        ResourcePressureActionReceiptInput {
            receipt_id: receipt_id.to_string(),
            correlation_id: None,
            action,
            target_domain,
            requested_at_ms: 1_000,
            completed_at_ms: None,
            status,
            dry_run: false,
            policy_decision: ResourcePressurePolicyDecision::Allow,
            evidence_state,
            attribution: ResourcePressureAttribution::default(),
            reason_codes: Vec::new(),
            artifact_paths: Vec::new(),
        }
    }

    fn receipt_for<'a>(
        report: &'a ResourcePressureActionReceiptReport,
        receipt_id: &str,
    ) -> &'a ResourcePressureActionReceipt {
        report
            .receipts
            .iter()
            .find(|receipt| receipt.receipt_id == receipt_id)
            .expect("receipt should be present")
    }

    fn domain_summary_for(
        report: &ResourcePressureActionReceiptReport,
        target_domain: ResourcePressureDomain,
    ) -> &ResourcePressureDomainReceiptSummary {
        report
            .domain_summaries
            .iter()
            .find(|summary| summary.target_domain == target_domain)
            .expect("domain summary should be present")
    }

    fn proc_kib_field(contents: &str, field: &str) -> Option<u64> {
        contents.lines().find_map(|line| {
            line.strip_prefix(field)?
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        })
    }

    fn proc_mem_total_kib() -> Option<u64> {
        let contents = std::fs::read_to_string("/proc/meminfo").ok()?;
        proc_kib_field(&contents, "MemTotal:")
    }

    fn proc_self_rss_kib() -> Option<u64> {
        let contents = std::fs::read_to_string("/proc/self/status").ok()?;
        proc_kib_field(&contents, "VmRSS:")
    }

    fn json_escape_for_test(value: &str) -> String {
        let mut escaped = String::new();
        for ch in value.chars() {
            match ch {
                '"' => escaped.push_str("\\\""),
                '\\' => escaped.push_str("\\\\"),
                '\n' => escaped.push_str("\\n"),
                '\r' => escaped.push_str("\\r"),
                '\t' => escaped.push_str("\\t"),
                ch if ch.is_control() => {
                    escaped.push_str(&format!("\\u{:04x}", ch as u32));
                }
                ch => escaped.push(ch),
            }
        }
        escaped
    }

    #[test]
    fn resource_pressure_soak_host_capability_probe() {
        let logical_cpus = std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(0);
        let (total_memory_bytes, _) = read_memory_info();
        let memory_kib = proc_mem_total_kib().unwrap_or(total_memory_bytes / 1024);
        let probe_rss_kib = proc_self_rss_kib().unwrap_or(0);
        let uname = format!("{} {}", std::env::consts::OS, std::env::consts::ARCH);

        println!(
            "FT_P3457_HOST_CAPABILITY_JSON:{{\"logical_cpus\":{},\"memory_kib\":{},\"probe_rss_kib\":{},\"uname\":\"{}\"}}",
            logical_cpus,
            memory_kib,
            probe_rss_kib,
            json_escape_for_test(&uname)
        );

        assert!(logical_cpus > 0, "host capability probe must report CPUs");
        assert!(memory_kib > 0, "host capability probe must report memory");
    }

    #[test]
    fn resource_pressure_receipts_fail_closed_on_missing_telemetry() {
        let report = evaluate_resource_pressure_action_receipts(&[receipt_input(
            "admission-missing-telemetry",
            ResourcePressureAction::DelayAdmission,
            ResourcePressureDomain::ResourceAdmission,
            ResourcePressureReceiptStatus::Applied,
            ResourcePressureEvidenceState::Unavailable,
        )]);

        let receipt = receipt_for(&report, "admission-missing-telemetry");
        assert_eq!(receipt.status, ResourcePressureReceiptStatus::Blocked);
        assert_eq!(
            receipt.policy_decision,
            ResourcePressurePolicyDecision::RequireApproval
        );
        assert!(
            receipt
                .reason_codes
                .iter()
                .any(|code| { code == "admission.fail_closed.missing_telemetry" })
        );
        assert!(
            receipt
                .reason_codes
                .iter()
                .any(|code| { code == "resource.telemetry.unavailable" })
        );
        assert!(
            receipt
                .reason_codes
                .iter()
                .any(|code| { code == "action_receipt.blocked" })
        );
        assert_eq!(report.blocked_receipts, 1);
        assert_eq!(report.unavailable_receipts, 1);
        assert_eq!(
            domain_summary_for(&report, ResourcePressureDomain::ResourceAdmission).blocked,
            1
        );
    }

    #[test]
    fn resource_pressure_receipts_preserve_stale_attribution_and_correlation() {
        let mut input = receipt_input(
            "pane-degrade-stale",
            ResourcePressureAction::DegradeCapture,
            ResourcePressureDomain::PaneBudget,
            ResourcePressureReceiptStatus::Planned,
            ResourcePressureEvidenceState::Stale,
        );
        input.correlation_id = Some("pressure-run-42".to_string());
        input.policy_decision = ResourcePressurePolicyDecision::RequireApproval;
        input.attribution = ResourcePressureAttribution {
            pane_id: Some(42),
            agent_name: Some("TopazPuma".to_string()),
            target_dir: Some("/tmp/ft-target".to_string()),
            queue_name: None,
            affected_bytes: Some(64 * 1024 * 1024),
        };
        input.artifact_paths = vec!["docs/resource-pressure/pane-42.json".to_string()];

        let report = evaluate_resource_pressure_action_receipts(&[input]);
        let receipt = receipt_for(&report, "pane-degrade-stale");

        assert_eq!(receipt.status, ResourcePressureReceiptStatus::Planned);
        assert_eq!(receipt.correlation_id.as_deref(), Some("pressure-run-42"));
        assert_eq!(receipt.attribution.pane_id, Some(42));
        assert_eq!(receipt.attribution.agent_name.as_deref(), Some("TopazPuma"));
        assert_eq!(receipt.artifact_paths.len(), 1);
        assert!(
            receipt
                .reason_codes
                .iter()
                .any(|code| { code == "resource.telemetry.stale" })
        );
        assert!(
            !receipt
                .reason_codes
                .iter()
                .any(|code| { code == "resource.attribution.unknown" })
        );
        assert_eq!(report.stale_receipts, 1);
        assert_eq!(
            domain_summary_for(&report, ResourcePressureDomain::PaneBudget).stale,
            1
        );
    }

    #[test]
    fn resource_pressure_receipts_summarize_multiple_domains() {
        let mut memory = receipt_input(
            "memory-scrollback-applied",
            ResourcePressureAction::EvictScrollback,
            ResourcePressureDomain::Memory,
            ResourcePressureReceiptStatus::Succeeded,
            ResourcePressureEvidenceState::Measured,
        );
        memory.attribution = ResourcePressureAttribution {
            pane_id: Some(7),
            agent_name: None,
            target_dir: None,
            queue_name: None,
            affected_bytes: Some(128 * 1024 * 1024),
        };

        let mut queue = receipt_input(
            "queue-throttle-failed",
            ResourcePressureAction::ThrottleQueue,
            ResourcePressureDomain::QueueBackpressure,
            ResourcePressureReceiptStatus::Failed,
            ResourcePressureEvidenceState::Measured,
        );
        queue.attribution = ResourcePressureAttribution {
            pane_id: None,
            agent_name: None,
            target_dir: None,
            queue_name: Some("capture".to_string()),
            affected_bytes: None,
        };

        let mut storage = receipt_input(
            "storage-dry-run",
            ResourcePressureAction::ShedOptionalWork,
            ResourcePressureDomain::StorageIo,
            ResourcePressureReceiptStatus::Planned,
            ResourcePressureEvidenceState::Simulated,
        );
        storage.dry_run = true;
        storage.policy_decision = ResourcePressurePolicyDecision::NotChecked;
        storage.reason_codes = vec!["storage_io.defer.search_freshness_lag".to_string()];

        let report = evaluate_resource_pressure_action_receipts(&[memory, queue, storage]);

        assert_eq!(report.receipts.len(), 3);
        assert_eq!(report.failed_receipts, 1);
        assert_eq!(report.domain_summaries.len(), 3);
        assert_eq!(
            domain_summary_for(&report, ResourcePressureDomain::Memory).applied,
            1
        );
        assert_eq!(
            domain_summary_for(&report, ResourcePressureDomain::QueueBackpressure).failed,
            1
        );
        assert_eq!(
            domain_summary_for(&report, ResourcePressureDomain::StorageIo).dry_run,
            1
        );
        assert!(
            report
                .reason_codes
                .iter()
                .any(|code| { code == "action_receipt.failed" })
        );
        assert!(
            report
                .reason_codes
                .iter()
                .any(|code| { code == "action_receipt.dry_run" })
        );
        assert!(
            report
                .reason_codes
                .iter()
                .any(|code| { code == "resource.telemetry.simulated" })
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_total_memory_readable() {
        let total = read_macos_total_memory();
        assert!(total > 0, "should detect total memory on macOS");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_available_memory_readable() {
        let available = read_macos_available_memory();
        assert!(available > 0, "should detect available memory on macOS");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_memory_ratio_sane() {
        let total = read_macos_total_memory();
        let available = read_macos_available_memory();
        assert!(
            available <= total,
            "available ({available}) should be <= total ({total})"
        );
    }

    #[test]
    fn read_memory_info_returns_values() {
        let (total, available) = read_memory_info();
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            assert!(total > 0);
            assert!(available > 0);
            assert!(available <= total);
        }
    }

    // -----------------------------------------------------------------------
    // Classify boundary conditions
    // -----------------------------------------------------------------------

    #[test]
    fn classify_at_exact_thresholds() {
        let monitor = MemoryPressureMonitor::new(test_config());
        // Exactly at threshold transitions.
        assert_eq!(monitor.classify(70.0), MemoryPressureTier::Yellow);
        assert_eq!(monitor.classify(85.0), MemoryPressureTier::Orange);
        assert_eq!(monitor.classify(95.0), MemoryPressureTier::Red);
    }

    #[test]
    fn classify_just_below_thresholds() {
        let monitor = MemoryPressureMonitor::new(test_config());
        // Epsilon below each threshold stays in lower tier.
        assert_eq!(monitor.classify(69.999999), MemoryPressureTier::Green);
        assert_eq!(monitor.classify(84.999999), MemoryPressureTier::Yellow);
        assert_eq!(monitor.classify(94.999999), MemoryPressureTier::Orange);
    }

    #[test]
    fn classify_zero_is_green() {
        let monitor = MemoryPressureMonitor::new(test_config());
        assert_eq!(monitor.classify(0.0), MemoryPressureTier::Green);
    }

    #[test]
    fn classify_hundred_is_red() {
        let monitor = MemoryPressureMonitor::new(test_config());
        assert_eq!(monitor.classify(100.0), MemoryPressureTier::Red);
    }

    #[test]
    fn classify_above_hundred_is_red() {
        let monitor = MemoryPressureMonitor::new(test_config());
        // >100% can happen with memory overcommit.
        assert_eq!(monitor.classify(150.0), MemoryPressureTier::Red);
    }

    #[test]
    fn classify_negative_is_green() {
        let monitor = MemoryPressureMonitor::new(test_config());
        assert_eq!(monitor.classify(-1.0), MemoryPressureTier::Green);
    }

    // -----------------------------------------------------------------------
    // Custom config thresholds
    // -----------------------------------------------------------------------

    #[test]
    fn custom_tight_thresholds() {
        let config = MemoryPressureConfig {
            yellow_threshold: 10.0,
            orange_threshold: 20.0,
            red_threshold: 30.0,
            ..MemoryPressureConfig::default()
        };
        let monitor = MemoryPressureMonitor::new(config);
        assert_eq!(monitor.classify(9.0), MemoryPressureTier::Green);
        assert_eq!(monitor.classify(10.0), MemoryPressureTier::Yellow);
        assert_eq!(monitor.classify(20.0), MemoryPressureTier::Orange);
        assert_eq!(monitor.classify(30.0), MemoryPressureTier::Red);
    }

    #[test]
    fn equal_thresholds_favor_highest_tier() {
        let config = MemoryPressureConfig {
            yellow_threshold: 50.0,
            orange_threshold: 50.0,
            red_threshold: 50.0,
            ..MemoryPressureConfig::default()
        };
        let monitor = MemoryPressureMonitor::new(config);
        // At 50.0, the >= checks proceed red→orange→yellow; red matches first.
        assert_eq!(monitor.classify(50.0), MemoryPressureTier::Red);
        assert_eq!(monitor.classify(49.9), MemoryPressureTier::Green);
    }

    // -----------------------------------------------------------------------
    // Atomic tier sharing
    // -----------------------------------------------------------------------

    #[test]
    fn tier_handle_round_trip_all_tiers() {
        let monitor = MemoryPressureMonitor::new(test_config());
        let handle = monitor.tier_handle();

        for (val, expected) in [
            (0u64, MemoryPressureTier::Green),
            (1, MemoryPressureTier::Yellow),
            (2, MemoryPressureTier::Orange),
            (3, MemoryPressureTier::Red),
        ] {
            handle.store(val, Ordering::Relaxed);
            assert_eq!(monitor.current_tier(), expected);
        }
    }

    #[test]
    fn unknown_tier_value_falls_back_to_green() {
        let monitor = MemoryPressureMonitor::new(test_config());
        let handle = monitor.tier_handle();
        // Values outside 0-3 should map to Green (the _ arm).
        handle.store(99, Ordering::Relaxed);
        assert_eq!(monitor.current_tier(), MemoryPressureTier::Green);
        handle.store(u64::MAX, Ordering::Relaxed);
        assert_eq!(monitor.current_tier(), MemoryPressureTier::Green);
    }

    // -----------------------------------------------------------------------
    // Tier ordering properties
    // -----------------------------------------------------------------------

    #[test]
    fn all_tiers_have_distinct_numeric_values() {
        let values: Vec<u8> = [
            MemoryPressureTier::Green,
            MemoryPressureTier::Yellow,
            MemoryPressureTier::Orange,
            MemoryPressureTier::Red,
        ]
        .iter()
        .map(|t| t.as_u8())
        .collect();
        // Check strictly monotonic.
        for w in values.windows(2) {
            assert!(w[0] < w[1]);
        }
    }

    #[test]
    fn tier_ord_matches_numeric_ord() {
        let tiers = [
            MemoryPressureTier::Green,
            MemoryPressureTier::Yellow,
            MemoryPressureTier::Orange,
            MemoryPressureTier::Red,
        ];
        for i in 0..tiers.len() {
            for j in (i + 1)..tiers.len() {
                assert!(tiers[i] < tiers[j]);
                assert!(tiers[i].as_u8() < tiers[j].as_u8());
            }
        }
    }

    // -----------------------------------------------------------------------
    // Tier serde exhaustive
    // -----------------------------------------------------------------------

    #[test]
    fn all_tiers_serde_roundtrip() {
        for tier in [
            MemoryPressureTier::Green,
            MemoryPressureTier::Yellow,
            MemoryPressureTier::Orange,
            MemoryPressureTier::Red,
        ] {
            let json = serde_json::to_string(&tier).unwrap();
            let parsed: MemoryPressureTier = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, tier);
        }
    }

    #[test]
    fn tier_serde_uses_snake_case() {
        assert_eq!(
            serde_json::to_string(&MemoryPressureTier::Green).unwrap(),
            "\"green\""
        );
        assert_eq!(
            serde_json::to_string(&MemoryPressureTier::Yellow).unwrap(),
            "\"yellow\""
        );
        assert_eq!(
            serde_json::to_string(&MemoryPressureTier::Orange).unwrap(),
            "\"orange\""
        );
        assert_eq!(
            serde_json::to_string(&MemoryPressureTier::Red).unwrap(),
            "\"red\""
        );
    }

    // -----------------------------------------------------------------------
    // Config serde with partial fields
    // -----------------------------------------------------------------------

    #[test]
    fn config_deserializes_with_partial_fields() {
        let json = r#"{"enabled": false, "sample_interval_ms": 5000}"#;
        let config: MemoryPressureConfig = serde_json::from_str(json).unwrap();
        assert!(!config.enabled);
        assert_eq!(config.sample_interval_ms, 5000);
        // Remaining fields should be defaults.
        assert!((config.yellow_threshold - 70.0).abs() < f64::EPSILON);
        assert_eq!(config.compress_idle_secs, 300);
    }

    // -----------------------------------------------------------------------
    // PaneMemoryInfo edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn pane_memory_info_zero_values() {
        let info = PaneMemoryInfo {
            pane_id: 0,
            rss_kb: 0,
            scrollback_compressed: false,
            scrollback_evicted: false,
            idle_secs: 0,
        };
        let json = serde_json::to_string(&info).unwrap();
        let parsed: PaneMemoryInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.pane_id, 0);
        assert_eq!(parsed.rss_kb, 0);
    }

    #[test]
    fn pane_memory_info_large_values() {
        let info = PaneMemoryInfo {
            pane_id: u64::MAX,
            rss_kb: u64::MAX,
            scrollback_compressed: true,
            scrollback_evicted: true,
            idle_secs: u64::MAX,
        };
        let json = serde_json::to_string(&info).unwrap();
        let parsed: PaneMemoryInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.pane_id, u64::MAX);
        assert_eq!(parsed.rss_kb, u64::MAX);
        assert!(parsed.scrollback_compressed);
        assert!(parsed.scrollback_evicted);
    }

    // -----------------------------------------------------------------------
    // Sample updates atomic tier
    // -----------------------------------------------------------------------

    #[test]
    fn sample_updates_current_tier() {
        let monitor = MemoryPressureMonitor::new(test_config());
        let _sample = monitor.sample();
        // After a sample, current_tier() should reflect the sampled tier.
        // We can't predict the exact tier (depends on actual system memory),
        // but the tier should be a valid value.
        let tier = monitor.current_tier();
        assert!(tier.as_u8() <= 3);
    }

    // -------------------------------------------------------------------------
    // LabRuntime deterministic tests for the Cx-first run entry point
    // (ft-xbnl0.2.x slice). Mirrors cpu_pressure.rs and memory_budget.rs
    // labruntime test patterns. We can't observe a sample count (no
    // counter on the telemetry struct) but we can observe that the atomic
    // tier handle is never written when the loop exits before sampling —
    // the initial value is 0 (Green) and a real sample on a loaded host
    // will never classify Green in this test config (sample_interval_ms
    // is the only knob; thresholds are defaults which would classify a
    // real sample as some tier).
    // -------------------------------------------------------------------------

    mod labruntime_memory_pressure {
        use super::*;
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;

        fn run_lab<F>(seed: u64, f: impl FnOnce() -> F + Send + 'static)
        where
            F: std::future::Future<Output = ()> + Send + 'static,
        {
            let mut runtime = asupersync::LabRuntime::new(
                asupersync::LabConfig::new(seed)
                    .with_auto_advance()
                    .worker_count(2)
                    .max_steps(50_000),
            );
            let region = runtime
                .state
                .create_root_region(asupersync::Budget::INFINITE);
            let (task_id, _handle) = runtime
                .state
                .create_task(region, asupersync::Budget::INFINITE, async move {
                    f().await;
                })
                .expect("spawn lab task");
            runtime.scheduler.lock().schedule(task_id, 0);

            let report = runtime.run_with_auto_advance();
            assert!(
                !matches!(
                    report.termination,
                    asupersync::lab::AutoAdvanceTermination::StuckBailout
                ),
                "LabRuntime got stuck; termination: {:?}",
                report.termination,
            );
        }

        /// `run_with_cx` must return before entering the loop if the
        /// caller's Cx is already cancelled. The atomic tier handle
        /// must remain at its initial Green value because no sample
        /// was taken.
        #[test]
        fn run_with_cx_pre_cancelled_exits_without_sampling() {
            run_lab(0x3E30_BED5_BED5_0303, || async move {
                let monitor = MemoryPressureMonitor::new(test_config());
                let tier_before = monitor.current_tier();
                assert_eq!(
                    tier_before,
                    MemoryPressureTier::Green,
                    "fresh monitor should start in Green"
                );
                let shutdown = Arc::new(AtomicBool::new(false));

                let budget = crate::cx::Budget::new().with_poll_quota(0);
                let cx = crate::cx::Cx::for_testing_with_budget(budget);
                cx.cancel_with(
                    crate::outcome::CancelKind::User,
                    Some("ft-xbnl0.2.x memory_pressure precancel"),
                );

                monitor.run_with_cx(&cx, Arc::clone(&shutdown)).await;

                // Tier handle must still be at the initial Green value —
                // any real sample would have classified a tier based on
                // actual memory usage. (Green is possible as a real
                // sample outcome, so this assertion is a necessary but
                // not sufficient check; paired with
                // `shutdown_set_exits_without_sampling` it pins the
                // short-circuit contract end-to-end.)
                assert_eq!(
                    monitor.current_tier(),
                    MemoryPressureTier::Green,
                    "pre-cancelled run_with_cx must not update the tier handle"
                );
                assert!(
                    !shutdown.load(Ordering::SeqCst),
                    "run_with_cx must return via the Cx path, not the shutdown flag"
                );
            });
        }

        /// `run_with_cx` with shutdown already set must exit before
        /// sampling — the shutdown check runs before `sample()` on the
        /// first tick. Pins the shared shutdown/Cx termination contract
        /// across all three sampling monitors (cpu_pressure,
        /// memory_budget, memory_pressure).
        #[test]
        fn run_with_cx_shutdown_set_exits_without_sampling() {
            run_lab(0x3E30_BED5_BED5_0404, || async move {
                let monitor = MemoryPressureMonitor::new(test_config());
                let shutdown = Arc::new(AtomicBool::new(true));
                let cx = crate::cx::for_request();

                monitor.run_with_cx(&cx, Arc::clone(&shutdown)).await;

                assert_eq!(
                    monitor.current_tier(),
                    MemoryPressureTier::Green,
                    "shutdown flag set before any tick must exit before sampling"
                );
            });
        }
    }
}
