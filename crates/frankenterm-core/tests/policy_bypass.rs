use frankenterm_core::policy::{ActionKind, ActorKind, PolicyInput, is_command_candidate};

#[test]
fn repro_policy_bypass_absolute_path() {
    // 1. Direct command "rm" is detected
    assert!(
        is_command_candidate("rm -rf /"),
        "Plain 'rm' should be detected"
    );

    // 2. Absolute path "/bin/rm" - CURRENTLY FAILS
    // The policy engine relies on is_command_candidate returning true to even trigger
    // the regex checks. If this returns false, the regexes are never run.
    assert!(
        is_command_candidate("/bin/rm -rf /"),
        "Absolute path '/bin/rm' should be detected"
    );

    // 3. Relative path "./rm"
    assert!(
        is_command_candidate("./rm -rf /"),
        "Relative path './rm' should be detected"
    );
}
