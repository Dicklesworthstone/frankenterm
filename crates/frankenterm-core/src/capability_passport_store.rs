//! Capability passport store + validation + persistence
//! (ft-1650n.1 slice 2).
//!
//! Sits atop the schema in `crate::capability_passport`. Provides:
//!
//! - [`PassportStore`]: thread-safe in-memory collection keyed by
//!   `(agent_id, pane_id)`.
//! - [`PassportValidator`]: structural validation (capability count
//!   cap, generation monotonicity, clock-skew bound) with explicit
//!   fail-closed error variants.
//! - JSONL persistence helpers ([`to_jsonl`] / [`from_jsonl`]) for
//!   import / export — one passport per newline-delimited line so
//!   stores can be appended incrementally without rewriting existing
//!   bytes.
//!
//! The full SQLite integration (durable persistence with upsert /
//! query / migration) is deferred to a follow-up bead; this slice
//! provides the in-memory store + validation + line-format helpers
//! that durable persistence will build on top of.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use crate::capability_passport::CapabilityPassport;

/// Stable composite key into the [`PassportStore`]. A passport may
/// be agent-scoped (pane-less) when it captures capabilities that
/// apply to every pane the agent operates, or pane-scoped when the
/// capability set differs per pane.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PassportKey {
    pub agent_id: String,
    #[serde(default)]
    pub pane_id: Option<u64>,
}

impl PassportKey {
    #[must_use]
    pub fn agent(agent_id: impl Into<String>) -> Self {
        Self {
            agent_id: agent_id.into(),
            pane_id: None,
        }
    }

    #[must_use]
    pub fn pane(agent_id: impl Into<String>, pane_id: u64) -> Self {
        Self {
            agent_id: agent_id.into(),
            pane_id: Some(pane_id),
        }
    }
}

impl From<&CapabilityPassport> for PassportKey {
    fn from(p: &CapabilityPassport) -> Self {
        Self {
            agent_id: p.agent_id.clone(),
            pane_id: p.pane_id,
        }
    }
}

/// Thread-safe in-memory store of [`CapabilityPassport`]s keyed by
/// [`PassportKey`]. Cloning the store is cheap — the underlying
/// `Arc<RwLock<...>>` shares state between clones.
#[derive(Debug, Clone, Default)]
pub struct PassportStore {
    inner: Arc<RwLock<HashMap<PassportKey, CapabilityPassport>>>,
}

impl PassportStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a passport. Returns the previous passport at this key
    /// (if any) so callers can compare generations.
    ///
    /// Note: this method does NOT validate. Use [`PassportValidator`]
    /// before insert if validation is required.
    pub fn insert(&self, passport: CapabilityPassport) -> Option<CapabilityPassport> {
        let key = PassportKey::from(&passport);
        let Ok(mut inner) = self.inner.write() else {
            return None;
        };
        inner.insert(key, passport)
    }

    /// Get a snapshot clone of the passport at `key`, if present.
    #[must_use]
    pub fn get(&self, key: &PassportKey) -> Option<CapabilityPassport> {
        let inner = self.inner.read().ok()?;
        inner.get(key).cloned()
    }

    /// Remove the passport at `key`, returning it if present.
    pub fn remove(&self, key: &PassportKey) -> Option<CapabilityPassport> {
        let mut inner = self.inner.write().ok()?;
        inner.remove(key)
    }

    /// Number of passports currently held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.read().map(|i| i.len()).unwrap_or(0)
    }

    /// True iff the store holds no passports.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Snapshot of all passport keys currently held. Useful for
    /// iteration without holding the read lock.
    #[must_use]
    pub fn keys(&self) -> Vec<PassportKey> {
        self.inner
            .read()
            .map(|i| i.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Snapshot of all passports currently held. Allocates `O(N)`.
    #[must_use]
    pub fn snapshot(&self) -> Vec<CapabilityPassport> {
        self.inner
            .read()
            .map(|i| i.values().cloned().collect())
            .unwrap_or_default()
    }
}

/// Default cap on the number of capability entries permitted in a
/// single passport. Above this threshold the validator rejects the
/// passport — protects against memory exhaustion from a malicious or
/// buggy producer that ships unbounded capability lists.
pub const DEFAULT_MAX_CAPABILITIES_PER_PASSPORT: usize = 1_024;

/// Default permitted clock skew between a passport's `signed_at_ms`
/// and the validator's `now_ms`. Passports signed more than this
/// many milliseconds in the future are rejected. Past timestamps
/// are not bounded by this constant — those are governed by the
/// freshness gate in `CapabilityEntry::is_fresh`.
pub const DEFAULT_MAX_CLOCK_SKEW_MS: u64 = 60_000;

/// Structural validation outcomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    /// Passport carries zero capabilities — nothing to assert.
    NoCapabilities,
    /// Passport's `signed_at_ms` is more than `max_clock_skew_ms`
    /// in the future relative to the validator's `now_ms`.
    SignedAtTooFarInFuture {
        signed_at_ms: u64,
        now_ms: u64,
        skew_ms: u64,
        max_skew_ms: u64,
    },
    /// Passport carries more capability entries than the configured
    /// cap. Defense against DoS via unbounded capability lists.
    TooManyCapabilities { count: usize, cap: usize },
    /// Generation must be strictly greater than the prior generation
    /// for the same `(agent_id, pane_id)` key. Stale or replayed
    /// passports must NOT overwrite a fresher entry.
    GenerationNonMonotonic {
        existing_generation: u64,
        incoming_generation: u64,
    },
    /// Empty agent_id — passports must always identify their agent.
    EmptyAgentId,
}

/// Validator for incoming passports.
#[derive(Debug, Clone)]
pub struct PassportValidator {
    pub max_capabilities: usize,
    pub max_clock_skew_ms: u64,
}

impl Default for PassportValidator {
    fn default() -> Self {
        Self {
            max_capabilities: DEFAULT_MAX_CAPABILITIES_PER_PASSPORT,
            max_clock_skew_ms: DEFAULT_MAX_CLOCK_SKEW_MS,
        }
    }
}

impl PassportValidator {
    /// Validate `incoming` against the optional `existing` passport
    /// at the same key (None when this is a first-time insert).
    /// `now_ms` is the validator's view of current time used for the
    /// clock-skew bound.
    pub fn validate(
        &self,
        incoming: &CapabilityPassport,
        existing: Option<&CapabilityPassport>,
        now_ms: u64,
    ) -> Result<(), ValidationError> {
        if incoming.agent_id.is_empty() {
            return Err(ValidationError::EmptyAgentId);
        }
        if incoming.capabilities.is_empty() {
            return Err(ValidationError::NoCapabilities);
        }
        if incoming.capabilities.len() > self.max_capabilities {
            return Err(ValidationError::TooManyCapabilities {
                count: incoming.capabilities.len(),
                cap: self.max_capabilities,
            });
        }
        if incoming.signed_at_ms > now_ms {
            let skew = incoming.signed_at_ms - now_ms;
            if skew > self.max_clock_skew_ms {
                return Err(ValidationError::SignedAtTooFarInFuture {
                    signed_at_ms: incoming.signed_at_ms,
                    now_ms,
                    skew_ms: skew,
                    max_skew_ms: self.max_clock_skew_ms,
                });
            }
        }
        if let Some(prior) = existing
            && incoming.generation <= prior.generation
        {
            return Err(ValidationError::GenerationNonMonotonic {
                existing_generation: prior.generation,
                incoming_generation: incoming.generation,
            });
        }
        Ok(())
    }
}

/// Serialize a slice of passports as newline-delimited JSON (JSONL).
/// One passport per line, no trailing newline. Each line is independent
/// — appending more passports to a JSONL store is just byte-append.
pub fn to_jsonl(passports: &[CapabilityPassport]) -> Result<String, serde_json::Error> {
    let mut buf = String::new();
    for (idx, passport) in passports.iter().enumerate() {
        if idx > 0 {
            buf.push('\n');
        }
        let line = serde_json::to_string(passport)?;
        buf.push_str(&line);
    }
    Ok(buf)
}

/// Parse a JSONL byte buffer into a vector of passports.
/// Empty lines (including a trailing newline) are skipped.
/// Returns the first parse error encountered with its line number.
pub fn from_jsonl(jsonl: &str) -> Result<Vec<CapabilityPassport>, JsonlParseError> {
    let mut out = Vec::new();
    for (idx, raw_line) in jsonl.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<CapabilityPassport>(line) {
            Ok(p) => out.push(p),
            Err(source) => {
                return Err(JsonlParseError {
                    line_number: idx + 1,
                    source,
                });
            }
        }
    }
    Ok(out)
}

/// Error type returned by [`from_jsonl`] when a line fails to parse.
#[derive(Debug)]
pub struct JsonlParseError {
    pub line_number: usize,
    pub source: serde_json::Error,
}

impl std::fmt::Display for JsonlParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JSONL parse error at line {}: {}", self.line_number, self.source)
    }
}

impl std::error::Error for JsonlParseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability_passport::{
        CapabilityClass, CapabilityEntry, CapabilityVerification, RedactedProof,
    };

    fn make_passport(agent: &str, pane: Option<u64>, generation: u64) -> CapabilityPassport {
        CapabilityPassport {
            agent_id: agent.into(),
            pane_id: pane,
            capabilities: vec![CapabilityEntry {
                class: CapabilityClass::ToolAvailability("bash".into()),
                verification: CapabilityVerification::Verified,
                last_observed_at_ms: Some(1_000),
                proof: RedactedProof::empty(),
            }],
            generation,
            signed_at_ms: 2_000,
        }
    }

    // ── PassportKey ────────────────────────────────────────────────────────

    #[test]
    fn passport_key_distinguishes_agent_scope_from_pane_scope() {
        let agent_only = PassportKey::agent("cc1");
        let pane_one = PassportKey::pane("cc1", 1);
        let pane_two = PassportKey::pane("cc1", 2);
        assert_ne!(agent_only, pane_one);
        assert_ne!(pane_one, pane_two);
        assert_eq!(pane_one, PassportKey::pane("cc1", 1));
    }

    #[test]
    fn passport_key_from_passport_carries_agent_and_pane() {
        let p = make_passport("cc1", Some(7), 1);
        let key = PassportKey::from(&p);
        assert_eq!(key.agent_id, "cc1");
        assert_eq!(key.pane_id, Some(7));
    }

    // ── PassportStore ──────────────────────────────────────────────────────

    #[test]
    fn store_insert_and_get_roundtrip() {
        let store = PassportStore::new();
        let p = make_passport("cc1", Some(3), 1);
        assert!(store.insert(p.clone()).is_none());

        let got = store.get(&PassportKey::pane("cc1", 3)).expect("present");
        assert_eq!(got, p);
    }

    #[test]
    fn store_insert_returns_prior_passport() {
        let store = PassportStore::new();
        let v1 = make_passport("cc1", Some(3), 1);
        let v2 = make_passport("cc1", Some(3), 2);
        assert!(store.insert(v1.clone()).is_none());
        let prior = store.insert(v2).expect("prior present");
        assert_eq!(prior, v1);
    }

    #[test]
    fn store_remove_returns_passport_when_present() {
        let store = PassportStore::new();
        let p = make_passport("cc1", None, 1);
        store.insert(p.clone());
        assert_eq!(store.len(), 1);
        let removed = store.remove(&PassportKey::agent("cc1")).expect("present");
        assert_eq!(removed, p);
        assert!(store.is_empty());
        assert!(store.remove(&PassportKey::agent("cc1")).is_none());
    }

    #[test]
    fn store_keys_lists_all_inserted_keys() {
        let store = PassportStore::new();
        store.insert(make_passport("cc1", Some(1), 1));
        store.insert(make_passport("cc1", Some(2), 1));
        store.insert(make_passport("cc2", None, 1));
        let mut keys = store.keys();
        keys.sort_by(|a, b| a.agent_id.cmp(&b.agent_id).then(a.pane_id.cmp(&b.pane_id)));
        assert_eq!(keys.len(), 3);
        assert_eq!(keys[0], PassportKey::pane("cc1", 1));
        assert_eq!(keys[1], PassportKey::pane("cc1", 2));
        assert_eq!(keys[2], PassportKey::agent("cc2"));
    }

    #[test]
    fn store_snapshot_returns_value_clones() {
        let store = PassportStore::new();
        store.insert(make_passport("cc1", Some(1), 1));
        let snap = store.snapshot();
        assert_eq!(snap.len(), 1);
        // Mutating the snapshot doesn't affect the store.
        let mut local = snap;
        local.clear();
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn store_clones_share_state() {
        let s1 = PassportStore::new();
        let s2 = s1.clone();
        s1.insert(make_passport("cc1", Some(9), 1));
        assert_eq!(s2.len(), 1, "Arc<RwLock<...>> shares state across clones");
    }

    // ── PassportValidator ──────────────────────────────────────────────────

    #[test]
    fn validator_accepts_well_formed_passport() {
        let v = PassportValidator::default();
        let p = make_passport("cc1", Some(1), 1);
        v.validate(&p, None, 5_000).expect("well-formed should pass");
    }

    #[test]
    fn validator_rejects_empty_agent_id() {
        let v = PassportValidator::default();
        let mut p = make_passport("cc1", None, 1);
        p.agent_id = String::new();
        assert_eq!(v.validate(&p, None, 5_000), Err(ValidationError::EmptyAgentId));
    }

    #[test]
    fn validator_rejects_zero_capabilities() {
        let v = PassportValidator::default();
        let mut p = make_passport("cc1", None, 1);
        p.capabilities.clear();
        assert_eq!(
            v.validate(&p, None, 5_000),
            Err(ValidationError::NoCapabilities)
        );
    }

    #[test]
    fn validator_rejects_capability_count_above_cap() {
        let v = PassportValidator {
            max_capabilities: 3,
            ..PassportValidator::default()
        };
        let mut p = make_passport("cc1", None, 1);
        // Inflate: original has 1; pad to 5 by cloning.
        let only = p.capabilities[0].clone();
        for _ in 0..4 {
            p.capabilities.push(only.clone());
        }
        assert_eq!(
            v.validate(&p, None, 5_000),
            Err(ValidationError::TooManyCapabilities { count: 5, cap: 3 })
        );
    }

    #[test]
    fn validator_rejects_signed_at_too_far_in_future() {
        let v = PassportValidator {
            max_clock_skew_ms: 1_000,
            ..PassportValidator::default()
        };
        let mut p = make_passport("cc1", None, 1);
        p.signed_at_ms = 5_000;
        let now_ms = 1_000;
        let result = v.validate(&p, None, now_ms);
        assert_eq!(
            result,
            Err(ValidationError::SignedAtTooFarInFuture {
                signed_at_ms: 5_000,
                now_ms: 1_000,
                skew_ms: 4_000,
                max_skew_ms: 1_000,
            })
        );
    }

    #[test]
    fn validator_accepts_signed_at_within_clock_skew_window() {
        let v = PassportValidator {
            max_clock_skew_ms: 5_000,
            ..PassportValidator::default()
        };
        let mut p = make_passport("cc1", None, 1);
        p.signed_at_ms = 3_000;
        // now_ms = 1_000 → 2_000 ms ahead, within 5_000 window.
        v.validate(&p, None, 1_000).expect("within skew window");
    }

    #[test]
    fn validator_accepts_past_signed_at_unconditionally() {
        // The clock-skew bound is one-sided: only future signatures
        // are rejected. Past signatures are valid (freshness is
        // governed elsewhere via CapabilityEntry::is_fresh).
        let v = PassportValidator::default();
        let mut p = make_passport("cc1", None, 1);
        p.signed_at_ms = 100;
        v.validate(&p, None, 100_000_000).expect("past signature is fine");
    }

    #[test]
    fn validator_rejects_non_monotonic_generation() {
        let v = PassportValidator::default();
        let prior = make_passport("cc1", Some(1), 5);
        let same = make_passport("cc1", Some(1), 5);
        let lower = make_passport("cc1", Some(1), 4);
        let higher = make_passport("cc1", Some(1), 6);

        // Equal generation → reject.
        assert_eq!(
            v.validate(&same, Some(&prior), 5_000),
            Err(ValidationError::GenerationNonMonotonic {
                existing_generation: 5,
                incoming_generation: 5,
            })
        );
        // Lower generation → reject.
        assert_eq!(
            v.validate(&lower, Some(&prior), 5_000),
            Err(ValidationError::GenerationNonMonotonic {
                existing_generation: 5,
                incoming_generation: 4,
            })
        );
        // Higher generation → accept.
        v.validate(&higher, Some(&prior), 5_000)
            .expect("higher generation accepted");
    }

    // ── JSONL persistence ──────────────────────────────────────────────────

    #[test]
    fn jsonl_roundtrip_preserves_passport_set() {
        let passports = vec![
            make_passport("cc1", Some(1), 1),
            make_passport("cc1", Some(2), 1),
            make_passport("cc2", None, 3),
        ];
        let serialized = to_jsonl(&passports).expect("serialize");
        // One line per passport — count newlines.
        assert_eq!(serialized.lines().count(), 3);
        let deserialized = from_jsonl(&serialized).expect("deserialize");
        assert_eq!(deserialized, passports);
    }

    #[test]
    fn jsonl_handles_empty_input() {
        let parsed = from_jsonl("").expect("empty input should parse cleanly");
        assert!(parsed.is_empty());
        let parsed_blank_lines = from_jsonl("\n\n\n").expect("blank lines should be skipped");
        assert!(parsed_blank_lines.is_empty());
    }

    #[test]
    fn jsonl_handles_trailing_newline_without_empty_passport() {
        let passports = vec![make_passport("cc1", Some(1), 1)];
        let serialized = to_jsonl(&passports).expect("serialize");
        let with_trailing = format!("{serialized}\n");
        let parsed = from_jsonl(&with_trailing).expect("trailing newline should be tolerated");
        assert_eq!(parsed, passports);
    }

    #[test]
    fn jsonl_parse_error_carries_line_number() {
        let passports = vec![make_passport("cc1", Some(1), 1)];
        let mut serialized = to_jsonl(&passports).expect("serialize");
        serialized.push_str("\nthis is not json");
        let err = from_jsonl(&serialized).expect_err("malformed line should error");
        // Line numbering: passport on line 1, malformed on line 2.
        assert_eq!(err.line_number, 2);
    }

    #[test]
    fn jsonl_supports_append_only_concatenation() {
        // Demonstrates the "byte-append" property — concatenating two
        // separately-serialized JSONL chunks (with a newline between)
        // yields the same set as serializing the union.
        let chunk_a = vec![make_passport("cc1", Some(1), 1)];
        let chunk_b = vec![make_passport("cc1", Some(2), 1), make_passport("cc2", None, 1)];

        let sa = to_jsonl(&chunk_a).unwrap();
        let sb = to_jsonl(&chunk_b).unwrap();
        let concatenated = format!("{sa}\n{sb}");
        let merged = from_jsonl(&concatenated).expect("concat parses");

        let mut union = chunk_a;
        union.extend(chunk_b);
        assert_eq!(merged, union);
    }
}
