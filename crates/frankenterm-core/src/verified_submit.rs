use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::patterns::{AgentType, SubmitProfile};
use crate::robot_types::SubmitReceiptState;
use crate::wezterm::{MuxSemanticSnapshot, MuxSemanticZoneKind};

const CAPTURE_CURSOR_PREFIX: &str = "pane";
const MAX_CLASSIFIER_CAPTURE_BYTES: usize = 32 * 1024;

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
    &text[start..]
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
        return snapshot.zones[index + 1..].iter().any(|later| {
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

    let has_later_output = snapshot.zones[input_index + 1..].iter().any(|later| {
        later.semantic_type == MuxSemanticZoneKind::Output && !later.text.trim().is_empty()
    });
    !has_later_output
        && snapshot.zones[input_index]
            .text
            .to_ascii_lowercase()
            .contains(command_lower)
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
}
