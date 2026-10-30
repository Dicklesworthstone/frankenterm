use crate::*;
use frankenterm_dynamic::{FromDynamic, ToDynamic};
use std::fs::{File, OpenOptions};
use std::path::PathBuf;

#[derive(Default, Debug, Clone, FromDynamic, ToDynamic)]
pub struct DaemonOptions {
    /// Read only to warn: upstream locked this file to keep a second
    /// `--daemonize` server off the runtime dir. Here the mux takes an
    /// exclusive lease on `<socket_path>.lock` before binding, so a second
    /// server on the same socket already fails fast, and nothing reads this.
    #[dynamic(
        deprecated = "has no effect: the mux server holds an exclusive lock on <socket_path>.lock, which already refuses a second server on the same socket"
    )]
    pub pid_file: Option<PathBuf>,
    pub stdout: Option<PathBuf>,
    pub stderr: Option<PathBuf>,
}

/// Set the sticky bit on path.
/// This is used in a couple of situations where we want files that
/// we create in the RUNTIME_DIR to not be removed by a potential
/// tmpwatch daemon.
pub fn set_sticky_bit(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = path.metadata() {
            let mut perms = metadata.permissions();
            let mode = perms.mode();
            perms.set_mode(mode | u32::from(libc::S_ISVTX));
            let _ = std::fs::set_permissions(&path, perms);
        }
    }

    #[cfg(windows)]
    {
        let _ = path;
    }
}

fn open_log(path: PathBuf) -> anyhow::Result<File> {
    create_user_owned_dirs(
        path.parent()
            .ok_or_else(|| anyhow!("path {} has no parent dir!?", path.display()))?,
    )?;
    let mut options = OpenOptions::new();
    options.write(true).create(true).append(true);
    options
        .open(&path)
        .map_err(|e| anyhow!("failed to open log stream: {}: {}", path.display(), e))
}

impl DaemonOptions {
    pub fn stdout(&self) -> PathBuf {
        self.stdout
            .as_ref()
            .cloned()
            .unwrap_or_else(|| RUNTIME_DIR.join("log"))
    }

    pub fn stderr(&self) -> PathBuf {
        self.stderr
            .as_ref()
            .cloned()
            .unwrap_or_else(|| RUNTIME_DIR.join("log"))
    }

    pub fn open_stdout(&self) -> anyhow::Result<File> {
        open_log(self.stdout())
    }

    pub fn open_stderr(&self) -> anyhow::Result<File> {
        open_log(self.stderr())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_options_default_has_no_overrides() {
        let opts = DaemonOptions::default();
        assert!(opts.pid_file.is_none());
        assert!(opts.stdout.is_none());
        assert!(opts.stderr.is_none());
    }

    #[test]
    fn a_configured_pid_file_is_reported_as_deprecated_not_rejected() {
        use frankenterm_dynamic::{FromDynamicOptions, UnknownFieldAction, Value};
        let mut fields = std::collections::BTreeMap::new();
        fields.insert(
            Value::String("pid_file".to_string()),
            Value::String("/custom/pid".to_string()),
        );
        let config = Value::Object(fields.into());

        // Strict config files deny unknown fields but only warn on deprecated
        // ones, so an existing pid_file keeps loading.
        let strict = FromDynamicOptions {
            unknown_fields: UnknownFieldAction::Deny,
            deprecated_fields: UnknownFieldAction::Warn,
        };
        assert!(DaemonOptions::from_dynamic(&config, strict).is_ok());

        let deny = FromDynamicOptions {
            unknown_fields: UnknownFieldAction::Deny,
            deprecated_fields: UnknownFieldAction::Deny,
        };
        let error = DaemonOptions::from_dynamic(&config, deny).unwrap_err();
        assert!(
            matches!(
                error,
                frankenterm_dynamic::Error::DeprecatedField {
                    field_name: "pid_file",
                    ..
                }
            ),
            "{:#}",
            error
        );
        let message = format!("{:#}", error);
        assert!(message.contains("<socket_path>.lock"), "{}", message);
    }

    #[test]
    fn stdout_uses_runtime_dir_when_none() {
        let opts = DaemonOptions::default();
        let path = opts.stdout();
        assert!(
            path.ends_with("log"),
            "expected path ending with 'log', got: {}",
            path.display()
        );
    }

    #[test]
    fn stdout_uses_override_when_some() {
        let opts = DaemonOptions {
            stdout: Some(PathBuf::from("/custom/stdout.log")),
            ..Default::default()
        };
        assert_eq!(opts.stdout(), PathBuf::from("/custom/stdout.log"));
    }

    #[test]
    fn stderr_uses_runtime_dir_when_none() {
        let opts = DaemonOptions::default();
        let path = opts.stderr();
        assert!(
            path.ends_with("log"),
            "expected path ending with 'log', got: {}",
            path.display()
        );
    }

    #[test]
    fn stderr_uses_override_when_some() {
        let opts = DaemonOptions {
            stderr: Some(PathBuf::from("/custom/stderr.log")),
            ..Default::default()
        };
        assert_eq!(opts.stderr(), PathBuf::from("/custom/stderr.log"));
    }

    #[test]
    fn stdout_and_stderr_default_to_same_path() {
        let opts = DaemonOptions::default();
        assert_eq!(opts.stdout(), opts.stderr());
    }

    #[test]
    fn open_stdout_with_temp_dir_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("daemon-test.log");
        let opts = DaemonOptions {
            stdout: Some(log_path.clone()),
            ..Default::default()
        };
        let file = opts.open_stdout();
        assert!(file.is_ok(), "open_stdout failed: {:?}", file.err());
        assert!(log_path.exists());
    }

    #[test]
    fn open_stderr_with_temp_dir_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("daemon-stderr.log");
        let opts = DaemonOptions {
            stderr: Some(log_path.clone()),
            ..Default::default()
        };
        let file = opts.open_stderr();
        assert!(file.is_ok(), "open_stderr failed: {:?}", file.err());
        assert!(log_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn set_sticky_bit_on_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sticky_test");
        std::fs::write(&path, b"test").unwrap();

        set_sticky_bit(&path);

        let metadata = path.metadata().unwrap();
        let mode = metadata.permissions().mode();
        assert!(
            mode & u32::from(libc::S_ISVTX) != 0,
            "sticky bit should be set, mode: {:#o}",
            mode
        );
    }

    #[test]
    fn set_sticky_bit_on_nonexistent_does_not_panic() {
        // Should silently handle missing files
        set_sticky_bit(Path::new("/tmp/nonexistent_daemon_test_file_12345"));
    }
}
