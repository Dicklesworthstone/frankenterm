use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use crate::patterns::{AgentType, SubmitProfile};
use crate::policy::InjectionResult;
use crate::robot_types::{SubmitGuaranteeLevel, SubmitReceipt, SubmitReceiptState};
use crate::wezterm::{MuxSemanticSnapshot, MuxSemanticZoneKind};

const CAPTURE_CURSOR_PREFIX: &str = "pane";
const MAX_CLASSIFIER_CAPTURE_BYTES: usize = 32 * 1024;
const MAX_CLASSIFIER_CAPTURE_LINES: usize = 200;
const MAX_CLASSIFIER_SEMANTIC_ZONES: usize = 256;
const MAX_BASELINE_PROFILE_IDENTITY_BYTES: usize = 256;
const MAX_BASELINE_ANCHORS_PER_GROUP: usize = 256;
const VERIFIED_SUBMIT_BASELINE_VERSION: u16 = 1;
const VERIFIED_SUBMIT_PROFILE_DOMAIN: &[u8] = b"frankenterm:verified-submit:profile:v1\0";
const SUBMIT_IDEMPOTENCY_REQUEST_DOMAIN: &[u8] =
    b"frankenterm:verified-submit:semantic-request:v2\0";
const SUBMIT_IDEMPOTENCY_EFFECT_DOMAIN: &[u8] =
    b"frankenterm:verified-submit:exact-outbound-effect:v2\0";
const SUBMIT_IDEMPOTENCY_KEY_DOMAIN: &[u8] = b"frankenterm:verified-submit:caller-claim-key:v1\0";

/// Wire-stable semantic contract included in every durable submit binding.
/// v3 dropped the verification-canary flag from the request digest (ft-07fx2).
pub const SUBMIT_IDEMPOTENCY_SEMANTICS_VERSION: u16 = 3;

/// Complete semantic request used to derive a durable submit binding.
///
/// Every caller-controlled field that can change injection, verified-submit
/// classification, or requested wait behavior is bound. Regex mode and timeout
/// are intentionally canonicalized away when `wait_for` is absent. Profile
/// identity/version metadata is excluded. The pane effect is always the exact
/// caller text.
#[derive(Debug, Clone, Copy)]
pub struct SubmitIdempotencyRequest<'a> {
    pub pane_id: u64,
    pub text: &'a str,
    pub caller_key: &'a str,
    pub guarantee_level: SubmitGuaranteeLevel,
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

    /// Exact outbound bytes whose digest is fenced by this binding.
    ///
    /// The binding owns these bytes so callers cannot accidentally pair a
    /// durable effect digest with a different arbitrary input string.
    #[must_use]
    pub fn outbound_text(&self) -> &str {
        self.outbound_text.as_ref()
    }

    /// Recompute the internal key from the exact caller nonce and validate all
    /// digest shapes. The durable store calls this before every read or
    /// transition so a forged binding fails before filesystem access.
    #[must_use]
    pub fn is_canonical(&self) -> bool {
        is_lower_hex_sha256(&self.request_sha256)
            && is_lower_hex_sha256(&self.effect_sha256)
            && !self.caller_key.is_empty()
            && self.key == submit_key_from_caller_key(self.pane_id, &self.caller_key)
            && self.effect_sha256
                == hex::encode(submit_effect_digest(
                    self.pane_id,
                    self.outbound_text.as_ref(),
                ))
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
    pub before_semantic_snapshot: Option<&'a MuxSemanticSnapshot>,
    pub after_text: Option<&'a str>,
    pub after_semantic_snapshot: Option<&'a MuxSemanticSnapshot>,
    pub attempts: u32,
    pub polls: usize,
}

/// Bounded evidence from one side of a send. Keep the pre-send observation
/// unchanged throughout polling; a later poll cannot become the baseline for
/// deciding whether an old input or status marker belongs to this send.
#[derive(Debug, Default)]
pub struct VerifiedSubmitObservation {
    pub text: Option<String>,
    pub semantic_snapshot: Option<MuxSemanticSnapshot>,
}

/// Content-free pre-send evidence that may be retained across process death.
///
/// Only bounded metadata, full SHA-256 digests, coordinates, and predicates
/// survive serialization. Neither the command nor pane/semantic-zone text is
/// stored. Recovery still requires the exact command and unchanged profile;
/// an absent pre-send observation never becomes an empty successful baseline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct VerifiedSubmitBaseline(VerifiedSubmitBaselineData);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VerifiedSubmitBaselineData {
    version: u16,
    pane_id: u64,
    effect_sha256: String,
    agent_type: AgentType,
    #[serde(deserialize_with = "Option::deserialize")]
    profile: Option<VerifiedSubmitProfileIdentity>,
    #[serde(deserialize_with = "Option::deserialize")]
    cursor_before: Option<String>,
    #[serde(deserialize_with = "Option::deserialize")]
    fresh_anchors: Option<VerifiedSubmitFreshAnchors>,
    #[serde(deserialize_with = "Option::deserialize")]
    composer_before: Option<bool>,
    #[serde(deserialize_with = "Option::deserialize")]
    semantic_zones: Option<Vec<VerifiedSubmitSemanticZone>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VerifiedSubmitProfileIdentity {
    id: String,
    version: String,
    sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VerifiedSubmitFreshAnchors {
    crash_to_shell: Vec<bool>,
    queued_behind_operation: Vec<bool>,
    working_state: Vec<bool>,
    composer_cleared: Vec<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VerifiedSubmitSemanticZone {
    start_y: isize,
    start_x: usize,
    end_y: isize,
    end_x: usize,
    semantic_type: MuxSemanticZoneKind,
    text_sha256: String,
    text_bytes: usize,
    contains_command: bool,
}

impl<'de> Deserialize<'de> for VerifiedSubmitBaseline {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let baseline = Self(VerifiedSubmitBaselineData::deserialize(deserializer)?);
        if baseline.is_bounded() {
            Ok(baseline)
        } else {
            Err(serde::de::Error::custom(
                "verified-submit baseline is malformed or exceeds evidence bounds",
            ))
        }
    }
}

impl VerifiedSubmitBaseline {
    /// Fence the stored evidence to the exact pane effect owned by a claim.
    #[must_use]
    pub fn matches_binding(&self, binding: &SubmitIdempotencyBinding) -> bool {
        self.is_bounded()
            && binding.is_canonical()
            && self.0.pane_id == binding.pane_id()
            && self.0.effect_sha256 == binding.effect_sha256()
    }

    fn is_bounded(&self) -> bool {
        let evidence = &self.0;
        if evidence.version != VERIFIED_SUBMIT_BASELINE_VERSION
            || !is_lower_hex_sha256(&evidence.effect_sha256)
            || evidence.profile.as_ref().is_some_and(|profile| {
                profile.id.len() > MAX_BASELINE_PROFILE_IDENTITY_BYTES
                    || profile.version.len() > MAX_BASELINE_PROFILE_IDENTITY_BYTES
                    || !is_lower_hex_sha256(&profile.sha256)
            })
            || evidence.fresh_anchors.as_ref().is_some_and(|anchors| {
                [
                    &anchors.crash_to_shell,
                    &anchors.queued_behind_operation,
                    &anchors.working_state,
                    &anchors.composer_cleared,
                ]
                .iter()
                .any(|group| group.len() > MAX_BASELINE_ANCHORS_PER_GROUP)
            })
            || evidence.fresh_anchors.is_some() != evidence.composer_before.is_some()
            || evidence.fresh_anchors.is_some()
                != (evidence.cursor_before.is_some() && evidence.profile.is_some())
        {
            return false;
        }
        if let Some(cursor) = &evidence.cursor_before {
            let prefix = format!(
                "{CAPTURE_CURSOR_PREFIX}:{}:capture:sha256:",
                evidence.pane_id
            );
            if !cursor.strip_prefix(&prefix).is_some_and(|digest| {
                digest.len() == 16
                    && digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            }) {
                return false;
            }
        }
        let Some(zones) = &evidence.semantic_zones else {
            return true;
        };
        if zones.len() > MAX_CLASSIFIER_SEMANTIC_ZONES {
            return false;
        }
        let mut remaining = MAX_CLASSIFIER_CAPTURE_BYTES;
        let mut previous_end = None;
        for zone in zones {
            let start = (zone.start_y, zone.start_x);
            let end = (zone.end_y, zone.end_x);
            if start > end
                || previous_end.is_some_and(|previous_end| previous_end > start)
                || !is_lower_hex_sha256(&zone.text_sha256)
            {
                return false;
            }
            let Some(next_remaining) = remaining.checked_sub(zone.text_bytes) else {
                return false;
            };
            remaining = next_remaining;
            previous_end = Some(end);
        }
        true
    }

    fn matches_input(&self, input: VerifiedSubmitInput<'_>) -> bool {
        self.is_bounded()
            && self.0.pane_id == input.pane_id
            && self.0.agent_type == input.agent_type
            && self.0.effect_sha256
                == hex::encode(submit_effect_digest(input.pane_id, input.command_text))
            && match (&self.0.profile, input.profile) {
                (None, None) => true,
                (Some(identity), Some(profile)) => {
                    identity.id == profile.id
                        && identity.version == profile.version
                        && identity.sha256 == submit_profile_fingerprint(profile)
                        && self.0.fresh_anchors.as_ref().is_none_or(|anchors| {
                            anchors.crash_to_shell.len() == profile.anchors.crash_to_shell.len()
                                && anchors.queued_behind_operation.len()
                                    == profile.anchors.queued_behind_operation.len()
                                && anchors.working_state.len()
                                    == profile.anchors.working_state.len()
                                && anchors.composer_cleared.len()
                                    == profile.anchors.composer_cleared.len()
                        })
                }
                _ => false,
            }
    }
}

/// Prepare bounded, serializable evidence before any pane effect is attempted.
/// `None` means that a durable recovery baseline cannot safely be retained.
#[must_use]
pub fn prepare_verified_submit_baseline(
    pane_id: u64,
    command_text: &str,
    agent_type: AgentType,
    profile: Option<&SubmitProfile>,
    before_text: Option<&str>,
    before_semantic_snapshot: Option<&MuxSemanticSnapshot>,
) -> Option<VerifiedSubmitBaseline> {
    if command_text.len() > MAX_CLASSIFIER_CAPTURE_BYTES
        || profile.is_some_and(|profile| {
            profile.id.len() > MAX_BASELINE_PROFILE_IDENTITY_BYTES
                || profile.version.len() > MAX_BASELINE_PROFILE_IDENTITY_BYTES
                || [
                    &profile.anchors.composer_nonempty,
                    &profile.anchors.composer_cleared,
                    &profile.anchors.working_state,
                    &profile.anchors.queued_behind_operation,
                    &profile.anchors.crash_to_shell,
                ]
                .iter()
                .any(|group| group.len() > MAX_BASELINE_ANCHORS_PER_GROUP)
        })
    {
        return None;
    }
    Some(build_verified_submit_baseline(
        pane_id,
        command_text,
        agent_type,
        profile,
        before_text,
        before_semantic_snapshot,
    ))
}

fn build_verified_submit_baseline(
    pane_id: u64,
    command_text: &str,
    agent_type: AgentType,
    profile: Option<&SubmitProfile>,
    before_text: Option<&str>,
    before_semantic_snapshot: Option<&MuxSemanticSnapshot>,
) -> VerifiedSubmitBaseline {
    let command_lower = if command_text.len() <= MAX_CLASSIFIER_CAPTURE_BYTES {
        command_text.trim().to_ascii_lowercase()
    } else {
        String::new()
    };
    let before_tail = before_text.map(|text| capture_tail(text).to_ascii_lowercase());
    let before_status = before_tail
        .as_ref()
        .map(|before| before.replace(command_lower.as_str(), "\0"));
    let fresh_anchors = profile
        .zip(before_status.as_deref())
        .map(|(profile, before)| {
            let eligible = |group: &[String]| {
                group
                    .iter()
                    .map(|anchor| {
                        let anchor = anchor.trim().to_ascii_lowercase();
                        !anchor.is_empty()
                            && !command_lower.contains(&anchor)
                            && !before.contains(&anchor)
                    })
                    .collect()
            };
            VerifiedSubmitFreshAnchors {
                crash_to_shell: eligible(&profile.anchors.crash_to_shell),
                queued_behind_operation: eligible(&profile.anchors.queued_behind_operation),
                working_state: eligible(&profile.anchors.working_state),
                composer_cleared: eligible(&profile.anchors.composer_cleared),
            }
        });
    let composer_before = profile
        .zip(before_tail.as_deref())
        .map(|(profile, before)| {
            composer_anchor_holds_command(
                profile,
                &profile.anchors.composer_nonempty,
                before,
                &command_lower,
            )
            .is_some()
        });
    VerifiedSubmitBaseline(VerifiedSubmitBaselineData {
        version: VERIFIED_SUBMIT_BASELINE_VERSION,
        pane_id,
        effect_sha256: hex::encode(submit_effect_digest(pane_id, command_text)),
        agent_type,
        profile: profile.map(|profile| VerifiedSubmitProfileIdentity {
            id: profile.id.clone(),
            version: profile.version.clone(),
            sha256: submit_profile_fingerprint(profile),
        }),
        cursor_before: before_text.map(|text| capture_cursor(pane_id, text)),
        fresh_anchors,
        composer_before,
        semantic_zones: before_semantic_snapshot
            .filter(|snapshot| bounded_semantic_snapshot(snapshot))
            .map(|snapshot| semantic_zone_fingerprints(snapshot, command_text.trim())),
    })
}

/// Capture bounded live evidence using the caller's exact cancellation scope.
/// Missing or oversized semantic evidence remains unavailable: truncating it
/// could turn an old input into an apparently new turn.
pub async fn capture_verified_submit_observation(
    cx: &crate::cx::Cx,
    client: &crate::wezterm::WeztermHandle,
    pane_id: u64,
) -> VerifiedSubmitObservation {
    if cx.checkpoint().is_err() {
        return VerifiedSubmitObservation::default();
    }
    let text = match client
        .get_text_tail_with_cx(cx, pane_id, false, Some(MAX_CLASSIFIER_CAPTURE_LINES))
        .await
    {
        Ok(capture) => Some(capture_tail(&capture.text).to_string()),
        Err(error) => {
            tracing::debug!(pane_id, %error, "verified-submit text capture unavailable");
            None
        }
    };
    if cx.checkpoint().is_err() {
        return VerifiedSubmitObservation::default();
    }
    let semantic_snapshot = match client.get_semantic_zones_with_cx(cx, pane_id).await {
        Ok(snapshot) if bounded_semantic_snapshot(&snapshot) => Some(snapshot),
        Ok(_) => {
            tracing::debug!(
                pane_id,
                "verified-submit semantic capture exceeds evidence bounds"
            );
            None
        }
        Err(error) => {
            tracing::debug!(pane_id, %error, "verified-submit semantic capture unavailable");
            None
        }
    };
    if cx.checkpoint().is_err() {
        return VerifiedSubmitObservation::default();
    }
    VerifiedSubmitObservation {
        text,
        semantic_snapshot,
    }
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

/// Fresh receipts built in this process, by agent type and state (W2.8).
/// Agent types come from the built-in submit profiles, so the key set stays
/// small; a receipt without one counts as `unknown`.
static SUBMIT_RECEIPT_COUNTS: Mutex<BTreeMap<(String, &'static str), u64>> =
    Mutex::new(BTreeMap::new());

/// Distinct (agent type, state) keys kept before new agent types fold into
/// `other`.
const SUBMIT_RECEIPT_COUNT_MAX_KEYS: usize = 64;

/// One process-wide verified-submit receipt counter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitReceiptCount {
    pub agent_type: String,
    pub state: &'static str,
    pub count: u64,
}

/// Count a freshly built receipt and emit its structured tracing event.
///
/// Call once per send, after the final receipt is built; receipts replayed
/// from the idempotency store are not new sends. The event carries ids,
/// state, timing, and the guarantee outcome, never the sent text or the
/// caller's idempotency key.
pub fn record_submit_receipt(pane_id: u64, receipt: &SubmitReceipt) {
    let agent_type = receipt.agent_type.as_deref().unwrap_or("unknown");
    let state = receipt.state.as_str();
    {
        let mut counts = SUBMIT_RECEIPT_COUNTS
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut key = (agent_type.to_string(), state);
        if counts.len() >= SUBMIT_RECEIPT_COUNT_MAX_KEYS && !counts.contains_key(&key) {
            key.0 = "other".to_string();
        }
        let count = counts.entry(key).or_insert(0);
        *count = count.saturating_add(1);
    }
    tracing::info!(
        target: "frankenterm::verified_submit",
        pane_id,
        agent_type,
        profile_id = receipt.profile_id.as_deref().unwrap_or(""),
        profile_version = receipt.profile_version.as_deref().unwrap_or(""),
        state,
        guarantee_level = receipt.guarantee_level.as_str(),
        guarantee_met = receipt.guarantee_met,
        elapsed_ms = receipt.elapsed_ms,
        polls = receipt.polls,
        attempts = receipt.attempts,
        "verified submit receipt"
    );
}

/// Snapshot of the process-wide receipt counters, ordered by agent type and
/// state.
#[must_use]
pub fn submit_receipt_counts() -> Vec<SubmitReceiptCount> {
    SUBMIT_RECEIPT_COUNTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .map(|((agent_type, state), count)| SubmitReceiptCount {
            agent_type: agent_type.clone(),
            state,
            count: *count,
        })
        .collect()
}

/// First capture delay after the send, re-capture interval, and poll budget
/// (~5 s) for [`classify_verified_submit_polled`].
pub const VERIFY_SUBMIT_FIRST_CAPTURE_MS: u64 = 120;
pub const VERIFY_SUBMIT_POLL_MS: u64 = 400;
pub const VERIFY_SUBMIT_MAX_POLLS: usize = 12;

/// Capture the pane after a send and classify it, re-capturing while the
/// verdict is `stuck_in_composer` or `verification_unavailable`.
///
/// An agent echoes the prompt into its transcript before its turn indicator
/// appears, so a single capture right after the send reads a submitted prompt
/// as stuck (seen against real Claude Code 2.1). `polls` is the caller's count,
/// which already includes the first capture; each re-capture adds one.
#[allow(clippy::too_many_arguments)]
pub async fn classify_verified_submit_polled(
    cx: &crate::cx::Cx,
    client: &crate::wezterm::WeztermHandle,
    pane_id: u64,
    command_text: &str,
    agent_type: AgentType,
    profile: Option<&SubmitProfile>,
    before_text: Option<&str>,
    before_semantic_snapshot: Option<&MuxSemanticSnapshot>,
    attempts: u32,
    polls: usize,
) -> VerifiedSubmitReport {
    let baseline = build_verified_submit_baseline(
        pane_id,
        command_text,
        agent_type,
        profile,
        before_text,
        before_semantic_snapshot,
    );
    classify_verified_submit_polled_with_baseline(
        cx,
        client,
        pane_id,
        command_text,
        agent_type,
        profile,
        &baseline,
        attempts,
        polls,
    )
    .await
}

/// Resume observation after a durable send without reissuing any input.
/// The frozen pre-send evidence remains the baseline for every resumed poll.
#[allow(clippy::too_many_arguments)]
pub async fn classify_verified_submit_polled_from_baseline(
    cx: &crate::cx::Cx,
    client: &crate::wezterm::WeztermHandle,
    pane_id: u64,
    command_text: &str,
    agent_type: AgentType,
    profile: Option<&SubmitProfile>,
    baseline: &VerifiedSubmitBaseline,
    attempts: u32,
    polls: usize,
) -> VerifiedSubmitReport {
    let input = VerifiedSubmitInput {
        pane_id,
        command_text,
        agent_type,
        profile,
        before_text: None,
        before_semantic_snapshot: None,
        after_text: None,
        after_semantic_snapshot: None,
        attempts,
        polls,
    };
    if !baseline.matches_input(input) {
        return baseline_mismatch_report(input);
    }
    classify_verified_submit_polled_with_baseline(
        cx,
        client,
        pane_id,
        command_text,
        agent_type,
        profile,
        baseline,
        attempts,
        polls,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn classify_verified_submit_polled_with_baseline(
    cx: &crate::cx::Cx,
    client: &crate::wezterm::WeztermHandle,
    pane_id: u64,
    command_text: &str,
    agent_type: AgentType,
    profile: Option<&SubmitProfile>,
    baseline: &VerifiedSubmitBaseline,
    attempts: u32,
    polls: usize,
) -> VerifiedSubmitReport {
    if command_text.len() > MAX_CLASSIFIER_CAPTURE_BYTES {
        return classify_verified_submit_with_baseline(
            baseline,
            VerifiedSubmitInput {
                pane_id,
                command_text,
                agent_type,
                profile,
                before_text: None,
                before_semantic_snapshot: None,
                after_text: None,
                after_semantic_snapshot: None,
                attempts,
                polls: 0,
            },
        );
    }
    let mut captures = 0usize;
    loop {
        captures += 1;
        let after = if profile.is_some() {
            let delay_ms = if captures == 1 {
                VERIFY_SUBMIT_FIRST_CAPTURE_MS
            } else {
                VERIFY_SUBMIT_POLL_MS
            };
            if crate::runtime_async::sleep_with_cx(cx, std::time::Duration::from_millis(delay_ms))
                .await
                .is_ok()
            {
                capture_verified_submit_observation(cx, client, pane_id).await
            } else {
                VerifiedSubmitObservation::default()
            }
        } else {
            VerifiedSubmitObservation::default()
        };
        let report = classify_verified_submit_with_baseline(
            baseline,
            VerifiedSubmitInput {
                pane_id,
                command_text,
                agent_type,
                profile,
                before_text: None,
                before_semantic_snapshot: None,
                after_text: after.text.as_deref(),
                after_semantic_snapshot: after.semantic_snapshot.as_ref(),
                attempts,
                polls: polls.saturating_add(captures - 1),
            },
        );
        let inconclusive = matches!(
            report.state,
            SubmitReceiptState::StuckInComposer | SubmitReceiptState::VerificationUnavailable
        );
        if profile.is_none()
            || !inconclusive
            || captures >= VERIFY_SUBMIT_MAX_POLLS
            || cx.checkpoint().is_err()
        {
            return report;
        }
    }
}

/// Classify the post-send terminal state using a data-driven submit profile.
#[must_use]
pub fn classify_verified_submit(input: VerifiedSubmitInput<'_>) -> VerifiedSubmitReport {
    let baseline = build_verified_submit_baseline(
        input.pane_id,
        input.command_text,
        input.agent_type,
        input.profile,
        input.before_text,
        input.before_semantic_snapshot,
    );
    classify_verified_submit_with_baseline(&baseline, input)
}

/// Classify using only the retained pre-send baseline and a new observation.
/// `input.before_text` and `input.before_semantic_snapshot` are ignored: a
/// post-send read can never replace the durable pre-send evidence.
#[must_use]
pub fn classify_verified_submit_from_baseline(
    baseline: &VerifiedSubmitBaseline,
    input: VerifiedSubmitInput<'_>,
) -> VerifiedSubmitReport {
    if !baseline.matches_input(input) {
        return baseline_mismatch_report(input);
    }
    classify_verified_submit_with_baseline(baseline, input)
}

fn baseline_mismatch_report(input: VerifiedSubmitInput<'_>) -> VerifiedSubmitReport {
    VerifiedSubmitReport {
        state: SubmitReceiptState::VerificationUnavailable,
        agent_type: receipt_agent_type(input.agent_type),
        profile_id: input.profile.map(|profile| profile.id.clone()),
        profile_version: input.profile.map(|profile| profile.version.clone()),
        attempts: input.attempts.max(1),
        evidence_rule_ids: vec![input.profile.map_or_else(
            || "submit_profile:baseline_mismatch".to_string(),
            |profile| format!("submit_profile:{}:baseline_mismatch", profile.id),
        )],
        polls: input.polls,
        cursor_before: None,
        cursor_after: input
            .after_text
            .map(|text| capture_cursor(input.pane_id, text)),
    }
}

fn classify_verified_submit_with_baseline(
    baseline: &VerifiedSubmitBaseline,
    input: VerifiedSubmitInput<'_>,
) -> VerifiedSubmitReport {
    let baseline = &baseline.0;
    let cursor_before = baseline.cursor_before.clone();
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

    if input.command_text.len() > MAX_CLASSIFIER_CAPTURE_BYTES {
        return unavailable_with_profile(
            profile,
            agent_type,
            attempts,
            input.polls,
            cursor_before,
            cursor_after,
            "command_exceeds_evidence_bounds",
        );
    }

    let Some(after_text) = input.after_text else {
        // The text and semantic mux RPCs are independent. A failed text
        // capture must not discard a *fresh*, bounded semantic turn that
        // already proves what happened to this exact command. No heuristic
        // anchor matching is possible without the text capture.
        let semantic = fresh_semantic_input(
            baseline.semantic_zones.as_deref(),
            input.after_semantic_snapshot,
            input.command_text.trim(),
        );
        return match semantic {
            Some(FreshSemanticInput::CurrentCommand { has_output }) => {
                let (state, evidence) = if has_output {
                    (SubmitReceiptState::Submitted, "semantic_output_after_input")
                } else {
                    (
                        SubmitReceiptState::StuckInComposer,
                        "semantic_input_contains_command",
                    )
                };
                report_with_profile(
                    state,
                    profile,
                    attempts,
                    input.polls,
                    cursor_before,
                    cursor_after,
                    vec![format!("submit_profile:{}:{evidence}", profile.id)],
                )
            }
            other => unavailable_with_profile(
                profile,
                agent_type,
                attempts,
                input.polls,
                cursor_before,
                cursor_after,
                match other {
                    Some(FreshSemanticInput::OtherCommand) => "latest_input_does_not_match",
                    Some(FreshSemanticInput::UnprovenCurrentCommand) => "semantic_input_not_new",
                    _ => "capture_unavailable",
                },
            ),
        };
    };

    let after_tail = capture_tail(after_text).to_ascii_lowercase();
    let command_lower = input.command_text.trim().to_ascii_lowercase();
    if command_lower.is_empty() {
        return unavailable_with_profile(
            profile,
            agent_type,
            attempts,
            input.polls,
            cursor_before,
            cursor_after,
            "insufficient_evidence",
        );
    }
    // A prompt echo can contain any profile marker. Remove complete command
    // spans from status evidence without joining their surrounding text into
    // a new marker. Both strings stay within the bounded capture budget.
    let after_status = after_tail.replace(command_lower.as_str(), "\0");
    if let Some(evidence_id) = first_fresh_anchor_match(
        profile,
        "crash_to_shell",
        &profile.anchors.crash_to_shell,
        baseline
            .fresh_anchors
            .as_ref()
            .map(|anchors| anchors.crash_to_shell.as_slice()),
        &after_status,
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

    if let Some(evidence_id) = first_fresh_anchor_match(
        profile,
        "queued_behind_operation",
        &profile.anchors.queued_behind_operation,
        baseline
            .fresh_anchors
            .as_ref()
            .map(|anchors| anchors.queued_behind_operation.as_slice()),
        &after_status,
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

    match fresh_semantic_input(
        baseline.semantic_zones.as_deref(),
        input.after_semantic_snapshot,
        input.command_text.trim(),
    ) {
        Some(FreshSemanticInput::CurrentCommand { has_output }) => {
            let (state, evidence) = if has_output {
                (SubmitReceiptState::Submitted, "semantic_output_after_input")
            } else {
                (
                    SubmitReceiptState::StuckInComposer,
                    "semantic_input_contains_command",
                )
            };
            return report_with_profile(
                state,
                profile,
                attempts,
                input.polls,
                cursor_before,
                cursor_after,
                vec![format!("submit_profile:{}:{evidence}", profile.id)],
            );
        }
        Some(FreshSemanticInput::OtherCommand) => {
            return unavailable_with_profile(
                profile,
                agent_type,
                attempts,
                input.polls,
                cursor_before,
                cursor_after,
                "latest_input_does_not_match",
            );
        }
        Some(FreshSemanticInput::UnprovenCurrentCommand) => {
            return unavailable_with_profile(
                profile,
                agent_type,
                attempts,
                input.polls,
                cursor_before,
                cursor_after,
                "semantic_input_not_new",
            );
        }
        None => {}
    }

    if let Some(evidence_id) = composer_anchor_holds_command(
        profile,
        &profile.anchors.composer_nonempty,
        &after_tail,
        &command_lower,
    ) {
        let new_composer_text = baseline.composer_before == Some(false);
        if !new_composer_text {
            return unavailable_with_profile(
                profile,
                agent_type,
                attempts,
                input.polls,
                cursor_before,
                cursor_after,
                "composer_input_not_new",
            );
        }
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

    if let Some(evidence_id) = first_fresh_anchor_match(
        profile,
        "working_state",
        &profile.anchors.working_state,
        baseline
            .fresh_anchors
            .as_ref()
            .map(|anchors| anchors.working_state.as_slice()),
        &after_status,
    )
    .or_else(|| {
        first_fresh_anchor_match(
            profile,
            "composer_cleared",
            &profile.anchors.composer_cleared,
            baseline
                .fresh_anchors
                .as_ref()
                .map(|anchors| anchors.composer_cleared.as_slice()),
            &after_status,
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

fn hash_len_prefixed(hasher: &mut Sha256, value: &str) {
    // Fixed-width u128 lengths make framing platform-independent and
    // collision-free even on a hypothetical target whose usize exceeds u64.
    hasher.update((value.len() as u128).to_be_bytes());
    hasher.update(value.as_bytes());
}

fn submit_profile_fingerprint(profile: &SubmitProfile) -> String {
    let mut hasher = Sha256::new();
    hasher.update(VERIFIED_SUBMIT_PROFILE_DOMAIN);
    hash_len_prefixed(&mut hasher, &profile.id);
    hash_len_prefixed(&mut hasher, &profile.version);
    hash_len_prefixed(&mut hasher, &profile.agent_type.to_string());
    for group in [
        &profile.anchors.composer_nonempty,
        &profile.anchors.composer_cleared,
        &profile.anchors.working_state,
        &profile.anchors.queued_behind_operation,
        &profile.anchors.crash_to_shell,
    ] {
        hasher.update((group.len() as u128).to_be_bytes());
        for anchor in group {
            hash_len_prefixed(&mut hasher, anchor);
        }
    }
    hasher.update((profile.remediation.len() as u128).to_be_bytes());
    for remediation in &profile.remediation {
        hash_len_prefixed(&mut hasher, &remediation.when);
        hash_len_prefixed(&mut hasher, &remediation.action);
    }
    hex::encode(hasher.finalize())
}

fn semantic_zone_fingerprints(
    snapshot: &MuxSemanticSnapshot,
    command_text: &str,
) -> Vec<VerifiedSubmitSemanticZone> {
    snapshot
        .zones
        .iter()
        .map(|zone| VerifiedSubmitSemanticZone {
            start_y: zone.start_y,
            start_x: zone.start_x,
            end_y: zone.end_y,
            end_x: zone.end_x,
            semantic_type: zone.semantic_type,
            text_sha256: hex::encode(Sha256::digest(zone.text.as_bytes())),
            text_bytes: zone.text.len(),
            contains_command: zone.text.contains(command_text),
        })
        .collect()
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
    let effect_sha256 = hex::encode(submit_effect_digest(request.pane_id, request.text));
    SubmitIdempotencyBinding {
        key,
        pane_id: request.pane_id,
        request_sha256,
        effect_sha256,
        caller_key: request.caller_key.to_string(),
        guarantee_level: request.guarantee_level,
        outbound_text: request.text.into(),
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

fn first_fresh_anchor_match(
    profile: &SubmitProfile,
    group: &str,
    anchors: &[String],
    eligible: Option<&[bool]>,
    after_tail_lower: &str,
) -> Option<String> {
    let eligible = eligible?;
    anchors
        .iter()
        .enumerate()
        .find(|(index, anchor)| {
            let anchor = anchor.trim().to_ascii_lowercase();
            // Eligibility was frozen before the send, after excluding every
            // marker appearing in the command, including wrapped/clipped echoes.
            eligible.get(*index) == Some(&true)
                && !anchor.is_empty()
                && after_tail_lower.contains(&anchor)
        })
        .map(|(index, _)| format!("submit_profile:{}:{group}:{index}", profile.id))
}

/// Agent CLIs draw the input box below the conversation and echo each
/// submitted turn with the same marker (Codex 0.162 renders both as
/// `› <text>`, Claude Code 2.1 as `❯ <text>`). So the command proves a stuck
/// composer only when it follows the anchor's last occurrence; matching it
/// anywhere on screen read every completed send as stuck. An anchor drawn
/// below the input box can only fall back to weaker evidence, never to a false
/// success.
fn composer_anchor_holds_command(
    profile: &SubmitProfile,
    anchors: &[String],
    after_tail_lower: &str,
    command_lower: &str,
) -> Option<String> {
    if command_lower.is_empty() {
        return None;
    }
    anchors
        .iter()
        .enumerate()
        .find(|(_, anchor)| {
            let anchor = anchor.trim().to_ascii_lowercase();
            !anchor.is_empty()
                && after_tail_lower
                    .rfind(&anchor)
                    .is_some_and(|at| after_tail_lower[at + anchor.len()..].contains(command_lower))
        })
        .map(|(index, _)| format!("submit_profile:{}:composer_nonempty:{index}", profile.id))
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

fn bounded_semantic_snapshot(snapshot: &MuxSemanticSnapshot) -> bool {
    if snapshot.zones.len() > MAX_CLASSIFIER_SEMANTIC_ZONES {
        return false;
    }
    let mut remaining = MAX_CLASSIFIER_CAPTURE_BYTES;
    let mut previous_end = None;
    for zone in &snapshot.zones {
        let start = (zone.start_y, zone.start_x);
        let end = (zone.end_y, zone.end_x);
        if start > end || previous_end.is_some_and(|previous_end| previous_end > start) {
            return false;
        }
        let Some(next_remaining) = remaining.checked_sub(zone.text.len()) else {
            return false;
        };
        remaining = next_remaining;
        previous_end = Some(end);
    }
    true
}

/// Match only the latest input, and only when the exact pre-send snapshot
/// proves that turn did not already exist. Preserved prefix zones fence
/// scrollback loss, coordinate reuse, and reflow; none can manufacture a new
/// command from a prior matching turn. An in-place composer edit is admissible
/// only while that Input was the last pre-send zone and did not hold this
/// command already.
#[allow(clippy::enum_variant_names)]
enum FreshSemanticInput {
    CurrentCommand { has_output: bool },
    OtherCommand,
    UnprovenCurrentCommand,
}

fn fresh_semantic_input(
    before: Option<&[VerifiedSubmitSemanticZone]>,
    after: Option<&MuxSemanticSnapshot>,
    command_text: &str,
) -> Option<FreshSemanticInput> {
    let after = after?;
    if command_text.is_empty() || !bounded_semantic_snapshot(after) {
        return None;
    }
    let input_index = after
        .zones
        .iter()
        .rposition(|zone| zone.semantic_type == MuxSemanticZoneKind::Input)?;
    let input = &after.zones[input_index];
    let matches_command = input.text.trim() == command_text;
    // A current matching input is not made fresh by a failed baseline read.
    // Keep this negative evidence authoritative over generic status markers,
    // including when the baseline was rejected for size or malformed zones.
    let Some(before) = before else {
        return matches_command.then_some(FreshSemanticInput::UnprovenCurrentCommand);
    };
    let after_fingerprints = semantic_zone_fingerprints(after, command_text);
    let appended_input = input_index >= before.len()
        && after_fingerprints.starts_with(before)
        && before
            .last()
            .is_none_or(|last| (last.end_y, last.end_x) < (input.start_y, input.start_x));
    let edited_composer = before.last().is_some_and(|prior| {
        prior.semantic_type == MuxSemanticZoneKind::Input
            && input_index == before.len() - 1
            && after_fingerprints[..input_index] == before[..input_index]
            && (input.start_y, input.start_x) == (prior.start_y, prior.start_x)
            && after_fingerprints[input_index].text_sha256 != prior.text_sha256
            && (!matches_command || !prior.contains_command)
    });
    if !appended_input && !edited_composer {
        return matches_command.then_some(FreshSemanticInput::UnprovenCurrentCommand);
    }
    if !matches_command {
        return Some(FreshSemanticInput::OtherCommand);
    }
    let has_output = after.zones[input_index + 1..].iter().any(|later| {
        later.semantic_type == MuxSemanticZoneKind::Output && !later.text.trim().is_empty()
    });
    Some(FreshSemanticInput::CurrentCommand { has_output })
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
            before_semantic_snapshot: None,
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
    fn stale_status_markers_and_unrelated_output_do_not_satisfy_submit_guarantees() {
        let profile = profile();
        for marker in ["Thinking", "Running", "operation in progress"] {
            let before = format!("{marker}\nPress Enter to send\n");
            for after in [before.clone(), format!("{before}background heartbeat\n")] {
                let report = classify_verified_submit(VerifiedSubmitInput {
                    before_text: Some(&before),
                    ..input(Some(&profile), Some(&after))
                });
                assert_eq!(
                    report.state,
                    SubmitReceiptState::VerificationUnavailable,
                    "stale marker {marker:?} cannot prove this send: {report:?}"
                );
                for guarantee in [
                    SubmitGuaranteeLevel::Submitted,
                    SubmitGuaranteeLevel::Working,
                ] {
                    assert!(!guarantee.is_met_by(report.state, &report.evidence_rule_ids));
                }
            }
        }
    }

    #[test]
    fn marker_echoes_in_current_composer_never_prove_submission() {
        let profile = profile();
        for command in [
            "print Thinking",
            "explain Running",
            "explain operation in progress",
        ] {
            let after = format!("Press Enter to send\n{command}\n");
            let report = classify_verified_submit(VerifiedSubmitInput {
                command_text: command,
                ..input(Some(&profile), Some(&after))
            });
            assert_eq!(report.state, SubmitReceiptState::StuckInComposer);
            assert!(
                !SubmitGuaranteeLevel::Submitted.is_met_by(report.state, &report.evidence_rule_ids)
            );
        }

        // A fresh background status cannot override direct evidence that the
        // new prompt remains in the current composer.
        let report = classify_verified_submit(input(
            Some(&profile),
            Some("Running\nPress Enter to send\nrun tests\n"),
        ));
        assert_eq!(report.state, SubmitReceiptState::StuckInComposer);

        let stale_composer = classify_verified_submit(VerifiedSubmitInput {
            before_text: Some("Press Enter to send\nrun tests\n"),
            ..input(
                Some(&profile),
                Some("Running\nPress Enter to send\nrun tests\n"),
            )
        });
        assert_eq!(
            stale_composer.state,
            SubmitReceiptState::VerificationUnavailable
        );
    }

    #[test]
    fn wrapped_echoes_and_oversized_commands_are_not_status_evidence() {
        let profile = profile();
        for (command, after) in [
            (
                "please print Thinking verbatim",
                "please print\nThinking verbatim\n",
            ),
            (
                "explain operation in progress",
                "explain\noperation in progress\n",
            ),
        ] {
            let report = classify_verified_submit(VerifiedSubmitInput {
                command_text: command,
                ..input(Some(&profile), Some(after))
            });
            assert_eq!(report.state, SubmitReceiptState::VerificationUnavailable);
        }

        let oversized = format!("{} Thinking", "x".repeat(MAX_CLASSIFIER_CAPTURE_BYTES));
        let report = classify_verified_submit(VerifiedSubmitInput {
            command_text: &oversized,
            ..input(Some(&profile), Some("clipped prompt tail: Thinking\n"))
        });
        assert_eq!(report.state, SubmitReceiptState::VerificationUnavailable);
        assert_eq!(
            report.evidence_rule_ids,
            vec!["submit_profile:codex.default:command_exceeds_evidence_bounds"]
        );
    }

    #[test]
    fn independent_fresh_marker_preserves_the_working_guarantee() {
        let mut profile = profile();
        profile.anchors.composer_cleared.push("Running".to_string());
        let report = classify_verified_submit(VerifiedSubmitInput {
            command_text: "print Thinking",
            ..input(
                Some(&profile),
                Some("print Thinking\nRunning\nPress Enter to send\n"),
            )
        });
        assert_eq!(report.state, SubmitReceiptState::Submitted);
        assert_eq!(
            report.evidence_rule_ids,
            vec!["submit_profile:codex.default:working_state:0"]
        );
        assert!(SubmitGuaranteeLevel::Working.is_met_by(report.state, &report.evidence_rule_ids));
    }

    fn semantic_snapshot(turns: &[(MuxSemanticZoneKind, &str)]) -> MuxSemanticSnapshot {
        MuxSemanticSnapshot {
            zones: turns
                .iter()
                .enumerate()
                .map(|(row, (kind, text))| MuxSemanticZone {
                    start_y: isize::try_from(row).unwrap(),
                    start_x: 0,
                    end_y: isize::try_from(row).unwrap(),
                    end_x: text.len(),
                    semantic_type: *kind,
                    text: (*text).to_string(),
                })
                .collect(),
            last_exit_code: None,
        }
    }

    #[test]
    fn fresh_semantic_turn_survives_independent_text_capture_failure() {
        let profile = profile();
        let before = semantic_snapshot(&[(MuxSemanticZoneKind::Output, "ready")]);
        let after_submitted = semantic_snapshot(&[
            (MuxSemanticZoneKind::Output, "ready"),
            (MuxSemanticZoneKind::Input, "run tests"),
            (MuxSemanticZoneKind::Output, "tests passed"),
        ]);
        let after_composer = semantic_snapshot(&[
            (MuxSemanticZoneKind::Output, "ready"),
            (MuxSemanticZoneKind::Input, "run tests"),
        ]);
        for (after, expected) in [
            (&after_submitted, SubmitReceiptState::Submitted),
            (&after_composer, SubmitReceiptState::StuckInComposer),
        ] {
            let evidence = VerifiedSubmitInput {
                before_text: None,
                before_semantic_snapshot: Some(&before),
                after_text: None,
                after_semantic_snapshot: Some(after),
                ..input(Some(&profile), None)
            };
            assert_recovered_classification(evidence, expected);
        }

        // A matching but pre-existing input cannot be treated as a fresh
        // submission just because the independent text RPC failed.
        let prior = semantic_snapshot(&[
            (MuxSemanticZoneKind::Input, "run tests"),
            (MuxSemanticZoneKind::Output, "old result"),
        ]);
        let unchanged = classify_verified_submit(VerifiedSubmitInput {
            before_text: None,
            before_semantic_snapshot: Some(&prior),
            after_text: None,
            after_semantic_snapshot: Some(&prior),
            ..input(Some(&profile), None)
        });
        assert_eq!(unchanged.state, SubmitReceiptState::VerificationUnavailable);
        assert!(unchanged
            .evidence_rule_ids
            .iter()
            .any(|rule| rule.ends_with(":semantic_input_not_new")));
    }

    fn assert_recovered_classification(
        input: VerifiedSubmitInput<'_>,
        expected: SubmitReceiptState,
    ) {
        let live = classify_verified_submit(input);
        assert_eq!(live.state, expected);
        let baseline = prepare_verified_submit_baseline(
            input.pane_id,
            input.command_text,
            input.agent_type,
            input.profile,
            input.before_text,
            input.before_semantic_snapshot,
        )
        .unwrap();
        let serialized = serde_json::to_string(&baseline).unwrap();
        let recovered: VerifiedSubmitBaseline = serde_json::from_str(&serialized).unwrap();
        // Restart cannot retrieve the old raw capture. Only the durable
        // predicates and fingerprints are permitted to establish freshness.
        let resumed = classify_verified_submit_from_baseline(
            &recovered,
            VerifiedSubmitInput {
                before_text: None,
                before_semantic_snapshot: None,
                ..input
            },
        );
        assert_eq!(resumed, live);
    }

    #[test]
    fn durable_baseline_preserves_text_classifier_verdicts_after_restart() {
        let profile = profile();
        for (command, before, after, state) in [
            (
                "run tests",
                Some("ready\n"),
                "Running\n",
                SubmitReceiptState::Submitted,
            ),
            (
                "run tests",
                Some("ready\n"),
                "Thinking\n",
                SubmitReceiptState::Submitted,
            ),
            (
                "run tests",
                Some("ready\n"),
                "operation in progress\n",
                SubmitReceiptState::QueuedBehindOperation,
            ),
            (
                "run tests",
                Some("ready\n"),
                "panicked at terminal.rs\nRunning\n",
                SubmitReceiptState::PaneCrashedToShell,
            ),
            (
                "run tests",
                Some("Running\n"),
                "Running\nnew unrelated output\n",
                SubmitReceiptState::VerificationUnavailable,
            ),
            (
                "run tests",
                None,
                "Running\nThinking\n",
                SubmitReceiptState::VerificationUnavailable,
            ),
            (
                "print Thinking",
                Some("ready\n"),
                "print\nThinking\n",
                SubmitReceiptState::VerificationUnavailable,
            ),
            (
                "print Thinking",
                Some("ready\n"),
                "print Thinking\nRunning\n",
                SubmitReceiptState::Submitted,
            ),
            (
                "run tests",
                Some("Press Enter to send\n"),
                "Running\nPress Enter to send\nrun tests\n",
                SubmitReceiptState::StuckInComposer,
            ),
            (
                "run tests",
                Some("Press Enter to send\nrun tests\n"),
                "Running\nPress Enter to send\nrun tests\n",
                SubmitReceiptState::VerificationUnavailable,
            ),
        ] {
            assert_recovered_classification(
                VerifiedSubmitInput {
                    command_text: command,
                    before_text: before,
                    attempts: 2,
                    polls: 5,
                    ..input(Some(&profile), Some(after))
                },
                state,
            );
        }
        assert_recovered_classification(
            input(None, Some("Running\n")),
            SubmitReceiptState::VerificationUnavailable,
        );
    }

    #[test]
    fn durable_semantic_baseline_preserves_prior_turn_and_coordinate_fences() {
        use MuxSemanticZoneKind::{Input, Output};
        let profile = profile();
        let empty = semantic_snapshot(&[]);
        let prior = semantic_snapshot(&[(Input, "run tests"), (Output, "old result")]);
        let repeated = semantic_snapshot(&[
            (Input, "run tests"),
            (Output, "old result"),
            (Input, "run tests"),
            (Output, "new result"),
        ]);
        let composer = semantic_snapshot(&[(Input, "")]);
        let edited = semantic_snapshot(&[(Input, "run tests"), (Output, "new result")]);
        let contains_prior_command = semantic_snapshot(&[(Input, "please run tests later")]);
        let other = semantic_snapshot(&[(Input, "different command"), (Output, "Running")]);
        let reused_origin = semantic_snapshot(&[(Output, "")]);
        let mut reused_coordinate =
            semantic_snapshot(&[(Output, ""), (Input, "run tests"), (Output, "new result")]);
        reused_coordinate.zones[1].start_y = 0;
        let mut oversized = composer.clone();
        oversized.zones[0].text = "x".repeat(MAX_CLASSIFIER_CAPTURE_BYTES + 1);
        let mut malformed = composer.clone();
        malformed.zones[0].start_y = 1;
        for (before, after, expected) in [
            (Some(&empty), &edited, SubmitReceiptState::Submitted),
            (Some(&prior), &repeated, SubmitReceiptState::Submitted),
            (Some(&composer), &edited, SubmitReceiptState::Submitted),
            (
                Some(&prior),
                &prior,
                SubmitReceiptState::VerificationUnavailable,
            ),
            (
                Some(&contains_prior_command),
                &edited,
                SubmitReceiptState::VerificationUnavailable,
            ),
            (
                Some(&empty),
                &other,
                SubmitReceiptState::VerificationUnavailable,
            ),
            (
                Some(&prior),
                &edited,
                SubmitReceiptState::VerificationUnavailable,
            ),
            (
                Some(&reused_origin),
                &reused_coordinate,
                SubmitReceiptState::VerificationUnavailable,
            ),
            (None, &edited, SubmitReceiptState::VerificationUnavailable),
            (
                Some(&oversized),
                &edited,
                SubmitReceiptState::VerificationUnavailable,
            ),
            (
                Some(&malformed),
                &edited,
                SubmitReceiptState::VerificationUnavailable,
            ),
        ] {
            assert_recovered_classification(
                VerifiedSubmitInput {
                    before_text: Some("ready\n"),
                    before_semantic_snapshot: before,
                    after_semantic_snapshot: Some(after),
                    ..input(Some(&profile), Some("run tests\nRunning\n"))
                },
                expected,
            );
        }
    }

    #[test]
    fn durable_baseline_roundtrip_never_persists_command_or_pane_content() {
        let profile = profile();
        let command = "private command with credential=DO_NOT_STORE_COMMAND";
        let before = "DO_NOT_STORE_CAPTURE\nPress Enter to send\n";
        let semantic = semantic_snapshot(&[
            (MuxSemanticZoneKind::Input, "DO_NOT_STORE_SEMANTIC_INPUT"),
            (MuxSemanticZoneKind::Output, "DO_NOT_STORE_SEMANTIC_OUTPUT"),
        ]);
        let baseline = prepare_verified_submit_baseline(
            7,
            command,
            AgentType::Codex,
            Some(&profile),
            Some(before),
            Some(&semantic),
        )
        .unwrap();
        let binding = idempotency_binding(SubmitIdempotencyRequest {
            pane_id: 7,
            text: command,
            caller_key: "DO_NOT_STORE_CALLER_KEY",
            guarantee_level: SubmitGuaranteeLevel::Submitted,
            wait_for: None,
            wait_for_regex: false,
            timeout_secs: 0,
        });
        assert!(baseline.matches_binding(&binding));
        let encoded = serde_json::to_string(&baseline).unwrap();
        assert!(!encoded.contains("DO_NOT_STORE"));
        assert!(!format!("{baseline:?}").contains("DO_NOT_STORE"));
        assert!(!encoded.contains("Press Enter to send"));
        assert!(!encoded.contains("panicked at"));
        assert_eq!(
            serde_json::from_str::<VerifiedSubmitBaseline>(&encoded).unwrap(),
            baseline
        );
    }

    #[test]
    fn durable_baseline_rejects_exact_command_pane_agent_and_profile_drift() {
        let profile = profile();
        let original = input(Some(&profile), Some("Running\n"));
        let baseline = prepare_verified_submit_baseline(
            original.pane_id,
            original.command_text,
            original.agent_type,
            original.profile,
            original.before_text,
            None,
        )
        .unwrap();
        let mut changed_anchor = profile.clone();
        changed_anchor.anchors.working_state[0] = "Other status".to_string();
        let mut changed_remediation = profile.clone();
        changed_remediation
            .remediation
            .push(SubmitProfileRemediation {
                when: "stuck".to_string(),
                action: "observe".to_string(),
            });
        for changed in [
            VerifiedSubmitInput {
                pane_id: 8,
                ..original
            },
            VerifiedSubmitInput {
                command_text: "run tests ",
                ..original
            },
            VerifiedSubmitInput {
                command_text: "RUN TESTS",
                ..original
            },
            VerifiedSubmitInput {
                agent_type: AgentType::ClaudeCode,
                ..original
            },
            VerifiedSubmitInput {
                profile: None,
                ..original
            },
            VerifiedSubmitInput {
                profile: Some(&changed_anchor),
                ..original
            },
            VerifiedSubmitInput {
                profile: Some(&changed_remediation),
                ..original
            },
        ] {
            let report = classify_verified_submit_from_baseline(&baseline, changed);
            assert_eq!(report.state, SubmitReceiptState::VerificationUnavailable);
            assert!(report.evidence_rule_ids[0].ends_with(":baseline_mismatch"));
            assert_eq!(report.cursor_before, None);
        }
        // Supplying a replacement raw baseline cannot turn an old marker fresh.
        let stale = prepare_verified_submit_baseline(
            7,
            "run tests",
            AgentType::Codex,
            Some(&profile),
            Some("Running\n"),
            None,
        )
        .unwrap();
        assert_eq!(
            classify_verified_submit_from_baseline(&stale, original).state,
            SubmitReceiptState::VerificationUnavailable
        );
    }

    #[test]
    fn durable_baseline_deserialization_and_preparation_enforce_bounds() {
        let profile = profile();
        let baseline = prepare_verified_submit_baseline(
            7,
            "run tests",
            AgentType::Codex,
            Some(&profile),
            Some("ready\n"),
            Some(&semantic_snapshot(&[])),
        )
        .unwrap();
        let value = serde_json::to_value(&baseline).unwrap();
        for field in [
            "profile",
            "cursor_before",
            "fresh_anchors",
            "composer_before",
            "semantic_zones",
        ] {
            let mut missing = value.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<VerifiedSubmitBaseline>(missing).is_err());
        }
        for (field, replacement) in [
            ("version", serde_json::json!(0)),
            ("effect_sha256", serde_json::json!("invalid")),
            (
                "cursor_before",
                serde_json::json!("pane:99:capture:sha256:0123456789abcdef"),
            ),
            ("composer_before", serde_json::Value::Null),
        ] {
            let mut invalid = value.clone();
            invalid[field] = replacement;
            assert!(serde_json::from_value::<VerifiedSubmitBaseline>(invalid).is_err());
        }
        let mut oversized = value.clone();
        oversized["fresh_anchors"]["working_state"] =
            serde_json::json!(vec![true; MAX_BASELINE_ANCHORS_PER_GROUP + 1]);
        assert!(serde_json::from_value::<VerifiedSubmitBaseline>(oversized).is_err());
        let mut unknown_content = value;
        unknown_content["before_text"] = serde_json::json!("must never be accepted");
        assert!(serde_json::from_value::<VerifiedSubmitBaseline>(unknown_content).is_err());

        let mut oversized_profile = profile;
        oversized_profile.anchors.working_state =
            vec!["Running".to_string(); MAX_BASELINE_ANCHORS_PER_GROUP + 1];
        assert!(
            prepare_verified_submit_baseline(
                7,
                "run tests",
                AgentType::Codex,
                Some(&oversized_profile),
                Some("ready\n"),
                None,
            )
            .is_none()
        );
        assert!(
            prepare_verified_submit_baseline(
                7,
                &"x".repeat(MAX_CLASSIFIER_CAPTURE_BYTES + 1),
                AgentType::Codex,
                None,
                None,
                None,
            )
            .is_none()
        );
    }

    #[test]
    fn durable_baseline_maximum_shape_stays_bounded_without_truncating_history() {
        let mut profile = profile();
        profile.id = "p".repeat(MAX_BASELINE_PROFILE_IDENTITY_BYTES);
        profile.version = "v".repeat(MAX_BASELINE_PROFILE_IDENTITY_BYTES);
        let anchors = vec!["Running".to_string(); MAX_BASELINE_ANCHORS_PER_GROUP];
        profile.anchors = SubmitProfileAnchors {
            composer_nonempty: anchors.clone(),
            composer_cleared: anchors.clone(),
            working_state: anchors.clone(),
            queued_behind_operation: anchors.clone(),
            crash_to_shell: anchors,
        };
        let mut snapshot = MuxSemanticSnapshot {
            zones: (0..MAX_CLASSIFIER_SEMANTIC_ZONES)
                .map(|index| MuxSemanticZone {
                    start_y: isize::MIN + isize::try_from(index).unwrap(),
                    start_x: usize::MAX,
                    end_y: isize::MIN + isize::try_from(index).unwrap(),
                    end_x: usize::MAX,
                    semantic_type: MuxSemanticZoneKind::Output,
                    text: "private historical output".to_string(),
                })
                .collect(),
            last_exit_code: None,
        };
        let baseline = prepare_verified_submit_baseline(
            u64::MAX,
            "run tests",
            AgentType::Codex,
            Some(&profile),
            Some("ready\n"),
            Some(&snapshot),
        )
        .unwrap();
        assert_eq!(
            baseline.0.semantic_zones.as_ref().unwrap().len(),
            MAX_CLASSIFIER_SEMANTIC_ZONES
        );
        let encoded = serde_json::to_vec(&baseline).unwrap();
        assert!(encoded.len() < 128 * 1024);
        assert_eq!(
            serde_json::from_slice::<VerifiedSubmitBaseline>(&encoded).unwrap(),
            baseline
        );
        snapshot.zones.push(MuxSemanticZone {
            start_y: 0,
            start_x: 0,
            end_y: 0,
            end_x: 0,
            semantic_type: MuxSemanticZoneKind::Input,
            text: String::new(),
        });
        let unavailable = prepare_verified_submit_baseline(
            7,
            "run tests",
            AgentType::Codex,
            Some(&profile),
            Some("ready\n"),
            Some(&snapshot),
        )
        .unwrap();
        assert_eq!(unavailable.0.semantic_zones, None);
    }

    #[test]
    fn semantic_repeated_command_requires_a_new_turn_after_the_exact_baseline() {
        let profile = profile();
        let before = semantic_snapshot(&[
            (MuxSemanticZoneKind::Input, "run tests"),
            (MuxSemanticZoneKind::Output, "old result"),
        ]);
        let stale = classify_verified_submit(VerifiedSubmitInput {
            before_text: Some("run tests\nold result\n"),
            before_semantic_snapshot: Some(&before),
            after_semantic_snapshot: Some(&before),
            ..input(Some(&profile), Some("run tests\nold result\nRunning\n"))
        });
        assert_eq!(stale.state, SubmitReceiptState::VerificationUnavailable);

        let after = semantic_snapshot(&[
            (MuxSemanticZoneKind::Input, "run tests"),
            (MuxSemanticZoneKind::Output, "old result"),
            (MuxSemanticZoneKind::Input, "run tests"),
            (MuxSemanticZoneKind::Output, "new result"),
        ]);
        let fresh = classify_verified_submit(VerifiedSubmitInput {
            before_text: Some("run tests\nold result\n"),
            before_semantic_snapshot: Some(&before),
            after_semantic_snapshot: Some(&after),
            ..input(Some(&profile), Some("run tests\nnew result\n"))
        });
        assert_eq!(fresh.state, SubmitReceiptState::Submitted);
        assert_eq!(
            fresh.evidence_rule_ids,
            vec!["submit_profile:codex.default:semantic_output_after_input"]
        );
        assert!(SubmitGuaranteeLevel::Working.is_met_by(fresh.state, &fresh.evidence_rule_ids));
    }

    #[test]
    fn semantic_old_matching_turn_cannot_borrow_output_from_a_later_input() {
        let profile = profile();
        let before = semantic_snapshot(&[]);
        let after = semantic_snapshot(&[
            (MuxSemanticZoneKind::Input, "run tests"),
            (MuxSemanticZoneKind::Output, "old result"),
            (MuxSemanticZoneKind::Input, "different command"),
            (MuxSemanticZoneKind::Output, "Running"),
        ]);
        let report = classify_verified_submit(VerifiedSubmitInput {
            before_semantic_snapshot: Some(&before),
            after_semantic_snapshot: Some(&after),
            ..input(
                Some(&profile),
                Some("run tests\ndifferent command\nRunning\n"),
            )
        });
        assert_eq!(report.state, SubmitReceiptState::VerificationUnavailable);
        assert_eq!(
            report.evidence_rule_ids,
            vec!["submit_profile:codex.default:latest_input_does_not_match"]
        );
    }

    #[test]
    fn semantic_composer_edit_is_new_but_an_unchanged_prior_command_is_not() {
        let profile = profile();
        let before = semantic_snapshot(&[(MuxSemanticZoneKind::Input, "")]);
        let after = semantic_snapshot(&[(MuxSemanticZoneKind::Input, "run tests")]);
        let classify = |baseline| {
            classify_verified_submit(VerifiedSubmitInput {
                before_semantic_snapshot: Some(baseline),
                after_semantic_snapshot: Some(&after),
                ..input(Some(&profile), Some("run tests\n"))
            })
        };
        assert_eq!(classify(&before).state, SubmitReceiptState::StuckInComposer);
        assert_eq!(
            classify(&after).state,
            SubmitReceiptState::VerificationUnavailable
        );
    }

    #[test]
    fn missing_baseline_and_rewritten_or_oversized_semantic_history_fail_closed() {
        let profile = profile();
        let without_baseline = classify_verified_submit(VerifiedSubmitInput {
            before_text: None,
            ..input(
                Some(&profile),
                Some("Thinking\nRunning\noperation in progress\n"),
            )
        });
        assert_eq!(
            without_baseline.state,
            SubmitReceiptState::VerificationUnavailable
        );

        let before = semantic_snapshot(&[
            (MuxSemanticZoneKind::Input, "old command"),
            (MuxSemanticZoneKind::Output, "old result"),
        ]);
        let rewritten = semantic_snapshot(&[
            (MuxSemanticZoneKind::Input, "run tests"),
            (MuxSemanticZoneKind::Output, "new result"),
        ]);
        for baseline in [None, Some(&before)] {
            let report = classify_verified_submit(VerifiedSubmitInput {
                before_semantic_snapshot: baseline,
                after_semantic_snapshot: Some(&rewritten),
                ..input(Some(&profile), Some("run tests\nnew result\n"))
            });
            assert_eq!(report.state, SubmitReceiptState::VerificationUnavailable);
        }

        let empty = semantic_snapshot(&[]);
        let mut oversized = rewritten;
        oversized.zones[1].text = "x".repeat(MAX_CLASSIFIER_CAPTURE_BYTES + 1);
        let report = classify_verified_submit(VerifiedSubmitInput {
            before_semantic_snapshot: Some(&empty),
            after_semantic_snapshot: Some(&oversized),
            ..input(Some(&profile), Some("run tests\nnew result\n"))
        });
        assert_eq!(report.state, SubmitReceiptState::VerificationUnavailable);
    }

    #[test]
    fn current_semantic_input_with_unusable_baseline_blocks_fresh_working_marker() {
        let profile = profile();
        let after = semantic_snapshot(&[(MuxSemanticZoneKind::Input, "run tests")]);
        let mut oversized = semantic_snapshot(&[(MuxSemanticZoneKind::Output, "")]);
        oversized.zones[0].text = "x".repeat(MAX_CLASSIFIER_CAPTURE_BYTES + 1);
        let mut malformed = semantic_snapshot(&[(MuxSemanticZoneKind::Input, "")]);
        malformed.zones[0].start_y = 1;
        for before in [None, Some(&oversized), Some(&malformed)] {
            let report = classify_verified_submit(VerifiedSubmitInput {
                before_text: Some("ready\n"),
                before_semantic_snapshot: before,
                after_semantic_snapshot: Some(&after),
                ..input(Some(&profile), Some("run tests\nRunning\n"))
            });
            assert_eq!(report.state, SubmitReceiptState::VerificationUnavailable);
            assert_eq!(
                report.evidence_rule_ids,
                vec!["submit_profile:codex.default:semantic_input_not_new"]
            );
            assert!(
                !SubmitGuaranteeLevel::Working.is_met_by(report.state, &report.evidence_rule_ids)
            );
        }
    }

    #[test]
    fn semantic_output_after_matching_input_is_submitted_even_if_text_remains_in_transcript() {
        let profile = profile();
        let before = MuxSemanticSnapshot {
            zones: Vec::new(),
            last_exit_code: None,
        };
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
            before_semantic_snapshot: Some(&before),
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
        let before = MuxSemanticSnapshot {
            zones: Vec::new(),
            last_exit_code: None,
        };
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
            before_semantic_snapshot: Some(&before),
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
    fn unrelated_capture_delta_without_command_echo_is_not_submit_evidence() {
        let profile = profile();
        let report = classify_verified_submit(input(Some(&profile), Some("done\n")));

        assert_eq!(report.state, SubmitReceiptState::VerificationUnavailable);
        assert_eq!(
            report.evidence_rule_ids,
            vec!["submit_profile:codex.default:insufficient_evidence"]
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
            before_semantic_snapshot: None,
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

    // Real pane captures (paths shortened) from codex-cli 0.162.1 and Claude
    // Code 2.1.296, 2026-10-12. Both CLIs echo a submitted turn with the same
    // glyph as the composer, which is drawn last.
    const CODEX_STATUS: &str = "  GPT-6-Astra xhigh \u{b7} ~/scratch/probe \u{2026}  \u{26a0} 2 warnings \u{b7} f2 to view\n";
    const CODEX_COMMAND: &str = "ftprobe: reply with the single word OK and do nothing else";
    const CLAUDE_RULE: &str = "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n";

    fn builtin_profile(agent_type: AgentType) -> SubmitProfile {
        crate::patterns::PatternEngine::default()
            .submit_profile_for_agent(agent_type)
            .expect("builtin submit profile")
            .clone()
    }

    fn classify_capture(
        profile: &SubmitProfile,
        command: &str,
        before: &str,
        after: &str,
    ) -> VerifiedSubmitReport {
        classify_verified_submit(VerifiedSubmitInput {
            pane_id: 7,
            command_text: command,
            agent_type: profile.agent_type,
            profile: Some(profile),
            before_text: Some(before),
            before_semantic_snapshot: None,
            after_text: Some(after),
            after_semantic_snapshot: None,
            attempts: 1,
            polls: 1,
        })
    }

    #[test]
    fn codex_0_162_captures_classify_each_composer_state() {
        let profile = builtin_profile(AgentType::Codex);
        let empty = format!("\u{203a} Ask Codex to do anything\n\n{CODEX_STATUS}");
        let history = format!("\u{203a} {CODEX_COMMAND}\n\n");

        let stuck = format!("\u{203a} {CODEX_COMMAND}\n\n{CODEX_STATUS}");
        let report = classify_capture(&profile, CODEX_COMMAND, &empty, &stuck);
        assert_eq!(report.state, SubmitReceiptState::StuckInComposer);
        assert_eq!(
            report.evidence_rule_ids,
            vec!["submit_profile:codex.default:composer_nonempty:0"]
        );

        let working =
            format!("{history}\u{2022} Working (1s \u{2022} esc to interrupt)\n\n{empty}");
        let report = classify_capture(&profile, CODEX_COMMAND, &empty, &working);
        assert_eq!(report.state, SubmitReceiptState::Submitted);

        // A turn that finished before the poll still shows the echoed command,
        // but the composer below it is empty again: never "stuck".
        let finished =
            format!("{history}\u{2022} OK\n\n  Worked for 3s \u{2022} 7:58 PM\n\n{empty}");
        let report = classify_capture(&profile, CODEX_COMMAND, &empty, &finished);
        assert_ne!(report.state, SubmitReceiptState::StuckInComposer);

        let queued = format!(
            "{history}\u{2022} Working (3s \u{2022} esc to interrupt)\n\n\
             \u{2022} Messages to be submitted after next tool call (press esc to interrupt and send immediately)\n\
             \x20 \u{21b3} ftprobe second message while busy\n\n{empty}"
        );
        let report = classify_capture(
            &profile,
            "ftprobe second message while busy",
            &empty,
            &queued,
        );
        assert_eq!(report.state, SubmitReceiptState::QueuedBehindOperation);

        let approval = format!(
            "{history}\u{2022} Running touch probe.txt\n\n  Would you like to run the following command?\n\n\
             \x20 $ touch probe.txt\n\n\u{203a} 1. Yes, proceed (y)\n\
             \x20 3. No, and tell Codex what to do differently (esc)\n\n\
             \x20 Press enter to confirm or esc to cancel\n"
        );
        let report = classify_capture(&profile, CODEX_COMMAND, &empty, &approval);
        assert_eq!(report.state, SubmitReceiptState::QueuedBehindOperation);
    }

    #[test]
    fn agent_output_mentioning_a_queued_job_does_not_hold_codex_sends() {
        let profile = builtin_profile(AgentType::Codex);
        let empty = format!("\u{203a} Ask Codex to do anything\n\n{CODEX_STATUS}");
        let after = format!(
            "\u{2022} The release build is queued on the worker.\n\n\u{203a} {CODEX_COMMAND}\n\n\
             \u{2022} Working (0s \u{2022} esc to interrupt)\n\n{empty}"
        );
        let report = classify_capture(&profile, CODEX_COMMAND, &empty, &after);
        assert_eq!(report.state, SubmitReceiptState::Submitted);
    }

    #[test]
    fn claude_code_2_1_captures_classify_each_composer_state() {
        let profile = builtin_profile(AgentType::ClaudeCode);
        let command = "ftprobe: reply with the single word OK and do nothing else";
        let footer = "  \u{23f5}\u{23f5} bypass permissions on (shift+tab to cycle)\n";
        let empty = format!("{CLAUDE_RULE}\u{276f}\n{CLAUDE_RULE}{footer}");

        let stuck = format!("{CLAUDE_RULE}\u{276f} {command}\n{CLAUDE_RULE}{footer}");
        let report = classify_capture(&profile, command, &empty, &stuck);
        assert_eq!(report.state, SubmitReceiptState::StuckInComposer);
        assert_eq!(
            report.evidence_rule_ids,
            vec!["submit_profile:claude_code.default:composer_nonempty:0"]
        );

        let history = format!("\u{276f} {command}\n\n\u{23fa} OK\n\n");
        let finished = format!("{history}\u{273b} Baked for 2s \u{b7} done 8:05 PM\n\n{empty}");
        let report = classify_capture(&profile, command, &empty, &finished);
        assert_ne!(report.state, SubmitReceiptState::StuckInComposer);

        let working = format!(
            "{history}{CLAUDE_RULE}\u{276f}\n{CLAUDE_RULE}  \u{23f5}\u{23f5} bypass permissions on \
             (shift+tab to cycle) \u{b7} esc to interrupt\n"
        );
        let report = classify_capture(&profile, command, &empty, &working);
        assert_eq!(report.state, SubmitReceiptState::Submitted);
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
            wait_for: None,
            wait_for_regex: false,
            timeout_secs: 30,
        }
    }

    #[test]
    fn idempotency_claim_key_is_stable_for_caller_nonce_and_pane() {
        let a = idempotency_key(7, "nonce");
        assert_eq!(a, idempotency_key(7, "nonce"), "stable for same inputs");
        assert_ne!(a, idempotency_key(8, "nonce"), "pane namespace must matter");
        assert_ne!(a, idempotency_key(7, "nonce-2"), "caller key must matter");
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
        assert_eq!(
            base_binding.request_sha256(),
            different_nonce.request_sha256()
        );
        assert_ne!(base_binding.key(), different_nonce.key());
    }

    #[test]
    fn bound_effect_is_the_exact_caller_text() {
        let request = SubmitIdempotencyRequest {
            guarantee_level: SubmitGuaranteeLevel::Submitted,
            ..idempotency_request(7, "deploy now", "attempt-1")
        };
        let binding = idempotency_binding(request);
        let same = idempotency_binding(request);
        assert_eq!(binding.outbound_text(), same.outbound_text());
        assert_eq!(binding.outbound_text(), request.text);

        let changed_wait = idempotency_binding(SubmitIdempotencyRequest {
            wait_for: Some("ready"),
            ..request
        });
        assert_eq!(binding.key(), changed_wait.key());
        assert_ne!(binding.request_sha256(), changed_wait.request_sha256());
        assert_eq!(binding.effect_sha256(), changed_wait.effect_sha256());

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
            ..request
        });
        assert_ne!(binding.request_sha256(), write.request_sha256());
        assert_eq!(binding.effect_sha256(), write.effect_sha256());

        let blank = idempotency_binding(SubmitIdempotencyRequest {
            guarantee_level: SubmitGuaranteeLevel::Submitted,
            ..idempotency_request(7, "   ", "attempt-blank")
        });
        assert!(blank.is_canonical());
        assert_eq!(blank.outbound_text(), "   ");
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

    #[test]
    fn receipt_counters_count_fresh_receipts_by_agent_type_and_state() {
        // A private agent type keeps receipts recorded by parallel tests out
        // of these counts.
        let agent_type = "w28-counter-probe";
        let receipt = |state| SubmitReceipt {
            state,
            guarantee_level: SubmitGuaranteeLevel::Write,
            guarantee_met: false,
            agent_type: Some(agent_type.to_string()),
            profile_id: Some("probe.default".to_string()),
            profile_version: Some("1".to_string()),
            attempts: 1,
            evidence_rule_ids: Vec::new(),
            elapsed_ms: 5,
            polls: 1,
            cursor_before: None,
            cursor_after: None,
            idempotency_key: "caller-nonce".to_string(),
        };
        for state in [
            SubmitReceiptState::Submitted,
            SubmitReceiptState::StuckInComposer,
            SubmitReceiptState::StuckInComposer,
            SubmitReceiptState::VerificationUnavailable,
        ] {
            record_submit_receipt(7, &receipt(state));
        }

        let probe: Vec<(&str, u64)> = submit_receipt_counts()
            .into_iter()
            .filter(|count| count.agent_type == agent_type)
            .map(|count| (count.state, count.count))
            .collect();
        assert_eq!(
            probe,
            vec![
                ("stuck_in_composer", 2),
                ("submitted", 1),
                ("verification_unavailable", 1),
            ]
        );
    }
}
