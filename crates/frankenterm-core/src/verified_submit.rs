use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;

use crate::patterns::{AgentType, SubmitProfile};
use crate::policy::InjectionResult;
use crate::robot_types::{SubmitGuaranteeLevel, SubmitReceipt, SubmitReceiptState};
use crate::wezterm::{MuxSemanticSnapshot, MuxSemanticZoneKind};

const CAPTURE_CURSOR_PREFIX: &str = "pane";
const MAX_CLASSIFIER_CAPTURE_BYTES: usize = 32 * 1024;
const VERIFIED_SUBMIT_CANARY_PREFIX: &str = "\u{2063}ft-vs:";
const VERIFIED_SUBMIT_CANARY_DIGEST_CHARS: usize = 16;
const SUBMIT_IDEMPOTENCY_REQUEST_DOMAIN: &[u8] =
    b"frankenterm:verified-submit:semantic-request:v2\0";
const SUBMIT_IDEMPOTENCY_EFFECT_DOMAIN: &[u8] =
    b"frankenterm:verified-submit:exact-outbound-effect:v2\0";
const SUBMIT_IDEMPOTENCY_KEY_DOMAIN: &[u8] =
    b"frankenterm:verified-submit:caller-claim-key:v1\0";
const SUBMIT_IDEMPOTENCY_CANARY_DOMAIN: &[u8] =
    b"frankenterm:verified-submit:effect-canary:v2\0";

/// Wire-stable semantic contract included in every durable submit binding.
pub const SUBMIT_IDEMPOTENCY_SEMANTICS_VERSION: u16 = 2;

/// Complete semantic request used to derive a durable submit binding.
///
/// Every caller-controlled field that can change injection, verified-submit
/// classification, or requested wait behavior is bound. Regex mode and timeout
/// are intentionally canonicalized away when `wait_for` is absent. Profile
/// identity/version metadata is excluded, but the resolved canary-presence
/// decision is bound because it changes the exact pane effect; availability
/// drift therefore conflicts safely on the caller's stable claim row.
#[derive(Debug, Clone, Copy)]
pub struct SubmitIdempotencyRequest<'a> {
    pub pane_id: u64,
    pub text: &'a str,
    pub caller_key: &'a str,
    pub guarantee_level: SubmitGuaranteeLevel,
    /// Whether live pane/profile resolution selected a supported semantic
    /// canary for the exact effect. This is an effect-bearing decision and is
    /// therefore part of the durable request identity.
    pub append_verification_canary: bool,
    pub wait_for: Option<&'a str>,
    pub wait_for_regex: bool,
    pub timeout_secs: u64,
}

/// Full, domain-separated binding for one durable verified-submit claim.
///
/// `key` is the stable internal claim identifier derived only from the caller
/// nonce and pane namespace. `request_sha256` is stored independently so reuse
/// of that nonce with changed semantics becomes a conflict rather than a new
/// send. `effect_sha256` separates pane-effect identity from post-send
/// observation semantics. All digests retain all 256 bits.
#[derive(Clone, PartialEq, Eq)]
pub struct SubmitIdempotencyBinding {
    key: String,
    pane_id: u64,
    request_sha256: String,
    effect_sha256: String,
    caller_key: String,
    guarantee_level: SubmitGuaranteeLevel,
    verification_canary: Option<String>,
    // The MCP durability path moves a binding through claim, post-effect,
    // and completion blocking tasks. Keep the potentially multi-megabyte
    // exact payload shared so those authority transitions do not copy it.
    outbound_text: Arc<str>,
}

impl std::fmt::Debug for SubmitIdempotencyBinding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SubmitIdempotencyBinding")
            .field("key", &self.key)
            .field("pane_id", &self.pane_id)
            .field("request_sha256", &self.request_sha256)
            .field("effect_sha256", &self.effect_sha256)
            .field("caller_key", &"[REDACTED]")
            .field("guarantee_level", &self.guarantee_level)
            .field("verification_canary", &self.verification_canary)
            .field("outbound_text", &"[REDACTED]")
            .finish()
    }
}

impl SubmitIdempotencyBinding {
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    #[must_use]
    pub const fn pane_id(&self) -> u64 {
        self.pane_id
    }

    #[must_use]
    pub fn request_sha256(&self) -> &str {
        &self.request_sha256
    }

    /// Digest of the exact pane effect, intentionally independent of
    /// post-send observation knobs such as wait pattern and timeout.
    #[must_use]
    pub fn effect_sha256(&self) -> &str {
        &self.effect_sha256
    }

    /// Exact bounded caller nonce echoed in the public receipt. This is not
    /// the internal derived claim key and must never be logged by this type.
    #[must_use]
    pub fn caller_key(&self) -> &str {
        &self.caller_key
    }

    /// Resolved caller-selected guarantee bound into the semantic request.
    #[must_use]
    pub const fn guarantee_level(&self) -> SubmitGuaranteeLevel {
        self.guarantee_level
    }

    /// Semantic canary to append to the exact original text, when the bound
    /// verified-submit mode selected one.
    #[must_use]
    pub fn verification_canary(&self) -> Option<&str> {
        self.verification_canary.as_deref()
    }

    /// Exact outbound bytes whose digest is fenced by this binding.
    ///
    /// The binding owns these bytes so callers cannot accidentally pair a
    /// durable effect digest with a different arbitrary input string.
    #[must_use]
    pub fn outbound_text(&self) -> &str {
        self.outbound_text.as_ref()
    }

    /// Recompute the internal key from the exact caller nonce and validate all
    /// digest/canary shapes. The durable store calls this before every read or
    /// transition so a forged binding fails before filesystem access.
    #[must_use]
    pub fn is_canonical(&self) -> bool {
        is_lower_hex_sha256(&self.request_sha256)
            && is_lower_hex_sha256(&self.effect_sha256)
            && !self.caller_key.is_empty()
            && self.key == submit_key_from_caller_key(self.pane_id, &self.caller_key)
            && self.verification_canary.as_deref().is_none_or(|canary| {
                is_semantic_canary(canary)
                    && self.outbound_text.strip_suffix(canary).is_some_and(|original| {
                        semantic_canary_from_effect_inputs(self.pane_id, original, true) == canary
                    })
            })
            && self.effect_sha256
                == hex::encode(submit_effect_digest(self.pane_id, self.outbound_text.as_ref()))
    }
}

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

fn hash_len_prefixed(hasher: &mut Sha256, value: &str) {
    // Fixed-width u128 lengths make framing platform-independent and
    // collision-free even on a hypothetical target whose usize exceeds u64.
    hasher.update((value.len() as u128).to_be_bytes());
    hasher.update(value.as_bytes());
}

const fn guarantee_level_tag(level: SubmitGuaranteeLevel) -> u8 {
    match level {
        SubmitGuaranteeLevel::Write => 1,
        SubmitGuaranteeLevel::Composer => 2,
        SubmitGuaranteeLevel::Submitted => 3,
        SubmitGuaranteeLevel::Working => 4,
    }
}

fn submit_request_digest(request: SubmitIdempotencyRequest<'_>) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(SUBMIT_IDEMPOTENCY_REQUEST_DOMAIN);
    hasher.update(SUBMIT_IDEMPOTENCY_SEMANTICS_VERSION.to_be_bytes());
    hasher.update(request.pane_id.to_be_bytes());
    hash_len_prefixed(&mut hasher, request.text);
    hasher.update([guarantee_level_tag(request.guarantee_level)]);
    hasher.update([u8::from(should_append_semantic_canary(request))]);
    match request.wait_for {
        Some(pattern) => {
            hasher.update([1]);
            hash_len_prefixed(&mut hasher, pattern);
            hasher.update([u8::from(request.wait_for_regex)]);
            hasher.update(request.timeout_secs.to_be_bytes());
        }
        None => hasher.update([0]),
    }
    hasher.finalize().into()
}

fn submit_effect_digest(pane_id: u64, outbound_text: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(SUBMIT_IDEMPOTENCY_EFFECT_DOMAIN);
    hasher.update(SUBMIT_IDEMPOTENCY_SEMANTICS_VERSION.to_be_bytes());
    hasher.update(pane_id.to_be_bytes());
    hash_len_prefixed(&mut hasher, outbound_text);
    hasher.finalize().into()
}

fn submit_key_from_caller_key(pane_id: u64, caller_key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(SUBMIT_IDEMPOTENCY_KEY_DOMAIN);
    hasher.update(pane_id.to_be_bytes());
    hash_len_prefixed(&mut hasher, caller_key);
    format!("idem:{pane_id}:{}", hex::encode(hasher.finalize()))
}

fn semantic_canary_from_effect_inputs(
    pane_id: u64,
    original_text: &str,
    append_verification_canary: bool,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(SUBMIT_IDEMPOTENCY_CANARY_DOMAIN);
    hasher.update(SUBMIT_IDEMPOTENCY_SEMANTICS_VERSION.to_be_bytes());
    hasher.update(pane_id.to_be_bytes());
    hash_len_prefixed(&mut hasher, original_text);
    hasher.update([u8::from(append_verification_canary)]);
    let digest = hex::encode(hasher.finalize());
    format!(
        "{VERIFIED_SUBMIT_CANARY_PREFIX}{}",
        &digest[..VERIFIED_SUBMIT_CANARY_DIGEST_CHARS]
    )
}

fn should_append_semantic_canary(request: SubmitIdempotencyRequest<'_>) -> bool {
    request.append_verification_canary
        && request.guarantee_level.requires_submit_profile()
        && !request.text.trim().is_empty()
}

fn is_semantic_canary(value: &str) -> bool {
    value
        .strip_prefix(VERIFIED_SUBMIT_CANARY_PREFIX)
        .is_some_and(|digest| {
            digest.len() == VERIFIED_SUBMIT_CANARY_DIGEST_CHARS
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
}

fn is_lower_hex_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Derive the durable claim binding for a complete verified-submit semantic
/// request. Every variable field is fixed-width length-framed and optional
/// values have explicit presence tags.
#[must_use]
pub fn idempotency_binding(request: SubmitIdempotencyRequest<'_>) -> SubmitIdempotencyBinding {
    let request_sha256 = hex::encode(submit_request_digest(request));
    let key = submit_key_from_caller_key(request.pane_id, request.caller_key);
    let verification_canary = should_append_semantic_canary(request)
        .then(|| semantic_canary_from_effect_inputs(request.pane_id, request.text, true));
    let mut outbound_text = String::with_capacity(
        request.text.len()
            + verification_canary
                .as_ref()
                .map_or(0, std::string::String::len),
    );
    outbound_text.push_str(request.text);
    if let Some(canary) = verification_canary.as_deref() {
        outbound_text.push_str(canary);
    }
    let effect_sha256 = hex::encode(submit_effect_digest(request.pane_id, &outbound_text));
    SubmitIdempotencyBinding {
        key,
        pane_id: request.pane_id,
        request_sha256,
        effect_sha256,
        caller_key: request.caller_key.to_string(),
        guarantee_level: request.guarantee_level,
        verification_canary,
        outbound_text: outbound_text.into(),
    }
}

/// Derive the stable internal claim key for one caller nonce in one pane.
///
/// This value is store-internal. API clients must keep sending their original
/// caller nonce, which is echoed separately in `SubmitReceipt.idempotency_key`.
#[must_use]
pub fn idempotency_key(pane_id: u64, caller_key: &str) -> String {
    submit_key_from_caller_key(pane_id, caller_key)
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

    fn idempotency_request<'a>(
        pane_id: u64,
        text: &'a str,
        caller_key: &'a str,
    ) -> SubmitIdempotencyRequest<'a> {
        SubmitIdempotencyRequest {
            pane_id,
            text,
            caller_key,
            guarantee_level: SubmitGuaranteeLevel::Write,
            append_verification_canary: false,
            wait_for: None,
            wait_for_regex: false,
            timeout_secs: 30,
        }
    }

    #[test]
    fn idempotency_claim_key_is_stable_for_caller_nonce_and_pane() {
        let a = idempotency_key(7, "nonce");
        assert_eq!(a, idempotency_key(7, "nonce"), "stable for same inputs");
        assert_ne!(
            a,
            idempotency_key(8, "nonce"),
            "pane namespace must matter"
        );
        assert_ne!(
            a,
            idempotency_key(7, "nonce-2"),
            "caller key must matter"
        );
        assert!(a.starts_with("idem:7:"), "key was {a}");
        assert_eq!(
            a.rsplit(':').next().map(str::len),
            Some(64),
            "idempotency key must retain the full SHA-256 digest"
        );
        let binding = idempotency_binding(idempotency_request(7, "deploy now", "nonce"));
        assert_eq!(binding.key(), a);
        assert_eq!(binding.caller_key(), "nonce");
        assert_eq!(binding.request_sha256().len(), 64);
        assert_eq!(binding.effect_sha256().len(), 64);
        assert!(binding.is_canonical());
        assert_eq!(binding.verification_canary(), None);

        let changed_semantics = idempotency_binding(SubmitIdempotencyRequest {
            text: "deploy later",
            guarantee_level: SubmitGuaranteeLevel::Submitted,
            wait_for: Some("ready"),
            ..idempotency_request(7, "deploy now", "nonce")
        });
        assert_eq!(
            binding.key(),
            changed_semantics.key(),
            "semantic drift must conflict on the same caller claim row"
        );
        assert_ne!(binding.request_sha256(), changed_semantics.request_sha256());
    }

    #[test]
    fn idempotency_key_field_framing_is_unambiguous() {
        // Caller nonce bytes are length-framed, including embedded NULs.
        assert_ne!(
            idempotency_key(7, "foo\u{0}bar"),
            idempotency_key(7, "foo"),
            "embedded NUL must remain part of the caller nonce"
        );
        assert_ne!(
            idempotency_key(7, "foo\u{0}bar"),
            idempotency_key(7, "foo\u{0}bar\u{0}"),
            "trailing NUL must remain significant"
        );
    }

    #[test]
    fn idempotency_semantic_request_fields_never_alias() {
        let base = idempotency_request(7, "deploy now", "attempt-1");
        let base_binding = idempotency_binding(base);
        let variants = [
            SubmitIdempotencyRequest {
                text: "deploy later",
                ..base
            },
            SubmitIdempotencyRequest {
                guarantee_level: SubmitGuaranteeLevel::Submitted,
                ..base
            },
            SubmitIdempotencyRequest {
                guarantee_level: SubmitGuaranteeLevel::Submitted,
                append_verification_canary: true,
                ..base
            },
            SubmitIdempotencyRequest {
                wait_for: Some("ready"),
                ..base
            },
            SubmitIdempotencyRequest {
                wait_for: Some("ready"),
                wait_for_regex: true,
                ..base
            },
            SubmitIdempotencyRequest {
                wait_for: Some("ready"),
                timeout_secs: 31,
                ..base
            },
        ];
        for variant in variants {
            assert_ne!(
                base_binding.request_sha256(),
                idempotency_binding(variant).request_sha256()
            );
        }

        let ineffective_write_canary = idempotency_binding(SubmitIdempotencyRequest {
            append_verification_canary: true,
            ..base
        });
        assert_eq!(
            base_binding.request_sha256(),
            ineffective_write_canary.request_sha256(),
            "the canary flag must canonicalize away when the Write guarantee cannot append one"
        );

        let irrelevant_wait_knobs = idempotency_binding(SubmitIdempotencyRequest {
            wait_for_regex: true,
            timeout_secs: u64::MAX,
            ..base
        });
        assert_eq!(
            base_binding.request_sha256(),
            irrelevant_wait_knobs.request_sha256(),
            "regex mode and timeout are irrelevant without a wait pattern"
        );

        let different_nonce = idempotency_binding(SubmitIdempotencyRequest {
            caller_key: "attempt-2",
            ..base
        });
        assert_eq!(base_binding.request_sha256(), different_nonce.request_sha256());
        assert_ne!(base_binding.key(), different_nonce.key());
    }

    #[test]
    fn bound_verification_canary_is_stable_for_the_exact_effect() {
        let request = SubmitIdempotencyRequest {
            guarantee_level: SubmitGuaranteeLevel::Submitted,
            append_verification_canary: true,
            ..idempotency_request(7, "deploy now", "attempt-1")
        };
        let binding = idempotency_binding(request);
        let same = idempotency_binding(request);
        assert_eq!(binding.verification_canary(), same.verification_canary());
        assert_eq!(binding.outbound_text(), same.outbound_text());
        assert!(binding.outbound_text().starts_with(request.text));

        let changed_wait = idempotency_binding(SubmitIdempotencyRequest {
            wait_for: Some("ready"),
            ..request
        });
        assert_eq!(binding.key(), changed_wait.key());
        assert_ne!(binding.request_sha256(), changed_wait.request_sha256());
        assert_eq!(binding.effect_sha256(), changed_wait.effect_sha256());
        assert_eq!(binding.verification_canary(), changed_wait.verification_canary());

        let changed_nonce = idempotency_binding(SubmitIdempotencyRequest {
            caller_key: "attempt-2",
            ..request
        });
        assert_eq!(binding.effect_sha256(), changed_nonce.effect_sha256());
        assert_eq!(binding.outbound_text(), changed_nonce.outbound_text());

        let working = idempotency_binding(SubmitIdempotencyRequest {
            guarantee_level: SubmitGuaranteeLevel::Working,
            ..request
        });
        assert_ne!(binding.request_sha256(), working.request_sha256());
        assert_eq!(binding.effect_sha256(), working.effect_sha256());
        assert_eq!(binding.outbound_text(), working.outbound_text());

        let write = idempotency_binding(SubmitIdempotencyRequest {
            guarantee_level: SubmitGuaranteeLevel::Write,
            append_verification_canary: true,
            ..request
        });
        assert_eq!(write.verification_canary(), None);
        assert_eq!(write.outbound_text(), request.text);
        assert_ne!(binding.effect_sha256(), write.effect_sha256());
        assert_ne!(binding.outbound_text(), write.outbound_text());

        let blank = idempotency_binding(SubmitIdempotencyRequest {
            guarantee_level: SubmitGuaranteeLevel::Submitted,
            append_verification_canary: true,
            ..idempotency_request(7, "   ", "attempt-blank")
        });
        assert_eq!(blank.verification_canary(), None);
        assert_eq!(blank.outbound_text(), "   ");
    }

    #[test]
    fn supported_profile_decision_is_bound_and_unsupported_effect_is_unmodified() {
        let unsupported = idempotency_binding(SubmitIdempotencyRequest {
            guarantee_level: SubmitGuaranteeLevel::Submitted,
            append_verification_canary: false,
            ..idempotency_request(7, "deploy now", "profile-drift")
        });
        assert_eq!(unsupported.verification_canary(), None);
        assert_eq!(unsupported.outbound_text(), "deploy now");

        let supported = idempotency_binding(SubmitIdempotencyRequest {
            guarantee_level: SubmitGuaranteeLevel::Submitted,
            append_verification_canary: true,
            ..idempotency_request(7, "deploy now", "profile-drift")
        });
        assert_eq!(supported.key(), unsupported.key());
        assert_ne!(supported.request_sha256(), unsupported.request_sha256());
        assert_ne!(supported.effect_sha256(), unsupported.effect_sha256());
        assert!(supported.verification_canary().is_some());
        assert!(supported.outbound_text().starts_with("deploy now"));
        assert_ne!(supported.outbound_text(), unsupported.outbound_text());
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
