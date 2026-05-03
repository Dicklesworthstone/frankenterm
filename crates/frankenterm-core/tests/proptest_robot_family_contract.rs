use std::collections::BTreeSet;

use proptest::prelude::*;

use frankenterm_core::robot_family_contract::{
    FamilyContract, ProptestStrategyHint, SchemaKind, checkpoint_family_contract,
    context_family_contract, fleet_family_contract, profile_family_contract, work_family_contract,
};

fn family_contract_by_index(index: usize) -> FamilyContract {
    match index % 5 {
        0 => profile_family_contract(),
        1 => checkpoint_family_contract(),
        2 => work_family_contract(),
        3 => fleet_family_contract(),
        _ => context_family_contract(),
    }
}

fn family_index_strategy() -> impl Strategy<Value = usize> {
    0usize..5
}

fn action_index_strategy() -> impl Strategy<Value = usize> {
    any::<usize>()
}

fn object_field_names(contract: &FamilyContract, action_name: &str) -> BTreeSet<String> {
    let action = contract
        .action(action_name)
        .expect("action from same contract should resolve");
    action
        .request_schema
        .fields
        .iter()
        .map(|field| field.name.clone())
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn proptest_robot_family_contract_builders_validate_cleanly(
        family_index in family_index_strategy(),
    ) {
        let contract = family_contract_by_index(family_index);

        prop_assert!(
            contract.validate().is_empty(),
            "family {} should validate cleanly",
            contract.family_name,
        );
        prop_assert!(!contract.family_name.is_empty());
        prop_assert!(!contract.description.is_empty());
        prop_assert_eq!(contract.action_count(), contract.actions.len());
        prop_assert!(!contract.actions.is_empty());
    }

    #[test]
    fn proptest_robot_family_contract_json_schema_discriminates_all_actions(
        family_index in family_index_strategy(),
    ) {
        let contract = family_contract_by_index(family_index);
        let schema = contract.json_schema();
        let one_of = schema
            .get("oneOf")
            .and_then(serde_json::Value::as_array)
            .expect("family schema should use oneOf");

        prop_assert_eq!(one_of.len(), contract.actions.len());
        let expected_id = format!(
            "https://frankenterm.dev/schema/robot/family/{}.json",
            contract.family_name,
        );
        prop_assert_eq!(
            schema.get("$id").and_then(serde_json::Value::as_str),
            Some(expected_id.as_str()),
        );

        let action_consts: BTreeSet<String> = one_of
            .iter()
            .map(|branch| {
                branch["properties"]["action"]["const"]
                    .as_str()
                    .expect("action const should be string")
                    .to_string()
            })
            .collect();
        let contract_actions: BTreeSet<String> =
            contract.actions.iter().map(|action| action.action.clone()).collect();

        prop_assert_eq!(action_consts, contract_actions);
        for branch in one_of {
            prop_assert_eq!(branch["type"].as_str(), Some("object"));
            prop_assert_eq!(branch["additionalProperties"].as_bool(), Some(false));
            let requires_action_and_params = branch["required"].as_array().is_some_and(|required| {
                required.iter().any(|value| value.as_str() == Some("action"))
                    && required.iter().any(|value| value.as_str() == Some("params"))
            });
            prop_assert!(requires_action_and_params);
        }
    }

    #[test]
    fn proptest_robot_family_contract_mcp_descriptors_match_actions(
        family_index in family_index_strategy(),
    ) {
        let contract = family_contract_by_index(family_index);
        let descriptors = contract.mcp_tool_descriptors();

        prop_assert_eq!(descriptors.len(), contract.actions.len());
        let descriptor_names: BTreeSet<String> =
            descriptors.iter().map(|descriptor| descriptor.name.clone()).collect();
        let action_tool_names: BTreeSet<String> =
            contract.actions.iter().map(|action| action.mcp_tool_name.clone()).collect();

        prop_assert_eq!(&descriptor_names, &action_tool_names);
        prop_assert_eq!(descriptor_names.len(), descriptors.len());
        let expected_prefix = format!("ft.{}.", contract.family_name);
        for descriptor in descriptors {
            prop_assert!(descriptor.name.starts_with(&expected_prefix));
            prop_assert!(!descriptor.description.is_empty());
            prop_assert_eq!(descriptor.input_schema["type"].as_str(), Some("object"));
        }
    }

    #[test]
    fn proptest_robot_family_contract_proptest_seeds_reference_request_fields(
        family_index in family_index_strategy(),
        action_index in action_index_strategy(),
    ) {
        let contract = family_contract_by_index(family_index);
        let seeds = contract.proptest_seeds();
        let seed = &seeds[action_index % seeds.len()];
        let request_field_names = object_field_names(&contract, &seed.action);
        let seed_field_names: BTreeSet<String> =
            seed.fields.iter().map(|field| field.name.clone()).collect();

        prop_assert_eq!(seeds.len(), contract.actions.len());
        prop_assert!(
            seed_field_names.is_subset(&request_field_names),
            "seed fields for {}.{} should be declared in request schema",
            contract.family_name,
            seed.action,
        );
        for field in &seed.fields {
            match &field.strategy {
                ProptestStrategyHint::AsciiString { max_len }
                | ProptestStrategyHint::OptionString { max_len } => {
                    prop_assert!(*max_len > 0);
                }
                ProptestStrategyHint::U32Range { min, max } => {
                    prop_assert!(min <= max);
                }
                ProptestStrategyHint::StringMap { max_entries } => {
                    prop_assert!(*max_entries > 0);
                }
                ProptestStrategyHint::Bool => {}
            }
        }
    }

    #[test]
    fn proptest_robot_family_contract_serde_roundtrip_preserves_contract_shape(
        family_index in family_index_strategy(),
    ) {
        let contract = family_contract_by_index(family_index);
        let encoded = serde_json::to_string(&contract).expect("contract serializes");
        let decoded: FamilyContract =
            serde_json::from_str(&encoded).expect("contract deserializes");

        prop_assert_eq!(&decoded, &contract);
        for action in &decoded.actions {
            if action.request_schema.kind == SchemaKind::Object {
                let request_schema = action.request_schema.to_json_schema();
                prop_assert_eq!(request_schema["type"].as_str(), Some("object"));
                prop_assert_eq!(request_schema["additionalProperties"].as_bool(), Some(false));
            }
            if action.response_schema.kind == SchemaKind::Object {
                let response_schema = action.response_schema.to_json_schema();
                prop_assert_eq!(response_schema["type"].as_str(), Some("object"));
                prop_assert_eq!(response_schema["additionalProperties"].as_bool(), Some(false));
            }
        }
    }
}
