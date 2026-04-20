#![cfg(feature = "agent-detection")]

use frankenterm_core::agent_detection::{
    AgentDetectOptions, AgentDetectRootOverride, InstalledAgentDetectionReport,
    detect_installed_agents,
};
use proptest::prelude::*;
use std::collections::BTreeSet;

const KNOWN_SLUGS: &[&str] = &[
    "claude",
    "cline",
    "codex",
    "cursor",
    "factory",
    "gemini",
    "github-copilot",
    "opencode",
    "windsurf",
];

fn arb_slug_subset() -> impl Strategy<Value = Vec<String>> {
    proptest::collection::vec(
        prop_oneof![
            Just("claude".to_string()),
            Just("cline".to_string()),
            Just("codex".to_string()),
            Just("cursor".to_string()),
            Just("factory".to_string()),
            Just("gemini".to_string()),
            Just("github-copilot".to_string()),
            Just("opencode".to_string()),
            Just("windsurf".to_string()),
        ],
        0..=KNOWN_SLUGS.len(),
    )
    .prop_map(|slugs| {
        let mut set = BTreeSet::new();
        slugs.into_iter().filter(|slug| set.insert(slug.clone())).collect()
    })
}

fn make_overrides(tmp: &tempfile::TempDir, installed: &[String]) -> Vec<AgentDetectRootOverride> {
    KNOWN_SLUGS
        .iter()
        .map(|slug| {
            let slug_str = (*slug).to_string();
            if installed.iter().any(|candidate| candidate == slug) {
                let dir = tmp.path().join(format!(".{slug}"));
                std::fs::create_dir_all(&dir).expect("create fixture dir");
                std::fs::write(
                    dir.join("config.json"),
                    format!(r#"{{"agent":"{slug}","version":"1.0.0"}}"#),
                )
                .expect("write fixture config");
                AgentDetectRootOverride {
                    slug: slug_str,
                    root: dir,
                }
            } else {
                AgentDetectRootOverride {
                    slug: slug_str.clone(),
                    root: tmp.path().join(format!("missing-{slug}")),
                }
            }
        })
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn detection_counts_match_installed_subset(
        installed in arb_slug_subset(),
    ) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let overrides = make_overrides(&tmp, &installed);
        let report = detect_installed_agents(&AgentDetectOptions {
            only_connectors: None,
            include_undetected: true,
            root_overrides: overrides,
        }).expect("detect installed agents");

        let detected: BTreeSet<String> = report
            .installed_agents
            .iter()
            .filter(|entry| entry.detected)
            .map(|entry| entry.slug.clone())
            .collect();
        let expected: BTreeSet<String> = installed.iter().cloned().collect();

        prop_assert!(expected.is_subset(&detected));
        prop_assert!(report.summary.detected_count >= expected.len());
        prop_assert!(report.summary.total_count >= KNOWN_SLUGS.len());
    }

    #[test]
    fn only_connectors_filter_limits_report_surface(
        installed in arb_slug_subset(),
        requested in arb_slug_subset(),
    ) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let overrides = make_overrides(&tmp, &installed);
        let requested_set: BTreeSet<String> = requested.iter().cloned().collect();
        let report = detect_installed_agents(&AgentDetectOptions {
            only_connectors: Some(requested.clone()),
            include_undetected: true,
            root_overrides: overrides,
        }).expect("detect filtered agents");

        let report_slugs: BTreeSet<String> =
            report.installed_agents.iter().map(|entry| entry.slug.clone()).collect();
        let detected: BTreeSet<String> = report
            .installed_agents
            .iter()
            .filter(|entry| entry.detected)
            .map(|entry| entry.slug.clone())
            .collect();
        let installed_set: BTreeSet<String> = installed.iter().cloned().collect();
        let expected_detected: BTreeSet<String> =
            installed_set.intersection(&requested_set).cloned().collect();

        prop_assert!(report_slugs.is_subset(&requested_set));
        prop_assert_eq!(report.summary.total_count, requested_set.len());
        prop_assert_eq!(detected, expected_detected);
    }

    #[test]
    fn detection_report_serde_roundtrip_preserves_summary_and_detected_set(
        installed in arb_slug_subset(),
    ) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let overrides = make_overrides(&tmp, &installed);
        let report = detect_installed_agents(&AgentDetectOptions {
            only_connectors: None,
            include_undetected: true,
            root_overrides: overrides,
        }).expect("detect installed agents");

        let json = serde_json::to_string(&report).expect("serialize report");
        let back: InstalledAgentDetectionReport =
            serde_json::from_str(&json).expect("deserialize report");

        let detected_before: BTreeSet<String> = report
            .installed_agents
            .iter()
            .filter(|entry| entry.detected)
            .map(|entry| entry.slug.clone())
            .collect();
        let detected_after: BTreeSet<String> = back
            .installed_agents
            .iter()
            .filter(|entry| entry.detected)
            .map(|entry| entry.slug.clone())
            .collect();

        prop_assert_eq!(back.format_version, report.format_version);
        prop_assert_eq!(back.summary.total_count, report.summary.total_count);
        prop_assert_eq!(back.summary.detected_count, report.summary.detected_count);
        prop_assert_eq!(detected_after, detected_before);
    }
}
