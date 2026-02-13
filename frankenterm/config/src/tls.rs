use crate::config::validate_domain_name;
use crate::*;
use frankenterm_dynamic::{FromDynamic, ToDynamic};

#[derive(Default, Debug, Clone, FromDynamic, ToDynamic)]
pub struct TlsDomainServer {
    /// The address:port combination on which the server will listen
    /// for client connections
    pub bind_address: String,

    /// the path to an x509 PEM encoded private key file
    pub pem_private_key: Option<PathBuf>,

    /// the path to an x509 PEM encoded certificate file
    pub pem_cert: Option<PathBuf>,

    /// the path to an x509 PEM encoded CA chain file
    pub pem_ca: Option<PathBuf>,

    /// A set of paths to load additional CA certificates.
    /// Each entry can be either the path to a directory
    /// or to a PEM encoded CA file.  If an entry is a directory,
    /// then its contents will be loaded as CA certs and added
    /// to the trust store.
    #[dynamic(default)]
    pub pem_root_certs: Vec<PathBuf>,
}

#[derive(Default, Debug, Clone, FromDynamic, ToDynamic)]
pub struct TlsDomainClient {
    /// The name of this specific domain.  Must be unique amongst
    /// all types of domain in the configuration file.
    #[dynamic(validate = "validate_domain_name")]
    pub name: String,

    /// If set, use ssh to connect, start the server, and obtain
    /// a certificate.
    /// The value is "user@host:port", just like "wezterm ssh" accepts.
    pub bootstrap_via_ssh: Option<String>,

    /// identifies the host:port pair of the remote server.
    pub remote_address: String,

    /// the path to an x509 PEM encoded private key file
    pub pem_private_key: Option<PathBuf>,

    /// the path to an x509 PEM encoded certificate file
    pub pem_cert: Option<PathBuf>,

    /// the path to an x509 PEM encoded CA chain file
    pub pem_ca: Option<PathBuf>,

    /// A set of paths to load additional CA certificates.
    /// Each entry can be either the path to a directory or to a PEM encoded
    /// CA file.  If an entry is a directory, then its contents will be
    /// loaded as CA certs and added to the trust store.
    #[dynamic(default)]
    pub pem_root_certs: Vec<PathBuf>,

    /// explicitly control whether the client checks that the certificate
    /// presented by the server matches the hostname portion of
    /// `remote_address`.  The default is true.  This option is made
    /// available for troubleshooting purposes and should not be used outside
    /// of a controlled environment as it weakens the security of the TLS
    /// channel.
    #[dynamic(default)]
    pub accept_invalid_hostnames: bool,

    /// the hostname string that we expect to match against the common name
    /// field in the certificate presented by the server.  This defaults to
    /// the hostname portion of the `remote_address` configuration and you
    /// should not normally need to override this value.
    pub expected_cn: Option<String>,

    /// If true, connect to this domain automatically at startup
    #[dynamic(default)]
    pub connect_automatically: bool,

    #[dynamic(default = "default_read_timeout")]
    pub read_timeout: Duration,

    #[dynamic(default = "default_write_timeout")]
    pub write_timeout: Duration,

    #[dynamic(default = "default_local_echo_threshold_ms")]
    pub local_echo_threshold_ms: Option<u64>,

    /// The path to the wezterm binary on the remote host
    pub remote_wezterm_path: Option<String>,

    /// Show time since last response when waiting for a response.
    /// It is recommended to use
    /// <https://wezterm.org/config/lua/pane/get_metadata.html#since_last_response_ms>
    /// instead.
    #[dynamic(default)]
    pub overlay_lag_indicator: bool,
}

impl TlsDomainClient {
    pub fn ssh_parameters(&self) -> Option<anyhow::Result<SshParameters>> {
        self.bootstrap_via_ssh
            .as_ref()
            .map(|user_at_host_and_port| user_at_host_and_port.parse())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_parameters_returns_none_when_unset() {
        let client = TlsDomainClient::default();
        assert!(client.ssh_parameters().is_none());
    }

    #[test]
    fn ssh_parameters_parses_valid_bootstrap_value() {
        let client = TlsDomainClient {
            bootstrap_via_ssh: Some("alice@example.com:2222".to_string()),
            ..TlsDomainClient::default()
        };

        let parsed = client
            .ssh_parameters()
            .expect("ssh bootstrap should be present")
            .expect("ssh bootstrap should parse");
        assert_eq!(parsed.username.as_deref(), Some("alice"));
        assert_eq!(parsed.host_and_port, "example.com:2222");
    }

    #[test]
    fn ssh_parameters_surfaces_parse_errors() {
        let client = TlsDomainClient {
            bootstrap_via_ssh: Some("a@b@c".to_string()),
            ..TlsDomainClient::default()
        };

        let err = client
            .ssh_parameters()
            .expect("ssh bootstrap should be present")
            .expect_err("invalid ssh bootstrap should fail");
        assert!(
            err.to_string().contains("failed to parse ssh parameters"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn tls_domain_server_default() {
        let server = TlsDomainServer::default();
        assert_eq!(server.bind_address, "");
        assert!(server.pem_private_key.is_none());
        assert!(server.pem_cert.is_none());
        assert!(server.pem_ca.is_none());
        assert!(server.pem_root_certs.is_empty());
    }

    #[test]
    fn tls_domain_server_clone() {
        let server = TlsDomainServer {
            bind_address: "127.0.0.1:8080".to_string(),
            pem_private_key: Some("/tmp/key.pem".into()),
            pem_cert: None,
            pem_ca: None,
            pem_root_certs: vec![],
        };
        let cloned = server.clone();
        assert_eq!(cloned.bind_address, "127.0.0.1:8080");
        assert!(cloned.pem_private_key.is_some());
    }

    #[test]
    fn tls_domain_server_debug() {
        let server = TlsDomainServer::default();
        let dbg = format!("{:?}", server);
        assert!(dbg.contains("TlsDomainServer"));
    }

    #[test]
    fn tls_domain_client_default() {
        let client = TlsDomainClient::default();
        assert_eq!(client.name, "");
        assert_eq!(client.remote_address, "");
        assert!(!client.accept_invalid_hostnames);
        assert!(!client.connect_automatically);
        assert!(!client.overlay_lag_indicator);
        assert!(client.bootstrap_via_ssh.is_none());
        assert!(client.expected_cn.is_none());
        assert!(client.remote_wezterm_path.is_none());
    }

    #[test]
    fn tls_domain_client_clone() {
        let client = TlsDomainClient {
            name: "remote".to_string(),
            remote_address: "host:1234".to_string(),
            connect_automatically: true,
            ..TlsDomainClient::default()
        };
        let cloned = client.clone();
        assert_eq!(cloned.name, "remote");
        assert_eq!(cloned.remote_address, "host:1234");
        assert!(cloned.connect_automatically);
    }

    #[test]
    fn tls_domain_client_debug() {
        let client = TlsDomainClient::default();
        let dbg = format!("{:?}", client);
        assert!(dbg.contains("TlsDomainClient"));
    }
}
