//! Property tests for tuning_config serde roundtrips and default invariants.

use proptest::prelude::*;

use frankenterm_core::tuning_config::{
    AuditTuning, BackpressureTuning, CassQueryConfig, IngestTuning, IpcTuning, PatternsTuning,
    PolicyTuning, RuntimeTuning, SearchTuning, SnapshotTuning, TuningConfig, WebTuning,
    WeztermTuning, WireProtocolTuning, WorkflowsTuning,
};

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn positive_f64() -> impl Strategy<Value = f64> {
    (1u32..=10_000).prop_map(|n| n as f64 / 100.0)
}

fn runtime_tuning_strategy() -> impl Strategy<Value = RuntimeTuning> {
    (
        1u64..1000,
        1u64..2000,
        1usize..1_000_000,
        1usize..8192,
        1u64..30000,
        1u64..60000,
        1usize..20,
        1usize..50,
        positive_f64(),
        positive_f64(),
        1u64..1_000_000_000,
        1u64..3600,
    )
        .prop_map(
            |(
                coalesce_window,
                coalesce_max_delay,
                coalesce_max_bytes,
                percentile_window,
                watchdog_warn,
                watchdog_critical,
                stalled_limit,
                sample_limit,
                lock_wait_warn,
                lock_hold_warn,
                cursor_mem_warn,
                state_max_age,
            )| {
                RuntimeTuning {
                    output_coalesce_window_ms: coalesce_window,
                    output_coalesce_max_delay_ms: coalesce_max_delay,
                    output_coalesce_max_bytes: coalesce_max_bytes,
                    telemetry_percentile_window: percentile_window,
                    resize_watchdog_warning_ms: watchdog_warn,
                    resize_watchdog_critical_ms: watchdog_critical,
                    resize_watchdog_stalled_limit: stalled_limit,
                    resize_watchdog_sample_limit: sample_limit,
                    storage_lock_wait_warn_ms: lock_wait_warn,
                    storage_lock_hold_warn_ms: lock_hold_warn,
                    cursor_snapshot_memory_warn_bytes: cursor_mem_warn,
                    state_detection_max_age_secs: state_max_age,
                }
            },
        )
}

fn backpressure_tuning_strategy() -> impl Strategy<Value = BackpressureTuning> {
    positive_f64().prop_map(|ratio| BackpressureTuning {
        warn_ratio: ratio.clamp(0.01, 1.0),
    })
}

fn snapshot_tuning_strategy() -> impl Strategy<Value = SnapshotTuning> {
    (1u64..300, 1u64..3600, 1u64..600).prop_map(|(tick, idle, cooldown)| SnapshotTuning {
        trigger_bridge_tick_secs: tick,
        idle_window_secs: idle,
        memory_trigger_cooldown_secs: cooldown,
    })
}

fn ingest_tuning_strategy() -> impl Strategy<Value = IngestTuning> {
    (1usize..1_000_000, 1usize..100_000_000).prop_map(|(seg, payload)| IngestTuning {
        max_persist_segment_bytes: seg,
        max_record_payload_bytes: payload,
    })
}

fn patterns_tuning_strategy() -> impl Strategy<Value = PatternsTuning> {
    (1usize..10_000, 1usize..65536, positive_f64()).prop_map(|(keys, tail, bloom)| {
        PatternsTuning {
            max_seen_keys: keys,
            max_tail_size_bytes: tail,
            bloom_false_positive_rate: bloom.clamp(0.0001, 0.5),
        }
    })
}

fn policy_tuning_strategy() -> impl Strategy<Value = PolicyTuning> {
    (1u64..3600, 1usize..1024, 1usize..256, 1usize..2048).prop_map(
        |(window, panes, events, cost)| PolicyTuning {
            rate_limit_window_secs: window,
            max_tracked_panes: panes,
            max_events_per_pane: events,
            cost_tracker_max_panes: cost,
        },
    )
}

fn audit_tuning_strategy() -> impl Strategy<Value = AuditTuning> {
    (1u32..365, 1u64..7200, 1usize..10_000, 1u32..365, 1u32..90).prop_map(
        |(retention, ttl, rows, artifact, shadow)| AuditTuning {
            retention_days: retention,
            approval_ttl_secs: ttl,
            max_raw_query_rows: rows,
            artifact_retention_days: artifact,
            shadow_rollout_days: shadow,
        },
    )
}

fn web_tuning_strategy() -> impl Strategy<Value = WebTuning> {
    (
        "[a-z0-9.]{1,15}",
        1u16..65535,
        1usize..10_000,
        1usize..1000,
        1usize..1_000_000,
        1u32..1000,
        1u32..5000,
        1u64..300,
        1usize..1024,
        1usize..32,
    )
        .prop_map(
            |(
                host,
                port,
                max_list,
                default_list,
                max_body,
                default_hz,
                max_hz,
                keepalive,
                scan_limit,
                scan_pages,
            )| {
                WebTuning {
                    default_host: host,
                    default_port: port,
                    max_list_limit: max_list,
                    default_list_limit: default_list,
                    max_request_body_bytes: max_body,
                    stream_default_max_hz: default_hz,
                    stream_max_max_hz: max_hz,
                    stream_keepalive_secs: keepalive,
                    stream_scan_limit: scan_limit,
                    stream_scan_max_pages: scan_pages,
                }
            },
        )
}

fn cass_query_config_strategy() -> impl Strategy<Value = CassQueryConfig> {
    (1usize..20, 1u64..60, 1u32..365, 1usize..1000, 1usize..500).prop_map(
        |(hints, timeout, lookback, query_chars, hint_chars)| CassQueryConfig {
            hint_limit: hints,
            timeout_secs: timeout,
            lookback_days: lookback,
            query_max_chars: query_chars,
            hint_max_chars: hint_chars,
        },
    )
}

fn workflows_tuning_strategy() -> impl Strategy<Value = WorkflowsTuning> {
    (
        (
            1usize..128,
            1u64..300_000,
            1u64..120_000,
            1usize..65536,
            1usize..8192,
            cass_query_config_strategy(),
            cass_query_config_strategy(),
            cass_query_config_strategy(),
        ),
        (
            1u64..120,
            1u64..3_600_000,
            1u64..3_600_000,
            1u64..3_600_000,
            1u64..3_600_000,
            1u64..3_600_000,
        ),
    )
        .prop_map(
            |(
                (steps, wait_timeout, sleep, text_len, match_len, cass_start, cass_error, cass_auth_cfg),
                (swarm_timeout, cc_cooldown, start_cooldown, error_cooldown, swarm_cooldown, auth_cooldown),
            )| {
                WorkflowsTuning {
                    max_steps: steps,
                    max_wait_timeout_ms: wait_timeout,
                    max_sleep_ms: sleep,
                    max_text_len: text_len,
                    max_match_len: match_len,
                    cass_session_start: cass_start,
                    cass_on_error: cass_error,
                    cass_auth: cass_auth_cfg,
                    swarm_learning_index_timeout_secs: swarm_timeout,
                    claude_code_limits_cooldown_ms: cc_cooldown,
                    session_start_context_cooldown_ms: start_cooldown,
                    on_error_cooldown_ms: error_cooldown,
                    swarm_learning_index_cooldown_ms: swarm_cooldown,
                    auth_cooldown_ms: auth_cooldown,
                }
            },
        )
}

fn search_tuning_strategy() -> impl Strategy<Value = SearchTuning> {
    (
        1usize..1000,
        1usize..10_000,
        1usize..500,
        1usize..10_000,
        1usize..200_000_000,
    )
        .prop_map(|(default, max, saved, export, memory)| SearchTuning {
            default_limit: default,
            max_limit: max,
            saved_search_limit: saved,
            cass_export_limit: export,
            tantivy_writer_memory_bytes: memory,
        })
}

fn wire_protocol_tuning_strategy() -> impl Strategy<Value = WireProtocolTuning> {
    (1usize..10_000_000, 1usize..1024).prop_map(|(msg_size, sender_len)| WireProtocolTuning {
        max_message_size: msg_size,
        max_sender_id_len: sender_len,
    })
}

fn ipc_tuning_strategy() -> impl Strategy<Value = IpcTuning> {
    (1usize..1_000_000, 1u64..5000).prop_map(|(msg_size, poll)| IpcTuning {
        max_message_size: msg_size,
        accept_poll_interval_ms: poll,
    })
}

fn wezterm_tuning_strategy() -> impl Strategy<Value = WeztermTuning> {
    (1u64..120, 1u64..5000, 1usize..65536, 1u64..30000, 1u64..30000, 1u64..30000).prop_map(
        |(timeout, retry, max_err, connect, read, write)| WeztermTuning {
            timeout_secs: timeout,
            retry_delay_ms: retry,
            max_error_bytes: max_err,
            connect_timeout_ms: connect,
            read_timeout_ms: read,
            write_timeout_ms: write,
        },
    )
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn f64_close(a: f64, b: f64) -> bool {
    if a == b {
        return true;
    }
    (a - b).abs() < 1e-10
}

// ---------------------------------------------------------------------------
// Serde roundtrip tests
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn runtime_tuning_serde_roundtrip(rt in runtime_tuning_strategy()) {
        let json = serde_json::to_string(&rt).unwrap();
        let back: RuntimeTuning = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(rt.output_coalesce_window_ms, back.output_coalesce_window_ms);
        prop_assert_eq!(rt.output_coalesce_max_delay_ms, back.output_coalesce_max_delay_ms);
        prop_assert_eq!(rt.output_coalesce_max_bytes, back.output_coalesce_max_bytes);
        prop_assert_eq!(rt.telemetry_percentile_window, back.telemetry_percentile_window);
        prop_assert_eq!(rt.resize_watchdog_warning_ms, back.resize_watchdog_warning_ms);
        prop_assert_eq!(rt.resize_watchdog_critical_ms, back.resize_watchdog_critical_ms);
        prop_assert_eq!(rt.resize_watchdog_stalled_limit, back.resize_watchdog_stalled_limit);
        prop_assert_eq!(rt.resize_watchdog_sample_limit, back.resize_watchdog_sample_limit);
        let close1 = f64_close(rt.storage_lock_wait_warn_ms, back.storage_lock_wait_warn_ms);
        prop_assert!(close1, "storage_lock_wait_warn_ms mismatch");
        let close2 = f64_close(rt.storage_lock_hold_warn_ms, back.storage_lock_hold_warn_ms);
        prop_assert!(close2, "storage_lock_hold_warn_ms mismatch");
        prop_assert_eq!(rt.cursor_snapshot_memory_warn_bytes, back.cursor_snapshot_memory_warn_bytes);
        prop_assert_eq!(rt.state_detection_max_age_secs, back.state_detection_max_age_secs);
    }

    #[test]
    fn backpressure_tuning_serde_roundtrip(bp in backpressure_tuning_strategy()) {
        let json = serde_json::to_string(&bp).unwrap();
        let back: BackpressureTuning = serde_json::from_str(&json).unwrap();
        let close = f64_close(bp.warn_ratio, back.warn_ratio);
        prop_assert!(close, "warn_ratio mismatch");
    }

    #[test]
    fn snapshot_tuning_serde_roundtrip(st in snapshot_tuning_strategy()) {
        let json = serde_json::to_string(&st).unwrap();
        let back: SnapshotTuning = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(st, back);
    }

    #[test]
    fn ingest_tuning_serde_roundtrip(it in ingest_tuning_strategy()) {
        let json = serde_json::to_string(&it).unwrap();
        let back: IngestTuning = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(it, back);
    }

    #[test]
    fn patterns_tuning_serde_roundtrip(pt in patterns_tuning_strategy()) {
        let json = serde_json::to_string(&pt).unwrap();
        let back: PatternsTuning = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(pt.max_seen_keys, back.max_seen_keys);
        prop_assert_eq!(pt.max_tail_size_bytes, back.max_tail_size_bytes);
        let close = f64_close(pt.bloom_false_positive_rate, back.bloom_false_positive_rate);
        prop_assert!(close, "bloom_false_positive_rate mismatch");
    }

    #[test]
    fn policy_tuning_serde_roundtrip(pol in policy_tuning_strategy()) {
        let json = serde_json::to_string(&pol).unwrap();
        let back: PolicyTuning = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(pol, back);
    }

    #[test]
    fn audit_tuning_serde_roundtrip(at in audit_tuning_strategy()) {
        let json = serde_json::to_string(&at).unwrap();
        let back: AuditTuning = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(at, back);
    }

    #[test]
    fn web_tuning_serde_roundtrip(wt in web_tuning_strategy()) {
        let json = serde_json::to_string(&wt).unwrap();
        let back: WebTuning = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(wt, back);
    }

    #[test]
    fn cass_query_config_serde_roundtrip(cqc in cass_query_config_strategy()) {
        let json = serde_json::to_string(&cqc).unwrap();
        let back: CassQueryConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(cqc, back);
    }

    #[test]
    fn workflows_tuning_serde_roundtrip(wft in workflows_tuning_strategy()) {
        let json = serde_json::to_string(&wft).unwrap();
        let back: WorkflowsTuning = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(wft.max_steps, back.max_steps);
        prop_assert_eq!(wft.max_wait_timeout_ms, back.max_wait_timeout_ms);
        prop_assert_eq!(wft.cass_session_start, back.cass_session_start);
        prop_assert_eq!(wft.cass_on_error, back.cass_on_error);
        prop_assert_eq!(wft.cass_auth, back.cass_auth);
    }

    #[test]
    fn search_tuning_serde_roundtrip(st in search_tuning_strategy()) {
        let json = serde_json::to_string(&st).unwrap();
        let back: SearchTuning = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(st, back);
    }

    #[test]
    fn wire_protocol_tuning_serde_roundtrip(wp in wire_protocol_tuning_strategy()) {
        let json = serde_json::to_string(&wp).unwrap();
        let back: WireProtocolTuning = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(wp, back);
    }

    #[test]
    fn ipc_tuning_serde_roundtrip(ipc in ipc_tuning_strategy()) {
        let json = serde_json::to_string(&ipc).unwrap();
        let back: IpcTuning = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(ipc, back);
    }

    #[test]
    fn wezterm_tuning_serde_roundtrip(wz in wezterm_tuning_strategy()) {
        let json = serde_json::to_string(&wz).unwrap();
        let back: WeztermTuning = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(wz, back);
    }

    #[test]
    fn tuning_config_serde_roundtrip(
        rt in runtime_tuning_strategy(),
        bp in backpressure_tuning_strategy(),
        sn in snapshot_tuning_strategy(),
    ) {
        let config = TuningConfig {
            runtime: rt,
            backpressure: bp,
            snapshot: sn,
            ..TuningConfig::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: TuningConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(
            config.runtime.output_coalesce_window_ms,
            back.runtime.output_coalesce_window_ms
        );
        prop_assert_eq!(
            config.snapshot.trigger_bridge_tick_secs,
            back.snapshot.trigger_bridge_tick_secs
        );
    }
}

// ---------------------------------------------------------------------------
// Default value invariant tests
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn default_tuning_config_roundtrips(_seed in 0u32..100) {
        let config = TuningConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let back: TuningConfig = serde_json::from_str(&json).unwrap();
        // All integer fields should survive roundtrip exactly
        prop_assert_eq!(
            config.runtime.output_coalesce_window_ms,
            back.runtime.output_coalesce_window_ms
        );
        prop_assert_eq!(
            config.policy.rate_limit_window_secs,
            back.policy.rate_limit_window_secs
        );
        prop_assert_eq!(
            config.audit.retention_days,
            back.audit.retention_days
        );
        prop_assert_eq!(
            config.web.default_port,
            back.web.default_port
        );
    }

    #[test]
    fn empty_json_produces_defaults(_seed in 0u32..100) {
        let config: TuningConfig = serde_json::from_str("{}").unwrap();
        let default = TuningConfig::default();
        prop_assert_eq!(
            config.runtime.output_coalesce_window_ms,
            default.runtime.output_coalesce_window_ms
        );
        prop_assert_eq!(
            config.policy.rate_limit_window_secs,
            default.policy.rate_limit_window_secs
        );
    }

    #[test]
    fn partial_json_fills_defaults(port in 1u16..65535) {
        let json = format!(r#"{{"web":{{"default_port":{}}}}}"#, port);
        let config: TuningConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config.web.default_port, port);
        // Other fields should get defaults
        let default = TuningConfig::default();
        prop_assert_eq!(
            config.runtime.output_coalesce_window_ms,
            default.runtime.output_coalesce_window_ms
        );
    }

    #[test]
    fn cass_constructors_are_deterministic(_seed in 0u32..100) {
        let a = CassQueryConfig::session_start();
        let b = CassQueryConfig::session_start();
        prop_assert_eq!(a, b);
        let c = CassQueryConfig::on_error();
        let d = CassQueryConfig::on_error();
        prop_assert_eq!(c, d);
        let e = CassQueryConfig::auth();
        let f = CassQueryConfig::auth();
        prop_assert_eq!(e, f);
    }

    #[test]
    fn cass_on_error_has_shorter_timeout(_seed in 0u32..100) {
        let start = CassQueryConfig::session_start();
        let error = CassQueryConfig::on_error();
        prop_assert!(
            error.timeout_secs <= start.timeout_secs,
            "on_error timeout ({}) should be <= session_start timeout ({})",
            error.timeout_secs,
            start.timeout_secs
        );
    }
}
