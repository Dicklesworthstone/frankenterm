#[cfg(test)]
mod tests {
    use frankenterm_core::patterns::{
        AgentType, DetectionContext, PatternEngine, PatternPack, RuleDef, Severity,
    };

    #[test]
    fn split_pattern_detection_fails_without_overlap() {
        // Define a rule that matches "Hello World"
        let rule = RuleDef {
            id: "codex.test.split_pattern".to_string(),
            agent_type: AgentType::Codex,
            event_type: "test".to_string(),
            severity: Severity::Info,
            anchors: vec!["Hello World".to_string()],
            regex: None,
            description: "test".to_string(),
            remediation: None,
            workflow: None,
            manual_fix: None,
            preview_command: None,
            learn_more_url: None,
        };

        let pack = PatternPack::new("test", "0.1.0", vec![rule]);
        let engine = PatternEngine::with_packs(vec![pack]).unwrap();
        let mut ctx = DetectionContext::new();

        // Simulate a stream split exactly in the middle of the pattern
        let chunk1 = "User said: Hello ";
        let chunk2 = "World to you.";

        // Detect on chunk 1
        let detections1 = engine.detect_with_context(chunk1, &mut ctx);
        assert!(detections1.is_empty(), "Chunk 1 should not match");

        // Detect on chunk 2 (simulating delta-only processing)
        let detections2 = engine.detect_with_context(chunk2, &mut ctx);

        // This is expected to FAIL currently because we don't stitch chunks
        // If it passes, then my assumption is wrong (or magic is happening)
        assert!(
            !detections2.is_empty(),
            "Chunk 2 should match when combined with Chunk 1 context"
        );
    }
}
