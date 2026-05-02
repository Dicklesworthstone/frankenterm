//! NTM differential test harness for robot family parity.
//!
//! **Bead:** [BR-RC-ROBOT-CONTRACT.0.1] / `ft-hac7w.1.1`
//! **Companion doc:** [`docs/robot-contracts/ntm-differential-rules.md`].
//!
//! # What this is
//!
//! The bridge plan requires that every robot family with an `ntm`
//! equivalent shows zero observable divergence on a 1000-request
//! fuzz corpus. This module is the methodology shared by every
//! per-family bead (`ft-hac7w.2`…`ft-hac7w.6`): a request stream
//! flows through ft's native handler AND through `ntm` (as a
//! subprocess), the two responses are normalized via the rule
//! table in the companion doc, and the harness asserts byte-for-byte
//! equality on the normalized values.
//!
//! Per-family beads plug into this harness by supplying:
//!
//! 1. The family's [`FamilyContract`] (already shipped in
//!    `robot_family_contract.rs` under ft-hac7w.1).
//! 2. A native handler closure with signature
//!    `Fn(&Value) -> Result<Value, String>`.
//! 3. Optionally, an `ntm` subcommand mapping for the action.
//!    Families that have NO `ntm` equivalent simply skip the
//!    differential half — the harness's normal conformance path
//!    still runs.
//!
//! # What this commit ships
//!
//! `ft-hac7w.1.1` is the methodology bead. This module ships:
//!
//! - The [`DifferentialHarness`] type with the public API every
//!   downstream family will call.
//! - The [`NormalizationRules`] table mirroring the companion
//!   doc's Layer 1 + Layer 2 entries.
//! - [`HarnessMode`] for switching between CI normalization
//!   (Layers 1 + 2) and host-state mode (Layer 1 only).
//! - The [`DivergenceReport`] data type the harness returns when
//!   responses diverge after normalization.
//!
//! What this commit does NOT ship (filed as follow-on beads):
//!
//! - The 1000-request fuzz corpus per family — each per-family
//!   bead authors its own corpus.
//! - The CI integration that runs the corpus on every PR — filed
//!   as a follow-on bead because the corpus must exist first.
//! - The actual `ntm` subprocess invoker (mocked for now via the
//!   [`NtmInvoker`] trait so the harness compiles + tests pass
//!   without a real `ntm` binary on the build machine).
//!
//! # Why a trait-shaped invoker
//!
//! `ntm` is an external CLI; on a developer machine without `ntm`
//! installed, the harness must still compile and run its
//! self-tests. The [`NtmInvoker`] trait abstracts the subprocess:
//! the production implementation shells out to `ntm <subcommand>
//! --json <request>`; tests use a [`MockNtmInvoker`] that returns
//! pre-canned responses keyed by `(family, action, request_hash)`.
//!
//! # Cross-references
//!
//! - [`crate::robot_family_contract`] — schema-DSL produced by
//!   ft-hac7w.1.
//! - `tests/robot_family_conformance.rs` — the conformance harness
//!   the differential harness composes with.

use serde_json::Value;
use std::collections::BTreeMap;

/// Replacement token written in place of a normalized field's
/// original value. Constant per-category so harness output stays
/// stable across runs.
pub mod tokens {
    pub const TS: &str = "<NORMALIZED:ts>";
    pub const DURATION: &str = "<NORMALIZED:duration>";
    pub const PID: &str = "<NORMALIZED:pid>";
    pub const UUID: &str = "<NORMALIZED:uuid>";
    pub const HOST: &str = "<NORMALIZED:host>";
    pub const VERSION: &str = "<NORMALIZED:version>";
    pub const CWD: &str = "<NORMALIZED:cwd>";
    pub const HOME: &str = "<NORMALIZED:home>";
    pub const TMP: &str = "<NORMALIZED:tmp>";
    pub const UID: &str = "<NORMALIZED:uid>";
    pub const SOCK: &str = "<NORMALIZED:sock>";
}

/// Harness operating mode controlling which normalization layers
/// fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarnessMode {
    /// CI mode: applies Layer 1 (trivial drift) + Layer 2
    /// (operational drift). Use when the test runs in CI or in
    /// any context where host-state divergence is acceptable.
    Ci,
    /// Host-state mode: applies Layer 1 only. Use when the test
    /// is a real integration test that intends to assert on the
    /// host's actual paths / uid / hostname.
    HostState,
}

/// One normalization rule. The harness walks the response JSON
/// looking for any field whose name matches `field_name` and
/// replaces its value with [`replacement`]. The match is on field
/// *names*, not full JSON pointers — a deeply nested `timestamp`
/// at any depth gets normalized.
#[derive(Debug, Clone)]
pub struct NormalizationRule {
    /// JSON object key to match on (e.g. `"timestamp"`,
    /// `"pid"`).
    pub field_name: &'static str,
    /// Replacement token. Constant per-category.
    pub replacement: &'static str,
    /// Which layer this rule belongs to. Determines whether it
    /// fires in [`HarnessMode::HostState`].
    pub layer: NormalizationLayer,
}

/// Drift-classification layer for a normalization rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NormalizationLayer {
    /// Layer 1 — trivial drift. Always fires.
    Trivial,
    /// Layer 2 — operational drift. Fires only in CI mode.
    Operational,
}

/// Default normalization rule table mirroring the companion doc.
/// New families add rules here as they discover trivial- or
/// operational-drift fields. Real divergence (Layer 3) is the
/// implicit default — anything not in this table is asserted on
/// byte-for-byte.
#[must_use]
pub fn default_normalization_rules() -> Vec<NormalizationRule> {
    use NormalizationLayer::{Operational, Trivial};
    vec![
        // Layer 1: trivial drift (Always fires).
        NormalizationRule {
            field_name: "timestamp",
            replacement: tokens::TS,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "created_at",
            replacement: tokens::TS,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "updated_at",
            replacement: tokens::TS,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "started_at",
            replacement: tokens::TS,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "completed_at",
            replacement: tokens::TS,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "last_used_at",
            replacement: tokens::TS,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "last_seen_at",
            replacement: tokens::TS,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "first_seen_at",
            replacement: tokens::TS,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "closed_at",
            replacement: tokens::TS,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "expires_at",
            replacement: tokens::TS,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "duration_ms",
            replacement: tokens::DURATION,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "elapsed_ms",
            replacement: tokens::DURATION,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "duration_us",
            replacement: tokens::DURATION,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "elapsed_us",
            replacement: tokens::DURATION,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "took_ms",
            replacement: tokens::DURATION,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "wait_ms",
            replacement: tokens::DURATION,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "pid",
            replacement: tokens::PID,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "process_id",
            replacement: tokens::PID,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "parent_pid",
            replacement: tokens::PID,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "child_pid",
            replacement: tokens::PID,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "runner_pid",
            replacement: tokens::PID,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "uuid",
            replacement: tokens::UUID,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "session_uuid",
            replacement: tokens::UUID,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "correlation_id",
            replacement: tokens::UUID,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "request_id",
            replacement: tokens::UUID,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "execution_id",
            replacement: tokens::UUID,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "run_id",
            replacement: tokens::UUID,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "trace_id",
            replacement: tokens::UUID,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "hostname",
            replacement: tokens::HOST,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "host_name",
            replacement: tokens::HOST,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "host",
            replacement: tokens::HOST,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "version",
            replacement: tokens::VERSION,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "build_sha",
            replacement: tokens::VERSION,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "git_sha",
            replacement: tokens::VERSION,
            layer: Trivial,
        },
        NormalizationRule {
            field_name: "commit_sha",
            replacement: tokens::VERSION,
            layer: Trivial,
        },
        // Layer 2: operational drift (Only fires in CI mode).
        NormalizationRule {
            field_name: "cwd",
            replacement: tokens::CWD,
            layer: Operational,
        },
        NormalizationRule {
            field_name: "working_dir",
            replacement: tokens::CWD,
            layer: Operational,
        },
        NormalizationRule {
            field_name: "work_dir",
            replacement: tokens::CWD,
            layer: Operational,
        },
        NormalizationRule {
            field_name: "working_directory",
            replacement: tokens::CWD,
            layer: Operational,
        },
        NormalizationRule {
            field_name: "home_dir",
            replacement: tokens::HOME,
            layer: Operational,
        },
        NormalizationRule {
            field_name: "home",
            replacement: tokens::HOME,
            layer: Operational,
        },
        NormalizationRule {
            field_name: "user_home",
            replacement: tokens::HOME,
            layer: Operational,
        },
        NormalizationRule {
            field_name: "temp_dir",
            replacement: tokens::TMP,
            layer: Operational,
        },
        NormalizationRule {
            field_name: "tmp",
            replacement: tokens::TMP,
            layer: Operational,
        },
        NormalizationRule {
            field_name: "runtime_dir",
            replacement: tokens::TMP,
            layer: Operational,
        },
        NormalizationRule {
            field_name: "uid",
            replacement: tokens::UID,
            layer: Operational,
        },
        NormalizationRule {
            field_name: "gid",
            replacement: tokens::UID,
            layer: Operational,
        },
        NormalizationRule {
            field_name: "euid",
            replacement: tokens::UID,
            layer: Operational,
        },
        NormalizationRule {
            field_name: "egid",
            replacement: tokens::UID,
            layer: Operational,
        },
        NormalizationRule {
            field_name: "socket_path",
            replacement: tokens::SOCK,
            layer: Operational,
        },
        NormalizationRule {
            field_name: "ipc_socket_path",
            replacement: tokens::SOCK,
            layer: Operational,
        },
        NormalizationRule {
            field_name: "sock",
            replacement: tokens::SOCK,
            layer: Operational,
        },
    ]
}

/// Apply the rule table to a response value in place. Walks the
/// JSON tree depth-first and rewrites every matching object key's
/// value to the rule's replacement token.
pub fn normalize(value: &mut Value, rules: &[NormalizationRule], mode: HarnessMode) {
    let active: Vec<&NormalizationRule> = rules
        .iter()
        .filter(|r| match (mode, r.layer) {
            (HarnessMode::Ci, _) => true,
            (HarnessMode::HostState, NormalizationLayer::Trivial) => true,
            (HarnessMode::HostState, NormalizationLayer::Operational) => false,
        })
        .collect();
    let lookup: BTreeMap<&str, &str> = active
        .iter()
        .map(|r| (r.field_name, r.replacement))
        .collect();
    walk_normalize(value, &lookup);
}

fn walk_normalize(value: &mut Value, lookup: &BTreeMap<&str, &str>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                if let Some(replacement) = lookup.get(k.as_str()) {
                    *v = Value::String((*replacement).to_string());
                } else {
                    walk_normalize(v, lookup);
                }
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                walk_normalize(item, lookup);
            }
        }
        _ => {}
    }
}

/// Trait abstracting the `ntm` subprocess so the harness compiles
/// + self-tests without a real `ntm` binary. The production
/// implementation shells out to `ntm <action> --json <request>`;
/// tests use [`MockNtmInvoker`].
pub trait NtmInvoker {
    /// Invoke the `ntm` equivalent of `family.action` with the
    /// given request, returning the response JSON or an error
    /// describing why the invocation failed (subprocess crashed,
    /// JSON parse error, action not implemented, etc.).
    fn invoke(&self, family: &str, action: &str, request: &Value) -> Result<Value, String>;
}

/// In-memory mock NTM invoker for testing. Indexed by
/// `(family, action)`; the harness asserts on whatever response
/// the test pre-loaded.
pub struct MockNtmInvoker {
    responses: BTreeMap<(String, String), Value>,
}

impl MockNtmInvoker {
    /// Construct an empty mock. Pre-load entries via [`Self::with_response`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            responses: BTreeMap::new(),
        }
    }

    /// Pre-load a `(family, action) -> response` mapping. The
    /// `request` is not part of the key in the mock — tests that
    /// need request-dependent responses subclass via wrapping.
    #[must_use]
    pub fn with_response(mut self, family: &str, action: &str, response: Value) -> Self {
        self.responses
            .insert((family.to_string(), action.to_string()), response);
        self
    }
}

impl Default for MockNtmInvoker {
    fn default() -> Self {
        Self::new()
    }
}

impl NtmInvoker for MockNtmInvoker {
    fn invoke(&self, family: &str, action: &str, _request: &Value) -> Result<Value, String> {
        self.responses
            .get(&(family.to_string(), action.to_string()))
            .cloned()
            .ok_or_else(|| format!("MockNtmInvoker: no response registered for {family}.{action}"))
    }
}

/// Differential-comparison report. `Match` means the normalized
/// responses are byte-equal; `Diverge` carries the two normalized
/// values plus a JSON-pointer-formatted explanation of the first
/// observed divergence.
#[derive(Debug, Clone)]
pub enum DivergenceReport {
    Match,
    Diverge {
        native: Value,
        ntm: Value,
        explanation: String,
    },
}

impl DivergenceReport {
    #[must_use]
    pub fn is_match(&self) -> bool {
        matches!(self, Self::Match)
    }
}

/// The differential harness itself. Each per-family bead constructs
/// one of these (with its handler + invoker + rules) and calls
/// [`Self::compare`] for every request in the corpus.
pub struct DifferentialHarness<'a, F>
where
    F: Fn(&Value) -> Result<Value, String>,
{
    family: &'a str,
    action: &'a str,
    native_handler: F,
    invoker: &'a dyn NtmInvoker,
    rules: Vec<NormalizationRule>,
    mode: HarnessMode,
}

impl<'a, F> DifferentialHarness<'a, F>
where
    F: Fn(&Value) -> Result<Value, String>,
{
    pub fn new(
        family: &'a str,
        action: &'a str,
        native_handler: F,
        invoker: &'a dyn NtmInvoker,
    ) -> Self {
        Self {
            family,
            action,
            native_handler,
            invoker,
            rules: default_normalization_rules(),
            mode: HarnessMode::Ci,
        }
    }

    /// Override the operating mode. Default is [`HarnessMode::Ci`].
    #[must_use]
    pub fn with_mode(mut self, mode: HarnessMode) -> Self {
        self.mode = mode;
        self
    }

    /// Replace the rule table (e.g. to add a family-specific
    /// trivial-drift field). Default is [`default_normalization_rules`].
    #[must_use]
    pub fn with_rules(mut self, rules: Vec<NormalizationRule>) -> Self {
        self.rules = rules;
        self
    }

    /// Compare the native + ntm responses for one request.
    /// Both responses are normalized via the rule table before
    /// equality comparison.
    pub fn compare(&self, request: &Value) -> Result<DivergenceReport, String> {
        let mut native_response = (self.native_handler)(request)?;
        let mut ntm_response = self.invoker.invoke(self.family, self.action, request)?;

        normalize(&mut native_response, &self.rules, self.mode);
        normalize(&mut ntm_response, &self.rules, self.mode);

        if native_response == ntm_response {
            Ok(DivergenceReport::Match)
        } else {
            let explanation = first_divergence(&native_response, &ntm_response, "");
            Ok(DivergenceReport::Diverge {
                native: native_response,
                ntm: ntm_response,
                explanation,
            })
        }
    }
}

/// Walk two values in parallel and return a JSON-pointer-formatted
/// description of the first observed difference. Used to give
/// `DivergenceReport::Diverge` an actionable hint.
fn first_divergence(a: &Value, b: &Value, pointer: &str) -> String {
    match (a, b) {
        (Value::Object(am), Value::Object(bm)) => {
            for (k, av) in am {
                let next_pointer = format!("{pointer}/{k}");
                match bm.get(k) {
                    Some(bv) => {
                        let recursed = first_divergence(av, bv, &next_pointer);
                        if !recursed.is_empty() {
                            return recursed;
                        }
                    }
                    None => return format!("missing in ntm: {next_pointer}"),
                }
            }
            for k in bm.keys() {
                if !am.contains_key(k) {
                    return format!("extra in ntm: {pointer}/{k}");
                }
            }
            String::new()
        }
        (Value::Array(av), Value::Array(bv)) => {
            if av.len() != bv.len() {
                return format!(
                    "array length differs at {pointer}: native={} ntm={}",
                    av.len(),
                    bv.len()
                );
            }
            for (i, (ai, bi)) in av.iter().zip(bv.iter()).enumerate() {
                let next_pointer = format!("{pointer}/{i}");
                let recursed = first_divergence(ai, bi, &next_pointer);
                if !recursed.is_empty() {
                    return recursed;
                }
            }
            String::new()
        }
        _ if a == b => String::new(),
        _ => format!("value differs at {pointer}: native={a} ntm={b}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalize_trivial_drift_rewrites_timestamp_keys() {
        let mut v = json!({
            "result": "ok",
            "timestamp": 1_715_000_000_000_i64,
            "nested": { "started_at": "2026-05-01T12:00:00Z" },
        });
        normalize(&mut v, &default_normalization_rules(), HarnessMode::Ci);
        assert_eq!(v["timestamp"], json!(tokens::TS));
        assert_eq!(v["nested"]["started_at"], json!(tokens::TS));
        assert_eq!(v["result"], json!("ok"));
    }

    #[test]
    fn normalize_host_state_mode_skips_operational_layer() {
        let mut v = json!({
            "timestamp": 100,
            "cwd": "/runners/cwd",
        });
        normalize(
            &mut v,
            &default_normalization_rules(),
            HarnessMode::HostState,
        );
        assert_eq!(v["timestamp"], json!(tokens::TS));
        // cwd is Layer 2 — preserved in HostState mode.
        assert_eq!(v["cwd"], json!("/runners/cwd"));
    }

    #[test]
    fn normalize_ci_mode_rewrites_both_layers() {
        let mut v = json!({"timestamp": 100, "cwd": "/runners/cwd"});
        normalize(&mut v, &default_normalization_rules(), HarnessMode::Ci);
        assert_eq!(v["timestamp"], json!(tokens::TS));
        assert_eq!(v["cwd"], json!(tokens::CWD));
    }

    #[test]
    fn divergence_report_match_for_post_normalize_equal() {
        let invoker = MockNtmInvoker::new().with_response(
            "profile",
            "show",
            json!({"name": "default", "timestamp": 999}),
        );
        let harness = DifferentialHarness::new(
            "profile",
            "show",
            |_req: &Value| Ok(json!({"name": "default", "timestamp": 1})),
            &invoker,
        );
        let report = harness.compare(&json!({"name": "default"})).unwrap();
        assert!(
            report.is_match(),
            "post-normalize equality should match: {report:?}"
        );
    }

    #[test]
    fn divergence_report_diverge_on_real_field() {
        let invoker =
            MockNtmInvoker::new().with_response("profile", "show", json!({"name": "default-A"}));
        let harness = DifferentialHarness::new(
            "profile",
            "show",
            |_req: &Value| Ok(json!({"name": "default-B"})),
            &invoker,
        );
        let report = harness.compare(&json!({})).unwrap();
        assert!(!report.is_match());
        if let DivergenceReport::Diverge { explanation, .. } = report {
            assert!(
                explanation.contains("/name"),
                "explanation should point at /name: {explanation}"
            );
        } else {
            panic!("expected Diverge, got Match");
        }
    }

    #[test]
    fn first_divergence_reports_array_length_mismatch() {
        let a = json!([1, 2, 3]);
        let b = json!([1, 2]);
        let exp = first_divergence(&a, &b, "");
        assert!(exp.contains("array length differs"));
    }

    #[test]
    fn first_divergence_reports_missing_field_path() {
        let a = json!({"a": {"b": 1}});
        let b = json!({"a": {}});
        let exp = first_divergence(&a, &b, "");
        assert!(exp.contains("missing in ntm: /a/b"), "got: {exp}");
    }

    #[test]
    fn mock_invoker_returns_registered_response() {
        let invoker =
            MockNtmInvoker::new().with_response("checkpoint", "save", json!({"ok": true}));
        let resp = invoker.invoke("checkpoint", "save", &json!({})).unwrap();
        assert_eq!(resp, json!({"ok": true}));
    }

    #[test]
    fn mock_invoker_errors_on_unregistered() {
        let invoker = MockNtmInvoker::new();
        let err = invoker
            .invoke("checkpoint", "save", &json!({}))
            .unwrap_err();
        assert!(err.contains("no response registered"));
    }
}
