#[cfg(test)]
mod tests {
    use wa_core::policy::is_command_candidate;

    #[test]
    fn test_dangerous_interpreters_are_detected() {
        // These dangerous commands using interpreters MUST be detected as command candidates.
        // The COMMAND_TOKENS list includes perl, ruby, php, lua to catch these.
        let dangerous_commands = vec![
            "perl -e 'system(\"rm -rf /\")'",
            "ruby -e 'system(\"rm -rf /\")'",
            "php -r 'system(\"rm -rf /\");'",
            "lua -e 'os.execute(\"rm -rf /\")'",
        ];

        for cmd in dangerous_commands {
            println!("Testing: {}", cmd);
            assert!(
                is_command_candidate(cmd),
                "Command '{}' should be detected as a command candidate",
                cmd
            );
        }
    }

    #[test]
    fn test_tclsh_is_detected() {
        // tclsh was added to COMMAND_TOKENS to catch Tcl interpreter abuse
        let cmd = "tclsh <<< 'exec rm -rf /'";
        assert!(
            is_command_candidate(cmd),
            "tclsh should be detected as a command candidate"
        );
    }

    #[test]
    fn test_eval_is_detected() {
        // eval was added to COMMAND_TOKENS to catch this pattern
        let cmd = "eval \"rm -rf /\"";
        assert!(
            is_command_candidate(cmd),
            "eval should be detected as a command candidate"
        );
    }
}
