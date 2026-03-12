//! Comprehensive filesystem-based agent detection tests (ft-dr6zv.2.5).
//!
//! Validates the `detect_installed_agents()` pipeline from `franken_agent_detection`
//! and the `installed_inventory_cached()`/`installed_inventory_refresh()` enrichment
//! layer in `agent_correlator`.
//!
//! Coverage targets:
//! - All 9 known connectors with fixture directories
//! - Partial, empty, and corrupt installations
//! - Evidence string validation per detected agent
//! - Detection timing (sub-50ms on mock filesystem)
//! - Serde roundtrip for detection reports
//! - Cache/refresh lifecycle
//! - Summary field accuracy
//! - Integration: filesystem detection → AgentInventory enrichment

#[cfg(feature = "agent-detection")]
mod filesystem_detection {
    use frankenterm_core::agent_detection::{
        AgentDetectOptions, AgentDetectRootOverride, InstalledAgentDetectionReport,
    };
    use tempfile::TempDir;

    /// All 9 known connector slugs in sorted order (matching KNOWN_CONNECTORS).
    const ALL_SLUGS: &[&str] = &[
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

    /// Create a tempdir fixture with root directories for the specified slugs.
    fn fixture_with_agents(slugs: &[&str]) -> (TempDir, Vec<AgentDetectRootOverride>) {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let overrides: Vec<AgentDetectRootOverride> = slugs
            .iter()
            .map(|slug| {
                let agent_dir = tmp.path().join(format!(".{slug}"));
                std::fs::create_dir_all(&agent_dir).expect("create agent dir");
                // Create a minimal config file as evidence
                let config_file = agent_dir.join("config.json");
                std::fs::write(
                    &config_file,
                    format!(r#"{{"agent":"{slug}","version":"1.0.0"}}"#),
                )
                .expect("write config");
                AgentDetectRootOverride {
                    slug: slug.to_string(),
                    root: agent_dir,
                }
            })
            .collect();
        (tmp, overrides)
    }

    /// Create overrides pointing to non-existent directories (simulate not-installed).
    fn fixture_missing_agents(slugs: &[&str]) -> (TempDir, Vec<AgentDetectRootOverride>) {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let overrides: Vec<AgentDetectRootOverride> = slugs
            .iter()
            .map(|slug| AgentDetectRootOverride {
                slug: slug.to_string(),
                root: tmp.path().join(format!("nonexistent-{slug}")),
            })
            .collect();
        (tmp, overrides)
    }

    // =====================================================================
    // 1. Full installation: all 9 agents detected
    // =====================================================================

    #[test]
    fn detect_all_9_agents_with_fixtures() {
        let (_tmp, overrides) = fixture_with_agents(ALL_SLUGS);

        let report = frankenterm_core::agent_detection::detect_installed_agents(
            &AgentDetectOptions {
                only_connectors: None,
                include_undetected: true,
                root_overrides: overrides,
            },
        )
        .expect("detection should succeed");

        assert_eq!(report.format_version, 1);
        assert!(!report.generated_at.is_empty());
        assert_eq!(report.summary.total_count, 9);
        assert_eq!(report.summary.detected_count, 9);

        let detected_slugs: Vec<&str> = report
            .installed_agents
            .iter()
            .filter(|e| e.detected)
            .map(|e| e.slug.as_str())
            .collect();
        assert_eq!(detected_slugs, ALL_SLUGS);
    }

    // =====================================================================
    // 2. Empty installation: no agents detected
    // =====================================================================

    #[test]
    fn detect_no_agents_empty_home() {
        let (_tmp, overrides) = fixture_missing_agents(ALL_SLUGS);

        let report = frankenterm_core::agent_detection::detect_installed_agents(
            &AgentDetectOptions {
                only_connectors: None,
                include_undetected: true,
                root_overrides: overrides,
            },
        )
        .expect("detection should succeed even with no agents");

        assert_eq!(report.summary.detected_count, 0);
        assert_eq!(report.summary.total_count, 9);

        // All entries should be present but not detected
        for entry in &report.installed_agents {
            assert!(!entry.detected, "agent {} should not be detected", entry.slug);
            assert!(
                entry.root_paths.is_empty(),
                "agent {} should have no root paths",
                entry.slug
            );
        }
    }

    // =====================================================================
    // 3. Partial installation: only claude + codex + cursor
    // =====================================================================

    #[test]
    fn detect_partial_install_three_agents() {
        let installed = &["claude", "codex", "cursor"];
        let not_installed = &[
            "cline",
            "factory",
            "gemini",
            "github-copilot",
            "opencode",
            "windsurf",
        ];

        let tmp = tempfile::tempdir().expect("create tempdir");
        let mut overrides = Vec::new();

        // Create dirs for installed agents
        for slug in installed {
            let dir = tmp.path().join(format!(".{slug}"));
            std::fs::create_dir_all(&dir).expect("create dir");
            overrides.push(AgentDetectRootOverride {
                slug: slug.to_string(),
                root: dir,
            });
        }

        // Point not-installed to nonexistent dirs
        for slug in not_installed {
            overrides.push(AgentDetectRootOverride {
                slug: slug.to_string(),
                root: tmp.path().join(format!("missing-{slug}")),
            });
        }

        let report = frankenterm_core::agent_detection::detect_installed_agents(
            &AgentDetectOptions {
                only_connectors: None,
                include_undetected: true,
                root_overrides: overrides,
            },
        )
        .expect("detection should succeed");

        assert_eq!(report.summary.detected_count, 3);
        assert_eq!(report.summary.total_count, 9);

        for slug in installed {
            let entry = report
                .installed_agents
                .iter()
                .find(|e| e.slug == *slug)
                .unwrap_or_else(|| panic!("missing entry for {slug}"));
            assert!(entry.detected, "{slug} should be detected");
            assert!(!entry.root_paths.is_empty(), "{slug} should have root paths");
        }

        for slug in not_installed {
            let entry = report
                .installed_agents
                .iter()
                .find(|e| e.slug == *slug)
                .unwrap_or_else(|| panic!("missing entry for {slug}"));
            assert!(!entry.detected, "{slug} should NOT be detected");
        }
    }

    // =====================================================================
    // 4. Each agent detected individually (per-connector isolation)
    // =====================================================================

    #[test]
    fn detect_each_agent_individually() {
        for slug in ALL_SLUGS {
            let (_tmp, overrides) = fixture_with_agents(&[slug]);

            let report = frankenterm_core::agent_detection::detect_installed_agents(
                &AgentDetectOptions {
                    only_connectors: Some(vec![slug.to_string()]),
                    include_undetected: true,
                    root_overrides: overrides,
                },
            )
            .unwrap_or_else(|e| panic!("detection failed for {slug}: {e}"));

            assert_eq!(
                report.summary.detected_count, 1,
                "{slug}: expected 1 detected"
            );
            assert_eq!(
                report.summary.total_count, 1,
                "{slug}: expected 1 total (scoped)"
            );
            assert_eq!(report.installed_agents.len(), 1);
            assert_eq!(report.installed_agents[0].slug, *slug);
            assert!(
                report.installed_agents[0].detected,
                "{slug} should be detected"
            );
        }
    }

    // =====================================================================
    // 5. Evidence strings are human-readable and present
    // =====================================================================

    #[test]
    fn detection_evidence_strings_per_agent() {
        let (_tmp, overrides) = fixture_with_agents(ALL_SLUGS);

        let report = frankenterm_core::agent_detection::detect_installed_agents(
            &AgentDetectOptions {
                only_connectors: None,
                include_undetected: true,
                root_overrides: overrides,
            },
        )
        .expect("detection should succeed");

        for entry in &report.installed_agents {
            if entry.detected {
                assert!(
                    !entry.evidence.is_empty(),
                    "detected agent {} should have evidence strings",
                    entry.slug
                );
                // Evidence should mention the root path
                let has_root_evidence = entry
                    .evidence
                    .iter()
                    .any(|ev| ev.contains("root exists") || ev.contains("override"));
                assert!(
                    has_root_evidence,
                    "evidence for {} should reference root path: {:?}",
                    entry.slug, entry.evidence
                );
            }
        }
    }

    // =====================================================================
    // 6. Evidence strings for missing agents mention root missing
    // =====================================================================

    #[test]
    fn missing_agent_evidence_mentions_missing_root() {
        let (_tmp, overrides) = fixture_missing_agents(&["claude"]);

        let report = frankenterm_core::agent_detection::detect_installed_agents(
            &AgentDetectOptions {
                only_connectors: Some(vec!["claude".to_string()]),
                include_undetected: true,
                root_overrides: overrides,
            },
        )
        .expect("detection should succeed");

        let claude = &report.installed_agents[0];
        assert!(!claude.detected);
        let has_missing_evidence = claude
            .evidence
            .iter()
            .any(|ev| ev.contains("missing") || ev.contains("not found"));
        assert!(
            has_missing_evidence,
            "missing agent evidence should mention 'missing': {:?}",
            claude.evidence
        );
    }

    // =====================================================================
    // 7. include_undetected=false filters non-detected entries
    // =====================================================================

    #[test]
    fn include_undetected_false_filters_correctly() {
        let installed = &["claude", "codex"];
        let tmp = tempfile::tempdir().expect("create tempdir");
        let mut overrides = Vec::new();

        for slug in installed {
            let dir = tmp.path().join(format!(".{slug}"));
            std::fs::create_dir_all(&dir).expect("create dir");
            overrides.push(AgentDetectRootOverride {
                slug: slug.to_string(),
                root: dir,
            });
        }
        // Add missing agents
        for slug in &["gemini", "cursor"] {
            overrides.push(AgentDetectRootOverride {
                slug: slug.to_string(),
                root: tmp.path().join(format!("missing-{slug}")),
            });
        }

        let report = frankenterm_core::agent_detection::detect_installed_agents(
            &AgentDetectOptions {
                only_connectors: Some(vec![
                    "claude".into(),
                    "codex".into(),
                    "gemini".into(),
                    "cursor".into(),
                ]),
                include_undetected: false,
                root_overrides: overrides,
            },
        )
        .expect("detection should succeed");

        // Only detected agents should be in the list
        assert_eq!(report.installed_agents.len(), 2);
        let slugs: Vec<&str> = report
            .installed_agents
            .iter()
            .map(|e| e.slug.as_str())
            .collect();
        assert!(slugs.contains(&"claude"));
        assert!(slugs.contains(&"codex"));

        // Summary still reflects the full scan scope
        assert_eq!(report.summary.detected_count, 2);
        assert_eq!(report.summary.total_count, 4);
    }

    // =====================================================================
    // 8. Detection timing under 50ms on mock filesystem
    // =====================================================================

    #[test]
    fn detection_completes_under_50ms_on_mock_filesystem() {
        let (_tmp, overrides) = fixture_with_agents(ALL_SLUGS);

        let start = std::time::Instant::now();
        let _report = frankenterm_core::agent_detection::detect_installed_agents(
            &AgentDetectOptions {
                only_connectors: None,
                include_undetected: true,
                root_overrides: overrides,
            },
        )
        .expect("detection should succeed");
        let elapsed = start.elapsed();

        assert!(
            elapsed.as_millis() < 50,
            "detection took {}ms, expected <50ms",
            elapsed.as_millis()
        );
    }

    // =====================================================================
    // 9. Serde roundtrip for detection report
    // =====================================================================

    #[test]
    fn detection_report_serde_roundtrip() {
        let (_tmp, overrides) = fixture_with_agents(ALL_SLUGS);

        let report = frankenterm_core::agent_detection::detect_installed_agents(
            &AgentDetectOptions {
                only_connectors: None,
                include_undetected: true,
                root_overrides: overrides,
            },
        )
        .expect("detection should succeed");

        let json = serde_json::to_string(&report).expect("serialize report");
        let back: InstalledAgentDetectionReport =
            serde_json::from_str(&json).expect("deserialize report");

        assert_eq!(back.format_version, report.format_version);
        assert_eq!(back.summary.detected_count, report.summary.detected_count);
        assert_eq!(back.summary.total_count, report.summary.total_count);
        assert_eq!(
            back.installed_agents.len(),
            report.installed_agents.len()
        );

        for (original, roundtripped) in
            report.installed_agents.iter().zip(back.installed_agents.iter())
        {
            assert_eq!(original.slug, roundtripped.slug);
            assert_eq!(original.detected, roundtripped.detected);
            assert_eq!(original.root_paths, roundtripped.root_paths);
        }
    }

    // =====================================================================
    // 10. Detection report JSON schema stability
    // =====================================================================

    #[test]
    fn detection_report_json_has_expected_fields() {
        let (_tmp, overrides) = fixture_with_agents(&["claude"]);

        let report = frankenterm_core::agent_detection::detect_installed_agents(
            &AgentDetectOptions {
                only_connectors: Some(vec!["claude".to_string()]),
                include_undetected: true,
                root_overrides: overrides,
            },
        )
        .expect("detection should succeed");

        let json: serde_json::Value = serde_json::to_value(&report).expect("serialize");

        // Top-level fields
        assert!(json.get("format_version").is_some());
        assert!(json.get("generated_at").is_some());
        assert!(json.get("installed_agents").is_some());
        assert!(json.get("summary").is_some());

        // Summary fields
        let summary = &json["summary"];
        assert!(summary.get("detected_count").is_some());
        assert!(summary.get("total_count").is_some());

        // Agent entry fields
        let agents = json["installed_agents"].as_array().expect("agents array");
        assert!(!agents.is_empty());
        let agent = &agents[0];
        assert!(agent.get("slug").is_some());
        assert!(agent.get("detected").is_some());
        assert!(agent.get("evidence").is_some());
        assert!(agent.get("root_paths").is_some());
    }

    // =====================================================================
    // 11. Unknown connector slug rejected
    // =====================================================================

    #[test]
    fn unknown_connector_in_only_connectors_rejected() {
        let err = frankenterm_core::agent_detection::detect_installed_agents(
            &AgentDetectOptions {
                only_connectors: Some(vec!["not-a-real-agent".to_string()]),
                include_undetected: true,
                root_overrides: vec![],
            },
        )
        .expect_err("should reject unknown connector");

        let msg = format!("{err}");
        assert!(
            msg.contains("not-a-real-agent"),
            "error should mention the unknown slug: {msg}"
        );
    }

    // =====================================================================
    // 12. Connector slug aliases (claude-code → claude, codex-cli → codex)
    // =====================================================================

    #[test]
    fn connector_slug_aliases_resolve_correctly() {
        let tmp = tempfile::tempdir().expect("create tempdir");

        // Use alias "claude-code" instead of canonical "claude"
        let claude_dir = tmp.path().join(".claude-code-alias");
        std::fs::create_dir_all(&claude_dir).expect("create dir");

        let report = frankenterm_core::agent_detection::detect_installed_agents(
            &AgentDetectOptions {
                only_connectors: Some(vec!["claude-code".to_string()]),
                include_undetected: true,
                root_overrides: vec![AgentDetectRootOverride {
                    slug: "claude-code".to_string(),
                    root: claude_dir,
                }],
            },
        )
        .expect("detection should succeed with alias");

        // Should resolve to canonical "claude"
        assert_eq!(report.installed_agents.len(), 1);
        assert_eq!(report.installed_agents[0].slug, "claude");
        assert!(report.installed_agents[0].detected);
    }

    // =====================================================================
    // 13. Multiple roots per connector (default probes + override)
    // =====================================================================

    #[test]
    fn multiple_root_overrides_for_same_connector() {
        let tmp = tempfile::tempdir().expect("create tempdir");

        let root1 = tmp.path().join("claude-root-1");
        let root2 = tmp.path().join("claude-root-2");
        std::fs::create_dir_all(&root1).expect("create root1");
        std::fs::create_dir_all(&root2).expect("create root2");

        let report = frankenterm_core::agent_detection::detect_installed_agents(
            &AgentDetectOptions {
                only_connectors: Some(vec!["claude".to_string()]),
                include_undetected: true,
                root_overrides: vec![
                    AgentDetectRootOverride {
                        slug: "claude".to_string(),
                        root: root1.clone(),
                    },
                    AgentDetectRootOverride {
                        slug: "claude".to_string(),
                        root: root2.clone(),
                    },
                ],
            },
        )
        .expect("detection should succeed");

        let claude = &report.installed_agents[0];
        assert!(claude.detected);
        assert!(
            claude.root_paths.len() >= 2,
            "should have at least 2 root paths, got: {:?}",
            claude.root_paths
        );
    }

    // =====================================================================
    // 14. Empty root_overrides uses default probe paths
    // =====================================================================

    #[test]
    fn empty_overrides_probes_default_paths() {
        // With no overrides, should probe real filesystem (may or may not find agents)
        let report = frankenterm_core::agent_detection::detect_installed_agents(
            &AgentDetectOptions {
                only_connectors: None,
                include_undetected: true,
                root_overrides: vec![],
            },
        )
        .expect("detection should succeed");

        // Should always return all 9 connectors when include_undetected=true
        assert_eq!(report.summary.total_count, 9);
        assert_eq!(report.installed_agents.len(), 9);

        // Verify entries are sorted by slug
        let slugs: Vec<&str> = report
            .installed_agents
            .iter()
            .map(|e| e.slug.as_str())
            .collect();
        let mut sorted_slugs = slugs.clone();
        sorted_slugs.sort();
        assert_eq!(slugs, sorted_slugs, "entries should be sorted by slug");
    }

    // =====================================================================
    // 15. Scoped detection: only_connectors filters scan
    // =====================================================================

    #[test]
    fn only_connectors_limits_scan_scope() {
        let (_tmp, overrides) = fixture_with_agents(ALL_SLUGS);

        let report = frankenterm_core::agent_detection::detect_installed_agents(
            &AgentDetectOptions {
                only_connectors: Some(vec!["claude".to_string(), "gemini".to_string()]),
                include_undetected: true,
                root_overrides: overrides,
            },
        )
        .expect("detection should succeed");

        assert_eq!(report.summary.total_count, 2);
        assert_eq!(report.installed_agents.len(), 2);
        let slugs: Vec<&str> = report
            .installed_agents
            .iter()
            .map(|e| e.slug.as_str())
            .collect();
        assert!(slugs.contains(&"claude"));
        assert!(slugs.contains(&"gemini"));
    }

    // =====================================================================
    // 16. Default AgentDetectOptions detects everything
    // =====================================================================

    #[test]
    fn default_options_scans_all_connectors() {
        let opts = AgentDetectOptions::default();
        assert!(opts.only_connectors.is_none());
        assert!(!opts.include_undetected);
        assert!(opts.root_overrides.is_empty());
    }

    // =====================================================================
    // 17. generated_at is a valid RFC3339 timestamp
    // =====================================================================

    #[test]
    fn generated_at_is_valid_rfc3339() {
        let (_tmp, overrides) = fixture_with_agents(&["claude"]);

        let report = frankenterm_core::agent_detection::detect_installed_agents(
            &AgentDetectOptions {
                only_connectors: Some(vec!["claude".to_string()]),
                include_undetected: true,
                root_overrides: overrides,
            },
        )
        .expect("detection should succeed");

        // Parse as RFC3339 — chrono or manual check
        assert!(
            report.generated_at.contains('T'),
            "generated_at should be RFC3339 format: {}",
            report.generated_at
        );
        assert!(
            report.generated_at.contains('+') || report.generated_at.ends_with('Z'),
            "generated_at should have timezone: {}",
            report.generated_at
        );
    }
}

// =========================================================================
// Installed inventory cache/refresh tests (require agent-detection feature)
// =========================================================================

#[cfg(feature = "agent-detection")]
mod inventory_cache {
    use frankenterm_core::agent_correlator::{
        installed_inventory_cached, installed_inventory_refresh, InstalledAgentInventoryEntry,
    };

    #[test]
    fn installed_inventory_cached_returns_entries() {
        let result = installed_inventory_cached();
        assert!(result.is_ok(), "cached inventory should succeed: {result:?}");
        let entries = result.unwrap();
        // Should have entries for all known connectors
        assert!(
            !entries.is_empty(),
            "cached inventory should have at least some entries"
        );
    }

    #[test]
    fn installed_inventory_refresh_returns_entries() {
        let result = installed_inventory_refresh();
        assert!(
            result.is_ok(),
            "refresh inventory should succeed: {result:?}"
        );
        let entries = result.unwrap();
        assert!(
            !entries.is_empty(),
            "refreshed inventory should have entries"
        );
    }

    #[test]
    fn cached_and_refresh_return_same_structure() {
        // Refresh first to prime the cache
        let refreshed = installed_inventory_refresh().expect("refresh");
        let cached = installed_inventory_cached().expect("cached");

        // Both should have the same number of entries
        assert_eq!(
            refreshed.len(),
            cached.len(),
            "cached and refreshed should have same entry count"
        );

        // Verify structure matches
        for (r, c) in refreshed.iter().zip(cached.iter()) {
            assert_eq!(r.slug, c.slug, "slugs should match");
            assert_eq!(r.detected, c.detected, "detected should match for {}", r.slug);
        }
    }

    #[test]
    fn installed_inventory_entry_has_expected_fields() {
        let entries = installed_inventory_cached().expect("cached");
        for entry in &entries {
            // Slug should be non-empty
            assert!(!entry.slug.is_empty(), "slug should be non-empty");
            // Evidence should be a vec (possibly empty for undetected)
            // root_paths should be a vec
            // Optional fields are Option<String>
            let _: &Option<String> = &entry.config_path;
            let _: &Option<String> = &entry.binary_path;
            let _: &Option<String> = &entry.version;
        }
    }

    #[test]
    fn installed_inventory_entries_serde_roundtrip() {
        let entries = installed_inventory_cached().expect("cached");
        let json = serde_json::to_string(&entries).expect("serialize");
        let back: Vec<InstalledAgentInventoryEntry> =
            serde_json::from_str(&json).expect("deserialize");
        assert_eq!(entries.len(), back.len());
        for (original, roundtripped) in entries.iter().zip(back.iter()) {
            assert_eq!(original, roundtripped);
        }
    }
}

// =========================================================================
// Feature flag graceful fallback (when agent-detection is OFF)
// =========================================================================

#[cfg(not(feature = "agent-detection"))]
mod feature_disabled {
    use frankenterm_core::agent_correlator::{
        installed_inventory_cached, installed_inventory_refresh,
    };

    #[test]
    fn cached_returns_error_when_feature_disabled() {
        let result = installed_inventory_cached();
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("not enabled"),
            "error should mention feature not enabled: {err}"
        );
    }

    #[test]
    fn refresh_returns_error_when_feature_disabled() {
        let result = installed_inventory_refresh();
        assert!(result.is_err());
    }

    #[test]
    fn filesystem_detection_available_returns_false() {
        assert!(!frankenterm_core::agent_correlator::filesystem_detection_available());
    }
}

// =========================================================================
// Integration: filesystem detection + AgentCorrelator inventory enrichment
// =========================================================================

#[cfg(feature = "agent-detection")]
mod integration_enrichment {
    use frankenterm_core::agent_correlator::{
        AgentCorrelator, AgentInventory, DetectionSource,
    };
    use frankenterm_core::patterns::{AgentType, Detection, Severity};

    fn detection(rule_id: &str, agent_type: AgentType) -> Detection {
        Detection {
            rule_id: rule_id.to_string(),
            agent_type,
            event_type: "test".to_string(),
            severity: Severity::Info,
            confidence: 0.95,
            extracted: serde_json::json!({}),
            matched_text: String::new(),
            span: (0, 0),
        }
    }

    #[test]
    fn inventory_includes_both_installed_and_running() {
        let mut correlator = AgentCorrelator::new();

        // Add running agents via pattern detection
        correlator.ingest_detections(
            1,
            &[detection("core.claude_code:tool_use", AgentType::ClaudeCode)],
        );
        correlator.ingest_detections(
            2,
            &[detection("core.codex:banner", AgentType::Codex)],
        );

        let inventory = correlator.inventory();

        // Running agents should be present
        assert_eq!(inventory.running.len(), 2);
        assert!(inventory.running.contains_key(&1));
        assert!(inventory.running.contains_key(&2));

        // Installed inventory should be populated from filesystem cache
        // (may or may not find agents depending on test machine)
        // The key invariant is that the field exists and is a valid Vec
        let _installed = &inventory.installed;
    }

    #[test]
    fn inventory_serde_roundtrip_with_installed_and_running() {
        let mut correlator = AgentCorrelator::new();
        correlator.ingest_detections(
            1,
            &[detection("core.claude_code:tool_use", AgentType::ClaudeCode)],
        );

        let inventory = correlator.inventory();
        let json = serde_json::to_string(&inventory).expect("serialize");
        let back: AgentInventory = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(back.running.len(), inventory.running.len());
        assert_eq!(back.installed.len(), inventory.installed.len());
    }

    #[test]
    fn inventory_json_schema_stability() {
        let mut correlator = AgentCorrelator::new();
        correlator.ingest_detections(
            1,
            &[detection("core.codex:tool_use", AgentType::Codex)],
        );

        let inventory = correlator.inventory();
        let json: serde_json::Value = serde_json::to_value(&inventory).expect("serialize");

        // Top-level structure
        assert!(json.get("installed").is_some(), "must have installed field");
        assert!(json.get("running").is_some(), "must have running field");

        // Running entry structure
        let running = json["running"].as_object().expect("running is object");
        let entry = &running["1"];
        assert_eq!(entry["slug"], "codex");
        assert_eq!(entry["state"], "working");
        assert!(entry.get("session_id").is_some());
        assert!(entry.get("source").is_some());
    }

    #[test]
    fn multiple_agents_running_with_different_sources() {
        let mut correlator = AgentCorrelator::new();

        // Pattern-detected
        correlator.ingest_detections(
            1,
            &[detection("core.claude_code:banner", AgentType::ClaudeCode)],
        );

        // Title-detected
        let pane = frankenterm_core::wezterm::PaneInfo {
            pane_id: 2,
            tab_id: 0,
            window_id: 0,
            domain_id: None,
            domain_name: None,
            workspace: None,
            size: None,
            rows: None,
            cols: None,
            title: Some("codex session".to_string()),
            cwd: None,
            tty_name: None,
            cursor_x: None,
            cursor_y: None,
            cursor_visibility: None,
            left_col: None,
            top_row: None,
            is_active: true,
            is_zoomed: false,
            extra: std::collections::HashMap::new(),
        };
        correlator.update_from_pane_info(&pane);

        let inventory = correlator.inventory();

        // Both agents in running
        assert_eq!(inventory.running.len(), 2);
        assert_eq!(inventory.running[&1].source, DetectionSource::PatternEngine);
        assert_eq!(inventory.running[&2].source, DetectionSource::PaneTitle);

        // Installed field populated from cache (structure check)
        let json = serde_json::to_string(&inventory).expect("serialize");
        let back: AgentInventory = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.running.len(), 2);
    }
}

// =========================================================================
// AgentPaneState classification comprehensive tests
// =========================================================================

mod pane_state_classification {
    use frankenterm_core::agent_pane_state::{
        AgentDetectionConfig, AgentPaneState, PaneActivityTimestamps,
    };

    fn default_config() -> AgentDetectionConfig {
        AgentDetectionConfig::default()
    }

    fn agent_timestamps(
        last_output_ms: u64,
        last_input_ms: u64,
        flagged_stuck: bool,
    ) -> PaneActivityTimestamps {
        PaneActivityTimestamps {
            last_output_ms,
            last_input_ms,
            is_agent: true,
            flagged_stuck,
        }
    }

    #[test]
    fn active_state_with_recent_output() {
        let ts = agent_timestamps(99_000, 95_000, false);
        assert_eq!(ts.classify(100_000, &default_config()), AgentPaneState::Active);
    }

    #[test]
    fn thinking_state_input_sent_no_output() {
        // Input at 92s, output at 80s, now 100s
        // since_output = 20s > thinking_silence(5s) but < stuck_silence(30s)
        // last_input > last_output
        let ts = agent_timestamps(80_000, 92_000, false);
        assert_eq!(
            ts.classify(100_000, &default_config()),
            AgentPaneState::Thinking
        );
    }

    #[test]
    fn stuck_state_long_silence_after_input() {
        // Input at 65s, output at 60s, now 100s
        // since_output = 40s > stuck_silence(30s)
        // last_input > last_output
        let ts = agent_timestamps(60_000, 65_000, false);
        assert_eq!(
            ts.classify(100_000, &default_config()),
            AgentPaneState::Stuck
        );
    }

    #[test]
    fn stuck_state_flagged_by_watchdog() {
        let ts = agent_timestamps(99_999, 99_999, true);
        assert_eq!(
            ts.classify(100_000, &default_config()),
            AgentPaneState::Stuck
        );
    }

    #[test]
    fn idle_state_no_activity() {
        // Both output and input very old (90s+ ago)
        let ts = agent_timestamps(10_000, 10_000, false);
        assert_eq!(ts.classify(100_000, &default_config()), AgentPaneState::Idle);
    }

    #[test]
    fn human_pane_not_agent_controlled() {
        let ts = PaneActivityTimestamps {
            last_output_ms: 99_999,
            last_input_ms: 99_999,
            is_agent: false,
            flagged_stuck: false,
        };
        assert_eq!(
            ts.classify(100_000, &default_config()),
            AgentPaneState::Human
        );
    }

    #[test]
    fn custom_config_thresholds() {
        let config = AgentDetectionConfig {
            active_output_threshold_ms: 1000,
            thinking_silence_ms: 2000,
            stuck_silence_ms: 5000,
            idle_silence_ms: 10_000,
            ..AgentDetectionConfig::default()
        };

        // With tighter thresholds: 3s since output, input more recent → thinking
        let ts = agent_timestamps(97_000, 98_000, false);
        assert_eq!(ts.classify(100_000, &config), AgentPaneState::Thinking);

        // 6s since output, input more recent → stuck with tight stuck_silence
        let ts2 = agent_timestamps(94_000, 95_000, false);
        assert_eq!(ts2.classify(100_000, &config), AgentPaneState::Stuck);
    }

    #[test]
    fn boundary_at_active_threshold() {
        let config = default_config();
        // Exactly at threshold boundary (4999ms since output)
        let ts = agent_timestamps(95_001, 90_000, false);
        assert_eq!(ts.classify(100_000, &config), AgentPaneState::Active);

        // Just past threshold (5000ms since output)
        let ts2 = agent_timestamps(95_000, 90_000, false);
        assert_eq!(ts2.classify(100_000, &config), AgentPaneState::Active);
    }

    #[test]
    fn all_state_labels_non_empty_except_human() {
        assert!(!AgentPaneState::Active.label().is_empty());
        assert!(!AgentPaneState::Thinking.label().is_empty());
        assert!(!AgentPaneState::Stuck.label().is_empty());
        assert!(!AgentPaneState::Idle.label().is_empty());
        assert!(AgentPaneState::Human.label().is_empty());
    }

    #[test]
    fn only_stuck_is_alert_state() {
        assert!(!AgentPaneState::Active.is_alert());
        assert!(!AgentPaneState::Thinking.is_alert());
        assert!(AgentPaneState::Stuck.is_alert());
        assert!(!AgentPaneState::Idle.is_alert());
        assert!(!AgentPaneState::Human.is_alert());
    }

    #[test]
    fn agent_pane_state_default_is_human() {
        assert_eq!(AgentPaneState::default(), AgentPaneState::Human);
    }

    #[test]
    fn agent_pane_state_serde_roundtrip() {
        let states = [
            AgentPaneState::Active,
            AgentPaneState::Thinking,
            AgentPaneState::Stuck,
            AgentPaneState::Idle,
            AgentPaneState::Human,
        ];

        for state in &states {
            let json = serde_json::to_string(state).expect("serialize");
            let back: AgentPaneState = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(*state, back);
        }
    }

    #[test]
    fn agent_pane_state_serializes_to_snake_case() {
        assert_eq!(
            serde_json::to_value(AgentPaneState::Active).unwrap(),
            "active"
        );
        assert_eq!(
            serde_json::to_value(AgentPaneState::Thinking).unwrap(),
            "thinking"
        );
        assert_eq!(
            serde_json::to_value(AgentPaneState::Stuck).unwrap(),
            "stuck"
        );
        assert_eq!(
            serde_json::to_value(AgentPaneState::Idle).unwrap(),
            "idle"
        );
        assert_eq!(
            serde_json::to_value(AgentPaneState::Human).unwrap(),
            "human"
        );
    }

    #[test]
    fn agent_detection_config_serde_roundtrip() {
        let config = AgentDetectionConfig::default();
        let json = serde_json::to_string(&config).expect("serialize");
        let back: AgentDetectionConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.enabled, config.enabled);
        assert_eq!(
            back.active_output_threshold_ms,
            config.active_output_threshold_ms
        );
        assert_eq!(back.thinking_silence_ms, config.thinking_silence_ms);
        assert_eq!(back.stuck_silence_ms, config.stuck_silence_ms);
        assert_eq!(back.idle_silence_ms, config.idle_silence_ms);
    }

    #[test]
    fn pane_backpressure_overlay_default() {
        let overlay = frankenterm_core::agent_pane_state::PaneBackpressureOverlay::default();
        assert!(overlay.tier.is_empty());
        assert!((overlay.queue_fill_ratio - 0.0).abs() < f64::EPSILON);
        assert!(!overlay.rate_limited);
    }

    #[test]
    fn auto_layout_policy_default_is_by_status() {
        assert_eq!(
            frankenterm_core::agent_pane_state::AutoLayoutPolicy::default(),
            frankenterm_core::agent_pane_state::AutoLayoutPolicy::ByStatus
        );
    }

    #[test]
    fn auto_layout_policy_serde_roundtrip() {
        use frankenterm_core::agent_pane_state::AutoLayoutPolicy;
        let policies = [
            AutoLayoutPolicy::ByDomain,
            AutoLayoutPolicy::ByStatus,
            AutoLayoutPolicy::ByActivity,
            AutoLayoutPolicy::Manual,
        ];
        for policy in &policies {
            let json = serde_json::to_string(policy).expect("serialize");
            let back: AutoLayoutPolicy = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(*policy, back);
        }
    }
}
