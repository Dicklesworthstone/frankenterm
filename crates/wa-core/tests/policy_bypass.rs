#[cfg(test)]
mod tests {
    use wa_core::policy::is_command_candidate;

    #[test]
    fn test_common_destructive_interpreters_bypassed() {
        // These should ideally be detected as command candidates to be checked for safety,
        // but current implementation might miss them.
        let dangerous_commands = vec![
            "perl -e 'system(\"rm -rf /\")'",
            "ruby -e 'system(\"rm -rf /\")'",
            "php -r 'system(\"rm -rf /\");'",
            "lua -e 'os.execute(\"rm -rf /\")'",
            "tclsh <<< 'exec rm -rf /'",
        ];

        for cmd in dangerous_commands {
            // We expect this to fail (return false) currently, confirming the vulnerability.
            // If it returns true, then it's already safe (or my understanding is wrong).
            println!("Testing: {}", cmd);
            assert!(
                !is_command_candidate(cmd),
                "Command '{}' was unexpectedly detected (it should have been missed by current buggy logic)",
                cmd
            );
        }
    }

    #[test]
    fn test_eval_bypass() {
        let cmd = "eval \"rm -rf /\"";
        assert!(
            !is_command_candidate(cmd),
            "eval should be missed by current logic"
        );
    }
}
