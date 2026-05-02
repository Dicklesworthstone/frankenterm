//! Per-variant golden corpus for `ErrorRenderer`. Pins the JSON output
//! shape (category, code, title, recovery_steps, hint) for every
//! top-level `Error` enum variant.
//!
//! Bead: ft-wvo1p (filed by cc_1's golden-artifacts audit).
//!
//! ## Why one golden per variant
//! `ErrorRenderer::new(OutputFormat::Json).render(&err)` is the
//! contract that TypeScript clients, MCP agents, `ft doctor --json`
//! consumers, and operator scripts grepping `error_code` depend on.
//! Before this corpus, exactly one variant (`PaneOperation`) was
//! pinned. New variants could land without goldens; field renames or
//! casing drift (the live `Wezterm` → `wezterm` snake_case drift this
//! bead surfaces) could land green. Now every variant has a frozen
//! reference output.
//!
//! ## Workflow
//! - Run `cargo test -p frankenterm-core --test conformance_error_renderer_corpus`
//!   to verify against goldens.
//! - Run `UPDATE_GOLDEN=1 cargo test -p frankenterm-core --test conformance_error_renderer_corpus`
//!   to regenerate goldens after intentional renderer changes.
//! - On drift, `<variant>.actual.json` is written next to the golden
//!   for diff inspection. `.actual.json` files are git-ignored.
//!
//! ## Coverage meta-test
//! `every_top_level_error_variant_has_a_golden` enumerates the
//! variants the corpus covers and asserts each has a fixture file.
//! New variants without a golden flip this test red — caught at
//! development time, not in production.

use frankenterm_core::error::{
    ConfigError, Error, PaneOperationSource, PatternError, RuntimeOperationSource, StorageError,
    WatchdogWarningSource, WeztermError, WorkflowError,
};
use frankenterm_core::output::{ErrorRenderer, OutputFormat};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

// ── helpers ──────────────────────────────────────────────────────────────────

fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("error_renderer")
}

fn golden_path(variant: &str) -> PathBuf {
    corpus_dir().join(format!("{variant}.json"))
}

/// Sort object keys recursively so golden output is order-independent.
fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = serde_json::Map::new();
            for k in keys {
                out.insert(k.clone(), canonicalize(&map[k]));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonicalize).collect()),
        other => other.clone(),
    }
}

fn render_to_canonical(error: &Error) -> String {
    let plain = ErrorRenderer::new(OutputFormat::Plain).render(error);
    let json_str = ErrorRenderer::new(OutputFormat::Json).render(error);
    let json_value: Value =
        serde_json::from_str(&json_str).expect("renderer must produce valid JSON");
    let combined = json!({
        "plain_lines": plain.trim_end_matches('\n').lines().collect::<Vec<_>>(),
        "json": json_value,
    });
    serde_json::to_string_pretty(&canonicalize(&combined)).expect("canonical pretty serialize")
}

fn assert_matches_golden(variant: &str, error: &Error) {
    let path = golden_path(variant);
    let actual = render_to_canonical(error);

    if std::env::var("UPDATE_GOLDEN").is_ok() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create corpus dir");
        }
        std::fs::write(&path, format!("{actual}\n")).expect("write golden");
        return;
    }

    let expected = std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "missing golden at {}: {err}. Regenerate with:\n  \
             UPDATE_GOLDEN=1 cargo test -p frankenterm-core --test conformance_error_renderer_corpus",
            path.display()
        )
    });
    let expected_trimmed = expected.trim_end_matches('\n');
    let actual_trimmed = actual.trim_end_matches('\n');

    if expected_trimmed != actual_trimmed {
        let actual_path = path.with_extension("actual.json");
        let _ = std::fs::write(&actual_path, format!("{actual}\n"));
        panic!(
            "golden drift for `{variant}`. Diff:\n  expected: {}\n  actual:   {}\n\n\
             If intentional, regenerate with:\n  \
             UPDATE_GOLDEN=1 cargo test -p frankenterm-core --test conformance_error_renderer_corpus",
            path.display(),
            actual_path.display()
        );
    }
}

/// Maintained list of every top-level `Error` variant the corpus pins.
/// `every_top_level_error_variant_has_a_golden` cross-checks that each
/// has a fixture file. When a new variant lands in `Error`, add it here
/// AND ship a golden — or this meta-test fires red.
const TOP_LEVEL_VARIANTS: &[&str] = &[
    "wezterm",
    "storage",
    "pattern",
    "workflow",
    "config",
    "policy",
    "io",
    "json",
    "runtime_operation",
    "pane_operation",
    "watchdog_warning_read",
    "runtime",
    "setup_error",
    "cancelled",
    "panicked",
];

// ── per-variant tests ────────────────────────────────────────────────────────

#[test]
fn wezterm_variant_matches_golden() {
    let error = Error::Wezterm(WeztermError::PaneNotFound(42));
    assert_matches_golden("wezterm", &error);
}

#[test]
fn storage_variant_matches_golden() {
    let error = Error::Storage(StorageError::Database("connection refused".into()));
    assert_matches_golden("storage", &error);
}

#[test]
fn pattern_variant_matches_golden() {
    let error = Error::Pattern(PatternError::MatchTimeout);
    assert_matches_golden("pattern", &error);
}

#[test]
fn workflow_variant_matches_golden() {
    let error = Error::Workflow(WorkflowError::PaneLocked);
    assert_matches_golden("workflow", &error);
}

#[test]
fn config_variant_matches_golden() {
    let error = Error::Config(ConfigError::FileNotFound("/etc/ft.toml".into()));
    assert_matches_golden("config", &error);
}

#[test]
fn policy_variant_matches_golden() {
    let error = Error::Policy("send blocked: redaction policy denies destructive command".into());
    assert_matches_golden("policy", &error);
}

#[test]
fn io_variant_matches_golden() {
    let error = Error::Io(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "no such file or directory",
    ));
    assert_matches_golden("io", &error);
}

#[test]
fn json_variant_matches_golden() {
    let json_err = serde_json::from_str::<serde_json::Value>("not json").unwrap_err();
    let error = Error::Json(json_err);
    assert_matches_golden("json", &error);
}

#[test]
fn runtime_operation_variant_matches_golden() {
    let error = Error::RuntimeOperation {
        operation: "config_reload",
        source: RuntimeOperationSource::WatchChannelClosed,
    };
    assert_matches_golden("runtime_operation", &error);
}

#[test]
fn pane_operation_variant_matches_golden() {
    let error = Error::PaneOperation {
        pane_id: 42,
        operation: "inject",
        source: PaneOperationSource::PaneNotFound,
    };
    assert_matches_golden("pane_operation", &error);
}

#[test]
fn watchdog_warning_read_variant_matches_golden() {
    let error = Error::WatchdogWarningRead {
        backend: "wezterm",
        source: WatchdogWarningSource::Backend("transport unavailable".into()),
    };
    assert_matches_golden("watchdog_warning_read", &error);
}

#[test]
#[allow(deprecated)]
fn runtime_variant_matches_golden() {
    let error = Error::Runtime("legacy runtime catch-all message".into());
    assert_matches_golden("runtime", &error);
}

#[test]
fn setup_error_variant_matches_golden() {
    let error = Error::SetupError("init step failed: missing platform tool".into());
    assert_matches_golden("setup_error", &error);
}

#[test]
fn cancelled_variant_matches_golden() {
    let error = Error::Cancelled("user cancelled before timeout".into());
    assert_matches_golden("cancelled", &error);
}

#[test]
fn panicked_variant_matches_golden() {
    let error = Error::Panicked("worker thread panicked: index out of bounds".into());
    assert_matches_golden("panicked", &error);
}

// ── coverage meta-test ───────────────────────────────────────────────────────

#[test]
fn every_top_level_error_variant_has_a_golden() {
    let dir = corpus_dir();
    assert!(
        dir.is_dir(),
        "corpus directory missing at {}; run UPDATE_GOLDEN=1 first",
        dir.display()
    );
    let mut missing = Vec::new();
    for variant in TOP_LEVEL_VARIANTS {
        let path = golden_path(variant);
        if !path.exists() {
            missing.push(variant.to_string());
        }
    }
    assert!(
        missing.is_empty(),
        "missing goldens in corpus dir {}: {missing:?}. \
         Regenerate with: UPDATE_GOLDEN=1 cargo test -p frankenterm-core \
         --test conformance_error_renderer_corpus",
        dir.display()
    );

    // Also confirm there are no orphan goldens — every fixture has a
    // matching entry in TOP_LEVEL_VARIANTS. Catches the inverse drift:
    // a variant deleted from Error{} without removing the golden.
    let mut orphans = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("read corpus dir") {
        let entry = entry.expect("dir entry");
        let p: &Path = &entry.path();
        if p.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        // Skip *.actual.json (drift artifacts; gitignored)
        if p.file_name()
            .and_then(|s| s.to_str())
            .map_or(false, |n| n.ends_with(".actual.json"))
        {
            continue;
        }
        let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if !TOP_LEVEL_VARIANTS.contains(&stem) {
            orphans.push(stem.to_string());
        }
    }
    assert!(
        orphans.is_empty(),
        "orphan goldens (no matching variant in TOP_LEVEL_VARIANTS): {orphans:?}",
    );
}
