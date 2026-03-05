#[cfg(test)]
mod tests {
    use crate::patterns::{DetectionContext, PatternEngine};
    use std::time::Duration;

    #[test]
    fn reproduction_dedup_suppresses_and_expires() {
        let engine = PatternEngine::new();
        let mut context = DetectionContext::new();
        // Set a very short TTL to test expiration
        context.set_ttl(Duration::from_millis(10));

        // Define a test text that triggers a rule
        let text = "Usage limit reached for all Pro models"; // triggers gemini.usage.reached

        // First detection
        let detections1 = engine.detect_with_context(text, &mut context);
        assert!(!detections1.is_empty(), "Should detect first time");

        // Second detection immediately after
        let detections2 = engine.detect_with_context(text, &mut context);
        assert!(detections2.is_empty(), "Should be deduplicated immediately");

        // Wait for TTL to expire
        std::thread::sleep(Duration::from_millis(15));

        // Third detection after TTL
        let detections3 = engine.detect_with_context(text, &mut context);
        assert!(
            !detections3.is_empty(),
            "Should detect again after TTL expiration"
        );
    }
}
