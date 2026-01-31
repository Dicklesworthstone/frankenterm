//! Distributed mode transport (TLS/mTLS scaffolding).
#![forbid(unsafe_code)]

use std::path::Path;
use std::sync::{Arc, OnceLock};

use thiserror::Error;

use crate::config::{DistributedAuthMode, DistributedConfig, DistributedTlsConfig};

#[cfg(feature = "distributed")]
use rustls::{ClientConfig, RootCertStore, ServerConfig};
#[cfg(feature = "distributed")]
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
#[cfg(feature = "distributed")]
use rustls_pemfile::{certs, private_key};

/// TLS configuration bundle for distributed mode.
#[cfg(feature = "distributed")]
#[derive(Clone)]
pub struct DistributedTlsBundle {
    pub server: Arc<ServerConfig>,
    pub client: Arc<ClientConfig>,
}

/// TLS errors for distributed mode.
#[derive(Error, Debug)]
pub enum DistributedTlsError {
    #[error("TLS is not enabled in distributed.tls")]
    TlsDisabled,

    #[error("Missing certificate path for TLS identity")]
    MissingCertPath,

    #[error("Missing private key path for TLS identity")]
    MissingKeyPath,

    #[error("Missing CA path for mTLS client verification")]
    MissingClientCaPath,

    #[error("Missing CA path for server verification")]
    MissingServerCaPath,

    #[error("Invalid minimum TLS version: {0}")]
    InvalidMinTlsVersion(String),

    #[error("Failed to read PEM file {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },

    #[error("No certificates found in PEM file: {0}")]
    EmptyCertChain(String),

    #[error("No private key found in PEM file: {0}")]
    EmptyPrivateKey(String),

    #[error("TLS config error: {0}")]
    Config(String),
}

impl DistributedTlsError {
    fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.display().to_string(),
            source,
        }
    }
}

#[cfg(feature = "distributed")]
fn ensure_crypto_provider() -> Result<(), DistributedTlsError> {
    static INSTALLED: OnceLock<()> = OnceLock::new();

    if INSTALLED.get().is_some() {
        return Ok(());
    }

    let provider = rustls::crypto::ring::default_provider();
    provider
        .install_default()
        .map_err(|_| DistributedTlsError::Config("failed to install crypto provider".to_string()))?;
    INSTALLED.set(()).ok();
    Ok(())
}

#[cfg(feature = "distributed")]
fn resolve_tls_versions(
    min_version: &str,
) -> Result<Vec<&'static rustls::SupportedProtocolVersion>, DistributedTlsError> {
    match min_version.trim() {
        "1.2" | "1.2+" => Ok(vec![&rustls::version::TLS13, &rustls::version::TLS12]),
        "1.3" | "1.3+" => Ok(vec![&rustls::version::TLS13]),
        other => Err(DistributedTlsError::InvalidMinTlsVersion(other.to_string())),
    }
}

#[cfg(feature = "distributed")]
fn load_cert_chain(path: &Path) -> Result<Vec<CertificateDer<'static>>, DistributedTlsError> {
    let mut reader = std::io::BufReader::new(
        std::fs::File::open(path).map_err(|e| DistributedTlsError::io(path, e))?,
    );
    let cert_chain = certs(&mut reader).map_err(|e| DistributedTlsError::io(path, e))?;
    if cert_chain.is_empty() {
        return Err(DistributedTlsError::EmptyCertChain(path.display().to_string()));
    }
    Ok(cert_chain)
}

#[cfg(feature = "distributed")]
fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>, DistributedTlsError> {
    let mut reader = std::io::BufReader::new(
        std::fs::File::open(path).map_err(|e| DistributedTlsError::io(path, e))?,
    );
    let key = private_key(&mut reader)
        .map_err(|e| DistributedTlsError::io(path, e))?
        .ok_or_else(|| DistributedTlsError::EmptyPrivateKey(path.display().to_string()))?;
    Ok(key)
}

#[cfg(feature = "distributed")]
fn add_to_root_store(
    root_store: &mut RootCertStore,
    certs: Vec<CertificateDer<'static>>,
) {
    let _ = root_store.add_parsable_certificates(certs);
}

#[cfg(feature = "distributed")]
fn build_server_config(
    tls: &DistributedTlsConfig,
    auth_mode: DistributedAuthMode,
) -> Result<Arc<ServerConfig>, DistributedTlsError> {
    if !tls.enabled {
        return Err(DistributedTlsError::TlsDisabled);
    }

    ensure_crypto_provider()?;

    let cert_path = tls
        .cert_path
        .as_deref()
        .ok_or(DistributedTlsError::MissingCertPath)?;
    let key_path = tls
        .key_path
        .as_deref()
        .ok_or(DistributedTlsError::MissingKeyPath)?;

    let cert_chain = load_cert_chain(Path::new(cert_path))?;
    let key = load_private_key(Path::new(key_path))?;
    let versions = resolve_tls_versions(&tls.min_tls_version)?;

    let builder = ServerConfig::builder()
        .with_protocol_versions(&versions)
        .map_err(|e| DistributedTlsError::Config(e.to_string()))?;

    let server_config = if auth_mode.requires_mtls() {
        let ca_path = tls
            .client_ca_path
            .as_deref()
            .ok_or(DistributedTlsError::MissingClientCaPath)?;
        let client_certs = load_cert_chain(Path::new(ca_path))?;
        let mut roots = RootCertStore::empty();
        add_to_root_store(&mut roots, client_certs);
        let verifier = rustls::server::WebPkiClientVerifier::builder(roots.into())
            .build()
            .map_err(|e| DistributedTlsError::Config(e.to_string()))?;
        builder
            .with_client_cert_verifier(verifier)
            .with_single_cert(cert_chain, key)
            .map_err(|e| DistributedTlsError::Config(e.to_string()))?
    } else {
        builder
            .with_no_client_auth()
            .with_single_cert(cert_chain, key)
            .map_err(|e| DistributedTlsError::Config(e.to_string()))?
    };

    Ok(Arc::new(server_config))
}

#[cfg(feature = "distributed")]
fn build_client_config(
    tls: &DistributedTlsConfig,
    auth_mode: DistributedAuthMode,
    server_ca_path: Option<&Path>,
) -> Result<Arc<ClientConfig>, DistributedTlsError> {
    if !tls.enabled {
        return Err(DistributedTlsError::TlsDisabled);
    }

    ensure_crypto_provider()?;

    let versions = resolve_tls_versions(&tls.min_tls_version)?;
    let mut roots = RootCertStore::empty();

    let ca_path = server_ca_path
        .and_then(|path| path.to_str().map(|value| value.to_string()))
        .or_else(|| tls.cert_path.clone())
        .ok_or(DistributedTlsError::MissingServerCaPath)?;
    let ca_certs = load_cert_chain(Path::new(&ca_path))?;
    add_to_root_store(&mut roots, ca_certs);

    let builder = ClientConfig::builder()
        .with_protocol_versions(&versions)
        .map_err(|e| DistributedTlsError::Config(e.to_string()))?
        .with_root_certificates(roots);

    let client_config = if auth_mode.requires_mtls() {
        let cert_path = tls
            .cert_path
            .as_deref()
            .ok_or(DistributedTlsError::MissingCertPath)?;
        let key_path = tls
            .key_path
            .as_deref()
            .ok_or(DistributedTlsError::MissingKeyPath)?;
        let cert_chain = load_cert_chain(Path::new(cert_path))?;
        let key = load_private_key(Path::new(key_path))?;
        builder
            .with_client_auth_cert(cert_chain, key)
            .map_err(|e| DistributedTlsError::Config(e.to_string()))?
    } else {
        builder.with_no_client_auth()
    };

    Ok(Arc::new(client_config))
}

#[cfg(feature = "distributed")]
#[must_use]
pub fn build_tls_bundle(
    config: &DistributedConfig,
    server_ca_path: Option<&Path>,
) -> Result<DistributedTlsBundle, DistributedTlsError> {
    let server = build_server_config(&config.tls, config.auth_mode)?;
    let client = build_client_config(&config.tls, config.auth_mode, server_ca_path)?;

    Ok(DistributedTlsBundle { server, client })
}

#[cfg(feature = "distributed")]
#[must_use]
pub fn build_tls_server_name(bind_addr: &str) -> Result<ServerName<'static>, DistributedTlsError> {
    let host = bind_addr
        .split(':')
        .next()
        .unwrap_or(bind_addr)
        .trim();
    let name = if host.is_empty() { "localhost" } else { host };
    ServerName::try_from(name.to_string())
        .map_err(|_| DistributedTlsError::Config("invalid server name".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "distributed")]
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    #[cfg(feature = "distributed")]
    use tokio::net::TcpListener;
    #[cfg(feature = "distributed")]
    use tokio_rustls::{TlsAcceptor, TlsConnector};

    const SERVER_CERT: &str = "-----BEGIN CERTIFICATE-----\nMIIDCTCCAfGgAwIBAgIUE80IDTEN+Gsw+YEyX3/RxEm7y20wDQYJKoZIhvcNAQEL\nBQAwFDESMBAGA1UEAwwJbG9jYWxob3N0MB4XDTI2MDEzMTA1MzY1M1oXDTI2MDIw\nMTA1MzY1M1owFDESMBAGA1UEAwwJbG9jYWxob3N0MIIBIjANBgkqhkiG9w0BAQEF\nAAOCAQ8AMIIBCgKCAQEAm5+VdJZPpI5juiQSfWUOLTjo3A/beyX1etb8RCKBB167\n+KqDCZsPUNalAOl/kgFhZJjHHxBKhEwZORxRmPIM4EIIUA+yq2D7kKw0wQd50VjI\ncyHbjwtrAf9Q+xVSzZZNTJ/SbZzp6OH0uviiGzJGbzNigX6at1djdpMxMsMaj3Gk\nICw9Aa7WhOqIZBfO0rwVJdr+WqKvlfR20SwI7QuEdgSbHdEw09zhQjb9rkw5a4QA\n2klTDAJCWV/jNEsmIk7j7GQ22AGiR5Mxn4MPBpFcdb5NQ2lWY0M+Ph9S+9D8Oo98\nXBcwKIvc4ySN2ksz39wkFJfNz1QUpHvi59D9KPLiywIDAQABo1MwUTAdBgNVHQ4E\nFgQUvmP/SY9W7MqInyEWbEwCGlTVHGAwHwYDVR0jBBgwFoAUvmP/SY9W7MqInyEW\nbEwCGlTVHGAwDwYDVR0TAQH/BAUwAwEB/zANBgkqhkiG9w0BAQsFAAOCAQEAW9Fg\noD5Z315yXijyqISH+AUxlfGHPCOtV1+zr7e9/AN48Fbwuw9F0PdbZUG6E3QIfVrW\nCgvAbkP8mNN+qKdsWdZ3QH9E6HFc9uAK/g8OhJ2ayYbR4bhc72QKy6uXC+Ku6w8f\nXbtIMe4mVOZC16ELB8NLT8lhqukR+XBzD23nQY3o+u9HZWUjDnJuHHs5KbrUp1TG\nyvIgZ+dtwZ8qfP08kU4JzZiJpMN6olXBTIywa10djmn4icvsaFs2gK8H4fZ5fSAG\neEdY/kurQVLOWuhJw7O7RAw+GwFKx4oQIAQ+u0OzyLo6LVEd1RKs5jAArOa7ojw4\nmUc9I1WRq6ivcBVZEg==\n-----END CERTIFICATE-----\n";
    const SERVER_KEY: &str = "-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQCbn5V0lk+kjmO6\nJBJ9ZQ4tOOjcD9t7JfV61vxEIoEHXrv4qoMJmw9Q1qUA6X+SAWFkmMcfEEqETBk5\nHFGY8gzgQghQD7KrYPuQrDTBB3nRWMhzIduPC2sB/1D7FVLNlk1Mn9JtnOno4fS6\n+KIbMkZvM2KBfpq3V2N2kzEywxqPcaQgLD0BrtaE6ohkF87SvBUl2v5aoq+V9HbR\nLAjtC4R2BJsd0TDT3OFCNv2uTDlrhADaSVMMAkJZX+M0SyYiTuPsZDbYAaJHkzGf\ngw8GkVx1vk1DaVZjQz4+H1L70Pw6j3xcFzAoi9zjJI3aSzPf3CQUl83PVBSke+Ln\n0P0o8uLLAgMBAAECggEAC/u9X/VlG5hFUKOPcu1xqoTL1Yo5DeIN5ZAha8O2aNFg\nKxVCMt6oTsLAgPH5+0206LClAJzaCBbuCEpIdD0LRcd/6V5UFUNKC8le5JBFTpwG\nD2uHgxvPeR9mZnctYZxVxnU2GG+xSVgA5tbXcL2t08lCqBMOKdkGVS6fZrfEV1A7\ngUM5yEv2YRuxM88jH3w39XOSj/vVQ31ua7P/GVByLCLwszIP+MKzVFjiFKmYRZPT\nI2sK1HZCFKTpfaUC9qXA2vUz+4yaz2c0ZA5Q0zzBkPXQ6i0k4l1V0LOPvgjJqcRW\npI6hoC5mrk5V/C0ptkd+O3TykjKIkjjjnLhdsw/HiQKBgQDbWis3xWvIltGUDjvS\nNDperl2GHjwvOJBp46cdaxrHe2GSPfxMK92xFsxc/gYtDoc+KOXd+dkcW8sqlD/c\nAVIQeJr1Wm9U+uRWgGT2v3y3gm7PqTmAZDXvmgTGUkIGC06eUqXNoWkVSEWyOoaQ\nrMiRrSUQMd+s1SZ3R5gmUyZNaQKBgQC1n7FF/STCv0wMIHnbQldvAeaG8bvW0I8z\nCxKWrJm9bnGVO4JzipDTB7fqcfHxuOBq+LQGyZ1qU5DO6te+hEbwlb7pCHYAsxPD\nzHiGIH9pb2xPhB35/M8N9e9Bem4uY4Ddleu7acnIteHpheOjWslHohs1LJQ/zDdF\nWItNUImEEwKBgB2B06588DLblvXDtGhifjeeXRZdmtr/UO3tod1jOwb6ofV6QSY+\nAGSCHJul0E7fVXIXLlTd90bJUls39h8yTkfI8Y9vyuozlePNAcfvmmdVNHLa4NFU\nqazBm0LSbe9CNiE2HPe8btZeEoIXPWLY22I9WG8FRoGJatNA3kbJiK2BAoGBAKNM\nDWtMCz0+GfXdTTKiF1KrdsVlmumtYtMV5YSrcx8qqdzvQH752vSiP+3+vPYEr2H+\ncn6Xz3zF3bDs3UViURYGzTtgbBh86gxlTwfpOCVoFQ35T6pwbwmyUYehuGbk9/xC\ny0wZ9V8MiS4ZP26kNcy+J5BLoI8r4ZXr4nOFMd8ZAoGASkM5+FpOEGOqMh3O8hSN\nOyaeaXyfP+lwB6HPCFaGFF5VQEercRxmso9l1jcjiTE6Iyu0oLBsrXa/odw1oNel\ncgV7HLLcnD13w2PbL+Z0cugOoYNvRg1RMkNFWSuQjL9Ol/a0Se0L1Ko/kCxlZ12r\n/l+dQ8wcNxsMVeXRyUXjnUU=\n-----END PRIVATE KEY-----\n";
    const CLIENT_CERT: &str = "-----BEGIN CERTIFICATE-----\nMIIDCTCCAfGgAwIBAgIUFeQdnrTv13mozVgbE4kfww8bIGgwDQYJKoZIhvcNAQEL\nBQAwFDESMBAGA1UEAwwJd2EtY2xpZW50MB4XDTI2MDEzMTA1MzY1OFoXDTI2MDIw\nMTA1MzY1OFowFDESMBAGA1UEAwwJd2EtY2xpZW50MIIBIjANBgkqhkiG9w0BAQEF\nAAOCAQ8AMIIBCgKCAQEArCXXSCsUISY9kVsZbwKwgxeKnvdyOMZRZiaZjCITIIax\nsANzwsVFpLjbMbCDxj1T3MkBXqE5WeLgdrATFupFev0uEKbkxOrifkLu2PdAvZiA\nrWqCsEaqchC6LV6l1GIcw42D8BPqY3q+ucE3lBmp6H4ZnmO4GiE+vYNL3EKN5h2a\ntgs2PEbq6azSHwaZpxNVYDV+D2US2nSFS7U1wWMOXlOpPiWPm6gJ6TQX+66iZfD9\nXUtL6+XQYtFzrRDI6zdUPp5bBvpvVqYTUjkW3tjzIhAVNvypCdTWlzIV+16MEgwF\nWEp8ZRvX6nNnQfsAegEHyhfg3XJmLZ2/dRk2mGrChQIDAQABo1MwUTAdBgNVHQ4E\nFgQUfCm6FqmAWt0R64dahvDaBh7SwoUwHwYDVR0jBBgwFoAUfCm6FqmAWt0R64da\nhvDaBh7SwoUwDwYDVR0TAQH/BAUwAwEB/zANBgkqhkiG9w0BAQsFAAOCAQEAM3lI\nOiLsdrQ/CTjVC43HrdujgtrCDHvrQldVGvJRZrnNWy5Kxdh/kDXh4jXNfQM7TnJj\n+rZC5Gjns2yBAHXSbHAFx9BnbiRUbXM3y5BtyZUDqpkGyzjeUKhtgyRsrWeRmtn6\na+DBMv6F6EBX1Wf9v8EsAs7NLXElAR0aUUQtITH7vrAE2E+KzVKgLVKTV9DOv5wB\noXYnSXgvs88q6oZcD7v1qlnKEJTcGFlDWonjw7VHWvzAnMtUISWchXg4ym3zGpLN\nfLvxu7fsQ0+7aJIL5GWqer3PflkyW/SrfkhNwhH378LlPIuQctPhAS4SocR+O+Qu\nOjvE9T/zUgTCs0ULtQ==\n-----END CERTIFICATE-----\n";
    const CLIENT_KEY: &str = "-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQCsJddIKxQhJj2R\nWxlvArCDF4qe93I4xlFmJpmMIhMghrGwA3PCxUWkuNsxsIPGPVPcyQFeoTlZ4uB2\nsBMW6kV6/S4QpuTE6uJ+Qu7Y90C9mICtaoKwRqpyELotXqXUYhzDjYPwE+pjer65\nwTeUGanofhmeY7gaIT69g0vcQo3mHZq2CzY8RurprNIfBpmnE1VgNX4PZRLadIVL\ntTXBYw5eU6k+JY+bqAnpNBf7rqJl8P1dS0vr5dBi0XOtEMjrN1Q+nlsG+m9WphNS\nORbe2PMiEBU2/KkJ1NaXMhX7XowSDAVYSnxlG9fqc2dB+wB6AQfKF+DdcmYtnb91\nGTaYasKFAgMBAAECggEAARX0Nsz2WzCRgVeLGfkXa2nZDDntaYz1dpJspzNc0WvA\nEA18uK0Dv8j4F8CWm2GHvdP/uZAE24htV+cYfu1ao4hLkG8F10JPzBhe4Mi+16NJ\nwBSn2KkT1wg8RsZ6zC1G1xEb4XlpFRg98gNIg853CAbplI2DW8wILp8dU2xd1qZ1\nACk1RjxUJRC4SLkw1lVwoYLuGPRZP/bbVTSBZc9k4rHRDBNX6W5iQX+WFM2ce/cK\n1zZWYt3wX7t0ppaQDiD9pdtRt3xHRH6+8/2xaAIRQPd/rlgdhw/y2WsibTaNTNY3\n31At86X6IEr2tU93hGVEwKIaHyVTLC3eh46ZCxYocQKBgQDZaLi7TVpplNrKv2SF\nTywbCl+LQR9k0fWDOBO2i3D39q0xav0iJbbBuw/aUedrcUI05WPD49MRfgHCzxuN\nhSoZUpyCXBu7OomR5Kjr8RMc2eqTxgqgJ9sxd4t4ZScw1a5TnCPqVMrXjKd6taaO\nI5Bu37NRsosCNE4xknSjLujKdQKBgQDKtGd8DUzStwl9e+PgbafkbvFZKNoBslfe\nRaYj4YFPa9RVpIc5TEOpLaanQ/cI5HCZTbXsi8oC0jaKiMqwLbH0kUu3xzURNiVV\nB7d/MB6Md62yluGzrUED/o7GP0566+dVrt1rHyGrYcwrzDqqWSx45SgcEQScjBDn\nVMylcb510QKBgFkhabrAN7TZZemBFcDheH+NfSYmCKzYtwKY1twbxrCh3NdJuYVq\nWMaL3i/vLCUMRcxRztjcagfd1PL83ZrjYSMIj0mSYXwcv5GkYOx55RAK9vSmWGzY\najCZhgBR2ANZVyLH/KFxKce8zlysZCAZzHjoFSgFPqAkuXnxRQgxjxaRAoGBAKqZ\nfML+Gpr5a6ZNv7Lh9zydVsjYRerJo4QxkCuAnikusm1F5H0Hv6ZCApGh9OZqBI4x\nxhwIZoNLUpXznMr8Glqgcl6A7RvIO1E+BmSoJf3It8qni9uBxEdNVfJp025G3tLR\nMg73C4pG6+QspW87T9+L1d8RP5VVmOBpETdo7IgRAoGAchELpTFBqL6gJS8UdBIY\n6fDjI7bLAbS857GK0nZGirnf5K7jsLqWhPXH8P1MdeQI07h3MhdFhNIVNMW+Aw/X\nv3laRSxE3sKVF8aRAjP/1JbwuN9/3hp7qd14X6cM8UR4VjdLrqBNvDcbIev5rlk+\ner8uVa4IHwqz2SbI76nmw1M=\n-----END PRIVATE KEY-----\n";

    #[cfg(feature = "distributed")]
    fn temp_pem(contents: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().expect("temp file");
        std::io::Write::write_all(file.as_file_mut(), contents.as_bytes())
            .expect("write pem");
        file
    }

    #[cfg(feature = "distributed")]
    #[tokio::test]
    async fn tls_handshake_succeeds() {
        let server_cert = temp_pem(SERVER_CERT);
        let server_key = temp_pem(SERVER_KEY);

        let mut config = DistributedConfig::default();
        config.enabled = true;
        config.tls.enabled = true;
        config.tls.cert_path = Some(server_cert.path().display().to_string());
        config.tls.key_path = Some(server_key.path().display().to_string());

        let server_config = build_server_config(&config.tls, DistributedAuthMode::Token)
            .expect("server config");
        let client_config = build_client_config(
            &config.tls,
            DistributedAuthMode::Token,
            Some(server_cert.path()),
        )
        .expect("client config");

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");

        let acceptor = TlsAcceptor::from(server_config);
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let mut tls_stream = acceptor.accept(stream).await.expect("accept tls");
            let mut buf = [0u8; 4];
            tls_stream.read_exact(&mut buf).await.expect("read");
            buf
        });

        let connector = TlsConnector::from(client_config);
        let server_name = ServerName::try_from("localhost")
            .expect("server name");
        let mut stream = connector
            .connect(server_name, tokio::net::TcpStream::connect(addr).await.expect("connect"))
            .await
            .expect("tls connect");
        stream.write_all(b"ping").await.expect("write");

        let received = server_task.await.expect("join");
        assert_eq!(&received, b"ping");
    }

    #[cfg(feature = "distributed")]
    #[tokio::test]
    async fn mtls_handshake_succeeds() {
        let server_cert = temp_pem(SERVER_CERT);
        let server_key = temp_pem(SERVER_KEY);
        let client_cert = temp_pem(CLIENT_CERT);
        let client_key = temp_pem(CLIENT_KEY);

        let mut server_config = DistributedConfig::default();
        server_config.enabled = true;
        server_config.auth_mode = DistributedAuthMode::Mtls;
        server_config.tls.enabled = true;
        server_config.tls.cert_path = Some(server_cert.path().display().to_string());
        server_config.tls.key_path = Some(server_key.path().display().to_string());
        server_config.tls.client_ca_path = Some(client_cert.path().display().to_string());

        let mut client_config = DistributedConfig::default();
        client_config.enabled = true;
        client_config.auth_mode = DistributedAuthMode::Mtls;
        client_config.tls.enabled = true;
        client_config.tls.cert_path = Some(client_cert.path().display().to_string());
        client_config.tls.key_path = Some(client_key.path().display().to_string());

        let server_tls = build_server_config(&server_config.tls, server_config.auth_mode)
            .expect("server config");
        let client_tls = build_client_config(
            &client_config.tls,
            client_config.auth_mode,
            Some(server_cert.path()),
        )
        .expect("client config");

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");

        let acceptor = TlsAcceptor::from(server_tls);
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let mut tls_stream = acceptor.accept(stream).await.expect("accept tls");
            let mut buf = [0u8; 2];
            tls_stream.read_exact(&mut buf).await.expect("read");
            buf
        });

        let connector = TlsConnector::from(client_tls);
        let server_name = ServerName::try_from("localhost")
            .expect("server name");
        let mut stream = connector
            .connect(server_name, tokio::net::TcpStream::connect(addr).await.expect("connect"))
            .await
            .expect("tls connect");
        stream.write_all(b"ok").await.expect("write");

        let received = server_task.await.expect("join");
        assert_eq!(&received, b"ok");
    }
}
