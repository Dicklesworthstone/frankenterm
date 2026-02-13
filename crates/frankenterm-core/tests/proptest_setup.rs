//! Property-based tests for the `setup` module.
//!
//! Covers shell-type parsing roundtrips, SSH config parsing invariants,
//! ft-block detection/extraction consistency, Lua generation determinism,
//! and identity-path redaction safety.

use frankenterm_core::setup::{
    ShellType, SshHost, extract_ft_block, generate_ssh_domains_lua, has_ft_block,
    has_shell_ft_block, parse_ssh_config,
};
use proptest::prelude::*;

// =========================================================================
// Strategies
// =========================================================================

fn arb_shell_type() -> impl Strategy<Value = ShellType> {
    prop_oneof![
        Just(ShellType::Bash),
        Just(ShellType::Zsh),
        Just(ShellType::Fish),
    ]
}

fn arb_ssh_host() -> impl Strategy<Value = SshHost> {
    (
        "[a-z][a-z0-9-]{0,15}",
        proptest::option::of("[a-z0-9.-]{3,20}"),
        proptest::option::of("[a-z]{3,10}"),
        proptest::option::of(1u16..65535),
        proptest::collection::vec("~/.ssh/[a-z_]{3,10}", 0..3),
    )
        .prop_map(|(alias, hostname, user, port, identity_files)| SshHost {
            alias,
            hostname,
            user,
            port,
            identity_files,
        })
}

// =========================================================================
// ShellType parsing roundtrips
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20))]

    /// ShellType::from_name(shell.name()) always roundtrips.
    #[test]
    fn prop_shell_type_name_roundtrip(shell in arb_shell_type()) {
        let name = shell.name();
        let parsed = ShellType::from_name(name);
        prop_assert_eq!(parsed, Some(shell));
    }

    /// ShellType::from_path with arbitrary prefix still parses correctly.
    #[test]
    fn prop_shell_type_from_path(
        shell in arb_shell_type(),
        prefix in "/[a-z]{1,5}(/[a-z]{1,5}){0,3}",
    ) {
        let path = format!("{}/{}", prefix, shell.name());
        let parsed = ShellType::from_path(&path);
        prop_assert_eq!(parsed, Some(shell));
    }
}

#[test]
fn shell_type_names_are_distinct() {
    let names: Vec<&str> = [ShellType::Bash, ShellType::Zsh, ShellType::Fish]
        .iter()
        .map(|s| s.name())
        .collect();
    assert_ne!(names[0], names[1]);
    assert_ne!(names[1], names[2]);
    assert_ne!(names[0], names[2]);
}

#[test]
fn shell_type_names_are_lowercase() {
    for shell in [ShellType::Bash, ShellType::Zsh, ShellType::Fish] {
        let name = shell.name();
        assert_eq!(name, name.to_lowercase());
        assert!(!name.is_empty());
    }
}

#[test]
fn shell_type_from_name_rejects_unknown() {
    assert_eq!(ShellType::from_name("powershell"), None);
    assert_eq!(ShellType::from_name(""), None);
    assert_eq!(ShellType::from_name("sh"), None);
}

#[test]
fn shell_type_osc133_snippets_nonempty() {
    for shell in [ShellType::Bash, ShellType::Zsh, ShellType::Fish] {
        let snippet = shell.osc133_snippet();
        assert!(
            !snippet.is_empty(),
            "{:?} snippet should be nonempty",
            shell
        );
        // All snippets should contain the OSC 133 escape
        assert!(
            snippet.contains("133"),
            "{:?} snippet should contain '133'",
            shell
        );
    }
}

// =========================================================================
// SSH config parsing
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Parsing produces one SshHost per non-wildcard Host alias.
    #[test]
    fn prop_ssh_config_host_count(
        aliases in proptest::collection::vec("[a-z][a-z0-9-]{2,10}", 1..5),
    ) {
        let mut config = String::new();
        for alias in &aliases {
            config.push_str(&format!("Host {}\n  HostName {}.example.com\n\n", alias, alias));
        }
        let hosts = parse_ssh_config(&config);
        // Unique aliases should map 1:1 to hosts
        let unique: std::collections::HashSet<_> = aliases.iter().collect();
        prop_assert_eq!(hosts.len(), unique.len());
    }

    /// Wildcard-only Host stanzas produce no entries.
    #[test]
    fn prop_ssh_config_wildcards_ignored(
        pattern in "\\*[a-z.]{0,5}",
    ) {
        let config = format!("Host {}\n  HostName example.com\n", pattern);
        let hosts = parse_ssh_config(&config);
        prop_assert!(hosts.is_empty(), "wildcard '{}' should produce no hosts", pattern);
    }

    /// HostName directive is correctly parsed.
    #[test]
    fn prop_ssh_config_hostname_parsed(
        alias in "[a-z][a-z0-9]{2,8}",
        hostname in "[a-z0-9.-]{3,20}",
    ) {
        let config = format!("Host {}\n  HostName {}\n", alias, hostname);
        let hosts = parse_ssh_config(&config);
        prop_assert_eq!(hosts.len(), 1);
        prop_assert_eq!(hosts[0].alias.as_str(), alias.as_str());
        prop_assert_eq!(hosts[0].hostname.as_deref(), Some(hostname.as_str()));
    }

    /// Port directive is correctly parsed.
    #[test]
    fn prop_ssh_config_port_parsed(
        alias in "[a-z][a-z0-9]{2,8}",
        port in 1u16..65535,
    ) {
        let config = format!("Host {}\n  Port {}\n", alias, port);
        let hosts = parse_ssh_config(&config);
        prop_assert_eq!(hosts.len(), 1);
        prop_assert_eq!(hosts[0].port, Some(port));
    }

    /// User directive is correctly parsed.
    #[test]
    fn prop_ssh_config_user_parsed(
        alias in "[a-z][a-z0-9]{2,8}",
        user in "[a-z]{3,10}",
    ) {
        let config = format!("Host {}\n  User {}\n", alias, user);
        let hosts = parse_ssh_config(&config);
        prop_assert_eq!(hosts.len(), 1);
        prop_assert_eq!(hosts[0].user.as_deref(), Some(user.as_str()));
    }

    /// IdentityFile entries are collected in order.
    #[test]
    fn prop_ssh_config_identity_files_collected(
        alias in "[a-z][a-z0-9]{2,8}",
        files in proptest::collection::vec("[a-z_]{3,8}", 1..4),
    ) {
        let mut config = format!("Host {}\n", alias);
        for file in &files {
            config.push_str(&format!("  IdentityFile ~/.ssh/{}\n", file));
        }
        let hosts = parse_ssh_config(&config);
        prop_assert_eq!(hosts.len(), 1);
        let expected: Vec<String> = files.iter().map(|f| format!("~/.ssh/{}", f)).collect();
        prop_assert_eq!(&hosts[0].identity_files, &expected);
    }
}

#[test]
fn ssh_config_empty_produces_no_hosts() {
    let hosts = parse_ssh_config("");
    assert!(hosts.is_empty());
}

#[test]
fn ssh_config_comments_only_produces_no_hosts() {
    let config = "# This is a comment\n# Another comment\n";
    let hosts = parse_ssh_config(config);
    assert!(hosts.is_empty());
}

#[test]
fn ssh_config_inline_comments_stripped() {
    let config = "Host myserver # a comment\n  HostName example.com # host\n  User admin # user\n";
    let hosts = parse_ssh_config(config);
    assert_eq!(hosts.len(), 1);
    assert_eq!(hosts[0].alias, "myserver");
    assert_eq!(hosts[0].hostname.as_deref(), Some("example.com"));
    assert_eq!(hosts[0].user.as_deref(), Some("admin"));
}

#[test]
fn ssh_config_equals_syntax() {
    let config = "Host myserver\n  HostName=example.com\n  Port=2222\n";
    let hosts = parse_ssh_config(config);
    assert_eq!(hosts.len(), 1);
    assert_eq!(hosts[0].hostname.as_deref(), Some("example.com"));
    assert_eq!(hosts[0].port, Some(2222));
}

// =========================================================================
// ft-block detection and extraction
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// has_ft_block ↔ extract_ft_block consistency.
    #[test]
    fn prop_ft_block_detection_extraction_consistent(
        hosts in proptest::collection::vec(arb_ssh_host(), 0..3),
        scrollback in 1000u64..100_000,
    ) {
        let block = generate_ssh_domains_lua(&hosts, scrollback);
        // The generated block should be detected
        prop_assert!(has_ft_block(&block), "generated block should be detected");
        let extracted = extract_ft_block(&block);
        prop_assert!(extracted.is_some(), "generated block should be extractable");
    }

    /// has_ft_block is false for arbitrary text without markers.
    #[test]
    fn prop_no_markers_no_block(text in "[a-zA-Z0-9 \\n]{0,200}") {
        // Only report as having block if both markers present
        if !text.contains("-- FT-BEGIN") || !text.contains("-- FT-END") {
            prop_assert!(!has_ft_block(&text));
        }
    }
}

#[test]
fn has_ft_block_requires_both_markers() {
    assert!(!has_ft_block("-- FT-BEGIN (do not edit this block)"));
    assert!(!has_ft_block("-- FT-END"));
    assert!(!has_ft_block("some random content"));
    assert!(has_ft_block(
        "-- FT-BEGIN (do not edit this block)\nstuff\n-- FT-END"
    ));
}

// =========================================================================
// Shell ft-block detection
// =========================================================================

#[test]
fn has_shell_ft_block_requires_both_markers() {
    assert!(!has_shell_ft_block("# FT-BEGIN (do not edit this block)"));
    assert!(!has_shell_ft_block("# FT-END"));
    assert!(!has_shell_ft_block("some random content"));
    assert!(has_shell_ft_block(
        "# FT-BEGIN (do not edit this block)\nstuff\n# FT-END"
    ));
}

// =========================================================================
// generate_ssh_domains_lua
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Generated Lua always contains FT-BEGIN and FT-END markers.
    #[test]
    fn prop_generated_lua_has_markers(
        hosts in proptest::collection::vec(arb_ssh_host(), 0..5),
        scrollback in 1000u64..100_000,
    ) {
        let lua = generate_ssh_domains_lua(&hosts, scrollback);
        prop_assert!(lua.contains("FT-BEGIN"), "should have begin marker");
        prop_assert!(lua.contains("FT-END"), "should have end marker");
    }

    /// Generated Lua contains scrollback_lines config.
    #[test]
    fn prop_generated_lua_has_scrollback(
        scrollback in 1000u64..100_000,
    ) {
        let lua = generate_ssh_domains_lua(&[], scrollback);
        let expected = format!("scrollback_lines = {scrollback}");
        prop_assert!(lua.contains(&expected),
            "should contain scrollback_lines = {}", scrollback);
    }

    /// Generation is deterministic.
    #[test]
    fn prop_generated_lua_deterministic(
        hosts in proptest::collection::vec(arb_ssh_host(), 0..3),
        scrollback in 1000u64..100_000,
    ) {
        let lua1 = generate_ssh_domains_lua(&hosts, scrollback);
        let lua2 = generate_ssh_domains_lua(&hosts, scrollback);
        prop_assert_eq!(lua1, lua2);
    }

    /// Each host's alias appears in the generated Lua.
    #[test]
    fn prop_generated_lua_contains_all_aliases(
        hosts in proptest::collection::vec(arb_ssh_host(), 1..4),
    ) {
        let lua = generate_ssh_domains_lua(&hosts, 10_000);
        for host in &hosts {
            prop_assert!(lua.contains(&host.alias),
                "alias '{}' should appear in generated Lua", host.alias);
        }
    }
}

// =========================================================================
// SshHost::redacted_identity_files
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// Redacted paths never contain the full directory prefix.
    #[test]
    fn prop_redacted_paths_no_full_directory(
        filenames in proptest::collection::vec("[a-z_]{3,10}", 1..4),
    ) {
        let host = SshHost {
            alias: "test".to_string(),
            hostname: None,
            user: None,
            port: None,
            identity_files: filenames.iter().map(|f| format!("/home/user/.ssh/{}", f)).collect(),
        };
        let redacted = host.redacted_identity_files();
        for path in &redacted {
            prop_assert!(!path.contains("/home/user"),
                "redacted path '{}' should not contain full directory", path);
        }
    }

    /// Tilde paths preserve the ~/filename pattern.
    #[test]
    fn prop_redacted_tilde_paths(
        filename in "[a-z_]{3,10}",
    ) {
        let host = SshHost {
            alias: "test".to_string(),
            hostname: None,
            user: None,
            port: None,
            identity_files: vec![format!("~/.ssh/{}", filename)],
        };
        let redacted = host.redacted_identity_files();
        prop_assert_eq!(redacted.len(), 1);
        prop_assert_eq!(&redacted[0], &format!("~/{}", filename));
    }
}
