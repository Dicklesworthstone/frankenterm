#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    // Mocking time is hard in Rust without a crate, but we can test the logic
    // by exposing a "clear_expired" method or similar.
    // For now, let's just demonstrate the "forever suppression" behavior.

    #[test]
    fn reproduction_dedup_suppresses_forever() {
        let engine = PatternEngine::new();
        let mut context = DetectionContext::new();
        
        // Define a test text that triggers a rule
        let text = "Usage limit reached for all Pro models"; // triggers gemini.usage.reached
        
        // First detection
        let detections1 = engine.detect_with_context(text, &mut context);
        assert!(!detections1.is_empty(), "Should detect first time");
        
        // Second detection immediately after
        let detections2 = engine.detect_with_context(text, &mut context);
        assert!(detections2.is_empty(), "Should be deduplicated immediately");
        
        // In the current implementation, this will stay suppressed forever 
        // (until 1000 other keys push it out).
        // We want to verify that we can't easily "expire" it.
        
        // Simulating "5 hours later" is impossible with the current struct 
        // because it doesn't store time.
        // This confirms the architectural missing feature.
    }
}
