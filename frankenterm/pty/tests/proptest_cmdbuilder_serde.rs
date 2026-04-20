#![cfg(feature = "serde_support")]

use portable_pty::CommandBuilder;
use portable_pty::PtySize;
use proptest::prelude::*;
use std::ffi::OsString;

fn arb_small_string() -> impl Strategy<Value = String> {
    proptest::collection::vec(any::<char>(), 1..16).prop_map(|chars| chars.into_iter().collect())
}

fn arb_optional_string() -> impl Strategy<Value = Option<String>> {
    prop_oneof![Just(None), arb_small_string().prop_map(Some),]
}

fn arb_env_pairs() -> impl Strategy<Value = Vec<(String, String)>> {
    proptest::collection::vec((arb_small_string(), arb_small_string()), 0..8)
}

fn arb_args() -> impl Strategy<Value = Vec<String>> {
    proptest::collection::vec(arb_small_string(), 1..8)
}

fn arb_pty_size() -> impl Strategy<Value = PtySize> {
    (0u16..=4096, 0u16..=4096, 0u16..=8192, 0u16..=8192).prop_map(
        |(rows, cols, pixel_width, pixel_height)| PtySize {
            rows,
            cols,
            pixel_width,
            pixel_height,
        },
    )
}

fn build_command(
    argv: &[String],
    cwd: Option<&String>,
    env_pairs: &[(String, String)],
    controlling_tty: bool,
) -> CommandBuilder {
    let mut cmd = CommandBuilder::new(&argv[0]);
    cmd.env_clear();
    cmd.args(argv.iter().skip(1));
    if let Some(cwd) = cwd {
        cmd.cwd(cwd);
    }
    for (key, value) in env_pairs {
        cmd.env(key, value);
    }
    cmd.set_controlling_tty(controlling_tty);
    cmd
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// JSON roundtrip for CommandBuilder argv, cwd, controlling_tty, and env.
    ///
    /// Note: `env_clear()` is called to remove the process-inherited
    /// `BTreeMap<OsString, EnvEntry>` base environment. Generated env
    /// pairs use ASCII-safe `String` keys that serialize as valid JSON
    /// map keys.
    #[test]
    fn command_builder_json_roundtrip(
        argv in arb_args(),
        cwd in arb_optional_string(),
        env_pairs in arb_env_pairs(),
        controlling_tty in any::<bool>(),
    ) {
        let cmd = build_command(&argv, cwd.as_ref(), &env_pairs, controlling_tty);

        let json = serde_json::to_string(&cmd).unwrap();
        let back: CommandBuilder = serde_json::from_str(&json).unwrap();

        let expected_argv: Vec<OsString> = argv.iter().map(OsString::from).collect();
        prop_assert_eq!(back.get_argv(), &expected_argv);
        prop_assert_eq!(back.get_controlling_tty(), controlling_tty);
        prop_assert_eq!(back.get_cwd().cloned(), cwd.clone().map(OsString::from));

        let mut expected_env = env_pairs.clone();
        expected_env.sort();
        let mut actual_env: Vec<(String, String)> = back
            .iter_extra_env_as_str()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        actual_env.sort();
        prop_assert_eq!(actual_env, expected_env);
    }

    #[test]
    fn default_prog_command_builder_json_roundtrip(
        cwd in arb_optional_string(),
        env_pairs in arb_env_pairs(),
        controlling_tty in any::<bool>(),
    ) {
        let mut cmd = CommandBuilder::new_default_prog();
        cmd.env_clear();
        if let Some(cwd) = &cwd {
            cmd.cwd(cwd);
        }
        for (key, value) in &env_pairs {
            cmd.env(key, value);
        }
        cmd.set_controlling_tty(controlling_tty);

        let json = serde_json::to_string(&cmd).unwrap();
        let back: CommandBuilder = serde_json::from_str(&json).unwrap();

        prop_assert!(back.is_default_prog());
        prop_assert_eq!(back.get_argv(), &Vec::<OsString>::new());
        prop_assert_eq!(back.get_controlling_tty(), controlling_tty);
        prop_assert_eq!(back.get_cwd().cloned(), cwd.clone().map(OsString::from));

        let mut expected_env = env_pairs.clone();
        expected_env.sort();
        let mut actual_env: Vec<(String, String)> = back
            .iter_extra_env_as_str()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        actual_env.sort();
        prop_assert_eq!(actual_env, expected_env);
    }

    #[test]
    fn pty_size_json_roundtrip(value in arb_pty_size()) {
        let json = serde_json::to_string(&value).unwrap();
        let back: PtySize = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, value);
    }

    #[cfg(unix)]
    #[test]
    fn command_builder_umask_json_roundtrip(
        argv in arb_args(),
        cwd in arb_optional_string(),
        controlling_tty in any::<bool>(),
        umask_val in prop_oneof![
            Just(None),
            (0u32..=0o7777u32).prop_map(|v| Some(v as libc::mode_t)),
        ],
    ) {
        let mut cmd = CommandBuilder::new(&argv[0]);
        cmd.env_clear();
        cmd.args(argv.iter().skip(1));
        if let Some(cwd) = &cwd {
            cmd.cwd(cwd);
        }
        cmd.set_controlling_tty(controlling_tty);
        cmd.umask(umask_val);

        let json = serde_json::to_string(&cmd).unwrap();
        let back: CommandBuilder = serde_json::from_str(&json).unwrap();

        // PartialEq covers all fields including umask
        prop_assert_eq!(&back, &cmd);

        // Also verify through JSON Value that umask field is preserved
        let val: serde_json::Value = serde_json::from_str(&json).unwrap();
        match umask_val {
            None => prop_assert_eq!(&val["umask"], &serde_json::Value::Null),
            Some(m) => prop_assert_eq!(val["umask"].as_u64().unwrap(), m as u64),
        }
    }
}
