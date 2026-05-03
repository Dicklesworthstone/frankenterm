use std::collections::{BTreeMap, HashMap};

use frankenterm_core::agent_profiles::AgentProfile;
use frankenterm_core::robot_profile_apply_rpc::{
    ProfileApplySpawnOutcome, ProfileApplySpawnReceipt, ProfileApplySpawnRequest,
    compute_apply_receipt_hash,
};
use proptest::prelude::*;

fn small_text() -> impl Strategy<Value = String> {
    "[A-Za-z0-9_./:= -]{0,48}".prop_map(String::from)
}

fn profile_name() -> impl Strategy<Value = String> {
    "[A-Za-z0-9_-]{1,32}".prop_map(String::from)
}

fn map_entry() -> impl Strategy<Value = (String, String)> {
    ("[A-Z_][A-Z0-9_]{0,12}", small_text()).prop_map(|(key, value)| (key, value))
}

fn hash_map() -> impl Strategy<Value = HashMap<String, String>> {
    prop::collection::hash_map("[A-Z_][A-Z0-9_]{0,12}", small_text(), 0..12)
}

fn btree_map() -> impl Strategy<Value = BTreeMap<String, String>> {
    prop::collection::btree_map("[A-Z_][A-Z0-9_]{0,12}", small_text(), 0..12)
}

fn profile() -> impl Strategy<Value = AgentProfile> {
    (
        profile_name(),
        small_text(),
        prop::collection::vec(small_text(), 0..12),
        small_text(),
        prop::option::of(small_text()),
        hash_map(),
        hash_map(),
        any::<i64>(),
        any::<i64>(),
    )
        .prop_map(
            |(name, role, tags, shell, command, env, metadata, created_at_ms, updated_at_ms)| {
                AgentProfile {
                    name,
                    role,
                    tags,
                    shell,
                    command,
                    env,
                    metadata,
                    created_at_ms,
                    updated_at_ms,
                }
            },
        )
}

fn receipt() -> impl Strategy<Value = ProfileApplySpawnReceipt> {
    (
        profile_name(),
        "[0-9a-f]{64}",
        prop::collection::vec(any::<u64>(), 0..16),
        any::<u64>(),
    )
        .prop_map(
            |(profile_name, content_hash, panes_spawned, finished_at_ms)| {
                ProfileApplySpawnReceipt {
                    profile_name,
                    content_hash,
                    panes_spawned,
                    finished_at_ms,
                }
            },
        )
}

fn outcome() -> impl Strategy<Value = ProfileApplySpawnOutcome> {
    prop_oneof![
        receipt().prop_map(|receipt| ProfileApplySpawnOutcome::FreshApply { receipt }),
        receipt().prop_map(|receipt| ProfileApplySpawnOutcome::IdempotentReplay { receipt }),
        ("robot.profile.[a-z_]{1,24}", small_text()).prop_map(|(error_code, reason)| {
            ProfileApplySpawnOutcome::Failed { error_code, reason }
        },),
    ]
}

fn env_from_entries(entries: &[(String, String)], reverse: bool) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if reverse {
        for (key, value) in entries.iter().rev() {
            map.insert(key.clone(), value.clone());
        }
    } else {
        for (key, value) in entries {
            map.insert(key.clone(), value.clone());
        }
    }
    map
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn request_constructor_hashes_arbitrary_profile_payloads(
        profile in profile(),
        count in any::<u32>(),
        env_overrides in btree_map(),
    ) {
        let request = ProfileApplySpawnRequest::new(
            profile.clone(),
            count,
            env_overrides.clone(),
        );

        prop_assert_eq!(
            &request.content_hash,
            &compute_apply_receipt_hash(&profile, count, &env_overrides),
        );
        prop_assert_eq!(&request.profile, &profile);
        prop_assert_eq!(request.count, count);
        prop_assert_eq!(&request.env_overrides, &env_overrides);
        prop_assert!(request.verify_hash());
    }

    #[test]
    fn verify_hash_rejects_arbitrary_payload_tampering(
        profile in profile(),
        env_overrides in btree_map(),
        original_count in any::<u32>(),
        extra in map_entry(),
    ) {
        let mut request = ProfileApplySpawnRequest::new(profile, original_count, env_overrides);
        request.count = original_count.wrapping_add(1);
        request.env_overrides.insert(extra.0, extra.1);

        prop_assert!(!request.verify_hash());
    }

    #[test]
    fn receipt_hash_ignores_profile_timestamps_for_same_content(
        mut profile in profile(),
        created_a in any::<i64>(),
        updated_a in any::<i64>(),
        created_b in any::<i64>(),
        updated_b in any::<i64>(),
        count in any::<u32>(),
        env_overrides in btree_map(),
    ) {
        let mut other = profile.clone();
        profile.created_at_ms = created_a;
        profile.updated_at_ms = updated_a;
        other.created_at_ms = created_b;
        other.updated_at_ms = updated_b;

        prop_assert_eq!(
            compute_apply_receipt_hash(&profile, count, &env_overrides),
            compute_apply_receipt_hash(&other, count, &env_overrides),
        );
    }

    #[test]
    fn receipt_hash_canonicalizes_unordered_profile_collections(
        mut profile in profile(),
        env_map in btree_map(),
        metadata_map in btree_map(),
        count in any::<u32>(),
        env_overrides in btree_map(),
    ) {
        let mut other = profile.clone();
        let env_entries: Vec<_> = env_map.into_iter().collect();
        let metadata_entries: Vec<_> = metadata_map.into_iter().collect();
        profile.env = env_from_entries(&env_entries, false);
        other.env = env_from_entries(&env_entries, true);
        profile.metadata = env_from_entries(&metadata_entries, false);
        other.metadata = env_from_entries(&metadata_entries, true);
        other.tags.reverse();

        prop_assert_eq!(
            compute_apply_receipt_hash(&profile, count, &env_overrides),
            compute_apply_receipt_hash(&other, count, &env_overrides),
        );
    }

    #[test]
    fn outcomes_roundtrip_and_accessors_match_variant_semantics(
        outcome in outcome(),
    ) {
        let encoded = serde_json::to_string(&outcome).unwrap();
        let decoded: ProfileApplySpawnOutcome = serde_json::from_str(&encoded).unwrap();
        prop_assert_eq!(&decoded, &outcome);

        match &decoded {
            ProfileApplySpawnOutcome::FreshApply { receipt }
            | ProfileApplySpawnOutcome::IdempotentReplay { receipt } => {
                prop_assert_eq!(decoded.error_code(), None);
                prop_assert_eq!(decoded.receipt(), Some(receipt));
            }
            ProfileApplySpawnOutcome::Failed { error_code, .. } => {
                prop_assert_eq!(decoded.error_code(), Some(error_code.as_str()));
                prop_assert_eq!(decoded.receipt(), None);
            }
        }
    }
}
