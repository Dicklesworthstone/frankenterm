use crate::config::validate_domain_name;
use crate::*;
use frankenterm_dynamic::{FromDynamic, ToDynamic};
use std::path::PathBuf;

/// Configures an instance of a multiplexer that can be communicated
/// with via a unix domain socket
#[derive(Debug, Clone, FromDynamic, ToDynamic)]
pub struct UnixDomain {
    /// The name of this specific domain.  Must be unique amongst
    /// all types of domain in the configuration file.
    #[dynamic(validate = "validate_domain_name")]
    pub name: String,

    /// The path to the socket.  If unspecified, a resonable default
    /// value will be computed.
    pub socket_path: Option<PathBuf>,

    /// If true, connect to this domain automatically at startup
    #[dynamic(default)]
    pub connect_automatically: bool,

    /// If true, do not attempt to start this server if we try and fail to
    /// connect to it.
    #[dynamic(default)]
    pub no_serve_automatically: bool,

    /// If we decide that we need to start the server, the command to run
    /// to set that up.  The default is to spawn:
    /// `wezterm-mux-server --daemonize`
    /// but it can be useful to set this to eg:
    /// `wsl -e wezterm-mux-server --daemonize` to start up
    /// a unix domain inside a wsl container.
    pub serve_command: Option<Vec<String>>,

    /// Instead of directly connecting to `socket_path`,
    /// spawn this command and use its stdin/stdout in place of
    /// the socket.
    pub proxy_command: Option<Vec<String>>,

    /// If true, bypass checking for secure ownership of the
    /// socket_path.  This is not recommended on a multi-user
    /// system, but is useful for example when running the
    /// server inside a WSL container but with the socket
    /// on the host NTFS volume.
    #[dynamic(default)]
    pub skip_permissions_check: bool,

    #[dynamic(default = "default_read_timeout")]
    pub read_timeout: Duration,

    #[dynamic(default = "default_write_timeout")]
    pub write_timeout: Duration,

    /// Don't use default_local_echo_threshold_ms() here to
    /// disable the predictive echo for Unix domains by default.
    pub local_echo_threshold_ms: Option<u64>,

    /// Show time since last response when waiting for a response.
    /// It is recommended to use
    /// <https://wezterm.org/config/lua/pane/get_metadata.html#since_last_response_ms>
    /// instead.
    #[dynamic(default)]
    pub overlay_lag_indicator: bool,
}

impl Default for UnixDomain {
    fn default() -> Self {
        Self {
            name: String::new(),
            socket_path: None,
            connect_automatically: false,
            no_serve_automatically: false,
            serve_command: None,
            skip_permissions_check: false,
            read_timeout: default_read_timeout(),
            write_timeout: default_write_timeout(),
            local_echo_threshold_ms: None,
            proxy_command: None,
            overlay_lag_indicator: false,
        }
    }
}

#[derive(Debug)]
pub enum UnixTarget {
    Socket(PathBuf),
    Proxy(Vec<String>),
}

impl UnixDomain {
    pub fn socket_path(&self) -> PathBuf {
        self.socket_path
            .as_ref()
            .cloned()
            .unwrap_or_else(|| RUNTIME_DIR.join("sock"))
    }

    pub fn target(&self) -> UnixTarget {
        if let Some(proxy) = &self.proxy_command {
            UnixTarget::Proxy(proxy.clone())
        } else {
            UnixTarget::Socket(self.socket_path())
        }
    }

    pub fn default_unix_domains() -> Vec<Self> {
        vec![UnixDomain {
            name: "unix".to_string(),
            read_timeout: default_read_timeout(),
            write_timeout: default_read_timeout(),
            ..Default::default()
        }]
    }

    pub fn serve_command(&self) -> anyhow::Result<Vec<OsString>> {
        match self.serve_command.as_ref() {
            Some(cmd) => Ok(cmd.iter().map(Into::into).collect()),
            None => Ok(vec![
                std::env::current_exe()?
                    .with_file_name(if cfg!(windows) {
                        "wezterm-mux-server.exe"
                    } else {
                        "wezterm-mux-server"
                    })
                    .into_os_string(),
                OsString::from("--daemonize"),
            ]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_target_is_runtime_socket() {
        let domain = UnixDomain::default();
        match domain.target() {
            UnixTarget::Socket(path) => assert_eq!(path, RUNTIME_DIR.join("sock")),
            UnixTarget::Proxy(_) => panic!("expected socket target"),
        }
    }

    #[test]
    fn proxy_command_takes_precedence_in_target() {
        let mut domain = UnixDomain::default();
        domain.proxy_command = Some(vec!["ssh".to_string(), "host".to_string()]);
        match domain.target() {
            UnixTarget::Proxy(cmd) => {
                assert_eq!(cmd, vec!["ssh".to_string(), "host".to_string()]);
            }
            UnixTarget::Socket(_) => panic!("expected proxy target"),
        }
    }

    #[test]
    fn default_unix_domains_contains_expected_entry() {
        let domains = UnixDomain::default_unix_domains();
        assert_eq!(domains.len(), 1);
        assert_eq!(domains[0].name, "unix");
        assert_eq!(domains[0].read_timeout, default_read_timeout());
        assert_eq!(domains[0].write_timeout, default_read_timeout());
    }

    #[test]
    fn serve_command_uses_override_when_configured() {
        let mut domain = UnixDomain::default();
        domain.serve_command = Some(vec!["custom-mux".to_string(), "--daemonize".to_string()]);

        let command = domain.serve_command().expect("serve command");
        assert_eq!(
            command,
            vec![OsString::from("custom-mux"), OsString::from("--daemonize")]
        );
    }

    #[test]
    fn serve_command_default_appends_daemonize_flag() {
        let domain = UnixDomain::default();
        let command = domain.serve_command().expect("serve command");

        assert!(
            command.len() >= 2,
            "default command should include executable and --daemonize"
        );
        assert_eq!(command[1], OsString::from("--daemonize"));
    }
}
