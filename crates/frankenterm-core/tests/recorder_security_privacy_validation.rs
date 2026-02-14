//! Security/privacy validation suite for recorder query and audit paths.
//!
//! Bead: wa-oegrb.7.6
//! - synthetic sensitive corpus coverage
//! - authz + redaction enforcement checks
//! - tamper-evident audit integrity checks
//! - leak-regression alarms for sensitive text exposure

use frankenterm_core::policy::{ActorKind, Redactor};
use frankenterm_core::recorder_audit::{
    AccessTier, ActorIdentity, AuditEventBuilder, AuditEventType, AuditLog, AuditLogConfig,
    AuthzDecision, GENESIS_HASH, RecorderAuditEntry, check_authorization, required_tier_for_event,
};

struct SensitiveSample {
    label: &'static str,
    raw: &'static str,
    forbidden_fragments: &'static [&'static str],
}

const SENSITIVE_CORPUS: &[SensitiveSample] = &[
    SensitiveSample {
        label: "openai_key",
        raw: "OPENAI_API_KEY=sk-abc123456789012345678901234567890123456789012345678901",
        forbidden_fragments: &["sk-abc123456789012345678901234567890123456789012345678901"],
    },
    SensitiveSample {
        label: "anthropic_key",
        raw: "ANTHROPIC_API_KEY=sk-ant-api03-abcdefghijklmnopqrstuvwxyz12345678901234567890",
        forbidden_fragments: &["sk-ant-api03-abcdefghijklmnopqrstuvwxyz12345678901234567890"],
    },
    SensitiveSample {
        label: "github_token",
        raw: "GITHUB_TOKEN=ghp_abcdefghijklmnopqrstuvwxyz1234567890",
        forbidden_fragments: &["ghp_abcdefghijklmnopqrstuvwxyz1234567890"],
    },
    SensitiveSample {
        label: "aws_secret",
        raw: "aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        forbidden_fragments: &["wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"],
    },
    SensitiveSample {
        label: "database_url_password",
        raw: "DATABASE_URL=postgres://user:supersecretpassword@localhost:5432/ft",
        forbidden_fragments: &["postgres://user:supersecretpassword@localhost:5432/ft"],
    },
    SensitiveSample {
        label: "generic_password",
        raw: "customer_email=alice@example.com password: mysecretpassword123",
        forbidden_fragments: &["mysecretpassword123"],
    },
];

fn contains_pii_like_markers(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("@example.com")
        || lower.contains("ssn:")
        || lower.contains("social_security")
        || lower.contains("phone:")
}

fn collect_leak_regressions(entries: &[RecorderAuditEntry]) -> Vec<String> {
    let redactor = Redactor::new();
    let mut leaks = Vec::new();

    for entry in entries {
        if let Some(query) = &entry.scope.query {
            if redactor.contains_secrets(query) || contains_pii_like_markers(query) {
                leaks.push(format!("ordinal={} query={query}", entry.ordinal));
            }
        }

        if let Some(details) = &entry.details {
            let serialized = details.to_string();
            if redactor.contains_secrets(&serialized) || contains_pii_like_markers(&serialized) {
                leaks.push(format!("ordinal={} details={serialized}", entry.ordinal));
            }
        }
    }

    leaks
}

#[test]
fn synthetic_sensitive_corpus_redacts_expected_patterns() {
    let redactor = Redactor::new();

    for sample in SENSITIVE_CORPUS {
        assert!(
            redactor.contains_secrets(sample.raw),
            "corpus sample {} should be detected as sensitive",
            sample.label
        );

        let redacted = redactor.redact(sample.raw);
        assert!(
            redacted.contains("[REDACTED]"),
            "corpus sample {} should include [REDACTED] marker",
            sample.label
        );

        for fragment in sample.forbidden_fragments {
            assert!(
                !redacted.contains(fragment),
                "corpus sample {} leaked fragment {}",
                sample.label,
                fragment
            );
        }
    }
}

#[test]
fn authorization_redaction_and_audit_integrity_for_sensitive_access() {
    let audit_log = AuditLog::new(AuditLogConfig::default());
    let now = 1_700_000_000_000u64;
    let redactor = Redactor::new();

    // A1 query for robot should be allowed.
    let robot_query_tier = required_tier_for_event(AuditEventType::RecorderQuery);
    assert_eq!(robot_query_tier, AccessTier::A1RedactedQuery);
    let robot_query_decision = check_authorization(ActorKind::Robot, robot_query_tier);
    assert_eq!(robot_query_decision, AuthzDecision::Allow);
    audit_log.append(
        AuditEventBuilder::new(
            AuditEventType::RecorderQuery,
            ActorIdentity::new(ActorKind::Robot, "robot-1"),
            now,
        )
        .with_decision(robot_query_decision)
        .with_query(redactor.redact("token: robot_sensitive_token_12345678"))
        .with_result_count(6),
    );

    // A3 query for robot should be denied.
    let privileged_tier = required_tier_for_event(AuditEventType::RecorderQueryPrivileged);
    assert_eq!(privileged_tier, AccessTier::A3PrivilegedRaw);
    let robot_privileged_decision = check_authorization(ActorKind::Robot, privileged_tier);
    assert_eq!(robot_privileged_decision, AuthzDecision::Deny);
    audit_log.append(
        AuditEventBuilder::new(
            AuditEventType::RecorderQueryPrivileged,
            ActorIdentity::new(ActorKind::Robot, "robot-1"),
            now + 1000,
        )
        .with_decision(robot_privileged_decision)
        .with_query(redactor.redact("password: blocked_robot_password_123")),
    );

    // Human privileged access requires elevation and explicit approval.
    let human_privileged_decision = check_authorization(ActorKind::Human, privileged_tier);
    assert_eq!(human_privileged_decision, AuthzDecision::Elevate);
    audit_log.append(
        AuditEventBuilder::new(
            AuditEventType::AccessApprovalGranted,
            ActorIdentity::new(ActorKind::Human, "operator-1"),
            now + 2000,
        )
        .with_decision(AuthzDecision::Allow)
        .with_justification("INC-9001 forensic review"),
    );
    audit_log.append(
        AuditEventBuilder::new(
            AuditEventType::RecorderQueryPrivileged,
            ActorIdentity::new(ActorKind::Human, "operator-1"),
            now + 3000,
        )
        .with_decision(AuthzDecision::Allow)
        .with_query(redactor.redact("DATABASE_URL=postgres://admin:hunter2@db/ft"))
        .with_result_count(1)
        .with_justification("INC-9001 approved raw query"),
    );

    let stats = audit_log.stats();
    assert_eq!(stats.total_entries, 4);
    assert_eq!(stats.denied_count, 1);
    assert_eq!(stats.by_actor.get("robot"), Some(&2));
    assert_eq!(stats.by_actor.get("human"), Some(&2));

    let chain = AuditLog::verify_chain(&audit_log.entries(), GENESIS_HASH);
    assert!(chain.chain_intact);
    assert!(chain.missing_ordinals.is_empty());

    let leaks = collect_leak_regressions(&audit_log.entries());
    assert!(leaks.is_empty(), "unexpected leak regressions: {leaks:#?}");

    let privileged_queries = audit_log.entries_by_type(AuditEventType::RecorderQueryPrivileged);
    assert_eq!(privileged_queries.len(), 2);
    let human_privileged_entry = privileged_queries
        .iter()
        .find(|entry| entry.actor.kind == ActorKind::Human)
        .expect("human privileged query entry missing");
    assert!(
        human_privileged_entry.justification.is_some(),
        "human privileged query must carry justification"
    );
}

#[test]
fn leak_regression_alarm_and_hash_chain_tamper_detection() {
    let audit_log = AuditLog::new(AuditLogConfig::default());
    let redactor = Redactor::new();

    for i in 0..3u64 {
        audit_log.append(
            AuditEventBuilder::new(
                AuditEventType::RecorderQuery,
                ActorIdentity::new(ActorKind::Workflow, "wf-validation"),
                1_700_000_100_000 + (i * 1000),
            )
            .with_decision(AuthzDecision::Allow)
            .with_query(redactor.redact("api_key = abcdef1234567890abcdef1234567890"))
            .with_result_count(2),
        );
    }

    let clean_entries = audit_log.entries();
    assert!(
        collect_leak_regressions(&clean_entries).is_empty(),
        "clean entries should not trigger leak alarm"
    );
    assert!(AuditLog::verify_chain(&clean_entries, GENESIS_HASH).chain_intact);

    // Simulate tampering by injecting unredacted sensitive text.
    let mut tampered = clean_entries.clone();
    tampered[1].scope.query =
        Some("customer_email=alice@example.com password: leaked_password_123".to_string());

    let leak_regressions = collect_leak_regressions(&tampered);
    assert!(
        !leak_regressions.is_empty(),
        "tampered entries should trigger leak regression alarms"
    );

    let chain = AuditLog::verify_chain(&tampered, GENESIS_HASH);
    assert!(!chain.chain_intact, "tampering should break hash chain");
    assert!(
        chain.first_break_at.is_some(),
        "tampering should report first break ordinal"
    );
}
