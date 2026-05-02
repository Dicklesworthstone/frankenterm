//! Substrate types for the daemon-side `RobotProfile.apply` RPC
//! (ft-4iz0q).
//!
//! The CLI handler at `crates/frankenterm/src/main.rs` turns
//! `RobotProfileCommands::Apply` into a typed request the daemon
//! picks up over its existing IPC channel; the daemon responds with
//! either a fresh apply receipt (when the spawn is new) or a replay
//! of the prior receipt (when the request's content hash matches a
//! row in the `profiles_applied_log` table). Both sides exchange
//! the types defined here so the wire shape lives in one place.
//!
//! ## What ships in this slice
//!
//! - [`ProfileApplySpawnRequest`] — the typed request envelope.
//!   Mirrors [`crate::robot_ntm_surface::ProfileApplyRequest`] but
//!   carries the resolved `AgentProfile` snapshot the daemon needs
//!   to spawn against, plus the content hash that gates idempotency.
//! - [`ProfileApplySpawnReceipt`] — the success-side response,
//!   matching the bead's "ApplyReceipt content-hash to detect
//!   duplicates" requirement.
//! - [`ProfileApplySpawnOutcome`] — the wire envelope unifying
//!   `FreshApply` / `IdempotentReplay` / `Failed` so the CLI handler
//!   has one match arm to translate into a `RobotResponse`.
//! - [`compute_apply_receipt_hash`] — pure SHA-256 of the
//!   canonical request shape. Stable across daemon restarts so the
//!   `profiles_applied_log` lookup keeps working past process
//!   boundaries.
//!
//! ## What is deferred
//!
//! - The daemon-side handler that translates a request into mux
//!   spawn calls and writes to `profiles_applied_log` (the actual
//!   "wire" part of the bead title).
//! - The CLI translator from `ProfileApplySpawnOutcome` into the
//!   `RobotResponse<serde_json::Value>` envelope (lives in
//!   `main.rs`, depends on the daemon-side handler shipping first).
//! - `profiles_applied_log` migration in `storage.rs`.
//!
//! Those slices land under the same bead but are sequenced after
//! the substrate types here so the daemon-side handler has a
//! typed surface to plug into.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::agent_profiles::AgentProfile;

/// Daemon-side request envelope. The CLI builds one of these from
/// the `RobotProfileCommands::Apply` invocation, looks up the
/// profile by name, then sends the request to the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileApplySpawnRequest {
    /// Frozen profile snapshot at request time. The CLI reads this
    /// from `agent_profiles` before sending so the daemon does not
    /// race against a concurrent profile edit mid-spawn.
    pub profile: AgentProfile,
    /// Number of panes to spawn. Mirrors
    /// [`crate::robot_ntm_surface::ProfileApplyRequest::count`].
    pub count: u32,
    /// Per-request env overlay. Wins over `profile.env` per the
    /// bead's spec.
    pub env_overrides: BTreeMap<String, String>,
    /// Content-addressed idempotency key. Computed via
    /// [`compute_apply_receipt_hash`] from the request's stable
    /// fields so duplicate apply attempts hash to the same value
    /// and the daemon can short-circuit to a replay receipt.
    pub content_hash: String,
}

impl ProfileApplySpawnRequest {
    /// Construct a request and pre-compute the content hash. Use
    /// this constructor at the CLI side so the daemon never sees a
    /// request whose claimed hash disagrees with its content
    /// (asserted on the daemon side via [`Self::verify_hash`]).
    #[must_use]
    pub fn new(
        profile: AgentProfile,
        count: u32,
        env_overrides: BTreeMap<String, String>,
    ) -> Self {
        let content_hash = compute_apply_receipt_hash(&profile, count, &env_overrides);
        Self {
            profile,
            count,
            env_overrides,
            content_hash,
        }
    }

    /// Daemon-side guard: recompute the content hash from the
    /// request's payload and reject the request when it disagrees
    /// with the claimed value. Defends against tampered requests +
    /// against bugs in the CLI's hash construction.
    #[must_use]
    pub fn verify_hash(&self) -> bool {
        compute_apply_receipt_hash(&self.profile, self.count, &self.env_overrides)
            == self.content_hash
    }
}

/// Successful apply receipt. Stored in `profiles_applied_log` keyed
/// by `content_hash`; subsequent requests with the same hash
/// short-circuit to an [`ProfileApplySpawnOutcome::IdempotentReplay`]
/// of this row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileApplySpawnReceipt {
    pub profile_name: String,
    /// Content hash that gated the spawn; matches the request's
    /// `content_hash`.
    pub content_hash: String,
    /// Pane IDs the daemon spawned for this request, in spawn
    /// order.
    pub panes_spawned: Vec<u64>,
    /// Wall-clock ms-since-epoch when the spawn finished. Carried
    /// so a replay can surface "this apply ran N hours ago".
    pub finished_at_ms: u64,
}

/// Wire envelope unifying every terminal outcome of an apply RPC.
/// The CLI translator at `main.rs` matches on this once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProfileApplySpawnOutcome {
    /// First-time apply. Spawned `panes_spawned.len()` panes.
    FreshApply { receipt: ProfileApplySpawnReceipt },
    /// Hash-collision short-circuit. Receipt carries the original
    /// pane IDs so the operator can re-attach if needed; no panes
    /// were spawned this round.
    IdempotentReplay { receipt: ProfileApplySpawnReceipt },
    /// Spawn failed mid-flight. `panes_spawned` is `None` because
    /// the bead's "MustNotPartiallyMutate" failure-semantic
    /// requires the daemon to clean up partial spawns before
    /// returning. `reason` is operator-friendly; `error_code` is
    /// the stable wire-level discriminant.
    Failed { error_code: String, reason: String },
}

impl ProfileApplySpawnOutcome {
    /// Stable error code mirroring
    /// [`crate::robot_profile_handler::ProfileHandlerError::error_code`]
    /// for the success-paths. Failed variants carry their own code
    /// in-band so this returns it verbatim.
    #[must_use]
    pub fn error_code(&self) -> Option<&str> {
        match self {
            Self::FreshApply { .. } | Self::IdempotentReplay { .. } => None,
            Self::Failed { error_code, .. } => Some(error_code.as_str()),
        }
    }

    /// Convenience accessor: returns the receipt for the success
    /// variants, `None` for `Failed`.
    #[must_use]
    pub fn receipt(&self) -> Option<&ProfileApplySpawnReceipt> {
        match self {
            Self::FreshApply { receipt } | Self::IdempotentReplay { receipt } => {
                Some(receipt)
            }
            Self::Failed { .. } => None,
        }
    }
}

/// Pure SHA-256 over the request's canonical content. Defines the
/// idempotency key the daemon's `profiles_applied_log` keys on.
///
/// The hash *deliberately* excludes wall-clock fields like the
/// profile's `created_at_ms` / `updated_at_ms` — two applies of the
/// same profile content with the same count + overrides must hash
/// to the same value even if the profile row was touched between
/// invocations. The opposite would mean a no-op profile edit
/// breaks the idempotency contract.
#[must_use]
pub fn compute_apply_receipt_hash(
    profile: &AgentProfile,
    count: u32,
    env_overrides: &BTreeMap<String, String>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"ft-b0g7g.apply.v1\0");
    hasher.update(profile.name.as_bytes());
    hasher.update(b"\0");
    hasher.update(profile.role.as_bytes());
    hasher.update(b"\0");
    // Tags + env + metadata: canonicalize ordering so two
    // semantically-identical profiles with a different HashMap
    // iteration order hash the same.
    let mut sorted_tags = profile.tags.clone();
    sorted_tags.sort();
    for tag in &sorted_tags {
        hasher.update(tag.as_bytes());
        hasher.update(b"\0");
    }
    hasher.update(b"|");
    hasher.update(profile.shell.as_bytes());
    hasher.update(b"\0");
    hasher.update(profile.command.as_deref().unwrap_or("").as_bytes());
    hasher.update(b"\0");
    let mut sorted_env: Vec<(&String, &String)> = profile.env.iter().collect();
    sorted_env.sort_by(|a, b| a.0.cmp(b.0));
    for (k, v) in &sorted_env {
        hasher.update(k.as_bytes());
        hasher.update(b"=");
        hasher.update(v.as_bytes());
        hasher.update(b"\0");
    }
    hasher.update(b"|");
    let mut sorted_meta: Vec<(&String, &String)> = profile.metadata.iter().collect();
    sorted_meta.sort_by(|a, b| a.0.cmp(b.0));
    for (k, v) in &sorted_meta {
        hasher.update(k.as_bytes());
        hasher.update(b"=");
        hasher.update(v.as_bytes());
        hasher.update(b"\0");
    }
    hasher.update(b"|");
    hasher.update(count.to_le_bytes());
    hasher.update(b"|");
    // env_overrides is already a BTreeMap, so iteration order is
    // deterministic. Hash anyway with the same shape.
    for (k, v) in env_overrides {
        hasher.update(k.as_bytes());
        hasher.update(b"=");
        hasher.update(v.as_bytes());
        hasher.update(b"\0");
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn synth_profile(name: &str) -> AgentProfile {
        let mut env = HashMap::new();
        env.insert("EDITOR".to_string(), "vim".to_string());
        let mut metadata = HashMap::new();
        metadata.insert("description".to_string(), format!("synth {name}"));
        AgentProfile {
            name: name.to_string(),
            role: "agent".to_string(),
            tags: vec!["t1".to_string()],
            shell: "/bin/sh".to_string(),
            command: Some("echo".to_string()),
            env,
            metadata,
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    #[test]
    fn hash_is_deterministic_across_calls() {
        let p = synth_profile("a");
        let mut overrides = BTreeMap::new();
        overrides.insert("X".to_string(), "1".to_string());
        let h1 = compute_apply_receipt_hash(&p, 3, &overrides);
        let h2 = compute_apply_receipt_hash(&p, 3, &overrides);
        assert_eq!(h1, h2);
        // Sanity: it is a 64-char lower-hex SHA-256.
        assert_eq!(h1.len(), 64);
        assert!(h1.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn hash_changes_when_count_changes() {
        let p = synth_profile("a");
        let overrides = BTreeMap::new();
        let h1 = compute_apply_receipt_hash(&p, 3, &overrides);
        let h2 = compute_apply_receipt_hash(&p, 4, &overrides);
        assert_ne!(h1, h2);
    }

    #[test]
    fn hash_changes_when_env_overrides_change() {
        let p = synth_profile("a");
        let mut o1 = BTreeMap::new();
        o1.insert("X".to_string(), "1".to_string());
        let mut o2 = BTreeMap::new();
        o2.insert("X".to_string(), "2".to_string());
        assert_ne!(
            compute_apply_receipt_hash(&p, 1, &o1),
            compute_apply_receipt_hash(&p, 1, &o2),
        );
    }

    #[test]
    fn hash_ignores_profile_timestamps() {
        // Two profiles with identical content but different
        // created_at_ms / updated_at_ms must hash the same — a
        // no-op profile touch must not break idempotency.
        let mut a = synth_profile("a");
        let mut b = synth_profile("a");
        a.created_at_ms = 1_700_000_000_000;
        a.updated_at_ms = 1_700_000_000_000;
        b.created_at_ms = 1_800_000_000_000;
        b.updated_at_ms = 1_800_000_000_000;
        let overrides = BTreeMap::new();
        assert_eq!(
            compute_apply_receipt_hash(&a, 1, &overrides),
            compute_apply_receipt_hash(&b, 1, &overrides),
        );
    }

    #[test]
    fn hash_independent_of_env_hashmap_iteration_order() {
        // Build two profiles whose `env` HashMaps were populated in
        // a different order. The hash must collapse the difference.
        let mut a = synth_profile("a");
        let mut b = synth_profile("a");
        a.env.clear();
        a.env.insert("A".to_string(), "1".to_string());
        a.env.insert("B".to_string(), "2".to_string());
        b.env.clear();
        b.env.insert("B".to_string(), "2".to_string());
        b.env.insert("A".to_string(), "1".to_string());
        let overrides = BTreeMap::new();
        assert_eq!(
            compute_apply_receipt_hash(&a, 1, &overrides),
            compute_apply_receipt_hash(&b, 1, &overrides),
        );
    }

    #[test]
    fn hash_independent_of_tags_order() {
        let mut a = synth_profile("a");
        let mut b = synth_profile("a");
        a.tags = vec!["x".into(), "y".into(), "z".into()];
        b.tags = vec!["z".into(), "x".into(), "y".into()];
        let overrides = BTreeMap::new();
        assert_eq!(
            compute_apply_receipt_hash(&a, 1, &overrides),
            compute_apply_receipt_hash(&b, 1, &overrides),
        );
    }

    #[test]
    fn hash_distinguishes_profile_names() {
        let a = synth_profile("a");
        let b = synth_profile("b");
        let overrides = BTreeMap::new();
        assert_ne!(
            compute_apply_receipt_hash(&a, 1, &overrides),
            compute_apply_receipt_hash(&b, 1, &overrides),
        );
    }

    #[test]
    fn request_new_pre_computes_matching_hash() {
        let p = synth_profile("a");
        let mut overrides = BTreeMap::new();
        overrides.insert("K".to_string(), "V".to_string());
        let req = ProfileApplySpawnRequest::new(p.clone(), 2, overrides.clone());
        assert_eq!(
            req.content_hash,
            compute_apply_receipt_hash(&p, 2, &overrides),
        );
        assert!(req.verify_hash());
    }

    #[test]
    fn request_verify_hash_rejects_tampering() {
        let req = {
            let mut r = ProfileApplySpawnRequest::new(
                synth_profile("a"),
                1,
                BTreeMap::new(),
            );
            r.count = 99; // tamper
            r
        };
        assert!(!req.verify_hash());
    }

    #[test]
    fn outcome_serde_roundtrips_through_json() {
        let receipt = ProfileApplySpawnReceipt {
            profile_name: "a".to_string(),
            content_hash: "deadbeef".to_string(),
            panes_spawned: vec![1, 2, 3],
            finished_at_ms: 1_700_000_000_000,
        };
        let outcomes = vec![
            ProfileApplySpawnOutcome::FreshApply {
                receipt: receipt.clone(),
            },
            ProfileApplySpawnOutcome::IdempotentReplay {
                receipt: receipt.clone(),
            },
            ProfileApplySpawnOutcome::Failed {
                error_code: "robot.profile.spawn_failed".to_string(),
                reason: "mux denied".to_string(),
            },
        ];
        for outcome in &outcomes {
            let s = serde_json::to_string(outcome).unwrap();
            let parsed: ProfileApplySpawnOutcome = serde_json::from_str(&s).unwrap();
            assert_eq!(&parsed, outcome);
        }
    }

    #[test]
    fn outcome_error_code_and_receipt_accessors() {
        let receipt = ProfileApplySpawnReceipt {
            profile_name: "a".to_string(),
            content_hash: "h".to_string(),
            panes_spawned: vec![7],
            finished_at_ms: 1,
        };
        let fresh = ProfileApplySpawnOutcome::FreshApply {
            receipt: receipt.clone(),
        };
        let replay = ProfileApplySpawnOutcome::IdempotentReplay {
            receipt: receipt.clone(),
        };
        let failed = ProfileApplySpawnOutcome::Failed {
            error_code: "robot.profile.spawn_failed".to_string(),
            reason: "x".to_string(),
        };
        assert_eq!(fresh.error_code(), None);
        assert_eq!(replay.error_code(), None);
        assert_eq!(failed.error_code(), Some("robot.profile.spawn_failed"));
        assert_eq!(fresh.receipt(), Some(&receipt));
        assert_eq!(replay.receipt(), Some(&receipt));
        assert_eq!(failed.receipt(), None);
    }

    #[test]
    fn outcome_serde_uses_snake_case_kind_tag() {
        let receipt = ProfileApplySpawnReceipt {
            profile_name: "a".to_string(),
            content_hash: "h".to_string(),
            panes_spawned: vec![],
            finished_at_ms: 0,
        };
        let v = serde_json::to_value(ProfileApplySpawnOutcome::FreshApply { receipt })
            .unwrap();
        assert_eq!(v["kind"].as_str(), Some("fresh_apply"));
    }
}
