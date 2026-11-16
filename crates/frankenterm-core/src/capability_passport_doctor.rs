//! Operator-facing renderer for [`CapabilityPassport`]
//! (ft-1650n.1 follow-up; closes ft-ykp2y).
//!
//! Produces both human-readable text and structured JSON renderings
//! so a `ft passport [pane_id]` doctor surface (or any other
//! observability consumer) can show capability state without
//! leaking the underlying secrets.
//!
//! # Fail-closed against secret leakage
//!
//! Every rendering path goes through
//! [`crate::capability_passport::RedactedProof::display_truncated`]
//! so the underlying probe evidence (which may be a hashed
//! credential) NEVER appears in the output verbatim. The SEC canary
//! test in this module's test block plants a synthetic
//! `ANTHROPIC_API_KEY=sk-…` string in a probe response and asserts
//! neither the credential bytes nor the identifier appear in either
//! the text or JSON rendering.

use serde::{Deserialize, Serialize};

use crate::capability_passport::{CapabilityEntry, CapabilityPassport, CapabilityVerification};

/// Default freshness window the renderer consults when deciding
/// whether to mark an entry's `freshness` as `fresh`, `stale`, or
/// `unknown`. 5 minutes — matches the default
/// [`crate::capability_preflight::DEFAULT_PREFLIGHT_FRESHNESS_MS`]
/// so the rendering aligns with the gate's freshness contract.
pub const DEFAULT_DOCTOR_FRESHNESS_MS: u64 = 300_000;

/// Configuration for the doctor renderer.
#[derive(Debug, Clone)]
pub struct PassportDoctorRenderer {
    pub now_ms: u64,
    pub max_age_ms: u64,
}

impl PassportDoctorRenderer {
    /// Construct with the supplied current time and the default
    /// freshness window ([`DEFAULT_DOCTOR_FRESHNESS_MS`]).
    #[must_use]
    pub fn at(now_ms: u64) -> Self {
        Self {
            now_ms,
            max_age_ms: DEFAULT_DOCTOR_FRESHNESS_MS,
        }
    }

    /// Override the freshness window.
    #[must_use]
    pub fn with_max_age_ms(mut self, max_age_ms: u64) -> Self {
        self.max_age_ms = max_age_ms;
        self
    }

    /// Render a passport as plain text suitable for a `ft doctor`
    /// surface. Emits one header line + one line per capability:
    ///
    /// ```text
    /// passport agent=cc1 pane=7 generation=3 signed_at_ms=1500000
    ///   [verified ] tool:bash         observed_at=900 freshness=fresh proof=a1b2c3d4e5f6…
    ///   [observed ] auth:openai_api   observed_at=850 freshness=stale proof=∅
    ///   [declared ] safety:no_force_push  observed_at=∅ freshness=unknown proof=∅
    ///   [unknown  ] net:loopback_only observed_at=∅ freshness=unknown proof=∅
    /// ```
    #[must_use]
    pub fn render_text(&self, passport: &CapabilityPassport) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "passport agent={} pane={} generation={} signed_at_ms={}\n",
            passport.agent_id,
            passport
                .pane_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "∅".into()),
            passport.generation,
            passport.signed_at_ms
        ));
        if passport.capabilities.is_empty() {
            out.push_str("  (no capabilities declared)\n");
            return out;
        }
        for entry in &passport.capabilities {
            out.push_str(&format!("  {}\n", self.render_entry_text(entry)));
        }
        out
    }

    fn render_entry_text(&self, entry: &CapabilityEntry) -> String {
        let verification_label = match entry.verification {
            CapabilityVerification::Verified => "[verified]",
            CapabilityVerification::Observed => "[observed]",
            CapabilityVerification::Declared => "[declared]",
            CapabilityVerification::Unknown => "[unknown ]",
        };
        let observed_at = entry
            .last_observed_at_ms
            .map(|t| t.to_string())
            .unwrap_or_else(|| "∅".into());
        let freshness = self.freshness_label(entry);
        let proof = if entry.proof.digest_hex.is_empty() {
            "∅".to_string()
        } else {
            entry.proof.display_truncated()
        };
        format!(
            "{verification_label} {} observed_at={observed_at} freshness={freshness} proof={proof}",
            entry.class.label()
        )
    }

    fn freshness_label(&self, entry: &CapabilityEntry) -> &'static str {
        if entry.last_observed_at_ms.is_none() {
            return "unknown";
        }
        if entry.is_fresh(self.now_ms, self.max_age_ms) {
            "fresh"
        } else {
            "stale"
        }
    }

    /// Render a passport as structured JSON (a [`PassportRendering`])
    /// suitable for machine consumption. Equivalent in content to
    /// `render_text` but with each field broken out for downstream
    /// filtering / alerting / dashboarding.
    #[must_use]
    pub fn render(&self, passport: &CapabilityPassport) -> PassportRendering {
        let entries = passport
            .capabilities
            .iter()
            .map(|e| EntryRendering {
                class_label: e.class.label(),
                verification: e.verification,
                confidence_label: e.verification.confidence_label().to_string(),
                last_observed_at_ms: e.last_observed_at_ms,
                freshness: self.freshness_label(e).to_string(),
                proof_digest_truncated: if e.proof.digest_hex.is_empty() {
                    None
                } else {
                    Some(e.proof.display_truncated())
                },
                proof_byte_len: e.proof.byte_len,
            })
            .collect();
        PassportRendering {
            agent_id: passport.agent_id.clone(),
            pane_id: passport.pane_id,
            generation: passport.generation,
            signed_at_ms: passport.signed_at_ms,
            entries,
            evaluated_at_ms: self.now_ms,
            freshness_window_ms: self.max_age_ms,
        }
    }
}

/// Structured doctor rendering of a [`CapabilityPassport`] —
/// human-readable labels alongside machine-parseable fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PassportRendering {
    pub agent_id: String,
    #[serde(default)]
    pub pane_id: Option<u64>,
    pub generation: u64,
    pub signed_at_ms: u64,
    pub entries: Vec<EntryRendering>,
    pub evaluated_at_ms: u64,
    pub freshness_window_ms: u64,
}

/// Per-capability entry rendering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryRendering {
    pub class_label: String,
    pub verification: CapabilityVerification,
    /// `high` / `medium` / `low` / `none` — see
    /// [`CapabilityVerification::confidence_label`].
    pub confidence_label: String,
    #[serde(default)]
    pub last_observed_at_ms: Option<u64>,
    /// `fresh` / `stale` / `unknown`. Computed against the
    /// renderer's `now_ms` + `max_age_ms`.
    pub freshness: String,
    /// First [`crate::capability_passport::REDACTED_PROOF_DISPLAY_LEN`]
    /// chars of the proof's digest + ellipsis. None when the entry
    /// carries an empty proof.
    #[serde(default)]
    pub proof_digest_truncated: Option<String>,
    /// Raw byte length of the underlying proof value. Surfaced so
    /// operators can distinguish "empty proof" from "redacted proof
    /// of some specific length" without leaking the value itself.
    #[serde(default)]
    pub proof_byte_len: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability_passport::{
        CapabilityClass, CapabilityEntry, CapabilityPassport, CapabilityVerification, RedactedProof,
    };

    fn entry(
        class: CapabilityClass,
        v: CapabilityVerification,
        observed: Option<u64>,
        proof: RedactedProof,
    ) -> CapabilityEntry {
        CapabilityEntry {
            class,
            verification: v,
            last_observed_at_ms: observed,
            proof,
        }
    }

    fn diverse_passport() -> CapabilityPassport {
        CapabilityPassport {
            agent_id: "cc1".into(),
            pane_id: Some(7),
            capabilities: vec![
                entry(
                    CapabilityClass::ToolAvailability("bash".into()),
                    CapabilityVerification::Verified,
                    Some(900),
                    RedactedProof::from_value(b"bash-handshake"),
                ),
                entry(
                    CapabilityClass::AuthSurface("openai_api".into()),
                    CapabilityVerification::Observed,
                    Some(850),
                    RedactedProof::empty(),
                ),
                entry(
                    CapabilityClass::SafetyConstraint("no_force_push".into()),
                    CapabilityVerification::Declared,
                    None,
                    RedactedProof::empty(),
                ),
                entry(
                    CapabilityClass::NetworkPolicy("loopback_only".into()),
                    CapabilityVerification::Unknown,
                    None,
                    RedactedProof::empty(),
                ),
            ],
            generation: 3,
            signed_at_ms: 1_500_000,
        }
    }

    #[test]
    fn text_render_carries_one_line_per_entry() {
        let r = PassportDoctorRenderer::at(1_000).with_max_age_ms(5_000);
        let out = r.render_text(&diverse_passport());
        // Header + 4 entries = 5 lines (plus trailing newline).
        assert_eq!(out.lines().count(), 5);
        assert!(out.contains("agent=cc1"));
        assert!(out.contains("pane=7"));
        assert!(out.contains("generation=3"));
    }

    #[test]
    fn text_render_marks_each_verification_state_distinctly() {
        let r = PassportDoctorRenderer::at(1_000).with_max_age_ms(5_000);
        let out = r.render_text(&diverse_passport());
        assert!(out.contains("[verified]"));
        assert!(out.contains("[observed]"));
        assert!(out.contains("[declared]"));
        assert!(out.contains("[unknown ]"));
    }

    #[test]
    fn text_render_freshness_is_stale_when_observation_outside_window() {
        // Verified at 900; renderer now=10_000 with window=5_000 →
        // elapsed 9_100 > 5_000 = stale.
        let r = PassportDoctorRenderer::at(10_000).with_max_age_ms(5_000);
        let out = r.render_text(&diverse_passport());
        assert!(out.contains("freshness=stale"));
    }

    #[test]
    fn text_render_freshness_is_fresh_when_observation_inside_window() {
        // Verified at 900; renderer now=1_000 with window=5_000 →
        // elapsed 100 < 5_000 = fresh.
        let r = PassportDoctorRenderer::at(1_000).with_max_age_ms(5_000);
        let out = r.render_text(&diverse_passport());
        assert!(out.contains("freshness=fresh"));
    }

    #[test]
    fn text_render_freshness_is_unknown_when_no_observation() {
        let r = PassportDoctorRenderer::at(1_000).with_max_age_ms(5_000);
        let out = r.render_text(&diverse_passport());
        // Declared + Unknown entries have last_observed_at_ms = None.
        let unknown_count = out.matches("freshness=unknown").count();
        assert_eq!(unknown_count, 2);
    }

    #[test]
    fn text_render_proof_uses_truncated_digest_never_raw() {
        let r = PassportDoctorRenderer::at(1_000);
        let out = r.render_text(&diverse_passport());
        // bash-handshake produces a deterministic digest; the
        // truncated form is the first 12 hex chars + ellipsis.
        let proof = RedactedProof::from_value(b"bash-handshake");
        let truncated = proof.display_truncated();
        assert!(out.contains(&truncated));
        // And the raw input MUST NOT appear.
        assert!(!out.contains("bash-handshake"));
    }

    #[test]
    fn text_render_empty_passport_shows_no_capabilities_marker() {
        let mut p = diverse_passport();
        p.capabilities.clear();
        let r = PassportDoctorRenderer::at(1_000);
        let out = r.render_text(&p);
        assert!(out.contains("(no capabilities declared)"));
    }

    #[test]
    fn text_render_pane_none_renders_as_glyph() {
        let mut p = diverse_passport();
        p.pane_id = None;
        let r = PassportDoctorRenderer::at(1_000);
        let out = r.render_text(&p);
        assert!(out.contains("pane=∅"));
    }

    #[test]
    fn json_render_carries_evaluated_at_and_window() {
        let r = PassportDoctorRenderer::at(1_500).with_max_age_ms(8_000);
        let rendering = r.render(&diverse_passport());
        assert_eq!(rendering.evaluated_at_ms, 1_500);
        assert_eq!(rendering.freshness_window_ms, 8_000);
        assert_eq!(rendering.entries.len(), 4);
    }

    #[test]
    fn json_render_entries_carry_confidence_label_and_freshness() {
        let r = PassportDoctorRenderer::at(1_000).with_max_age_ms(5_000);
        let rendering = r.render(&diverse_passport());
        // First entry is the bash Verified one — confidence=high, fresh.
        assert_eq!(rendering.entries[0].confidence_label, "high");
        assert_eq!(rendering.entries[0].freshness, "fresh");
        // Last entry is Unknown — confidence=none, freshness=unknown.
        assert_eq!(rendering.entries[3].confidence_label, "none");
        assert_eq!(rendering.entries[3].freshness, "unknown");
    }

    #[test]
    fn json_render_omits_proof_when_empty_carries_truncated_when_present() {
        let r = PassportDoctorRenderer::at(1_000);
        let rendering = r.render(&diverse_passport());
        // bash entry has proof.
        assert!(rendering.entries[0].proof_digest_truncated.is_some());
        assert!(rendering.entries[0].proof_byte_len.is_some());
        // openai_api / safety / net entries are RedactedProof::empty().
        assert!(rendering.entries[1].proof_digest_truncated.is_none());
        assert!(rendering.entries[2].proof_digest_truncated.is_none());
        assert!(rendering.entries[3].proof_digest_truncated.is_none());
    }

    #[test]
    fn json_render_serde_roundtrip_preserves_all_fields() {
        let r = PassportDoctorRenderer::at(1_000).with_max_age_ms(5_000);
        let rendering = r.render(&diverse_passport());
        let json = serde_json::to_string(&rendering).expect("serialize");
        let back: PassportRendering = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, rendering);
    }

    /// SEC fail-closed canary: plant a synthetic credential in a
    /// probe response and assert NEITHER the credential bytes NOR
    /// the identifier appear in EITHER the text OR the JSON
    /// rendering. Pre-fix a renderer that included raw proof bytes
    /// would leak; post-fix every proof access goes through
    /// RedactedProof::display_truncated so the underlying value is
    /// never surfaced.
    #[test]
    fn doctor_render_does_not_leak_planted_credential_ft_ykp2y() {
        let secret = "ANTHROPIC_API_KEY=sk-fake-doctor-canary-1234567890ABCDEFGHIJ";
        let mut p = diverse_passport();
        p.capabilities.push(CapabilityEntry {
            class: CapabilityClass::AuthSurface("anthropic_api".into()),
            verification: CapabilityVerification::Verified,
            last_observed_at_ms: Some(950),
            proof: RedactedProof::from_value(secret.as_bytes()),
        });
        let r = PassportDoctorRenderer::at(1_000);

        let text = r.render_text(&p);
        assert!(
            !text.contains("sk-fake-doctor-canary-1234567890ABCDEFGHIJ"),
            "raw credential leaked into text rendering: {}",
            text
        );
        assert!(
            !text.contains("ANTHROPIC_API_KEY"),
            "credential identifier leaked into text rendering: {}",
            text
        );

        let json_value = serde_json::to_string(&r.render(&p)).expect("serialize");
        assert!(
            !json_value.contains("sk-fake-doctor-canary-1234567890ABCDEFGHIJ"),
            "raw credential leaked into JSON rendering: {}",
            json_value
        );
        assert!(
            !json_value.contains("ANTHROPIC_API_KEY"),
            "credential identifier leaked into JSON rendering: {}",
            json_value
        );
    }
}
