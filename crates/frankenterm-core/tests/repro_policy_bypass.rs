use frankenterm_core::policy::is_command_candidate;

#[test]
fn test_quoted_command_is_candidate() {
    assert!(is_command_candidate("\"rm\" -rf /"));
}
