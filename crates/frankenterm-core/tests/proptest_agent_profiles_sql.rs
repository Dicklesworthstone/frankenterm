use std::collections::{BTreeSet, HashMap};

use proptest::prelude::*;
use rusqlite::{Connection, params};

use frankenterm_core::agent_profiles::{
    AGENT_PROFILES_ROLE_INDEX, AGENT_PROFILES_SCHEMA, AgentProfile,
};
use frankenterm_core::storage::agent_profiles_sql::{
    AgentProfileSqlError, delete_agent_profile, get_agent_profile, insert_agent_profile,
    list_agent_profiles,
};

fn fresh_conn() -> Connection {
    let conn = Connection::open_in_memory().expect("open in-memory DB");
    conn.execute_batch(&format!(
        "{AGENT_PROFILES_SCHEMA};\n{AGENT_PROFILES_ROLE_INDEX};"
    ))
    .expect("agent_profiles schema");
    conn
}

fn valid_word() -> impl Strategy<Value = String> {
    "[A-Za-z0-9_-]{1,24}"
}

fn valid_tag() -> impl Strategy<Value = String> {
    "[A-Za-z0-9_-]{1,16}"
}

fn invalid_name() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(String::new()),
        "[A-Za-z0-9_-]{1,12} [A-Za-z0-9_-]{1,12}",
        "[A-Za-z0-9_-]{1,12}/bad",
    ]
}

fn small_string_map() -> impl Strategy<Value = HashMap<String, String>> {
    prop::collection::hash_map(
        "[A-Za-z_][A-Za-z0-9_]{0,12}",
        "[A-Za-z0-9_ ./-]{0,24}",
        0..=4,
    )
}

fn profile(
    name: String,
    role: String,
    tags: Vec<String>,
    command: Option<String>,
    env: HashMap<String, String>,
    metadata: HashMap<String, String>,
) -> AgentProfile {
    AgentProfile {
        name,
        role,
        tags,
        shell: "/bin/sh".to_string(),
        command,
        env,
        metadata,
        created_at_ms: 10,
        updated_at_ms: 20,
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn proptest_agent_profiles_sql_insert_get_roundtrips_profile(
        name in valid_word(),
        role in valid_word(),
        tags in prop::collection::vec(valid_tag(), 0..=4),
        command in prop::option::of(valid_word()),
        env in small_string_map(),
        metadata in small_string_map(),
    ) {
        let conn = fresh_conn();
        let expected = profile(name.clone(), role, tags, command, env, metadata);

        let inserted_name = insert_agent_profile(&conn, &expected).expect("insert profile");
        let actual = get_agent_profile(&conn, &name)
            .expect("get profile")
            .expect("inserted profile exists");

        prop_assert_eq!(inserted_name, name);
        prop_assert_eq!(actual, expected);
    }

    #[test]
    fn proptest_agent_profiles_sql_list_orders_and_role_filters(
        target_role in valid_word(),
        other_role in valid_word(),
        target_tag in valid_tag(),
        other_tag in valid_tag(),
    ) {
        let conn = fresh_conn();
        let profiles = [
            profile(
                "target_b".to_string(),
                target_role.clone(),
                vec![target_tag.clone()],
                None,
                HashMap::new(),
                HashMap::new(),
            ),
            profile(
                "target_a".to_string(),
                target_role.clone(),
                vec![target_tag.clone(), other_tag.clone()],
                None,
                HashMap::new(),
                HashMap::new(),
            ),
            profile(
                "other".to_string(),
                other_role,
                vec![other_tag],
                None,
                HashMap::new(),
                HashMap::new(),
            ),
        ];
        for profile in &profiles {
            insert_agent_profile(&conn, profile).expect("insert profile");
        }

        let all_names: Vec<_> = list_agent_profiles(&conn, None)
            .expect("list all")
            .into_iter()
            .map(|profile| profile.name)
            .collect();
        let mut sorted_names = all_names.clone();
        sorted_names.sort();
        prop_assert_eq!(all_names, sorted_names);

        let filtered = list_agent_profiles(&conn, Some(&target_role)).expect("list by role");
        let filtered_names: BTreeSet<_> = filtered.iter().map(|profile| profile.name.as_str()).collect();

        prop_assert!(filtered.iter().all(|profile| profile.role == target_role));
        prop_assert!(filtered_names.contains("target_a"));
        prop_assert!(filtered_names.contains("target_b"));
    }

    #[test]
    fn proptest_agent_profiles_sql_delete_is_presence_sensitive(
        name in valid_word(),
        role in valid_word(),
    ) {
        let conn = fresh_conn();
        let expected = profile(
            name.clone(),
            role,
            Vec::new(),
            None,
            HashMap::new(),
            HashMap::new(),
        );
        insert_agent_profile(&conn, &expected).expect("insert profile");

        prop_assert!(delete_agent_profile(&conn, &name).expect("first delete succeeds"));
        prop_assert!(get_agent_profile(&conn, &name).expect("get after delete").is_none());
        prop_assert!(!delete_agent_profile(&conn, &name).expect("second delete succeeds"));
    }

    #[test]
    fn proptest_agent_profiles_sql_invalid_profile_rejected_without_insert(
        name in invalid_name(),
        role in valid_word(),
    ) {
        let conn = fresh_conn();
        let invalid = profile(
            name,
            role,
            Vec::new(),
            None,
            HashMap::new(),
            HashMap::new(),
        );

        let err = insert_agent_profile(&conn, &invalid).unwrap_err();

        prop_assert!(matches!(err, AgentProfileSqlError::Invalid(_)));
        prop_assert!(list_agent_profiles(&conn, None).expect("list profiles").is_empty());
    }

    #[test]
    fn proptest_agent_profiles_sql_decode_errors_name_corrupt_json_column(
        column in prop::sample::select(vec!["tags", "env", "metadata"]),
    ) {
        let conn = fresh_conn();
        let tags = if column == "tags" { "not-json" } else { "[]" };
        let env = if column == "env" { "not-json" } else { "{}" };
        let metadata = if column == "metadata" { "not-json" } else { "{}" };

        conn.execute(
            "INSERT INTO agent_profiles
             (name, role, tags, shell, command, env, metadata, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                "corrupt",
                "role",
                tags,
                "/bin/sh",
                Option::<&str>::None,
                env,
                metadata,
                0_i64,
                0_i64,
            ],
        )
        .expect("insert corrupt row");

        let err = get_agent_profile(&conn, "corrupt").unwrap_err();

        match err {
            AgentProfileSqlError::Decode { column: observed, .. } => {
                prop_assert_eq!(observed, column);
            }
            other => prop_assert!(false, "expected decode error, got {other:?}"),
        }
    }
}
