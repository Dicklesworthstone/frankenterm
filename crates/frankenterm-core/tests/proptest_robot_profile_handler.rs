use std::collections::HashMap;

use proptest::prelude::*;
use rusqlite::Connection;
use serde_json::{Value, json};

use frankenterm_core::agent_profiles::{
    AGENT_PROFILES_ROLE_INDEX, AGENT_PROFILES_SCHEMA, AgentProfile,
};
use frankenterm_core::robot_profile_handler::{ProfileHandlerError, handle_profile_command};
use frankenterm_core::storage::agent_profiles_sql::insert_agent_profile;

fn fresh_conn() -> Connection {
    let conn = Connection::open_in_memory().expect("open in-memory DB");
    conn.execute_batch(&format!(
        "{AGENT_PROFILES_SCHEMA};\n{AGENT_PROFILES_ROLE_INDEX};"
    ))
    .expect("agent_profiles schema");
    conn
}

fn profile(name: &str, role: &str, tags: Vec<String>) -> AgentProfile {
    AgentProfile {
        name: name.to_string(),
        role: role.to_string(),
        tags,
        shell: "/bin/sh".to_string(),
        command: None,
        env: HashMap::new(),
        metadata: HashMap::new(),
        created_at_ms: 0,
        updated_at_ms: 0,
    }
}

fn named_profile(
    name: &str,
    role: &str,
    tags: Vec<String>,
    command: Option<String>,
    env: HashMap<String, String>,
    metadata: HashMap<String, String>,
) -> AgentProfile {
    AgentProfile {
        name: name.to_string(),
        role: role.to_string(),
        tags,
        shell: "/bin/sh".to_string(),
        command,
        env,
        metadata,
        created_at_ms: 10,
        updated_at_ms: 20,
    }
}

fn valid_word() -> impl Strategy<Value = String> {
    "[A-Za-z0-9_-]{1,24}"
}

fn valid_tag() -> impl Strategy<Value = String> {
    "[A-Za-z0-9_-]{1,16}"
}

fn small_string_map() -> impl Strategy<Value = HashMap<String, String>> {
    prop::collection::hash_map(
        "[A-Za-z_][A-Za-z0-9_]{0,12}",
        "[A-Za-z0-9_ ./-]{0,24}",
        0..=4,
    )
}

fn non_string_name_param() -> impl Strategy<Value = Value> {
    prop_oneof![
        Just(json!({})),
        Just(json!({"name": null})),
        any::<bool>().prop_map(|value| json!({"name": value})),
        any::<u64>().prop_map(|value| json!({"name": value})),
        prop::collection::vec(valid_word(), 0..=3).prop_map(|value| json!({"name": value})),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn proptest_robot_profile_handler_rejects_missing_or_non_string_name(
        action in prop::sample::select(vec!["show", "apply", "validate"]),
        params in non_string_name_param(),
    ) {
        let conn = fresh_conn();
        let err = handle_profile_command(action, &params, &conn).unwrap_err();

        prop_assert!(matches!(err, ProfileHandlerError::BadParams(_)));
        prop_assert_eq!(err.error_code(), "robot.profile.bad_params");
        prop_assert!(err.to_string().contains("invalid profile request"));
    }

    #[test]
    fn proptest_robot_profile_handler_list_role_and_tag_filters_are_intersections(
        target_role in valid_word(),
        other_role in valid_word(),
        target_tag in valid_tag(),
        other_tag in valid_tag(),
    ) {
        let conn = fresh_conn();
        insert_agent_profile(
            &conn,
            &profile("target_profile", &target_role, vec![target_tag.clone()]),
        )
        .expect("insert target");
        insert_agent_profile(
            &conn,
            &profile("other_profile", &other_role, vec![other_tag.clone()]),
        )
        .expect("insert other");

        let listed = handle_profile_command(
            "list",
            &json!({"role_filter": target_role.clone(), "tag_filter": target_tag.clone()}),
            &conn,
        )
        .expect("list succeeds");
        let profiles = listed["profiles"].as_array().expect("profiles array");

        prop_assert!(profiles.iter().any(|entry| entry["name"] == "target_profile"));
        for entry in profiles {
            prop_assert_eq!(entry["role"].as_str(), Some(target_role.as_str()));
            prop_assert!(entry["tags"]
                .as_array()
                .expect("tags array")
                .iter()
                .any(|tag| tag.as_str() == Some(target_tag.as_str())));
        }
    }

    #[test]
    fn proptest_robot_profile_handler_show_preserves_serialized_profile_shape(
        name in valid_word(),
        role in valid_word(),
        tags in prop::collection::vec(valid_tag(), 0..=4),
        env in small_string_map(),
        metadata in small_string_map(),
    ) {
        let conn = fresh_conn();
        let profile = named_profile(
            &name,
            &role,
            tags.clone(),
            None,
            env.clone(),
            metadata.clone(),
        );
        insert_agent_profile(&conn, &profile).expect("insert profile");

        let shown = handle_profile_command("show", &json!({"name": name.clone()}), &conn)
            .expect("show succeeds");

        prop_assert_eq!(shown["name"].as_str(), Some(name.as_str()));
        prop_assert_eq!(shown["role"].as_str(), Some(role.as_str()));
        let expected_environment = serde_json::to_value(&env).unwrap();
        let expected_tags = serde_json::to_value(&tags).unwrap();
        prop_assert_eq!(&shown["environment"], &expected_environment);
        prop_assert_eq!(&shown["tags"], &expected_tags);
        prop_assert!(shown.get("spawn_command").is_none());
        match metadata.get("description") {
            Some(description) => {
                prop_assert_eq!(shown["description"].as_str(), Some(description.as_str()));
            }
            None => prop_assert!(shown.get("description").is_none()),
        }
    }

    #[test]
    fn proptest_robot_profile_handler_validate_missing_profile_is_successful_invalid_response(
        missing_name in valid_word(),
    ) {
        let conn = fresh_conn();
        let response =
            handle_profile_command("validate", &json!({"name": missing_name.clone()}), &conn)
                .expect("validate missing profile returns data");

        prop_assert_eq!(response["name"].as_str(), Some(missing_name.as_str()));
        prop_assert_eq!(response["valid"].as_bool(), Some(false));
        let issues = response["issues"].as_array().expect("issues array");
        prop_assert_eq!(issues.len(), 1);
        prop_assert!(issues[0]
            .as_str()
            .expect("issue string")
            .contains("not found"));
    }

    #[test]
    fn proptest_robot_profile_handler_apply_paths_are_count_and_dry_run_exact(
        name in valid_word(),
        requested_count in any::<u64>(),
    ) {
        let conn = fresh_conn();
        insert_agent_profile(&conn, &profile(&name, "worker", Vec::new()))
            .expect("insert profile");

        let dry_run = handle_profile_command(
            "apply",
            &json!({"name": name.clone(), "count": requested_count, "dry_run": true}),
            &conn,
        )
        .expect("dry-run apply succeeds");
        prop_assert_eq!(dry_run["profile_name"].as_str(), Some(name.as_str()));
        prop_assert_eq!(dry_run["dry_run"].as_bool(), Some(true));
        prop_assert!(dry_run["panes_spawned"]
            .as_array()
            .expect("panes_spawned array")
            .is_empty());

        let err = handle_profile_command(
            "apply",
            &json!({"name": name.clone(), "count": requested_count, "dry_run": false}),
            &conn,
        )
        .unwrap_err();
        match err {
            ProfileHandlerError::SpawnFailed { reason } => {
                let expected_count = format!("count={}", u32::try_from(requested_count).unwrap_or(1));
                prop_assert!(reason.contains(&name));
                prop_assert!(reason.contains(&expected_count));
                prop_assert!(reason.contains("daemon-mediated pane spawning"));
            }
            other => prop_assert!(false, "expected SpawnFailed, got {other:?}"),
        }
    }
}
