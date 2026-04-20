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

    /// [ft-rrrn5] Every distinct `OsString` env key must survive the JSON
    /// roundtrip. The previous `to_string_lossy` map adapter collapsed any
    /// pair of non-UTF-8 keys that produced the same `U+FFFD` replacement
    /// sequence, silently dropping entries. The replacement adapter emits
    /// a sequence of `(OsString, EnvEntry)` pairs so every distinct input
    /// key is preserved byte-for-byte, regardless of UTF-8 validity.
    #[cfg(unix)]
    #[test]
    fn command_builder_env_preserves_non_utf8_keys_across_roundtrip(
        // Generate several arbitrary byte keys, some of which are
        // deliberately non-UTF-8 so `to_string_lossy` would collapse them.
        raw_keys in proptest::collection::vec(
            proptest::collection::vec(any::<u8>(), 1..12),
            1..8,
        ),
        values in proptest::collection::vec(arb_small_string(), 1..8),
    ) {
        use std::os::unix::ffi::OsStringExt;

        // Zip keys and values, deduplicate by the exact byte content
        // (BTreeMap semantics). Use only entries where a value exists.
        let pair_count = raw_keys.len().min(values.len());
        let mut seen_keys: std::collections::BTreeSet<Vec<u8>> = Default::default();
        let mut distinct_pairs: Vec<(OsString, String)> = Vec::new();
        for (raw, value) in raw_keys.into_iter().zip(values.into_iter()).take(pair_count) {
            if seen_keys.insert(raw.clone()) {
                distinct_pairs.push((OsString::from_vec(raw), value));
            }
        }
        prop_assume!(!distinct_pairs.is_empty());

        let mut cmd = CommandBuilder::new("/bin/true");
        cmd.env_clear();
        for (key, value) in &distinct_pairs {
            cmd.env(key, value);
        }

        let json = serde_json::to_string(&cmd).unwrap();
        let back: CommandBuilder = serde_json::from_str(&json).unwrap();

        // Same number of env entries — no silent de-duplication.
        prop_assert_eq!(&back, &cmd);

        for (key, value) in &distinct_pairs {
            let got = back.get_env(key);
            prop_assert_eq!(
                got,
                Some(std::ffi::OsStr::new(value)),
                "key {:?} missing or wrong value after roundtrip",
                key,
            );
        }
    }

    /// [ft-rrrn5] Two distinct non-UTF-8 keys that both lossy-encode to
    /// `U+FFFD` must remain distinct after a serde roundtrip. This is the
    /// regression test for the `to_string_lossy` map-collision bug.
    #[cfg(unix)]
    #[test]
    fn command_builder_env_collision_regression_distinct_replacement_keys(
        value_a in arb_small_string(),
        value_b in arb_small_string(),
    ) {
        use std::os::unix::ffi::OsStringExt;

        // Two different invalid-UTF-8 byte sequences — both contain a lone
        // continuation byte (0x80) but at a different position, so neither
        // is valid UTF-8 and both would render as U+FFFD under lossy
        // conversion, collapsing to the same String key.
        let key_a = OsString::from_vec(vec![0x80, 0x41]);
        let key_b = OsString::from_vec(vec![0x42, 0x80]);
        prop_assume!(key_a != key_b);

        let mut cmd = CommandBuilder::new("/bin/true");
        cmd.env_clear();
        cmd.env(&key_a, &value_a);
        cmd.env(&key_b, &value_b);

        let json = serde_json::to_string(&cmd).unwrap();
        let back: CommandBuilder = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(&back, &cmd);
        prop_assert_eq!(
            back.get_env(&key_a),
            Some(std::ffi::OsStr::new(value_a.as_str())),
        );
        prop_assert_eq!(
            back.get_env(&key_b),
            Some(std::ffi::OsStr::new(value_b.as_str())),
        );
    }
}
