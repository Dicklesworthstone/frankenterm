use frankenterm_core::ingest::{CapturedSegmentKind, PaneCursor};

#[test]
fn repro_alt_screen_transition_data_loss() {
    let mut cursor = PaneCursor::new(1);

    // 1. Initial state: "A"
    let seg1 = cursor.capture_snapshot("A", 100, Some(false));
    assert!(seg1.is_some());
    let s1 = seg1.unwrap();
    assert_eq!(s1.content, "A");
    assert!(matches!(s1.kind, CapturedSegmentKind::Delta));

    // 2. Alt-screen transition with content "AB"
    // Ideally, entering alt-screen clears the screen, so the new content should be fully captured.
    // "AB" shares prefix "A" with previous state.
    // We simulate the backend reporting "in_alt_screen = true".

    // Previous snapshot was "A". Current is "AB".
    // extract_delta("A", "AB") -> Content("B") (because "A" is a prefix).

    let seg2 = cursor.capture_snapshot("AB", 100, Some(true));
    assert!(seg2.is_some());
    let s2 = seg2.unwrap();

    // Verify it's a Gap
    assert!(
        matches!(s2.kind, CapturedSegmentKind::Gap { .. }),
        "Should be a Gap due to alt-screen entry"
    );

    // CRITICAL CHECK: The content should be the FULL snapshot "AB", because the consumer
    // treats a Gap as a reset. If we send "B", the consumer renders "B", losing "A".
    assert_eq!(
        s2.content, "AB",
        "Gap segment MUST contain full snapshot, not delta"
    );
}
