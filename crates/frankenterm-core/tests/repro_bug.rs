#[cfg(test)]
mod tests {
    use frankenterm_core::patterns::{AgentType, PatternEngine, PatternPack, RuleDef, Severity};

    #[test]
    fn detect_finds_multiple_occurrences() {
        let rule = RuleDef {
            id: "codex.test.multiple".to_string(),
            agent_type: AgentType::Codex,
            event_type: "test".to_string(),
            severity: Severity::Info,
            anchors: vec!["Error:".to_string()],
            regex: Some(r"Error: (?P<msg>\w+)".to_string()),
            description: "test".to_string(),
            remediation: None,
            workflow: None,
            manual_fix: None,
            preview_command: None,
            learn_more_url: None,
        };

        let pack = PatternPack::new("test", "0.1.0", vec![rule]);
        let engine = PatternEngine::with_packs(vec![pack]).unwrap();

        let text = "Error: First\nSome noise\nError: Second";
        let detections = engine.detect(text);

        // Current implementation likely finds only 1
        assert_eq!(detections.len(), 2, "Should find both occurrences");
        assert_eq!(detections[0].extracted["msg"], "First");
        assert_eq!(detections[1].extracted["msg"], "Second");
    }
}
