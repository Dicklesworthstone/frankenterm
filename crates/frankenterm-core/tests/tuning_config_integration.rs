//! Integration tests for end-to-end `[tuning]` config loading across tracks T2-T7.

use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use frankenterm_core::config::{Config, ConfigOverrides};
use frankenterm_core::policy::{
    ActionKind, ActorKind, PaneCapabilities, PolicyEngine, PolicyInput,
};
use frankenterm_core::recorder_audit::{
    AuditLogConfig, approval_ttl_seconds_from_tuning, max_raw_query_rows_from_tuning,
};
#[cfg(feature = "recorder-lexical")]
use frankenterm_core::recorder_lexical_ingest::LexicalIndexerConfig;
use frankenterm_core::runtime::{ObservationRuntime, RuntimeConfig};
use frankenterm_core::runtime_compat::RwLock;
use frankenterm_core::storage::StorageHandle;
use frankenterm_core::workflows::{DescriptorLimits, WorkflowDescriptor};
use frankenterm_core::{patterns::PatternEngine, tuning_config::TuningConfig};
use tempfile::TempDir;

fn write_config(toml: &str) -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("ft.toml");
    std::fs::write(&path, toml).expect("write config file");
    (dir, path)
}

fn load_config(toml: &str) -> Config {
    let (_dir, path) = write_config(toml);
    Config::load_with_overrides(Some(&path), true, &ConfigOverrides::default())
        .expect("load config with overrides")
}

fn temp_db() -> (TempDir, String) {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("test.db").to_string_lossy().to_string();
    (dir, path)
}

fn run_async_test<F>(future: F)
where
    F: std::future::Future<Output = ()>,
{
    use frankenterm_core::runtime_compat::CompatRuntime;

    let runtime = frankenterm_core::runtime_compat::RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("build runtime");
    runtime.block_on(future);
}

#[test]
fn t2_wire_and_ipc_limits_load_from_tuning() {
    let config = load_config(
        r"
[tuning.wire_protocol]
max_message_size = 2097152
max_sender_id_len = 144

[tuning.ipc]
max_message_size = 262144
accept_poll_interval_ms = 250
",
    );

    let wire_limits =
        frankenterm_core::wire_protocol::resolve_limits(Some(&config.tuning.wire_protocol));
    assert_eq!(wire_limits.max_message_size, 2_097_152);
    assert_eq!(wire_limits.max_sender_id_len, 144);

    let ipc_limits = frankenterm_core::ipc::resolve_limits(Some(&config.tuning.ipc));
    assert_eq!(ipc_limits.max_message_size, 262_144);
    assert_eq!(ipc_limits.accept_poll_interval_ms, 250);
}

#[cfg(feature = "web")]
#[test]
fn t2_web_limits_load_from_tuning() {
    let config = load_config(
        r#"
[tuning.web]
default_host = "0.0.0.0"
default_port = 9911
"#,
    );

    assert_eq!(
        frankenterm_core::web::resolve_host(Some(&config.tuning.web)),
        "0.0.0.0"
    );
    assert_eq!(
        frankenterm_core::web::resolve_port(Some(&config.tuning.web)),
        9911
    );
}

#[test]
fn t3_runtime_receives_loaded_tuning_from_config() {
    run_async_test(async {
        let config = load_config(
            r"
[tuning.runtime]
output_coalesce_window_ms = 77
resize_watchdog_warning_ms = 3333

[tuning.backpressure]
warn_ratio = 0.55

[tuning.snapshot]
idle_window_secs = 42
",
        );

        let (_dir, db_path) = temp_db();
        let storage = StorageHandle::new(&db_path).await.expect("create storage");
        let pattern_engine = Arc::new(RwLock::new(PatternEngine::new()));

        let runtime =
            ObservationRuntime::new(RuntimeConfig::default(), storage.clone(), pattern_engine)
                .with_tuning(config.tuning.clone());

        let applied: &TuningConfig = runtime.tuning();
        assert_eq!(applied.runtime.output_coalesce_window_ms, 77);
        assert_eq!(applied.runtime.resize_watchdog_warning_ms, 3333);
        assert!((applied.backpressure.warn_ratio - 0.55).abs() < f64::EPSILON);
        assert_eq!(applied.snapshot.idle_window_secs, 42);

        drop(runtime);
        storage.shutdown().await.expect("shutdown storage");
    });
}

#[cfg(feature = "recorder-lexical")]
#[test]
fn t4_search_indexer_uses_loaded_tuning_values() {
    let config = load_config(
        r"
[tuning.search]
tantivy_writer_memory_bytes = 16777216
",
    );

    let indexer = LexicalIndexerConfig::from_tuning("/tmp/ft-lexical", &config.tuning);
    assert_eq!(indexer.index_dir, PathBuf::from("/tmp/ft-lexical"));
    assert_eq!(indexer.writer_memory_bytes, 16 * 1024 * 1024);
}

#[test]
fn t5_policy_and_audit_surfaces_use_loaded_tuning() {
    let config = load_config(
        r"
[tuning.policy]
rate_limit_window_secs = 1

[tuning.audit]
retention_days = 365
max_raw_query_rows = 321
approval_ttl_secs = 77
",
    );

    let mut engine = PolicyEngine::new(1, 100, false).with_tuning(&config.tuning);
    let send_input = || {
        PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(7)
            .with_capabilities(PaneCapabilities::prompt())
    };

    assert!(engine.authorize(&send_input()).is_allowed());
    assert!(engine.authorize(&send_input()).requires_approval());
    thread::sleep(Duration::from_millis(1_100));
    assert!(
        engine.authorize(&send_input()).is_allowed(),
        "custom 1s rate-limit window should expire before the third send"
    );

    let audit_config = AuditLogConfig::from_tuning(&config.tuning);
    assert_eq!(audit_config.retention_days, 365);
    assert_eq!(max_raw_query_rows_from_tuning(&config.tuning), 321);
    assert_eq!(approval_ttl_seconds_from_tuning(&config.tuning), 77);
}

#[test]
fn t6_workflow_descriptor_validation_uses_loaded_limits() {
    let config = load_config(
        r"
[tuning.workflows]
max_steps = 1
max_wait_timeout_ms = 5
max_sleep_ms = 5
max_text_len = 4
max_match_len = 4
",
    );

    let descriptor = WorkflowDescriptor {
        workflow_schema_version: 1,
        name: "tiny".to_string(),
        description: None,
        triggers: vec![],
        steps: vec![
            frankenterm_core::workflows::DescriptorStep::Sleep {
                id: "sleep-1".to_string(),
                description: None,
                duration_ms: 1,
            },
            frankenterm_core::workflows::DescriptorStep::Sleep {
                id: "sleep-2".to_string(),
                description: None,
                duration_ms: 1,
            },
        ],
        on_failure: None,
    };
    let limits = DescriptorLimits::from_tuning(&config.tuning);
    let err = descriptor
        .validate(&limits)
        .expect_err("two steps should exceed the configured max_steps=1");
    let msg = err.to_string();
    assert!(
        msg.to_lowercase().contains("too many steps"),
        "unexpected validation error: {msg}"
    );
}

#[cfg(feature = "web")]
#[test]
fn t7_web_stream_limits_use_loaded_tuning_values() {
    let config = load_config(
        r"
[tuning.web]
max_list_limit = 123
default_list_limit = 45
max_request_body_bytes = 54321
stream_default_max_hz = 12
stream_max_max_hz = 34
stream_keepalive_secs = 56
stream_scan_limit = 78
stream_scan_max_pages = 9
",
    );

    let limits = frankenterm_core::web::resolve_runtime_limits(Some(&config.tuning.web));
    assert_eq!(limits.max_list_limit, 123);
    assert_eq!(limits.default_list_limit, 45);
    assert_eq!(limits.max_request_body_bytes, 54_321);
    assert_eq!(limits.stream_default_max_hz, 12);
    assert_eq!(limits.stream_max_max_hz, 34);
    assert_eq!(limits.stream_keepalive_secs, 56);
    assert_eq!(limits.stream_scan_limit, 78);
    assert_eq!(limits.stream_scan_max_pages, 9);
}
