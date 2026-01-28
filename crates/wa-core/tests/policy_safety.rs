#[cfg(test)]
mod tests {
    use wa_core::policy::is_command_candidate;

    #[test]
    fn test_dangerous_interpreters_detected() {
        // These MUST be detected as command candidates to be checked for safety.
        let dangerous_commands = vec![
            "perl -e 'system(\"rm -rf /\")'",
            "ruby -e 'system(\"rm -rf /\")'",
            "php -r 'system(\"rm -rf /\");'",
            "lua -e 'os.execute(\"rm -rf /\")'",
            "tclsh <<< 'exec rm -rf /'",
            "eval \"rm -rf /\"",
            "exec rm -rf /",
            "busybox rm -rf /",
            "env rm -rf /",
            "xargs rm -rf",
            "{ rm -rf / }", // Block syntax
        ];

        for cmd in dangerous_commands {
            println!("Testing: {}", cmd);
            assert!(
                is_command_candidate(cmd),
                "Command '{}' was NOT detected as a candidate!",
                cmd
            );
        }
    }

    #[test]
    fn test_safe_text_ignored() {
        let safe_texts = vec!["hello world", "just a comment", "  leading spaces but safe"];
        for text in safe_texts {
            assert!(
                !is_command_candidate(text),
                "Text '{}' was falsely detected as command",
                text
            );
        }
    }
}
