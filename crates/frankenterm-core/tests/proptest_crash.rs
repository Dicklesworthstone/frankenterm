//! Property-based tests for crash.rs
//!
//! Tests invariants for HealthSnapshot, ShutdownSummary, CrashReport, CrashManifest,
//! IncidentKind, ReplayMode, ReplayCheck, ReplayResult, CrashLoopConfig,
//! CrashLoopDetector, CrashLoopDiagnostics, PaneCaptureState, CaptureCheckpoint,
//! RedactionReport, FileRedactionEntry, and DbMetadata.

use frankenterm_core::crash::*;
use proptest::prelude::*;

// ============================================================================
// Strategies
// ============================================================================

fn arb_incident_kind() -> impl Strategy<Value = IncidentKind> {
    prop_oneof![Just(IncidentKind::Crash), Just(IncidentKind::Manual),]
}

fn arb_replay_mode() -> impl Strategy<Value = ReplayMode> {
    prop_oneof![Just(ReplayMode::Policy), Just(ReplayMode::Rules),]
}

fn arb_pane_priority_override() -> impl Strategy<Value = PanePriorityOverrideSnapshot> {
    (
        0..10000u64,
        0..100u32,
        proptest::option::of(1_000_000u64..2_000_000_000u64),
    )
        .prop_map(
            |(pane_id, priority, expires_at)| PanePriorityOverrideSnapshot {
                pane_id,
                priority,
                expires_at,
            },
        )
}

fn arb_health_snapshot() -> impl Strategy<Value = HealthSnapshot> {
    (
        1_000_000u64..2_000_000_000u64,
        0..100usize,
        0..1000usize,
        0..1000usize,
        prop::collection::vec((0..100u64, 0..10000i64), 0..10),
        prop::collection::vec("[a-z ]{5,30}", 0..5),
        0.0f64..1000.0,
        0..5000u64,
        proptest::bool::ANY,
        proptest::option::of(1_000_000u64..2_000_000_000u64),
    )
        .prop_map(
            |(
                timestamp,
                observed_panes,
                capture_queue_depth,
                write_queue_depth,
                last_seq_by_pane,
                warnings,
                ingest_lag_avg_ms,
                ingest_lag_max_ms,
                db_writable,
                db_last_write_at,
            )| {
                HealthSnapshot {
                    timestamp,
                    observed_panes,
                    capture_queue_depth,
                    write_queue_depth,
                    last_seq_by_pane,
                    warnings,
                    ingest_lag_avg_ms,
                    ingest_lag_max_ms,
                    db_writable,
                    db_last_write_at,
                    pane_priority_overrides: vec![],
                    scheduler: None,
                    backpressure_tier: None,
                    last_activity_by_pane: vec![],
                    restart_count: 0,
                    last_crash_at: None,
                    consecutive_crashes: 0,
                    current_backoff_ms: 0,
                    in_crash_loop: false,
                }
            },
        )
}

fn arb_crash_report() -> impl Strategy<Value = CrashReport> {
    (
        "[a-zA-Z0-9 ]{5,50}",
        proptest::option::of("[a-z/_.]{5,30}:[0-9]{1,5}:[0-9]{1,3}"),
        proptest::option::of("[a-zA-Z0-9 :]{10,100}"),
        1_000_000u64..2_000_000_000u64,
        1..100000u32,
        proptest::option::of("[a-z_]{3,15}"),
    )
        .prop_map(
            |(message, location, backtrace, timestamp, pid, thread_name)| CrashReport {
                message,
                location,
                backtrace,
                timestamp,
                pid,
                thread_name,
            },
        )
}

fn arb_shutdown_summary() -> impl Strategy<Value = ShutdownSummary> {
    (
        0..100000u64,
        0..1000usize,
        0..1000usize,
        0..100000u64,
        0..1000000u64,
        prop::collection::vec((0..100u64, 0..10000i64), 0..10),
        proptest::bool::ANY,
        prop::collection::vec("[a-z ]{5,30}", 0..3),
    )
        .prop_map(
            |(
                elapsed_secs,
                final_capture_queue,
                final_write_queue,
                segments_persisted,
                events_recorded,
                last_seq_by_pane,
                clean,
                warnings,
            )| {
                ShutdownSummary {
                    elapsed_secs,
                    final_capture_queue,
                    final_write_queue,
                    segments_persisted,
                    events_recorded,
                    last_seq_by_pane,
                    clean,
                    warnings,
                }
            },
        )
}

fn arb_crash_manifest() -> impl Strategy<Value = CrashManifest> {
    (
        "[0-9]\\.[0-9]\\.[0-9]",
        "[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}",
        prop::collection::vec("[a-z_]{3,20}\\.json", 0..5),
        proptest::bool::ANY,
        0..10000u64,
    )
        .prop_map(
            |(wa_version, created_at, files, has_health_snapshot, bundle_size_bytes)| {
                CrashManifest {
                    wa_version,
                    created_at,
                    files,
                    has_health_snapshot,
                    has_resize_forensics: false,
                    bundle_size_bytes,
                }
            },
        )
}

fn arb_replay_check() -> impl Strategy<Value = ReplayCheck> {
    (
        "[a-z_]{3,20}",
        proptest::bool::ANY,
        proptest::option::of("[a-zA-Z0-9 ]{5,50}"),
    )
        .prop_map(|(name, passed, detail)| ReplayCheck {
            name,
            passed,
            detail,
        })
}

fn arb_crash_loop_config() -> impl Strategy<Value = CrashLoopConfig> {
    (
        60..600u64,
        2..10u32,
        100..5000u64,
        5000..120000u64,
        1.1f64..4.0,
    )
        .prop_map(
            |(window_secs, crash_threshold, initial_delay_ms, max_delay_ms, backoff_factor)| {
                CrashLoopConfig {
                    window_secs,
                    crash_threshold,
                    initial_delay_ms,
                    max_delay_ms,
                    backoff_factor,
                }
            },
        )
}

fn arb_pane_capture_state() -> impl Strategy<Value = PaneCaptureState> {
    (
        0..10000u64,
        0..100000i64,
        0..1000000u64,
        1_000_000u64..2_000_000_000u64,
    )
        .prop_map(
            |(pane_id, last_seq, cursor_offset, last_capture_at)| PaneCaptureState {
                pane_id,
                last_seq,
                cursor_offset,
                last_capture_at,
            },
        )
}

fn arb_crash_loop_diagnostics() -> impl Strategy<Value = CrashLoopDiagnostics> {
    (
        0..100u32,
        proptest::option::of(1_000_000u64..2_000_000_000u64),
        0..20u32,
        0..120000u64,
        proptest::bool::ANY,
    )
        .prop_map(
            |(
                restart_count,
                last_crash_at,
                consecutive_crashes,
                current_backoff_ms,
                in_crash_loop,
            )| {
                CrashLoopDiagnostics {
                    restart_count,
                    last_crash_at,
                    consecutive_crashes,
                    current_backoff_ms,
                    in_crash_loop,
                }
            },
        )
}

fn arb_redaction_report() -> impl Strategy<Value = RedactionReport> {
    prop::collection::vec(
        ("[a-z_]{3,20}\\.json", 0..100usize)
            .prop_map(|(file, count)| FileRedactionEntry { file, count }),
        0..5,
    )
    .prop_map(|per_file| {
        let total_redactions = per_file.iter().map(|e| e.count).sum();
        RedactionReport {
            total_redactions,
            per_file,
        }
    })
}

fn arb_db_metadata() -> impl Strategy<Value = DbMetadata> {
    (
        proptest::option::of(1i64..100),
        proptest::option::of(0..100000000u64),
        proptest::option::of(prop_oneof![
            Just("wal".to_string()),
            Just("delete".to_string()),
            Just("truncate".to_string()),
        ]),
        proptest::option::of(0..1000000i64),
        proptest::option::of(0..100000i64),
    )
        .prop_map(
            |(schema_version, db_size_bytes, journal_mode, event_count, segment_count)| {
                DbMetadata {
                    schema_version,
                    db_size_bytes,
                    journal_mode,
                    event_count,
                    segment_count,
                }
            },
        )
}

// ============================================================================
// Property Tests: IncidentKind
// ============================================================================

proptest! {
    /// Property 1: IncidentKind serde roundtrip
    #[test]
    fn prop_incident_kind_serde_roundtrip(kind in arb_incident_kind()) {
        let json = serde_json::to_string(&kind).unwrap();
        let back: IncidentKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, kind);
    }

    /// Property 2: IncidentKind Display produces lowercase
    #[test]
    fn prop_incident_kind_display_lowercase(kind in arb_incident_kind()) {
        let s = kind.to_string();
        prop_assert!(!s.is_empty());
        prop_assert!(s.chars().all(|c| c.is_lowercase()),
                    "Display should be lowercase: {}", s);
    }

    /// Property 3: IncidentKind serde uses snake_case
    #[test]
    fn prop_incident_kind_serde_snake_case(kind in arb_incident_kind()) {
        let json = serde_json::to_string(&kind).unwrap();
        let inner = json.trim_matches('"');
        prop_assert!(inner.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                    "Serde should be snake_case: {}", inner);
    }
}

// ============================================================================
// Property Tests: ReplayMode
// ============================================================================

proptest! {
    /// Property 4: ReplayMode serde roundtrip
    #[test]
    fn prop_replay_mode_serde_roundtrip(mode in arb_replay_mode()) {
        let json = serde_json::to_string(&mode).unwrap();
        let back: ReplayMode = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, mode);
    }

    /// Property 5: ReplayMode Display produces lowercase
    #[test]
    fn prop_replay_mode_display_lowercase(mode in arb_replay_mode()) {
        let s = mode.to_string();
        prop_assert!(!s.is_empty());
        prop_assert!(s.chars().all(|c| c.is_lowercase()),
                    "Display should be lowercase: {}", s);
    }
}

// ============================================================================
// Property Tests: Serde Roundtrips
// ============================================================================

proptest! {
    /// Property 6: HealthSnapshot serde roundtrip
    #[test]
    fn prop_health_snapshot_serde_roundtrip(snap in arb_health_snapshot()) {
        let json = serde_json::to_string(&snap).unwrap();
        let back: HealthSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.timestamp, snap.timestamp);
        prop_assert_eq!(back.observed_panes, snap.observed_panes);
        prop_assert_eq!(back.capture_queue_depth, snap.capture_queue_depth);
        prop_assert_eq!(back.write_queue_depth, snap.write_queue_depth);
        prop_assert_eq!(back.db_writable, snap.db_writable);
        prop_assert_eq!(back.db_last_write_at, snap.db_last_write_at);
        prop_assert_eq!(back.warnings, snap.warnings);
        prop_assert_eq!(back.last_seq_by_pane, snap.last_seq_by_pane);
        prop_assert!((back.ingest_lag_avg_ms - snap.ingest_lag_avg_ms).abs() < 1e-10,
                    "ingest_lag_avg_ms mismatch");
    }

    /// Property 7: PanePriorityOverrideSnapshot serde roundtrip
    #[test]
    fn prop_pane_priority_override_serde_roundtrip(p in arb_pane_priority_override()) {
        let json = serde_json::to_string(&p).unwrap();
        let back: PanePriorityOverrideSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pane_id, p.pane_id);
        prop_assert_eq!(back.priority, p.priority);
        prop_assert_eq!(back.expires_at, p.expires_at);
    }

    /// Property 8: ShutdownSummary serde roundtrip
    #[test]
    fn prop_shutdown_summary_serde_roundtrip(s in arb_shutdown_summary()) {
        let json = serde_json::to_string(&s).unwrap();
        let back: ShutdownSummary = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.elapsed_secs, s.elapsed_secs);
        prop_assert_eq!(back.final_capture_queue, s.final_capture_queue);
        prop_assert_eq!(back.final_write_queue, s.final_write_queue);
        prop_assert_eq!(back.segments_persisted, s.segments_persisted);
        prop_assert_eq!(back.events_recorded, s.events_recorded);
        prop_assert_eq!(back.clean, s.clean);
        prop_assert_eq!(back.warnings, s.warnings);
    }

    /// Property 9: CrashReport serde roundtrip
    #[test]
    fn prop_crash_report_serde_roundtrip(r in arb_crash_report()) {
        let json = serde_json::to_string(&r).unwrap();
        let back: CrashReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.message, &r.message);
        prop_assert_eq!(&back.location, &r.location);
        prop_assert_eq!(&back.backtrace, &r.backtrace);
        prop_assert_eq!(back.timestamp, r.timestamp);
        prop_assert_eq!(back.pid, r.pid);
        prop_assert_eq!(&back.thread_name, &r.thread_name);
    }

    /// Property 10: CrashManifest serde roundtrip
    #[test]
    fn prop_crash_manifest_serde_roundtrip(m in arb_crash_manifest()) {
        let json = serde_json::to_string(&m).unwrap();
        let back: CrashManifest = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.wa_version, &m.wa_version);
        prop_assert_eq!(&back.created_at, &m.created_at);
        prop_assert_eq!(&back.files, &m.files);
        prop_assert_eq!(back.has_health_snapshot, m.has_health_snapshot);
        prop_assert_eq!(back.bundle_size_bytes, m.bundle_size_bytes);
    }

    /// Property 11: ReplayCheck serde roundtrip
    #[test]
    fn prop_replay_check_serde_roundtrip(c in arb_replay_check()) {
        let json = serde_json::to_string(&c).unwrap();
        let back: ReplayCheck = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.name, &c.name);
        prop_assert_eq!(back.passed, c.passed);
        prop_assert_eq!(&back.detail, &c.detail);
    }

    /// Property 12: CrashLoopDiagnostics serde roundtrip
    #[test]
    fn prop_crash_loop_diagnostics_serde_roundtrip(d in arb_crash_loop_diagnostics()) {
        let json = serde_json::to_string(&d).unwrap();
        let back: CrashLoopDiagnostics = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.restart_count, d.restart_count);
        prop_assert_eq!(back.last_crash_at, d.last_crash_at);
        prop_assert_eq!(back.consecutive_crashes, d.consecutive_crashes);
        prop_assert_eq!(back.current_backoff_ms, d.current_backoff_ms);
        prop_assert_eq!(back.in_crash_loop, d.in_crash_loop);
    }

    /// Property 13: PaneCaptureState serde roundtrip preserves equality
    #[test]
    fn prop_pane_capture_state_serde_roundtrip(s in arb_pane_capture_state()) {
        let json = serde_json::to_string(&s).unwrap();
        let back: PaneCaptureState = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, s);
    }

    /// Property 14: RedactionReport serde roundtrip
    #[test]
    fn prop_redaction_report_serde_roundtrip(r in arb_redaction_report()) {
        let json = serde_json::to_string(&r).unwrap();
        let back: RedactionReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.total_redactions, r.total_redactions);
        prop_assert_eq!(back.per_file.len(), r.per_file.len());
    }

    /// Property 15: DbMetadata serde roundtrip
    #[test]
    fn prop_db_metadata_serde_roundtrip(m in arb_db_metadata()) {
        let json = serde_json::to_string(&m).unwrap();
        let back: DbMetadata = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.schema_version, m.schema_version);
        prop_assert_eq!(back.db_size_bytes, m.db_size_bytes);
        prop_assert_eq!(&back.journal_mode, &m.journal_mode);
        prop_assert_eq!(back.event_count, m.event_count);
        prop_assert_eq!(back.segment_count, m.segment_count);
    }
}

// ============================================================================
// Property Tests: CrashLoopConfig
// ============================================================================

proptest! {
    /// Property 16: CrashLoopConfig serde roundtrip
    #[test]
    fn prop_crash_loop_config_serde_roundtrip(cfg in arb_crash_loop_config()) {
        let json = serde_json::to_string(&cfg).unwrap();
        let back: CrashLoopConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.window_secs, cfg.window_secs);
        prop_assert_eq!(back.crash_threshold, cfg.crash_threshold);
        prop_assert_eq!(back.initial_delay_ms, cfg.initial_delay_ms);
        prop_assert_eq!(back.max_delay_ms, cfg.max_delay_ms);
        prop_assert!((back.backoff_factor - cfg.backoff_factor).abs() < 1e-10,
                    "backoff_factor mismatch");
    }

    /// Property 17: CrashLoopConfig defaults are reasonable
    #[test]
    fn prop_crash_loop_config_defaults(_dummy in Just(())) {
        let cfg = CrashLoopConfig::default();
        prop_assert_eq!(cfg.window_secs, 300);
        prop_assert_eq!(cfg.crash_threshold, 3);
        prop_assert_eq!(cfg.initial_delay_ms, 1_000);
        prop_assert_eq!(cfg.max_delay_ms, 60_000);
        prop_assert!((cfg.backoff_factor - 2.0).abs() < f64::EPSILON);
    }
}

// ============================================================================
// Property Tests: CrashLoopDetector State Machine
// ============================================================================

proptest! {
    /// Property 18: New detector starts with no crashes
    #[test]
    fn prop_detector_starts_empty(cfg in arb_crash_loop_config()) {
        let det = CrashLoopDetector::new(cfg);
        prop_assert_eq!(det.consecutive_crashes(), 0);
        prop_assert_eq!(det.total_restarts(), 0);
        prop_assert!(det.last_crash_timestamp().is_none());
        prop_assert!(!det.is_crash_loop());
        prop_assert_eq!(det.next_delay_ms(), 0);
    }

    /// Property 19: record_crash increments consecutive_crashes
    #[test]
    fn prop_detector_record_crash_increments(
        cfg in arb_crash_loop_config(),
        n_crashes in 1..20u32,
        base_ts in 1_000_000u64..2_000_000_000u64,
    ) {
        let mut det = CrashLoopDetector::new(cfg);
        for i in 0..n_crashes {
            det.record_crash(base_ts + u64::from(i));
        }
        prop_assert_eq!(det.consecutive_crashes(), n_crashes);
    }

    /// Property 20: record_success resets consecutive_crashes to 0
    #[test]
    fn prop_detector_success_resets(
        cfg in arb_crash_loop_config(),
        n_crashes in 1..10u32,
        ts in 1_000_000u64..2_000_000_000u64,
    ) {
        let mut det = CrashLoopDetector::new(cfg);
        for i in 0..n_crashes {
            det.record_crash(ts + u64::from(i));
        }
        prop_assert!(det.consecutive_crashes() > 0);
        det.record_success();
        prop_assert_eq!(det.consecutive_crashes(), 0);
        prop_assert_eq!(det.next_delay_ms(), 0);
    }

    /// Property 21: Backoff delay is monotonically non-decreasing with consecutive crashes
    #[test]
    fn prop_detector_backoff_monotonic(
        cfg in arb_crash_loop_config(),
        ts in 1_000_000u64..2_000_000_000u64,
    ) {
        let mut det = CrashLoopDetector::new(cfg);
        let mut prev_delay = 0u64;
        for i in 0..15u32 {
            det.record_crash(ts + u64::from(i));
            let delay = det.next_delay_ms();
            prop_assert!(delay >= prev_delay,
                        "Delay should be non-decreasing: prev={}, cur={} at crash #{}",
                        prev_delay, delay, i + 1);
            prev_delay = delay;
        }
    }

    /// Property 22: Backoff delay never exceeds max_delay_ms
    #[test]
    fn prop_detector_backoff_capped(
        cfg in arb_crash_loop_config(),
        n_crashes in 1..30u32,
        ts in 1_000_000u64..2_000_000_000u64,
    ) {
        let mut det = CrashLoopDetector::new(cfg.clone());
        for i in 0..n_crashes {
            det.record_crash(ts + u64::from(i));
        }
        let delay = det.next_delay_ms();
        prop_assert!(delay <= cfg.max_delay_ms,
                    "Delay {} should not exceed max {}", delay, cfg.max_delay_ms);
    }

    /// Property 23: First crash delay equals initial_delay_ms
    #[test]
    fn prop_detector_first_crash_delay(
        cfg in arb_crash_loop_config(),
        ts in 1_000_000u64..2_000_000_000u64,
    ) {
        let mut det = CrashLoopDetector::new(cfg.clone());
        det.record_crash(ts);
        prop_assert_eq!(det.next_delay_ms(), cfg.initial_delay_ms,
                       "First crash delay should equal initial_delay_ms");
    }

    /// Property 24: Crash loop detected after threshold crashes in window
    #[test]
    fn prop_detector_crash_loop_triggers(
        cfg in arb_crash_loop_config(),
        ts in 1_000_000u64..2_000_000_000u64,
    ) {
        let mut det = CrashLoopDetector::new(cfg.clone());
        // Record exactly threshold crashes within the window
        for i in 0..cfg.crash_threshold {
            det.record_crash(ts + u64::from(i));
        }
        prop_assert!(det.is_crash_loop(),
                    "Should be in crash loop after {} crashes (threshold={})",
                    cfg.crash_threshold, cfg.crash_threshold);
    }

    /// Property 25: Below threshold is not a crash loop
    #[test]
    fn prop_detector_below_threshold_no_loop(
        cfg in arb_crash_loop_config(),
        ts in 1_000_000u64..2_000_000_000u64,
    ) {
        let mut det = CrashLoopDetector::new(cfg.clone());
        // Record fewer than threshold crashes
        for i in 0..(cfg.crash_threshold.saturating_sub(1)) {
            det.record_crash(ts + u64::from(i));
        }
        if cfg.crash_threshold > 1 {
            prop_assert!(!det.is_crash_loop(),
                        "Should not be in crash loop with {} crashes (threshold={})",
                        cfg.crash_threshold - 1, cfg.crash_threshold);
        }
    }

    /// Property 26: Crashes outside window don't count toward loop
    #[test]
    fn prop_detector_old_crashes_pruned(
        cfg in arb_crash_loop_config(),
        ts in 1_000_000u64..2_000_000_000u64,
    ) {
        let mut det = CrashLoopDetector::new(cfg.clone());
        // Record crashes far in the past
        let old_ts = ts.saturating_sub(cfg.window_secs + 100);
        for i in 0..cfg.crash_threshold {
            det.record_crash(old_ts + u64::from(i));
        }
        // Now record one recent crash to trigger pruning
        det.record_crash(ts);
        // Only 1 crash should be in the window
        prop_assert_eq!(det.crashes_in_window(ts), 1,
                       "Old crashes should be pruned from window");
    }

    /// Property 27: last_crash_timestamp returns the most recent crash
    #[test]
    fn prop_detector_last_crash_timestamp(
        cfg in arb_crash_loop_config(),
        ts in 1_000_000u64..2_000_000_000u64,
        n_crashes in 1..10u32,
    ) {
        let mut det = CrashLoopDetector::new(cfg);
        let last_ts = ts + u64::from(n_crashes - 1);
        for i in 0..n_crashes {
            det.record_crash(ts + u64::from(i));
        }
        prop_assert_eq!(det.last_crash_timestamp(), Some(last_ts));
    }

    /// Property 28: diagnostics() is consistent with individual methods
    #[test]
    fn prop_detector_diagnostics_consistent(
        cfg in arb_crash_loop_config(),
        n_crashes in 0..10u32,
        ts in 1_000_000u64..2_000_000_000u64,
    ) {
        let mut det = CrashLoopDetector::new(cfg);
        for i in 0..n_crashes {
            det.record_crash(ts + u64::from(i));
        }
        let diag = det.diagnostics();
        prop_assert_eq!(diag.consecutive_crashes, det.consecutive_crashes());
        prop_assert_eq!(diag.restart_count, det.total_restarts());
        prop_assert_eq!(diag.last_crash_at, det.last_crash_timestamp());
        prop_assert_eq!(diag.current_backoff_ms, det.next_delay_ms());
        prop_assert_eq!(diag.in_crash_loop, det.is_crash_loop());
    }

    /// Property 29: CrashLoopDetector serde roundtrip preserves state
    #[test]
    fn prop_detector_serde_roundtrip(
        cfg in arb_crash_loop_config(),
        n_crashes in 0..5u32,
        ts in 1_000_000u64..2_000_000_000u64,
    ) {
        let mut det = CrashLoopDetector::new(cfg);
        for i in 0..n_crashes {
            det.record_crash(ts + u64::from(i));
        }
        let json = serde_json::to_string(&det).unwrap();
        let back: CrashLoopDetector = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.consecutive_crashes(), det.consecutive_crashes());
        prop_assert_eq!(back.total_restarts(), det.total_restarts());
        prop_assert_eq!(back.last_crash_timestamp(), det.last_crash_timestamp());
        prop_assert_eq!(back.is_crash_loop(), det.is_crash_loop());
    }
}

// ============================================================================
// Property Tests: CaptureCheckpoint
// ============================================================================

proptest! {
    /// Property 30: CaptureCheckpoint::with_timestamp sets the given timestamp
    #[test]
    fn prop_checkpoint_with_timestamp(
        panes in prop::collection::vec(arb_pane_capture_state(), 0..10),
        ts in 1_000_000u64..2_000_000_000u64,
    ) {
        let ckpt = CaptureCheckpoint::with_timestamp(panes.clone(), ts);
        prop_assert_eq!(ckpt.created_at, ts);
        prop_assert_eq!(ckpt.panes.len(), panes.len());
        prop_assert_eq!(ckpt.version, 1); // CHECKPOINT_FORMAT_VERSION
    }

    /// Property 31: pane_state returns correct pane by ID
    #[test]
    fn prop_checkpoint_pane_state_lookup(
        panes in prop::collection::vec(arb_pane_capture_state(), 1..10),
    ) {
        let ckpt = CaptureCheckpoint::with_timestamp(panes.clone(), 1_000_000);
        for pane in &panes {
            let found = ckpt.pane_state(pane.pane_id);
            prop_assert!(found.is_some(),
                        "pane_state should find pane_id {}", pane.pane_id);
            let found = found.unwrap();
            prop_assert_eq!(found.pane_id, pane.pane_id);
        }
    }

    /// Property 32: pane_state returns None for unknown pane_id
    #[test]
    fn prop_checkpoint_pane_state_unknown(
        panes in prop::collection::vec(arb_pane_capture_state(), 0..5),
    ) {
        let ckpt = CaptureCheckpoint::with_timestamp(panes, 1_000_000);
        // Use a pane_id that's very unlikely to be in the generated set
        let result = ckpt.pane_state(999_999_999);
        prop_assert!(result.is_none(),
                    "pane_state should return None for unknown pane_id");
    }

    /// Property 33: should_skip_segment returns true for seq <= last_seq
    #[test]
    fn prop_checkpoint_skip_old_segments(
        pane_id in 0..1000u64,
        last_seq in 10..1000i64,
        delta in 0..10i64,
    ) {
        let pane = PaneCaptureState {
            pane_id,
            last_seq,
            cursor_offset: 0,
            last_capture_at: 1_000_000,
        };
        let ckpt = CaptureCheckpoint::with_timestamp(vec![pane], 1_000_000);
        // Seq at or before last_seq should be skipped
        prop_assert!(ckpt.should_skip_segment(pane_id, last_seq - delta),
                    "seq {} should be skipped (last_seq={})", last_seq - delta, last_seq);
    }

    /// Property 34: should_skip_segment returns false for seq > last_seq
    #[test]
    fn prop_checkpoint_no_skip_new_segments(
        pane_id in 0..1000u64,
        last_seq in 0..1000i64,
        delta in 1..100i64,
    ) {
        let pane = PaneCaptureState {
            pane_id,
            last_seq,
            cursor_offset: 0,
            last_capture_at: 1_000_000,
        };
        let ckpt = CaptureCheckpoint::with_timestamp(vec![pane], 1_000_000);
        prop_assert!(!ckpt.should_skip_segment(pane_id, last_seq + delta),
                    "seq {} should NOT be skipped (last_seq={})", last_seq + delta, last_seq);
    }

    /// Property 35: should_skip_segment returns false for unknown pane_id
    #[test]
    fn prop_checkpoint_no_skip_unknown_pane(seq in 0..1000i64) {
        let ckpt = CaptureCheckpoint::with_timestamp(vec![], 1_000_000);
        prop_assert!(!ckpt.should_skip_segment(999, seq),
                    "Unknown pane should not skip any segment");
    }

    /// Property 36: CaptureCheckpoint serde roundtrip
    #[test]
    fn prop_checkpoint_serde_roundtrip(
        panes in prop::collection::vec(arb_pane_capture_state(), 0..5),
        ts in 1_000_000u64..2_000_000_000u64,
    ) {
        let ckpt = CaptureCheckpoint::with_timestamp(panes.clone(), ts);
        let json = serde_json::to_string(&ckpt).unwrap();
        let back: CaptureCheckpoint = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.version, ckpt.version);
        prop_assert_eq!(back.created_at, ckpt.created_at);
        prop_assert_eq!(back.panes.len(), ckpt.panes.len());
        for (a, b) in back.panes.iter().zip(ckpt.panes.iter()) {
            prop_assert_eq!(a, b);
        }
    }

    /// Property 37: ReplayResult serde roundtrip
    #[test]
    fn prop_replay_result_serde_roundtrip(
        mode in arb_replay_mode(),
        status in prop_oneof![Just("pass".to_string()), Just("fail".to_string()), Just("incomplete".to_string())],
        checks in prop::collection::vec(arb_replay_check(), 0..5),
        warnings in prop::collection::vec("[a-z ]{5,20}", 0..3),
    ) {
        let result = ReplayResult { mode, status: status.clone(), checks, warnings };
        let json = serde_json::to_string(&result).unwrap();
        let back: ReplayResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.mode, result.mode);
        prop_assert_eq!(&back.status, &result.status);
        prop_assert_eq!(back.checks.len(), result.checks.len());
        prop_assert_eq!(back.warnings.len(), result.warnings.len());
    }

    /// Property 38: IncidentBundleResult serde roundtrip
    #[test]
    fn prop_incident_bundle_result_serde_roundtrip(
        kind in arb_incident_kind(),
        files in prop::collection::vec("[a-z_]{3,15}\\.json", 0..5),
        total_size in 0..1000000u64,
    ) {
        let result = IncidentBundleResult {
            path: std::path::PathBuf::from("/tmp/test_bundle"),
            kind,
            files: files.clone(),
            total_size_bytes: total_size,
            wa_version: "0.1.0".to_string(),
            exported_at: "2025-01-01T00:00:00Z".to_string(),
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: IncidentBundleResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.kind, result.kind);
        prop_assert_eq!(&back.files, &result.files);
        prop_assert_eq!(back.total_size_bytes, result.total_size_bytes);
    }
}

// ============================================================================
// Property Tests: RedactionReport Consistency
// ============================================================================

proptest! {
    /// Property 39: RedactionReport total equals sum of per_file counts
    #[test]
    fn prop_redaction_report_total_consistent(r in arb_redaction_report()) {
        let sum: usize = r.per_file.iter().map(|e| e.count).sum();
        prop_assert_eq!(r.total_redactions, sum,
                       "total_redactions should equal sum of per_file counts");
    }
}
