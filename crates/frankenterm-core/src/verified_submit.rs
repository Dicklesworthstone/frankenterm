use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::patterns::{AgentType, SubmitProfile};
use crate::policy::InjectionResult;
use crate::robot_types::{SubmitGuaranteeLevel, SubmitReceipt, SubmitReceiptState};
use crate::wezterm::{MuxSemanticSnapshot, MuxSemanticZoneKind};

const CAPTURE_CURSOR_PREFIX: &str = "pane";
const MAX_CLASSIFIER_CAPTURE_BYTES: usize = 32 * 1024;
const VERIFIED_SUBMIT_CANARY_PREFIX: &str = "\u{2063}ft-vs:";
const VERIFIED_SUBMIT_CANARY_DIGEST_CHARS: usize = 16;

/// Inputs sampled around a policy-approved send operation.
#[derive(Debug, Clone, Copy)]
pub struct VerifiedSubmitInput<'a> {
    pub pane_id: u64,
    pub command_text: &'a str,
    pub agent_type: AgentType,
    pub profile: Option<&'a SubmitProfile>,
    pub before_text: Option<&'a str>,
    pub after_text: Option<&'a str>,
    pub after_semantic_snapshot: Option<&'a MuxSemanticSnapshot>,
    pub attempts: u32,
    pub polls: usize,
}

/// Classification result recorded into a durable submit receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifiedSubmitReport {
    pub state: SubmitReceiptState,
    pub agent_type: Option<String>,
    pub profile_id: Option<String>,
    pub profile_version: Option<String>,
    pub attempts: u32,
    pub evidence_rule_ids: Vec<String>,
    pub polls: usize,
    pub cursor_before: Option<String>,
    pub cursor_after: Option<String>,
}

#[must_use]
pub fn submit_receipt_state(
    injection: &InjectionResult,
    verification_report: Option<&VerifiedSubmitReport>,
) -> SubmitReceiptState {
    match injection {
        InjectionResult::Allowed { .. } => {
            verification_report.map_or(SubmitReceiptState::Submitted, |report| report.state)
        }
        InjectionResult::Denied { .. } => SubmitReceiptState::PolicyDenied,
        InjectionResult::RequiresApproval { .. } => SubmitReceiptState::RequiresApproval,
        InjectionResult::Error { .. } => SubmitReceiptState::SendFailed,
    }
}

#[must_use]
pub fn submit_receipt_evidence_rule_ids(
    injection: &InjectionResult,
    verification_report: Option<&VerifiedSubmitReport>,
) -> Vec<String> {
    let rule_id = match injection {
        InjectionResult::Allowed { decision, .. }
        | InjectionResult::Denied { decision, .. }
        | InjectionResult::RequiresApproval { decision, .. }
        | InjectionResult::Error { decision, .. } => decision.rule_id(),
    };
    let mut evidence_rule_ids: Vec<String> = rule_id.map(str::to_string).into_iter().collect();
    if let Some(report) = verification_report {
        evidence_rule_ids.extend(report.evidence_rule_ids.iter().cloned());
    }
    evidence_rule_ids.sort();
    evidence_rule_ids.dedup();
    evidence_rule_ids
}

#[must_use]
pub fn build_submit_receipt(
    pane_id: u64,
    original_text: &str,
    injection: &InjectionResult,
    verification_report: Option<&VerifiedSubmitReport>,
    elapsed_ms: u64,
) -> SubmitReceipt {
    build_submit_receipt_with_guarantee(
        pane_id,
        original_text,
        injection,
        verification_report,
        elapsed_ms,
        SubmitGuaranteeLevel::Submitted,
    )
}

#[must_use]
pub fn build_submit_receipt_with_guarantee(
    pane_id: u64,
    original_text: &str,
    injection: &InjectionResult,
    verification_report: Option<&VerifiedSubmitReport>,
    elapsed_ms: u64,
    guarantee_level: SubmitGuaranteeLevel,
) -> SubmitReceipt {
    let state = submit_receipt_state(injection, verification_report);
    let evidence_rule_ids = submit_receipt_evidence_rule_ids(injection, verification_report);
    let guarantee_met = guarantee_level.is_met_by(state, &evidence_rule_ids);
    SubmitReceipt {
        state,
        guarantee_level,
        guarantee_met,
        agent_type: verification_report.and_then(|report| report.agent_type.clone()),
        profile_id: verification_report.and_then(|report| report.profile_id.clone()),
        profile_version: verification_report.and_then(|report| report.profile_version.clone()),
        attempts: verification_report.map_or(1, |report| report.attempts),
        evidence_rule_ids,
        elapsed_ms,
        polls: verification_report.map_or(0, |report| report.polls),
        cursor_before: verification_report.and_then(|report| report.cursor_before.clone()),
        cursor_after: verification_report.and_then(|report| report.cursor_after.clone()),
        idempotency_key: crate::robot_idempotency::send_text_key(pane_id, original_text)
            .to_string(),
    }
}

#[must_use]
pub fn submit_guarantee_failure_message(receipt: &SubmitReceipt) -> Option<String> {
    (!receipt.guarantee_met).then(|| {
        format!(
            "submit guarantee '{}' not met: state={}",
            receipt.guarantee_level.as_str(),
            receipt.state.as_str()
        )
    })
}

/// Classify the post-send terminal state using a data-driven submit profile.
#[must_use]
pub fn classify_verified_submit(input: VerifiedSubmitInput<'_>) -> VerifiedSubmitReport {
    let cursor_before = input
        .before_text
        .map(|text| capture_cursor(input.pane_id, text));
    let cursor_after = input
        .after_text
        .map(|text| capture_cursor(input.pane_id, text));
    let attempts = input.attempts.max(1);
    let agent_type = receipt_agent_type(input.agent_type);

    let Some(profile) = input.profile else {
        return VerifiedSubmitReport {
            state: SubmitReceiptState::VerificationUnavailable,
            agent_type,
            profile_id: None,
            profile_version: None,
            attempts,
            evidence_rule_ids: vec!["submit_profile:unavailable".to_string()],
            polls: input.polls,
            cursor_before,
            cursor_after,
        };
    };

    if profile.agent_type != input.agent_type {
        return VerifiedSubmitReport {
            state: SubmitReceiptState::VerificationUnavailable,
            agent_type,
            profile_id: Some(profile.id.clone()),
            profile_version: Some(profile.version.clone()),
            attempts,
            evidence_rule_ids: vec![format!("submit_profile:{}:agent_type_mismatch", profile.id)],
            polls: input.polls,
            cursor_before,
            cursor_after,
        };
    }

    let Some(after_text) = input.after_text else {
        return unavailable_with_profile(
            profile,
            agent_type,
            attempts,
            input.polls,
            cursor_before,
            cursor_after,
            "capture_unavailable",
        );
    };

    let after_tail = capture_tail(after_text).to_ascii_lowercase();
    let command_lower = input.command_text.trim().to_ascii_lowercase();
    let verification_canary =
        extract_verification_canary(input.command_text).map(str::to_ascii_lowercase);

    if let Some(evidence_id) = first_anchor_match(
        profile,
        "crash_to_shell",
        &profile.anchors.crash_to_shell,
        &after_tail,
    ) {
        return report_with_profile(
            SubmitReceiptState::PaneCrashedToShell,
            profile,
            attempts,
            input.polls,
            cursor_before,
            cursor_after,
            vec![evidence_id],
        );
    }

    if let Some(evidence_id) = first_anchor_match(
        profile,
        "queued_behind_operation",
        &profile.anchors.queued_behind_operation,
        &after_tail,
    ) {
        return report_with_profile(
            SubmitReceiptState::QueuedBehindOperation,
            profile,
            attempts,
            input.polls,
            cursor_before,
            cursor_after,
            vec![evidence_id],
        );
    }

    if let Some(canary) = verification_canary.as_deref() {
        return match semantic_canary_status(input.after_semantic_snapshot, canary) {
            CanarySemanticStatus::Submitted => report_with_profile(
                SubmitReceiptState::Submitted,
                profile,
                attempts,
                input.polls,
                cursor_before,
                cursor_after,
                vec![format!(
                    "submit_profile:{}:canary_semantic_output_after_input",
                    profile.id
                )],
            ),
            CanarySemanticStatus::StuckInComposer => report_with_profile(
                SubmitReceiptState::StuckInComposer,
                profile,
                attempts,
                input.polls,
                cursor_before,
                cursor_after,
                vec![format!(
                    "submit_profile:{}:canary_semantic_input_without_output",
                    profile.id
                )],
            ),
            CanarySemanticStatus::Missing => unavailable_with_profile(
                profile,
                agent_type,
                attempts,
                input.polls,
                cursor_before,
                cursor_after,
                "canary_semantic_unavailable",
            ),
        };
    }

    if semantic_has_output_after_matching_input(input.after_semantic_snapshot, &command_lower) {
        return report_with_profile(
            SubmitReceiptState::Submitted,
            profile,
            attempts,
            input.polls,
            cursor_before,
            cursor_after,
            vec![format!(
                "submit_profile:{}:semantic_output_after_input",
                profile.id
            )],
        );
    }

    if let Some(evidence_id) = first_anchor_match(
        profile,
        "composer_cleared",
        &profile.anchors.composer_cleared,
        &after_tail,
    )
    .or_else(|| {
        first_anchor_match(
            profile,
            "working_state",
            &profile.anchors.working_state,
            &after_tail,
        )
    }) {
        return report_with_profile(
            SubmitReceiptState::Submitted,
            profile,
            attempts,
            input.polls,
            cursor_before,
            cursor_after,
            vec![evidence_id],
        );
    }

    if semantic_latest_input_contains_command(input.after_semantic_snapshot, &command_lower) {
        return report_with_profile(
            SubmitReceiptState::StuckInComposer,
            profile,
            attempts,
            input.polls,
            cursor_before,
            cursor_after,
            vec![format!(
                "submit_profile:{}:semantic_input_contains_command",
                profile.id
            )],
        );
    }

    if let Some(evidence_id) = first_anchor_match(
        profile,
        "composer_nonempty",
        &profile.anchors.composer_nonempty,
        &after_tail,
    ) {
        if !command_lower.is_empty() && after_tail.contains(&command_lower) {
            return report_with_profile(
                SubmitReceiptState::StuckInComposer,
                profile,
                attempts,
                input.polls,
                cursor_before,
                cursor_after,
                vec![evidence_id],
            );
        }
    }

    if capture_delta_submitted(input.before_text, input.after_text, &command_lower) {
        return report_with_profile(
            SubmitReceiptState::Submitted,
            profile,
            attempts,
            input.polls,
            cursor_before,
            cursor_after,
            vec![format!("submit_profile:{}:capture_delta", profile.id)],
        );
    }

    unavailable_with_profile(
        profile,
        agent_type,
        attempts,
        input.polls,
        cursor_before,
        cursor_after,
        "insufficient_evidence",
    )
}

#[must_use]
pub fn capture_cursor(pane_id: u64, text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    let digest = hex::encode(digest);
    format!(
        "{CAPTURE_CURSOR_PREFIX}:{pane_id}:capture:sha256:{}",
        &digest[..16]
    )
}

#[must_use]
pub fn append_verification_canary(pane_id: u64, command_text: &str) -> String {
    let canary = verification_canary(pane_id, command_text);
    let mut marked = String::with_capacity(command_text.len() + canary.len());
    marked.push_str(command_text);
    marked.push_str(&canary);
    marked
}

#[must_use]
pub fn verification_canary(pane_id: u64, command_text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"verified-submit-canary-v1");
    hasher.update(pane_id.to_le_bytes());
    hasher.update(command_text.len().to_string().as_bytes());
    hasher.update(b":");
    hasher.update(command_text.as_bytes());
    let digest = hex::encode(hasher.finalize());
    format!(
        "{VERIFIED_SUBMIT_CANARY_PREFIX}{}",
        &digest[..VERIFIED_SUBMIT_CANARY_DIGEST_CHARS]
    )
}

/// ft-7h5da.3.5: derive the idempotency key for a verified-submit send. The same
/// `(pane_id, text, caller_key)` triple always maps to the same key; a different
/// caller-supplied key (or different text / pane) yields a distinct key, letting
/// a meta-agent force a genuinely new send. `caller_key` is an opaque token the
/// caller controls (e.g. a retry / session nonce); `None` keys purely by content.
#[must_use]
pub fn idempotency_key(pane_id: u64, text: &str, caller_key: Option<&str>) -> String {
    // Length-prefix every variable-length field and tag the Option so field
    // boundaries are unambiguous. Without framing, ("a\x00b", None) vs
    // ("a", Some("b\x00")) — or a None vs Some("") caller key — would hash to
    // the same key, and a *different* prompt could then be suppressed as a
    // duplicate (a silent prompt drop). Decimal lengths keep the encoding
    // platform-independent (matching the canonical-string pattern in
    // `steering.rs`).
    let mut hasher = Sha256::new();
    hasher.update(pane_id.to_le_bytes());
    hasher.update(text.len().to_string().as_bytes());
    hasher.update(b":");
    hasher.update(text.as_bytes());
    match caller_key {
        Some(k) => {
            hasher.update(b"some:");
            hasher.update(k.len().to_string().as_bytes());
            hasher.update(b":");
            hasher.update(k.as_bytes());
        }
        None => hasher.update(b"none"),
    }
    let digest = hex::encode(hasher.finalize());
    format!("idem:{pane_id}:{}", &digest[..16])
}

/// ft-7h5da.3.5: whether a fresh send keyed by an idempotency key should be
/// suppressed as a duplicate of a prior in-flight / completed send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdempotencyOutcome {
    /// A prior send for this key is already `submitted` or
    /// `queued_behind_operation` — the replay is a NO-OP and the caller MUST
    /// return the original receipt rather than re-injecting the prompt.
    DuplicateNoop,
    /// No conflicting prior send (never sent, or the prior attempt did not stick)
    /// — the send proceeds.
    Proceed,
}

/// ft-7h5da.3.5: decide a send's idempotency outcome from the last recorded
/// receipt state for its key.
///
/// Only `Submitted` / `QueuedBehindOperation` suppress a replay — the prompt is
/// already delivered or in flight, so re-sending would double-submit. Every
/// other state (including `None` = never sent, and the non-delivered terminals
/// `StuckInComposer` / `SendFailed` / `PaneCrashedToShell` /
/// `VerificationUnavailable` / `PolicyDenied` / `RequiresApproval`) allows the
/// send to proceed, because the prompt was NOT durably delivered and a
/// disconnected meta-agent must be able to retry.
#[must_use]
pub fn idempotency_outcome(prior: Option<SubmitReceiptState>) -> IdempotencyOutcome {
    match prior {
        Some(SubmitReceiptState::Submitted | SubmitReceiptState::QueuedBehindOperation) => {
            IdempotencyOutcome::DuplicateNoop
        }
        _ => IdempotencyOutcome::Proceed,
    }
}

fn receipt_agent_type(agent_type: AgentType) -> Option<String> {
    match agent_type {
        AgentType::Codex | AgentType::ClaudeCode | AgentType::Gemini => {
            Some(agent_type.to_string())
        }
        AgentType::Wezterm | AgentType::Unknown => None,
    }
}

fn report_with_profile(
    state: SubmitReceiptState,
    profile: &SubmitProfile,
    attempts: u32,
    polls: usize,
    cursor_before: Option<String>,
    cursor_after: Option<String>,
    evidence_rule_ids: Vec<String>,
) -> VerifiedSubmitReport {
    VerifiedSubmitReport {
        state,
        agent_type: Some(profile.agent_type.to_string()),
        profile_id: Some(profile.id.clone()),
        profile_version: Some(profile.version.clone()),
        attempts,
        evidence_rule_ids,
        polls,
        cursor_before,
        cursor_after,
    }
}

fn unavailable_with_profile(
    profile: &SubmitProfile,
    agent_type: Option<String>,
    attempts: u32,
    polls: usize,
    cursor_before: Option<String>,
    cursor_after: Option<String>,
    reason: &str,
) -> VerifiedSubmitReport {
    VerifiedSubmitReport {
        state: SubmitReceiptState::VerificationUnavailable,
        agent_type,
        profile_id: Some(profile.id.clone()),
        profile_version: Some(profile.version.clone()),
        attempts,
        evidence_rule_ids: vec![format!("submit_profile:{}:{reason}", profile.id)],
        polls,
        cursor_before,
        cursor_after,
    }
}

fn first_anchor_match(
    profile: &SubmitProfile,
    group: &str,
    anchors: &[String],
    after_tail_lower: &str,
) -> Option<String> {
    anchors
        .iter()
        .enumerate()
        .find(|(_, anchor)| {
            let anchor = anchor.trim();
            !anchor.is_empty() && after_tail_lower.contains(&anchor.to_ascii_lowercase())
        })
        .map(|(index, _)| format!("submit_profile:{}:{group}:{index}", profile.id))
}

fn capture_tail(text: &str) -> &str {
    if text.len() <= MAX_CLASSIFIER_CAPTURE_BYTES {
        return text;
    }

    let mut start = text.len() - MAX_CLASSIFIER_CAPTURE_BYTES;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text.get(start..).unwrap_or_default()
}

fn extract_verification_canary(command_text: &str) -> Option<&str> {
    let marker_start = command_text.rfind(VERIFIED_SUBMIT_CANARY_PREFIX)?;
    let marker_end =
        marker_start + VERIFIED_SUBMIT_CANARY_PREFIX.len() + VERIFIED_SUBMIT_CANARY_DIGEST_CHARS;
    if marker_end != command_text.len() {
        return None;
    }
    let marker = command_text.get(marker_start..marker_end)?;
    let digest = marker.get(VERIFIED_SUBMIT_CANARY_PREFIX.len()..)?;
    digest
        .chars()
        .all(|ch| ch.is_ascii_hexdigit())
        .then_some(marker)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CanarySemanticStatus {
    Submitted,
    StuckInComposer,
    Missing,
}

fn semantic_canary_status(
    snapshot: Option<&MuxSemanticSnapshot>,
    canary_lower: &str,
) -> CanarySemanticStatus {
    let Some(snapshot) = snapshot else {
        return CanarySemanticStatus::Missing;
    };
    if canary_lower.is_empty() {
        return CanarySemanticStatus::Missing;
    }

    if let Some(input_index) = snapshot.zones.iter().rposition(|zone| {
        zone.semantic_type == MuxSemanticZoneKind::Input
            && zone.text.to_ascii_lowercase().contains(canary_lower)
    }) {
        let tail_start = input_index.saturating_add(1);
        let has_later_output =
            snapshot
                .zones
                .get(tail_start..)
                .unwrap_or(&[])
                .iter()
                .any(|later| {
                    later.semantic_type == MuxSemanticZoneKind::Output
                        && !later.text.trim().is_empty()
                });
        return if has_later_output {
            CanarySemanticStatus::Submitted
        } else {
            CanarySemanticStatus::StuckInComposer
        };
    }

    if snapshot.zones.iter().any(|zone| {
        zone.semantic_type == MuxSemanticZoneKind::Output
            && zone.text.to_ascii_lowercase().contains(canary_lower)
    }) {
        CanarySemanticStatus::Submitted
    } else {
        CanarySemanticStatus::Missing
    }
}

fn semantic_has_output_after_matching_input(
    snapshot: Option<&MuxSemanticSnapshot>,
    command_lower: &str,
) -> bool {
    let Some(snapshot) = snapshot else {
        return false;
    };
    if command_lower.is_empty() {
        return false;
    }

    for (index, zone) in snapshot.zones.iter().enumerate().rev() {
        if zone.semantic_type != MuxSemanticZoneKind::Input {
            continue;
        }
        if !zone.text.to_ascii_lowercase().contains(command_lower) {
            continue;
        }
        let tail_start = index.saturating_add(1);
        return snapshot
            .zones
            .get(tail_start..)
            .unwrap_or(&[])
            .iter()
            .any(|later| {
                later.semantic_type == MuxSemanticZoneKind::Output && !later.text.trim().is_empty()
            });
    }

    false
}

fn semantic_latest_input_contains_command(
    snapshot: Option<&MuxSemanticSnapshot>,
    command_lower: &str,
) -> bool {
    let Some(snapshot) = snapshot else {
        return false;
    };
    if command_lower.is_empty() {
        return false;
    }

    let Some(input_index) = snapshot
        .zones
        .iter()
        .rposition(|zone| zone.semantic_type == MuxSemanticZoneKind::Input)
    else {
        return false;
    };

    let Some(input_zone) = snapshot.zones.get(input_index) else {
        return false;
    };

    let tail_start = input_index.saturating_add(1);
    let has_later_output = snapshot
        .zones
        .get(tail_start..)
        .unwrap_or(&[])
        .iter()
        .any(|later| {
            later.semantic_type == MuxSemanticZoneKind::Output && !later.text.trim().is_empty()
        });
    !has_later_output && input_zone.text.to_ascii_lowercase().contains(command_lower)
}

fn capture_delta_submitted(
    before_text: Option<&str>,
    after_text: Option<&str>,
    command_lower: &str,
) -> bool {
    let (Some(before_text), Some(after_text)) = (before_text, after_text) else {
        return false;
    };
    if before_text == after_text {
        return false;
    }

    if command_lower.is_empty() {
        return false;
    }

    !capture_tail(after_text)
        .to_ascii_lowercase()
        .contains(command_lower)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patterns::{SubmitProfileAnchors, SubmitProfileRemediation};
    use crate::wezterm::{MuxSemanticZone, MuxSemanticZoneKind};

    fn profile() -> SubmitProfile {
        SubmitProfile {
            id: "codex.default".to_string(),
            agent_type: AgentType::Codex,
            version: "2026-06-08".to_string(),
            anchors: SubmitProfileAnchors {
                composer_nonempty: vec!["Press Enter to send".to_string()],
                composer_cleared: vec!["Thinking".to_string()],
                working_state: vec!["Running".to_string()],
                queued_behind_operation: vec!["operation in progress".to_string()],
                crash_to_shell: vec!["panicked at".to_string()],
            },
            remediation: Vec::<SubmitProfileRemediation>::new(),
        }
    }

    fn input<'a>(
        profile: Option<&'a SubmitProfile>,
        after_text: Option<&'a str>,
    ) -> VerifiedSubmitInput<'a> {
        VerifiedSubmitInput {
            pane_id: 7,
            command_text: "run tests",
            agent_type: AgentType::Codex,
            profile,
            before_text: Some("Press Enter to send\n"),
            after_text,
            after_semantic_snapshot: None,
            attempts: 1,
            polls: 1,
        }
    }

    fn zone(kind: MuxSemanticZoneKind, y: isize, text: &str) -> MuxSemanticZone {
        MuxSemanticZone {
            start_y: y,
            start_x: 0,
            end_y: y,
            end_x: text.chars().count(),
            semantic_type: kind,
            text: text.to_string(),
        }
    }

    #[test]
    fn unknown_profile_is_fail_open_unavailable() {
        let report = classify_verified_submit(input(None, Some("anything")));

        assert_eq!(report.state, SubmitReceiptState::VerificationUnavailable);
        assert_eq!(report.agent_type.as_deref(), Some("codex"));
        assert_eq!(report.profile_id, None);
        assert_eq!(report.evidence_rule_ids, vec!["submit_profile:unavailable"]);
    }

    #[test]
    fn queued_state_wins_before_retryable_states() {
        let profile = profile();
        let report = classify_verified_submit(input(
            Some(&profile),
            Some("operation in progress\nPress Enter to send\nrun tests"),
        ));

        assert_eq!(report.state, SubmitReceiptState::QueuedBehindOperation);
        assert_eq!(
            report.evidence_rule_ids,
            vec!["submit_profile:codex.default:queued_behind_operation:0"]
        );
    }

    #[test]
    fn crash_state_wins_before_queued_state() {
        let profile = profile();
        let report = classify_verified_submit(input(
            Some(&profile),
            Some("panicked at terminal.rs\noperation in progress"),
        ));

        assert_eq!(report.state, SubmitReceiptState::PaneCrashedToShell);
        assert_eq!(
            report.evidence_rule_ids,
            vec!["submit_profile:codex.default:crash_to_shell:0"]
        );
    }

    #[test]
    fn submitted_state_uses_cleared_or_working_anchors() {
        let profile = profile();
        let report = classify_verified_submit(input(Some(&profile), Some("Thinking\n")));

        assert_eq!(report.state, SubmitReceiptState::Submitted);
        assert_eq!(
            report.evidence_rule_ids,
            vec!["submit_profile:codex.default:composer_cleared:0"]
        );
    }

    #[test]
    fn semantic_output_after_matching_input_is_submitted_even_if_text_remains_in_transcript() {
        let profile = profile();
        let snapshot = MuxSemanticSnapshot {
            zones: vec![
                MuxSemanticZone {
                    start_y: 0,
                    start_x: 0,
                    end_y: 0,
                    end_x: 9,
                    semantic_type: MuxSemanticZoneKind::Input,
                    text: "run tests".to_string(),
                },
                MuxSemanticZone {
                    start_y: 1,
                    start_x: 0,
                    end_y: 1,
                    end_x: 7,
                    semantic_type: MuxSemanticZoneKind::Output,
                    text: "Running".to_string(),
                },
            ],
            last_exit_code: None,
        };

        let report = classify_verified_submit(VerifiedSubmitInput {
            after_semantic_snapshot: Some(&snapshot),
            after_text: Some("run tests\nRunning"),
            ..input(Some(&profile), Some("run tests\nRunning"))
        });

        assert_eq!(report.state, SubmitReceiptState::Submitted);
        assert_eq!(
            report.evidence_rule_ids,
            vec!["submit_profile:codex.default:semantic_output_after_input"]
        );
    }

    #[test]
    fn semantic_latest_input_without_output_is_stuck_in_composer() {
        let profile = profile();
        let snapshot = MuxSemanticSnapshot {
            zones: vec![MuxSemanticZone {
                start_y: 0,
                start_x: 0,
                end_y: 0,
                end_x: 9,
                semantic_type: MuxSemanticZoneKind::Input,
                text: "run tests".to_string(),
            }],
            last_exit_code: None,
        };

        let report = classify_verified_submit(VerifiedSubmitInput {
            after_semantic_snapshot: Some(&snapshot),
            after_text: Some("run tests"),
            ..input(Some(&profile), Some("run tests"))
        });

        assert_eq!(report.state, SubmitReceiptState::StuckInComposer);
        assert_eq!(
            report.evidence_rule_ids,
            vec!["submit_profile:codex.default:semantic_input_contains_command"]
        );
    }

    #[test]
    fn verification_canary_is_invisible_stable_and_discriminating() {
        let marked = append_verification_canary(7, "run tests");
        let canary = verification_canary(7, "run tests");

        assert_eq!(
            marked,
            format!("run tests{canary}"),
            "marker must append at payload end"
        );
        assert!(canary.starts_with("\u{2063}ft-vs:"));
        assert_eq!(canary, verification_canary(7, "run tests"));
        assert_ne!(canary, verification_canary(8, "run tests"));
        assert_ne!(canary, verification_canary(7, "run something else"));
    }

    #[test]
    fn canary_semantic_output_after_input_is_submitted() {
        let profile = profile();
        let marked = append_verification_canary(7, "run tests");
        let snapshot = MuxSemanticSnapshot {
            zones: vec![
                zone(MuxSemanticZoneKind::Input, 0, &marked),
                zone(MuxSemanticZoneKind::Output, 1, "Running"),
            ],
            last_exit_code: None,
        };

        let report = classify_verified_submit(VerifiedSubmitInput {
            command_text: &marked,
            after_semantic_snapshot: Some(&snapshot),
            after_text: Some("run tests\nRunning"),
            ..input(Some(&profile), Some("run tests\nRunning"))
        });

        assert_eq!(report.state, SubmitReceiptState::Submitted);
        assert_eq!(
            report.evidence_rule_ids,
            vec!["submit_profile:codex.default:canary_semantic_output_after_input"]
        );
    }

    #[test]
    fn canary_latest_input_without_output_stays_stuck_despite_text_anchors() {
        let profile = profile();
        let marked = append_verification_canary(7, "run tests");
        let snapshot = MuxSemanticSnapshot {
            zones: vec![zone(MuxSemanticZoneKind::Input, 0, &marked)],
            last_exit_code: None,
        };

        let report = classify_verified_submit(VerifiedSubmitInput {
            command_text: &marked,
            after_semantic_snapshot: Some(&snapshot),
            after_text: Some("Thinking\n"),
            ..input(Some(&profile), Some("Thinking\n"))
        });

        assert_eq!(report.state, SubmitReceiptState::StuckInComposer);
        assert_eq!(
            report.evidence_rule_ids,
            vec!["submit_profile:codex.default:canary_semantic_input_without_output"]
        );
    }

    #[test]
    fn canary_missing_semantic_evidence_does_not_fall_back_to_text_submission() {
        let profile = profile();
        let marked = append_verification_canary(7, "run tests");

        let report = classify_verified_submit(VerifiedSubmitInput {
            command_text: &marked,
            after_text: Some("Thinking\n"),
            after_semantic_snapshot: None,
            ..input(Some(&profile), Some("Thinking\n"))
        });

        assert_eq!(report.state, SubmitReceiptState::VerificationUnavailable);
        assert_eq!(
            report.evidence_rule_ids,
            vec!["submit_profile:codex.default:canary_semantic_unavailable"]
        );
    }

    #[test]
    fn capture_delta_without_command_echo_is_submitted() {
        let profile = profile();
        let report = classify_verified_submit(input(Some(&profile), Some("done\n")));

        assert_eq!(report.state, SubmitReceiptState::Submitted);
        assert_eq!(
            report.evidence_rule_ids,
            vec!["submit_profile:codex.default:capture_delta"]
        );
        assert!(report.cursor_before.is_some());
        assert!(report.cursor_after.is_some());
    }

    #[test]
    fn blank_command_capture_delta_is_verification_unavailable() {
        let profile = profile();
        let report = classify_verified_submit(VerifiedSubmitInput {
            pane_id: 7,
            command_text: "   ",
            agent_type: AgentType::Codex,
            profile: Some(&profile),
            before_text: Some("Press Enter to send\n"),
            after_text: Some("background output changed\n"),
            after_semantic_snapshot: None,
            attempts: 1,
            polls: 1,
        });

        assert_eq!(report.state, SubmitReceiptState::VerificationUnavailable);
        assert_eq!(
            report.evidence_rule_ids,
            vec!["submit_profile:codex.default:insufficient_evidence"]
        );
    }

    #[test]
    fn missing_capture_is_unavailable_with_profile_metadata() {
        let profile = profile();
        let report = classify_verified_submit(input(Some(&profile), None));

        assert_eq!(report.state, SubmitReceiptState::VerificationUnavailable);
        assert_eq!(report.profile_id.as_deref(), Some("codex.default"));
        assert_eq!(report.profile_version.as_deref(), Some("2026-06-08"));
        assert_eq!(
            report.evidence_rule_ids,
            vec!["submit_profile:codex.default:capture_unavailable"]
        );
    }

    // ft-7h5da.3.5: idempotency key + duplicate protection.

    #[test]
    fn idempotency_key_is_stable_and_discriminating() {
        let a = idempotency_key(7, "deploy now", None);
        assert_eq!(
            a,
            idempotency_key(7, "deploy now", None),
            "stable for same inputs"
        );
        assert_ne!(
            a,
            idempotency_key(7, "deploy later", None),
            "text must matter"
        );
        assert_ne!(
            a,
            idempotency_key(8, "deploy now", None),
            "pane must matter"
        );
        assert_ne!(
            a,
            idempotency_key(7, "deploy now", Some("nonce")),
            "caller key must matter"
        );
        assert!(a.starts_with("idem:7:"), "key was {a}");
    }

    #[test]
    fn idempotency_key_field_framing_is_unambiguous() {
        // Field boundaries must be length-framed: a NUL embedded in the text
        // must not collide with the same bytes split across (text, caller_key),
        // and an absent caller key must differ from an empty one. A collision
        // here would suppress a *different* prompt as a duplicate.
        assert_ne!(
            idempotency_key(7, "foo\u{0}bar", None),
            idempotency_key(7, "foo", Some("bar\u{0}")),
            "embedded NUL must not shift the field boundary"
        );
        assert_ne!(
            idempotency_key(7, "foo\u{0}", Some("bar")),
            idempotency_key(7, "foo", Some("\u{0}bar")),
            "bytes must not migrate between text and caller_key"
        );
        assert_ne!(
            idempotency_key(7, "deploy now", None),
            idempotency_key(7, "deploy now", Some("")),
            "absent caller key must differ from empty caller key"
        );
    }

    #[test]
    fn submitted_and_queued_suppress_replay() {
        assert_eq!(
            idempotency_outcome(Some(SubmitReceiptState::Submitted)),
            IdempotencyOutcome::DuplicateNoop
        );
        assert_eq!(
            idempotency_outcome(Some(SubmitReceiptState::QueuedBehindOperation)),
            IdempotencyOutcome::DuplicateNoop
        );
    }

    #[test]
    fn never_sent_or_non_delivered_states_proceed() {
        assert_eq!(idempotency_outcome(None), IdempotencyOutcome::Proceed);
        let retryable = [
            SubmitReceiptState::StuckInComposer,
            SubmitReceiptState::SendFailed,
            SubmitReceiptState::PaneCrashedToShell,
            SubmitReceiptState::VerificationUnavailable,
            SubmitReceiptState::PolicyDenied,
            SubmitReceiptState::RequiresApproval,
        ];
        for state in retryable {
            let label = format!("{state:?}");
            assert_eq!(
                idempotency_outcome(Some(state)),
                IdempotencyOutcome::Proceed,
                "{label} was not durably delivered and must allow retry"
            );
        }
    }

    #[test]
    fn submit_guarantee_matrix_pins_terminal_state_semantics() {
        use SubmitGuaranteeLevel::{Composer, Submitted, Working, Write};

        let no_evidence = Vec::new();
        let working_evidence = vec!["submit_profile:codex.default:working_state:0".to_string()];
        let semantic_evidence =
            vec!["submit_profile:codex.default:semantic_output_after_input".to_string()];

        let cases = [
            (Write, SubmitReceiptState::Submitted, &no_evidence, true),
            (
                Write,
                SubmitReceiptState::QueuedBehindOperation,
                &no_evidence,
                true,
            ),
            (
                Write,
                SubmitReceiptState::StuckInComposer,
                &no_evidence,
                true,
            ),
            (
                Write,
                SubmitReceiptState::PaneCrashedToShell,
                &no_evidence,
                false,
            ),
            (
                Composer,
                SubmitReceiptState::StuckInComposer,
                &no_evidence,
                true,
            ),
            (
                Composer,
                SubmitReceiptState::VerificationUnavailable,
                &no_evidence,
                false,
            ),
            (
                Submitted,
                SubmitReceiptState::QueuedBehindOperation,
                &no_evidence,
                true,
            ),
            (
                Submitted,
                SubmitReceiptState::StuckInComposer,
                &no_evidence,
                false,
            ),
            (
                Working,
                SubmitReceiptState::Submitted,
                &working_evidence,
                true,
            ),
            (
                Working,
                SubmitReceiptState::Submitted,
                &semantic_evidence,
                true,
            ),
            (Working, SubmitReceiptState::Submitted, &no_evidence, false),
            (
                Working,
                SubmitReceiptState::QueuedBehindOperation,
                &working_evidence,
                false,
            ),
        ];

        for (level, state, evidence, expected) in cases {
            assert_eq!(
                level.is_met_by(state, evidence),
                expected,
                "level={} state={}",
                level.as_str(),
                state.as_str()
            );
        }
    }
}
