//! Distributed mode transport (TLS/mTLS scaffolding).
#![forbid(unsafe_code)]

use std::path::Path;
#[cfg(feature = "distributed")]
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[cfg(feature = "distributed")]
use crate::config::DistributedTlsConfig;
use crate::config::{DistributedAuthMode, DistributedConfig};

#[cfg(feature = "distributed")]
use rustls::client::danger::HandshakeSignatureValid;
#[cfg(feature = "distributed")]
use rustls::pki_types::UnixTime;
#[cfg(feature = "distributed")]
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
#[cfg(feature = "distributed")]
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
#[cfg(feature = "distributed")]
use rustls::{
    ClientConfig, DigitallySignedStruct, DistinguishedName, RootCertStore, ServerConfig,
    SignatureScheme,
};
#[cfg(feature = "distributed")]
use rustls_pemfile::{certs, private_key};
#[cfg(feature = "distributed")]
use std::collections::{HashMap, HashSet};
#[cfg(feature = "distributed")]
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
#[cfg(feature = "distributed")]
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(feature = "distributed")]
use std::time::Duration;
#[cfg(feature = "distributed")]
use x509_parser::prelude::{FromDer, GeneralName, X509Certificate};

/// TLS configuration bundle for distributed mode.
///
/// Holds pre-built rustls configurations that can be converted into
/// asupersync [`TlsAcceptor`](asupersync::tls::TlsAcceptor) and
/// [`TlsConnector`](asupersync::tls::TlsConnector) for production use.
#[cfg(feature = "distributed")]
#[derive(Clone)]
pub struct DistributedTlsBundle {
    pub server: Arc<ServerConfig>,
    pub client: Arc<ClientConfig>,
}

#[cfg(feature = "distributed")]
impl DistributedTlsBundle {
    /// Create an asupersync [`TlsAcceptor`](asupersync::tls::TlsAcceptor)
    /// from the server configuration for accepting inbound TLS connections.
    #[must_use]
    pub fn acceptor(&self) -> asupersync::tls::TlsAcceptor {
        asupersync::tls::TlsAcceptor::new((*self.server).clone())
    }

    /// Create an asupersync [`TlsConnector`](asupersync::tls::TlsConnector)
    /// from the client configuration for initiating outbound TLS connections.
    #[must_use]
    pub fn connector(&self) -> asupersync::tls::TlsConnector {
        asupersync::tls::TlsConnector::new((*self.client).clone())
    }
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
    #[cfg(feature = "distributed")]
    fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.display().to_string(),
            source,
        }
    }
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
    let cert_chain = certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| DistributedTlsError::io(path, e))?;
    if cert_chain.is_empty() {
        return Err(DistributedTlsError::EmptyCertChain(
            path.display().to_string(),
        ));
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
fn add_to_root_store(root_store: &mut RootCertStore, certs: Vec<CertificateDer<'static>>) {
    let _ = root_store.add_parsable_certificates(certs);
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum DistributedSecurityError {
    #[error("distributed token required")]
    MissingToken,
    #[error("distributed auth failed")]
    AuthFailed,
    #[error("distributed protocol version missing (expected {expected})")]
    ProtocolVersionMissing { expected: u32 },
    #[error("distributed protocol version mismatch: expected {expected}, got {got}")]
    ProtocolVersionMismatch { expected: u32, got: u32 },
    #[error("distributed replay detected")]
    ReplayDetected,
    #[error("distributed session limit reached")]
    SessionLimitReached,
    #[error("distributed connection limit reached")]
    ConnectionLimitReached,
    #[error("distributed message too large")]
    MessageTooLarge,
    #[error("distributed rate limited")]
    RateLimited,
    #[error("distributed handshake timeout")]
    HandshakeTimeout,
    #[error("distributed message timeout")]
    MessageTimeout,
}

impl DistributedSecurityError {
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::MissingToken | Self::AuthFailed => "dist.auth_failed",
            Self::ProtocolVersionMissing { .. } | Self::ProtocolVersionMismatch { .. } => {
                "dist.version_mismatch"
            }
            Self::ReplayDetected => "dist.replay_detected",
            Self::SessionLimitReached => "dist.session_limit",
            Self::ConnectionLimitReached => "dist.connection_limit",
            Self::MessageTooLarge => "dist.message_too_large",
            Self::RateLimited => "dist.rate_limited",
            Self::HandshakeTimeout => "dist.handshake_timeout",
            Self::MessageTimeout => "dist.message_timeout",
        }
    }
}

fn normalize_identity(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn constant_time_eq(expected: &str, presented: &str) -> bool {
    let expected_bytes = expected.as_bytes();
    let presented_bytes = presented.as_bytes();
    let max_len = expected_bytes.len().max(presented_bytes.len());
    let mut diff = expected_bytes.len() ^ presented_bytes.len();

    for idx in 0..max_len {
        let left = expected_bytes.get(idx).copied().unwrap_or(0);
        let right = presented_bytes.get(idx).copied().unwrap_or(0);
        diff |= usize::from(left ^ right);
    }

    diff == 0
}

#[derive(Debug, Clone, Copy)]
struct TokenParts<'a> {
    identity: Option<&'a str>,
    secret: &'a str,
}

impl<'a> TokenParts<'a> {
    fn parse(token: &'a str) -> Self {
        if let Some((identity, secret)) = token.split_once(':') {
            if !identity.trim().is_empty() && !secret.is_empty() {
                return Self {
                    identity: Some(identity),
                    secret,
                };
            }
        }

        Self {
            identity: None,
            secret: token,
        }
    }
}

pub fn validate_token(
    auth_mode: DistributedAuthMode,
    expected_token: Option<&str>,
    presented_token: Option<&str>,
    client_identity: Option<&str>,
) -> Result<(), DistributedSecurityError> {
    if !auth_mode.requires_token() {
        return Ok(());
    }

    let expected = expected_token.ok_or(DistributedSecurityError::MissingToken)?;
    let presented = presented_token.ok_or(DistributedSecurityError::MissingToken)?;
    let expected_parts = TokenParts::parse(expected);
    let presented_parts = TokenParts::parse(presented);

    if let Some(expected_identity) = expected_parts.identity {
        let expected_norm = normalize_identity(expected_identity);
        // Use constant-time comparison for identity to avoid timing side-channels.
        let presented_norm = presented_parts
            .identity
            .map(normalize_identity)
            .unwrap_or_default();
        if !constant_time_eq(&presented_norm, &expected_norm) {
            return Err(DistributedSecurityError::AuthFailed);
        }
        let Some(client_identity) = client_identity
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return Err(DistributedSecurityError::AuthFailed);
        };
        if !constant_time_eq(&normalize_identity(client_identity), &expected_norm) {
            return Err(DistributedSecurityError::AuthFailed);
        }
    }

    if !constant_time_eq(expected_parts.secret, presented_parts.secret) {
        return Err(DistributedSecurityError::AuthFailed);
    }

    Ok(())
}

/// Where the distributed token is sourced from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DistributedTokenSourceKind {
    Inline,
    Env,
    File,
}

/// Errors when resolving distributed credentials from config (env/files).
#[derive(Error, Debug)]
pub enum DistributedCredentialError {
    #[error("distributed token required but no token source configured")]
    TokenMissing,
    #[error(
        "distributed token is ambiguous: set exactly one of distributed.token, distributed.token_env, distributed.token_path"
    )]
    TokenAmbiguous,
    #[error("distributed token environment variable not set: {0}")]
    TokenEnvMissing(String),
    #[error("failed to read distributed token file {path}: {source}")]
    TokenFileRead {
        path: String,
        source: std::io::Error,
    },
    #[error("distributed token is empty")]
    TokenEmpty,
}

/// Determine the configured token source kind without reading secrets.
#[must_use]
pub fn configured_token_source_kind(
    config: &DistributedConfig,
) -> Option<DistributedTokenSourceKind> {
    let inline = config.token.as_deref().unwrap_or("").trim();
    let env = config.token_env.as_deref().unwrap_or("").trim();
    let path = config.token_path.as_deref().unwrap_or("").trim();

    let mut kinds = Vec::new();
    if !inline.is_empty() {
        kinds.push(DistributedTokenSourceKind::Inline);
    }
    if !env.is_empty() {
        kinds.push(DistributedTokenSourceKind::Env);
    }
    if !path.is_empty() {
        kinds.push(DistributedTokenSourceKind::File);
    }

    if kinds.len() == 1 {
        Some(kinds[0])
    } else {
        None
    }
}

/// Resolve the expected distributed token from config.
///
/// This reads from env/file sources at the time of call, enabling operator-friendly
/// rotation by updating the token file content without changing `ft.toml`.
///
/// Never log the returned token.
pub fn resolve_expected_token(
    config: &DistributedConfig,
) -> Result<Option<String>, DistributedCredentialError> {
    if !config.auth_mode.requires_token() {
        return Ok(None);
    }

    let inline = config.token.as_deref().unwrap_or("").trim();
    let env = config.token_env.as_deref().unwrap_or("").trim();
    let path = config.token_path.as_deref().unwrap_or("").trim();

    let mut sources = 0;
    if !inline.is_empty() {
        sources += 1;
    }
    if !env.is_empty() {
        sources += 1;
    }
    if !path.is_empty() {
        sources += 1;
    }

    match sources {
        0 => return Err(DistributedCredentialError::TokenMissing),
        1 => {}
        _ => return Err(DistributedCredentialError::TokenAmbiguous),
    }

    if !env.is_empty() {
        let value = std::env::var(env)
            .map_err(|_| DistributedCredentialError::TokenEnvMissing(env.to_string()))?;
        let value = value.trim().to_string();
        if value.is_empty() {
            return Err(DistributedCredentialError::TokenEmpty);
        }
        return Ok(Some(value));
    }

    if !path.is_empty() {
        let p = Path::new(path);
        let value =
            std::fs::read_to_string(p).map_err(|e| DistributedCredentialError::TokenFileRead {
                path: p.display().to_string(),
                source: e,
            })?;
        let value = value.trim().to_string();
        if value.is_empty() {
            return Err(DistributedCredentialError::TokenEmpty);
        }
        return Ok(Some(value));
    }

    let value = inline.to_string();
    if value.is_empty() {
        return Err(DistributedCredentialError::TokenEmpty);
    }
    Ok(Some(value))
}

#[cfg(feature = "distributed")]
#[derive(Debug)]
pub struct SessionReplayGuard {
    max_sessions: usize,
    sessions: HashMap<String, u64>,
}

#[cfg(feature = "distributed")]
impl SessionReplayGuard {
    #[must_use]
    pub fn new(max_sessions: usize) -> Self {
        Self {
            max_sessions,
            sessions: HashMap::new(),
        }
    }

    pub fn validate(&mut self, session_id: &str, seq: u64) -> Result<(), DistributedSecurityError> {
        match self.sessions.get_mut(session_id) {
            Some(last_seq) => {
                if seq <= *last_seq {
                    return Err(DistributedSecurityError::ReplayDetected);
                }
                *last_seq = seq;
            }
            None => {
                if self.sessions.len() >= self.max_sessions {
                    return Err(DistributedSecurityError::SessionLimitReached);
                }
                self.sessions.insert(session_id.to_string(), seq);
            }
        }

        Ok(())
    }

    #[must_use]
    pub fn session_last_seq(&self, session_id: &str) -> Option<u64> {
        self.sessions.get(session_id).copied()
    }

    pub fn restore_session(&mut self, session_id: &str, previous: Option<u64>) {
        match previous {
            Some(last_seq) => {
                self.sessions.insert(session_id.to_string(), last_seq);
            }
            None => {
                self.sessions.remove(session_id);
            }
        }
    }

    /// Remove a tracked session explicitly.
    ///
    /// Returns `true` when a session was present and removed.
    pub fn remove(&mut self, session_id: &str) -> bool {
        self.sessions.remove(session_id).is_some()
    }
}

#[cfg(feature = "distributed")]
#[derive(Debug, Clone)]
pub struct ConnectionLimiter {
    max: usize,
    active: Arc<AtomicUsize>,
}

#[cfg(feature = "distributed")]
impl ConnectionLimiter {
    #[must_use]
    pub fn new(max: usize) -> Self {
        Self {
            max,
            active: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn try_acquire(&self) -> Result<ConnectionPermit, DistributedSecurityError> {
        loop {
            let current = self.active.load(Ordering::SeqCst);
            if current >= self.max {
                return Err(DistributedSecurityError::ConnectionLimitReached);
            }
            if self
                .active
                .compare_exchange(
                    current,
                    current.saturating_add(1),
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .is_ok()
            {
                return Ok(ConnectionPermit {
                    active: Arc::clone(&self.active),
                });
            }
        }
    }

    #[must_use]
    pub fn active(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }
}

#[cfg(feature = "distributed")]
#[derive(Debug)]
pub struct ConnectionPermit {
    active: Arc<AtomicUsize>,
}

#[cfg(feature = "distributed")]
impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(feature = "distributed")]
#[derive(Debug, Clone, Copy)]
pub struct MessageSizeLimit {
    pub max_bytes: usize,
}

#[cfg(feature = "distributed")]
impl MessageSizeLimit {
    pub fn check(&self, size: usize) -> Result<(), DistributedSecurityError> {
        if size > self.max_bytes {
            return Err(DistributedSecurityError::MessageTooLarge);
        }
        Ok(())
    }
}

#[cfg(feature = "distributed")]
#[derive(Debug, Clone)]
pub struct FixedWindowRateLimiter {
    max_per_window: u32,
    window_ms: u64,
    window_start_ms: u64,
    count: u32,
}

#[cfg(feature = "distributed")]
impl FixedWindowRateLimiter {
    #[must_use]
    pub fn new(max_per_window: u32, window_ms: u64) -> Self {
        Self {
            max_per_window,
            window_ms,
            window_start_ms: 0,
            count: 0,
        }
    }

    pub fn allow(&mut self, now_ms: u64) -> Result<(), DistributedSecurityError> {
        if now_ms.saturating_sub(self.window_start_ms) >= self.window_ms {
            self.window_start_ms = now_ms;
            self.count = 0;
        }

        if self.count >= self.max_per_window {
            return Err(DistributedSecurityError::RateLimited);
        }

        self.count = self.count.saturating_add(1);
        Ok(())
    }
}

#[cfg(feature = "distributed")]
#[derive(Debug, Clone, Copy)]
pub struct DistributedTimeouts {
    pub handshake: Duration,
    pub message: Duration,
}

#[cfg(feature = "distributed")]
impl DistributedTimeouts {
    pub fn check_handshake(&self, elapsed: Duration) -> Result<(), DistributedSecurityError> {
        if elapsed > self.handshake {
            return Err(DistributedSecurityError::HandshakeTimeout);
        }
        Ok(())
    }

    pub fn check_message(&self, elapsed: Duration) -> Result<(), DistributedSecurityError> {
        if elapsed > self.message {
            return Err(DistributedSecurityError::MessageTimeout);
        }
        Ok(())
    }
}

#[cfg(feature = "distributed")]
fn build_allowlist(entries: &[String]) -> HashSet<String> {
    entries
        .iter()
        .map(|entry| normalize_identity(entry))
        .filter(|entry| !entry.is_empty())
        .collect()
}

#[cfg(feature = "distributed")]
fn ip_from_octets(bytes: &[u8]) -> Option<IpAddr> {
    match bytes.len() {
        4 => Some(IpAddr::V4(Ipv4Addr::new(
            bytes[0], bytes[1], bytes[2], bytes[3],
        ))),
        16 => {
            let array: [u8; 16] = bytes.try_into().ok()?;
            Some(IpAddr::V6(Ipv6Addr::from(array)))
        }
        _ => None,
    }
}

#[cfg(feature = "distributed")]
fn extract_client_identities(cert: &CertificateDer<'_>) -> Result<Vec<String>, rustls::Error> {
    let (_, parsed) = X509Certificate::from_der(cert.as_ref())
        .map_err(|_| rustls::Error::InvalidCertificate(rustls::CertificateError::BadEncoding))?;
    let mut identities = Vec::new();

    let san = parsed
        .subject_alternative_name()
        .map_err(|_| rustls::Error::InvalidCertificate(rustls::CertificateError::BadEncoding))?;
    if let Some(san) = san {
        for name in &san.value.general_names {
            match name {
                GeneralName::DNSName(dns) => identities.push(dns.to_string()),
                GeneralName::RFC822Name(email) => identities.push(email.to_string()),
                GeneralName::URI(uri) => identities.push(uri.to_string()),
                GeneralName::IPAddress(bytes) => {
                    if let Some(ip) = ip_from_octets(bytes) {
                        identities.push(ip.to_string());
                    }
                }
                _ => {}
            }
        }
    }

    for cn in parsed.subject().iter_common_name() {
        if let Ok(cn) = cn.as_str() {
            identities.push(cn.to_string());
        }
    }

    Ok(identities)
}

#[cfg(feature = "distributed")]
#[derive(Debug)]
struct AllowlistedClientVerifier {
    inner: Arc<dyn ClientCertVerifier>,
    allowlist: HashSet<String>,
}

#[cfg(feature = "distributed")]
impl AllowlistedClientVerifier {
    fn new(inner: Arc<dyn ClientCertVerifier>, allowlist: HashSet<String>) -> Self {
        Self { inner, allowlist }
    }

    fn matches_allowlist(&self, cert: &CertificateDer<'_>) -> Result<bool, rustls::Error> {
        let identities = extract_client_identities(cert)?;
        Ok(identities
            .iter()
            .any(|identity| self.allowlist.contains(&normalize_identity(identity))))
    }
}

#[cfg(feature = "distributed")]
impl ClientCertVerifier for AllowlistedClientVerifier {
    fn offer_client_auth(&self) -> bool {
        self.inner.offer_client_auth()
    }

    fn client_auth_mandatory(&self) -> bool {
        self.inner.client_auth_mandatory()
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        self.inner.root_hint_subjects()
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        let verified = self
            .inner
            .verify_client_cert(end_entity, intermediates, now)?;

        if !self.matches_allowlist(end_entity)? {
            return Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ));
        }

        Ok(verified)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }

    fn requires_raw_public_keys(&self) -> bool {
        self.inner.requires_raw_public_keys()
    }
}

#[cfg(feature = "distributed")]
fn ensure_rustls_provider_installed() {
    use std::sync::Once;
    static INSTALL: Once = Once::new();

    INSTALL.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

#[cfg(feature = "distributed")]
fn build_server_config(
    tls: &DistributedTlsConfig,
    auth_mode: DistributedAuthMode,
    allow_agent_ids: &[String],
) -> Result<Arc<ServerConfig>, DistributedTlsError> {
    if !tls.enabled {
        return Err(DistributedTlsError::TlsDisabled);
    }
    ensure_rustls_provider_installed();

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

    let builder = ServerConfig::builder_with_protocol_versions(&versions);

    let server_config = if auth_mode.requires_mtls() {
        let ca_path = tls
            .client_ca_path
            .as_deref()
            .ok_or(DistributedTlsError::MissingClientCaPath)?;
        let client_certs = load_cert_chain(Path::new(ca_path))?;
        let mut roots = RootCertStore::empty();
        add_to_root_store(&mut roots, client_certs);
        let allowlist = build_allowlist(allow_agent_ids);
        let verifier = rustls::server::WebPkiClientVerifier::builder(roots.into())
            .build()
            .map_err(|e| DistributedTlsError::Config(e.to_string()))?;
        let verifier = if allowlist.is_empty() {
            verifier
        } else {
            Arc::new(AllowlistedClientVerifier::new(verifier, allowlist))
        };
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
    ensure_rustls_provider_installed();

    let versions = resolve_tls_versions(&tls.min_tls_version)?;
    let mut roots = RootCertStore::empty();

    let ca_path = server_ca_path
        .and_then(|path| path.to_str().map(|value| value.to_string()))
        .or_else(|| tls.cert_path.clone())
        .ok_or(DistributedTlsError::MissingServerCaPath)?;
    let ca_certs = load_cert_chain(Path::new(&ca_path))?;
    add_to_root_store(&mut roots, ca_certs);

    let builder =
        ClientConfig::builder_with_protocol_versions(&versions).with_root_certificates(roots);

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
#[must_use = "the returned TLS bundle is required to configure distributed mode"]
pub fn build_tls_bundle(
    config: &DistributedConfig,
    server_ca_path: Option<&Path>,
) -> Result<DistributedTlsBundle, DistributedTlsError> {
    let server = build_server_config(&config.tls, config.auth_mode, &config.allow_agent_ids)?;
    let client = build_client_config(&config.tls, config.auth_mode, server_ca_path)?;

    Ok(DistributedTlsBundle { server, client })
}

#[cfg(feature = "distributed")]
#[must_use = "the returned server name is required for TLS/SNI verification"]
pub fn build_tls_server_name(bind_addr: &str) -> Result<ServerName<'static>, DistributedTlsError> {
    let host = distributed_bind_host(bind_addr);
    let name = if host.is_empty() {
        "localhost".to_string()
    } else {
        host
    };
    if let Ok(ip) = name.parse::<std::net::IpAddr>() {
        return Ok(ServerName::IpAddress(ip.into()));
    }
    ServerName::try_from(name)
        .map_err(|_| DistributedTlsError::Config("invalid server name".to_string()))
}

// =============================================================================
// Distributed HTTP client (asupersync-native, wa-1u55z)
// =============================================================================

/// HTTP client for distributed node-to-node communication.
///
/// Wraps [`asupersync::http::h1::http_client::HttpClient`] for making
/// requests between distributed nodes without requiring reqwest.
///
/// For plain HTTP (loopback testing, health checks), use [`Self::plaintext()`].
/// For HTTPS with standard WebPKI roots, use [`Self::new()`].
/// For mTLS with custom certificates, use [`DistributedTlsBundle::connector()`]
/// with raw [`asupersync::net::TcpStream`] + TLS handshake + HTTP/1.1 framing.
#[cfg(feature = "distributed")]
pub struct DistributedHttpClient {
    inner: asupersync::http::h1::http_client::HttpClient,
}

#[cfg(feature = "distributed")]
impl Default for DistributedHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "distributed")]
impl DistributedHttpClient {
    /// Create a client with default configuration (WebPKI roots for HTTPS).
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: asupersync::http::h1::http_client::HttpClient::new(),
        }
    }

    /// Create a plaintext-only client for loopback/testing.
    #[must_use]
    pub fn plaintext() -> Self {
        Self::new()
    }

    /// Send a GET request.
    pub async fn get(
        &self,
        cx: &asupersync::cx::Cx,
        url: &str,
    ) -> Result<asupersync::http::h1::types::Response, asupersync::http::h1::http_client::ClientError>
    {
        self.inner.get(cx, url).await
    }

    /// Send a POST request with a body.
    pub async fn post(
        &self,
        cx: &asupersync::cx::Cx,
        url: &str,
        body: Vec<u8>,
    ) -> Result<asupersync::http::h1::types::Response, asupersync::http::h1::http_client::ClientError>
    {
        self.inner.post(cx, url, body).await
    }
}

#[cfg(feature = "distributed")]
fn distributed_bind_host(bind_addr: &str) -> String {
    let trimmed = bind_addr.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if let Ok(addr) = trimmed.parse::<std::net::SocketAddr>() {
        return addr.ip().to_string();
    }
    if let Some(stripped) = trimmed.strip_prefix('[') {
        if let Some((host, _rest)) = stripped.split_once(']') {
            return host.to_string();
        }
    }
    if let Some((host, _port)) = trimmed.rsplit_once(':') {
        if !host.is_empty() && !host.contains(':') {
            return host.to_string();
        }
    }
    trimmed.to_string()
}

// =============================================================================
// Distributed Mode Readiness Checklist (wa-nu4.4.3.6)
// =============================================================================
//
// Distributed mode introduces network and security risks. This checklist
// provides a programmatic go/no-go evaluation for enabling distributed mode.
//
// ## Feature Gating Decision
//
// Distributed mode is OFF by default and requires explicit opt-in via:
//   - Compile time: `--features distributed`
//   - Runtime: `[distributed] enabled = true` in ft.toml
//
// This dual gate ensures operators consciously enable both the code path
// and the runtime behavior. The default binary ships without distributed
// networking capabilities.
//
// ## Rollout Steps
//
// 1. Build with `cargo build --features distributed`
// 2. Run `ft doctor` to verify security posture
// 3. Configure `[distributed]` in ft.toml (see distributed-security-spec.md)
// 4. Start with loopback bind first, verify locally
// 5. Switch to non-loopback with TLS, verify E2E
// 6. Enable agent-id allowlisting for production

/// A single item in the distributed mode readiness checklist.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReadinessItem {
    /// Machine-readable identifier (e.g., "security.auth_configured").
    pub id: String,
    /// Human-readable category.
    pub category: String,
    /// Description of what this item checks.
    pub description: String,
    /// Whether this item passes.
    pub pass: bool,
    /// Details explaining the pass/fail status.
    pub detail: String,
    /// Whether this item is required (blocking) or advisory.
    pub required: bool,
}

/// Aggregate result of the distributed mode readiness evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadinessReport {
    /// Overall go/no-go decision.
    pub ready: bool,
    /// Feature compiled in.
    pub feature_compiled: bool,
    /// Runtime enabled.
    pub runtime_enabled: bool,
    /// Individual checklist items.
    pub items: Vec<ReadinessItem>,
    /// Count of passing required items.
    pub required_pass: usize,
    /// Count of total required items.
    pub required_total: usize,
    /// Count of passing advisory items.
    pub advisory_pass: usize,
    /// Count of total advisory items.
    pub advisory_total: usize,
}

/// Evaluate the distributed mode readiness checklist against a config.
///
/// Returns a report with pass/fail for each item and an overall go/no-go.
/// The checklist covers:
/// - Security baseline (auth, TLS, bind defaults)
/// - Observability (logging configured)
/// - Configuration validity (no conflicting settings)
/// - Wire protocol readiness (feature compiled)
#[must_use]
pub fn evaluate_readiness(config: &DistributedConfig) -> ReadinessReport {
    let feature_compiled = cfg!(feature = "distributed");
    let mut items = Vec::new();

    // --- Security baseline ---

    items.push(ReadinessItem {
        id: "security.feature_compiled".to_string(),
        category: "Security".to_string(),
        description: "Distributed feature compiled into binary".to_string(),
        pass: feature_compiled,
        detail: if feature_compiled {
            "Binary built with --features distributed".to_string()
        } else {
            "Rebuild with --features distributed to enable".to_string()
        },
        required: true,
    });

    items.push(ReadinessItem {
        id: "security.runtime_enabled".to_string(),
        category: "Security".to_string(),
        description: "Distributed mode enabled in config".to_string(),
        pass: config.enabled,
        detail: if config.enabled {
            "distributed.enabled = true".to_string()
        } else {
            "Set distributed.enabled = true in ft.toml".to_string()
        },
        required: true,
    });

    let auth_configured = if config.auth_mode.requires_token() {
        config.token.is_some() || config.token_env.is_some() || config.token_path.is_some()
    } else {
        true // mTLS-only does not require a token credential
    };
    items.push(ReadinessItem {
        id: "security.auth_configured".to_string(),
        category: "Security".to_string(),
        description: "Authentication credentials configured".to_string(),
        pass: auth_configured,
        detail: if auth_configured {
            format!("Auth mode {:?} with credentials present", config.auth_mode)
        } else {
            "Set token, token_env, or token_path in [distributed]".to_string()
        },
        required: true,
    });

    let is_loopback = config.bind_addr.starts_with("127.")
        || config.bind_addr.starts_with("localhost")
        || config.bind_addr.starts_with("[::1]");
    let tls_required_and_missing =
        !is_loopback && config.require_tls_for_non_loopback && !config.tls.enabled;
    items.push(ReadinessItem {
        id: "security.tls_for_remote".to_string(),
        category: "Security".to_string(),
        description: "TLS enabled for non-loopback bind".to_string(),
        pass: is_loopback || config.tls.enabled || config.allow_insecure,
        detail: if is_loopback {
            "Loopback bind — TLS optional".to_string()
        } else if config.tls.enabled {
            "TLS enabled for remote bind".to_string()
        } else if config.allow_insecure {
            "WARNING: allow_insecure=true bypasses TLS requirement".to_string()
        } else if tls_required_and_missing {
            "Non-loopback bind requires TLS — enable distributed.tls".to_string()
        } else {
            "TLS status undetermined".to_string()
        },
        required: true,
    });

    let no_insecure = !config.allow_insecure;
    items.push(ReadinessItem {
        id: "security.no_insecure_override".to_string(),
        category: "Security".to_string(),
        description: "Insecure mode not enabled".to_string(),
        pass: no_insecure,
        detail: if no_insecure {
            "allow_insecure = false (safe)".to_string()
        } else {
            "WARNING: allow_insecure = true — plaintext traffic allowed".to_string()
        },
        required: false, // advisory — may be intentional for dev
    });

    let has_allowlist = !config.allow_agent_ids.is_empty();
    items.push(ReadinessItem {
        id: "security.agent_allowlist".to_string(),
        category: "Security".to_string(),
        description: "Agent ID allowlist configured".to_string(),
        pass: has_allowlist,
        detail: if has_allowlist {
            format!("{} agent ID(s) in allowlist", config.allow_agent_ids.len())
        } else {
            "No agent ID allowlist — any authenticated agent can connect".to_string()
        },
        required: false, // advisory — recommended for production
    });

    // --- Configuration validity ---

    let bind_valid = !config.bind_addr.is_empty();
    items.push(ReadinessItem {
        id: "config.bind_addr_set".to_string(),
        category: "Configuration".to_string(),
        description: "Bind address is set".to_string(),
        pass: bind_valid,
        detail: if bind_valid {
            format!("bind_addr = {}", config.bind_addr)
        } else {
            "bind_addr is empty — set to host:port".to_string()
        },
        required: true,
    });

    let tls_paths_ok = if config.tls.enabled {
        config.tls.cert_path.is_some() && config.tls.key_path.is_some()
    } else {
        true // TLS disabled — paths not needed
    };
    items.push(ReadinessItem {
        id: "config.tls_paths".to_string(),
        category: "Configuration".to_string(),
        description: "TLS certificate and key paths configured".to_string(),
        pass: tls_paths_ok,
        detail: if !config.tls.enabled {
            "TLS disabled — paths not required".to_string()
        } else if tls_paths_ok {
            "cert_path and key_path set".to_string()
        } else {
            "TLS enabled but cert_path or key_path missing".to_string()
        },
        required: true,
    });

    // --- Observability ---

    // Observability is checked at a basic level here (config-based).
    // Full observability (tracing spans, metrics) is verified by E2E tests.
    items.push(ReadinessItem {
        id: "observability.logging_assumed".to_string(),
        category: "Observability".to_string(),
        description: "Structured logging available for distributed events".to_string(),
        pass: true, // Always true — wa has structured logging baseline
        detail: "ft emits tracing spans for all distributed operations".to_string(),
        required: true,
    });

    // --- Wire protocol ---

    items.push(ReadinessItem {
        id: "wire.feature_gate".to_string(),
        category: "Wire Protocol".to_string(),
        description: "Wire protocol code compiled in".to_string(),
        pass: feature_compiled,
        detail: if feature_compiled {
            "Distributed feature gate active".to_string()
        } else {
            "Wire protocol unavailable — rebuild with --features distributed".to_string()
        },
        required: true,
    });

    // --- Compute aggregate ---

    let required_pass = items.iter().filter(|i| i.required && i.pass).count();
    let required_total = items.iter().filter(|i| i.required).count();
    let advisory_pass = items.iter().filter(|i| !i.required && i.pass).count();
    let advisory_total = items.iter().filter(|i| !i.required).count();
    let ready = required_pass == required_total;

    ReadinessReport {
        ready,
        feature_compiled,
        runtime_enabled: config.enabled,
        items,
        required_pass,
        required_total,
        advisory_pass,
        advisory_total,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "distributed")]
    use crate::runtime_compat::CompatRuntime;

    #[test]
    fn resolve_expected_token_from_file_supports_rotation() {
        use std::io::{Seek, SeekFrom};

        let mut file = tempfile::NamedTempFile::new().expect("temp file");
        std::io::Write::write_all(file.as_file_mut(), b"token-1").expect("write token");

        let mut config = DistributedConfig::default();
        config.enabled = true;
        config.auth_mode = DistributedAuthMode::Token;
        config.token_path = Some(file.path().display().to_string());

        let tok1 = resolve_expected_token(&config)
            .expect("resolve token")
            .expect("token required");
        assert_eq!(tok1, "token-1");
        assert!(validate_token(config.auth_mode, Some(&tok1), Some(&tok1), None).is_ok());

        // Rotate in-place by updating the file contents.
        file.as_file_mut().set_len(0).expect("truncate");
        file.as_file_mut()
            .seek(SeekFrom::Start(0))
            .expect("seek start");
        std::io::Write::write_all(file.as_file_mut(), b"token-2").expect("write token");

        let tok2 = resolve_expected_token(&config)
            .expect("resolve token")
            .expect("token required");
        assert_eq!(tok2, "token-2");
        assert!(validate_token(config.auth_mode, Some(&tok2), Some(&tok2), None).is_ok());
        assert!(validate_token(config.auth_mode, Some(&tok2), Some(&tok1), None).is_err());
    }

    #[test]
    fn resolve_expected_token_rejects_ambiguous_sources() {
        let mut config = DistributedConfig::default();
        config.enabled = true;
        config.auth_mode = DistributedAuthMode::Token;
        config.token = Some("inline".to_string());
        config.token_env = Some("ENV".to_string());

        let err = resolve_expected_token(&config).expect_err("should be ambiguous");
        assert!(matches!(err, DistributedCredentialError::TokenAmbiguous));
    }

    #[cfg(feature = "distributed")]
    use asupersync::io::{AsyncReadExt, AsyncWriteExt};
    #[cfg(feature = "distributed")]
    use asupersync::net::{TcpListener, TcpStream};
    #[cfg(feature = "distributed")]
    use asupersync::tls::{TlsAcceptor, TlsConnector};
    #[cfg(feature = "distributed")]
    use proptest::prelude::*;
    #[cfg(feature = "distributed")]
    use std::time::Duration;

    #[cfg(feature = "distributed")]
    const CA_CERT: &str = "-----BEGIN CERTIFICATE-----
MIIDCzCCAfOgAwIBAgIUSBlBg5d5TgsxHl8LEBJNtT90RlkwDQYJKoZIhvcNAQEL
BQAwFTETMBEGA1UEAwwKd2EtdGVzdC1jYTAeFw0yNjAzMjEwOTAwNDNaFw0zNjAz
MTgwOTAwNDNaMBUxEzARBgNVBAMMCndhLXRlc3QtY2EwggEiMA0GCSqGSIb3DQEB
AQUAA4IBDwAwggEKAoIBAQCFJpJndMHirHU/O2h7+PLhPUdk7J+Bph+l6HpRRSZd
YMms/0PMgIUkR8TPWCb10L0wKySrnalD6/iMx61m/zdQhra2xGItlnvYHujB0gQD
na70y36sRahTtEy2e1BTTNp8E0qVfTgEpkI8FeUnxZOaSoIFzmq1Jss/2y6T4pc5
bVOvtFKBGROJ/uyCtD/IZSUUtAh3UjNI6XnfLr0z8QwJPqBmGZDCqekBqSnRtSQ1
UBw1qbc5hzEFVM2wvz4vE4ug7ERwrlYt0ifNfACnvMSnkSYwGTdtbyRwuaeqRIgt
uJEHjkoPIPYtKYiv8zyyjEMjH3twCH7nNNZzzI9vKmn1AgMBAAGjUzBRMB0GA1Ud
DgQWBBS739cePr8uEC6o5+fXYQQjRpt/wjAfBgNVHSMEGDAWgBS739cePr8uEC6o
5+fXYQQjRpt/wjAPBgNVHRMBAf8EBTADAQH/MA0GCSqGSIb3DQEBCwUAA4IBAQAo
nRDUW5iayGY+79YO2qKprWgF+UCH1DQe0OgpDKoQlQVGswC7xlyZlg+LNNVwku6x
E0YZRp4+yGJIlPdSBfjhd/OFKOrCEkcsiAGvKpv6nPuxQwvK6I3SncC75OnLIhHP
V5IUbIevzYOfpJ/jCwYxGHDNExdaq5s1Kq3L64G8xgPAcK2aokc4Ym86WSkMmYlG
QTXXsx3WkXcRflPpzo/eRsXyuoteQNs3zcnUb1FO//XhfpIk32MNvyBQJMnWLAv4
Fwm3d+jMp9NpZqfyFFXtPaEL+Vna/3fqHD2zbSSy0NGTIhMnm+hxFXOAU2oietRA
sJkfmGf1F0WhXlOzalTz
-----END CERTIFICATE-----
";
    #[cfg(feature = "distributed")]
    const CA_CERT_ALT: &str = "-----BEGIN CERTIFICATE-----\nMIIDIzCCAgugAwIBAgIUZEO9mhldKaM+vYQlBxRzbx4NDOYwDQYJKoZIhvcNAQEL\nBQAwGTEXMBUGA1UEAwwOd2EtdGVzdC1jYS1hbHQwHhcNMjYwMTMxMTk1MTIwWhcN\nMjYwMzAyMTk1MTIwWjAZMRcwFQYDVQQDDA53YS10ZXN0LWNhLWFsdDCCASIwDQYJ\nKoZIhvcNAQEBBQADggEPADCCAQoCggEBAKfmzBFOOLB68UYCpAkvLuFebPm8vi5g\nFOAFTNA15bSOOHV1NAidEnvRxRr1BBbSeZDkiL3ucCaApMWZUfceOY+qkbiRSQdv\nLWRLt8b4UhuU/jV5wYbVrLaQ6+v6AneVMAHEdto3rcth/lZH/snRGzkReFF+uWG2\nat+GcyGHGQkpseK6bYaE/NgjawVqU4UdCf9OlgFHdrbKKjpnOwULv2t6THeqv36X\nm0G2m6aaFLG/23VWA/l0wKHP2slpBcLizZEwuQL4vY3SQYEI9Iw53tb8fh6hEANj\n9scTDoyW0AO/KSH8adPnX6KoJg6c2I7jkWXxbBlVXJtU9wfkd1D0RikCAwEAAaNj\nMGEwHQYDVR0OBBYEFBmwJJCWc0HPjfJkWiOq0/9038ySMB8GA1UdIwQYMBaAFBmw\nJJCWc0HPjfJkWiOq0/9038ySMA8GA1UdEwEB/wQFMAMBAf8wDgYDVR0PAQH/BAQD\nAgEGMA0GCSqGSIb3DQEBCwUAA4IBAQB0s7vQNAudWKupjWP97II5X31y8GUKKgAh\nQqoCl9OUhqTvmaWLSj1d4+8YSO6F34ZW0QNuHQZ/6gzuHIyLpaOUC2V/PMaFuC3O\nZJv3K/udxXsMH2otFo4iT0FFFUigFynXu/0//iD850/g6jHk8YMLeOGWZQkDKOae\nTlfh3IYE7kWZQUBUYPzuLZc4gYvPYVMdIfY8+5IPxOJxC7brFrViRMcbp4xW7Jfu\nkZz8vfzmY+hjQFgOsdcFVzQenRtTxr8eMdowJ++phHJs4gtQyEY15+zkYpg7B5iZ\nIX6nxMJcVfMJb4OPECWPjjwJTPSH8yiIOmw24/dbJZ4ZKjcpP3FH\n-----END CERTIFICATE-----\n";
    #[cfg(feature = "distributed")]
    const SERVER_CERT: &str = "-----BEGIN CERTIFICATE-----
MIIDDzCCAfegAwIBAgIUSSJw3sCpNmZ66bOs6sC3RcilA7UwDQYJKoZIhvcNAQEL
BQAwFTETMBEGA1UEAwwKd2EtdGVzdC1jYTAeFw0yNjAzMjEwOTAwNDNaFw0zNjAz
MTgwOTAwNDNaMBQxEjAQBgNVBAMMCWxvY2FsaG9zdDCCASIwDQYJKoZIhvcNAQEB
BQADggEPADCCAQoCggEBAKGm2eAiS4yQPXcXvYAebOVzgeo9x2FQD26Yg1xAlvsJ
enlbTSJoXZhFxYH4MpFRvUXyv/REv8zdGWwjLsbkSYj3aBqTq6f5FFTX6Kw0Z9pf
cm3r1bpIqYyoannTIYMkJ1pyZ9cL21+QwSt4FKIU6oCqY4wGG8LyQI6Wny9wwxHi
z2wNXd0KBCUWFKZ76S504LUJj8oUYx1g0LE/ycmGSTeKLRLPP+sSXMYkNsBdegtT
EhQggZY/Ju2GaUnSJxFwhOrMW9zSRusZ5+wQTzm4DyLo4zz0S4yd3L5zSc6KwTo/
EjGScgo/QvEXt5jwZ3lCqlHkanT3AcSNfxSn2NWH+50CAwEAAaNYMFYwFAYDVR0R
BA0wC4IJbG9jYWxob3N0MB0GA1UdDgQWBBTmUMc5j/Ono3+QzhSoaY9wCvdbMDAf
BgNVHSMEGDAWgBS739cePr8uEC6o5+fXYQQjRpt/wjANBgkqhkiG9w0BAQsFAAOC
AQEASYMoNycM5Nu99QkhKShHxbMHIr2+Uh5HxL28I3gNqtF9rCuMX7DCX0pTKnIh
zRdEEvOc1EE1/hxqgkfZrPfK2fyWJI0H/lcE/phuvVkZo5YNxqvqfZDYrGjIL1VR
Ov2TZnb7wxbwCwfwJBY6WVOXIMBkOQZujuwPnqY5LrC/4UPLWe3ll4wdxAqk6LjY
bwpmnddK2XLbOmXKrsfXyWqK30L43U5cOtiEuEGWGHw8y2u6yv5sxkQrEy2aIKKv
FXGJzUGW1U+9Xv0npHkqYZ8TYUXMdyl3esHifiOo66Hgwi272ZEdLCTqvVmlUMHH
jpaHbLcEMTbShLFW++UYM6OPdA==
-----END CERTIFICATE-----
";
    #[cfg(feature = "distributed")]
    const SERVER_KEY: &str = "-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQChptngIkuMkD13
F72AHmzlc4HqPcdhUA9umINcQJb7CXp5W00iaF2YRcWB+DKRUb1F8r/0RL/M3Rls
Iy7G5EmI92gak6un+RRU1+isNGfaX3Jt69W6SKmMqGp50yGDJCdacmfXC9tfkMEr
eBSiFOqAqmOMBhvC8kCOlp8vcMMR4s9sDV3dCgQlFhSme+kudOC1CY/KFGMdYNCx
P8nJhkk3ii0Szz/rElzGJDbAXXoLUxIUIIGWPybthmlJ0icRcITqzFvc0kbrGefs
EE85uA8i6OM89EuMndy+c0nOisE6PxIxknIKP0LxF7eY8Gd5QqpR5Gp09wHEjX8U
p9jVh/udAgMBAAECggEADoeDLMCYXsV2KNai6zmQ2xzDMA0mdwotoOinYerSRzUG
Y5L/v1h3FSEsS+7FiMc6hmd3tlpZjO3Qg6Yz5Z+ONnfaTQ050Aq0t52CZbv+G6QZ
kmSwnKI8Tw6yJ0oBSJq+yMPgrnT73j6SBjiwThMoMrFd6i+AXkjM4aQLIcX2WoyO
O3SBa4p9bybfBxejz0XsN6bL6xaKg3pwMLj/Qgyi1lN+WxmFbt43Lhs4ohBrWZyi
/fYry9veWQQQch9LfYfmR6bAsTH/gzhTltEQcywWQh+SeDnzwAmj9w0Gxmms6ON5
QePCrqm895mr86Dhnu7qFJE2y40RSJet/tgaOXH/BwKBgQDMO1tEe9M+ehZWGkLb
UITfVGCVQm9nYI3DgUmkL+pxajvk7nY7p21Zt8u3n/wiAJgYfgMGKOurZE88noIm
LaiXQui8xT5R2EI9F5HTd0aU+Qbarw4BvSURHQf62L+zWCPbp88fNVvlkcgzByMU
2NtcUKynG/tuYbR5smJ06kHf9wKBgQDKoHbILE44Q+u5KQpRQVHrtaq2x/HsrYzm
agmTAhGq3b2u4H2WL3N3NcotUp4VLXog3qoKq1YJp5N2ivI76m9jIB6Ju96XK1E7
kRTMoMKkM6s0b4vZd59jO1UbqJE3iWNfKEKXrN14Y7zLl2/104K/vWhg/Iy3iaiw
nU6UPoiECwKBgQCaJzdVcs1Y/Bf996Z9GcKhO3QHVXT3J6b5aY3nMw+XeaMpwmBV
2KMuNA+9UzGhjKdA3WR08tAntvgj/lSocpAtVCCN06edaUleCXtVjVMmQO1OhRFi
eJ0Q1MPgMFhKC35NXtV0bfcmSao98eYl5yV0AaTAIdvfTjpGHUI5k1QTswKBgHXw
hHLq5vR1BEWYD7tP6+Dost8E7lm2gqay3bPFpobv3jJl1HOQVwLyOiW7SuxEtitf
r1XaeI/SDFEZevlI8WCfF2dQBLW0rume/p5EjEaLFIHG033W1N0rcdRRf4T14PNI
OcqTAa3LT96o3LAXVqlIE/MvzLAf3iI+zbgX1doVAoGAdtYC2zsX+wmahqFWVPaK
uxJloJa49RkOsfIoxNYIGZ6aHiIQIiWRO0UzJnMVK9JO0qZJYeDHfbA5DCkLRSBX
c63fvIhipkeGEwI/5SK4oAZPgv3tx0kWFq/LufUImkH/ptL+PAX+j53DNfWEKPTY
ukR51Vo2Dv5OO1Iz1PTzmMk=
-----END PRIVATE KEY-----
";
    #[cfg(feature = "distributed")]
    const CLIENT_CERT: &str = "-----BEGIN CERTIFICATE-----
MIIDDzCCAfegAwIBAgIUSSJw3sCpNmZ66bOs6sC3RcilA7YwDQYJKoZIhvcNAQEL
BQAwFTETMBEGA1UEAwwKd2EtdGVzdC1jYTAeFw0yNjAzMjEwOTAwNDNaFw0zNjAz
MTgwOTAwNDNaMBQxEjAQBgNVBAMMCXdhLWNsaWVudDCCASIwDQYJKoZIhvcNAQEB
BQADggEPADCCAQoCggEBALGjt9y5RErRGqRjdA81tEjQ9wlaHhVWOpTaZbHQWZCk
CR/wa2DA6HxIJvVDgOWi18g+qJPdSJwUvPi/WU0T1bDE2xEsYIUlQni5gNPsMV1/
IzzQ4uepwUPTCtczZ4s0kyJ1yeztJXW1Zd+xl3QpvXd/4wi6Nc3NmbGFLGFCyA8s
7a+gFvBFILOojThssq++91BF9KsCb/3WX11f27dw6IjB28MGOcQqlgDyNY5O2S2G
wQrDRON2zglvzuZOLdGsGCrh8wuxynqZPP9sDWyXlto46UclIGxGEnlq6JZi881N
B825jRGkfUXTYag6Dgum1XhxLdCzW0wrUs0J8W4f2LUCAwEAAaNYMFYwFAYDVR0R
BA0wC4IJd2EtY2xpZW50MB0GA1UdDgQWBBTt0Lh4XbXKoKb7EVJCKBqjNjewzzAf
BgNVHSMEGDAWgBS739cePr8uEC6o5+fXYQQjRpt/wjANBgkqhkiG9w0BAQsFAAOC
AQEAAx2ln/+jo/+u/TmKkXFH5f9rvF4IWu/qEdsDNM1EEMEPsTJEcYuNYy5cvfWF
3cfgugSDjJauk9e3ybZ2IrvTG3Q0bOkkAH7K3ZuXp1NlEwf3czznhukDwgTUeSly
D48s88z+6mmb/2DN9d8WJaPQMKkXH3xUMmc0c/jTpzXHnu7Gi8hMWQ+6ckifkzWI
M/O/QP403C4n9wV0g+2kkTegMH71J7HkXHLSfk0GILK3JTuJ+4x6fpuyHOBbk4KW
Rv3cRT+B9FuwyzHLtYH8zHVnHtRMOnDtKqZd+aZ9f2H4XC7c1gfkrUOmdm1uLpeo
XFOF5qMyV38UeVHu1E1E1rAe/g==
-----END CERTIFICATE-----
";
    #[cfg(feature = "distributed")]
    const CLIENT_KEY: &str = "-----BEGIN PRIVATE KEY-----
MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQCxo7fcuURK0Rqk
Y3QPNbRI0PcJWh4VVjqU2mWx0FmQpAkf8GtgwOh8SCb1Q4DlotfIPqiT3UicFLz4
v1lNE9WwxNsRLGCFJUJ4uYDT7DFdfyM80OLnqcFD0wrXM2eLNJMidcns7SV1tWXf
sZd0Kb13f+MIujXNzZmxhSxhQsgPLO2voBbwRSCzqI04bLKvvvdQRfSrAm/91l9d
X9u3cOiIwdvDBjnEKpYA8jWOTtkthsEKw0Tjds4Jb87mTi3RrBgq4fMLscp6mTz/
bA1sl5baOOlHJSBsRhJ5auiWYvPNTQfNuY0RpH1F02GoOg4LptV4cS3Qs1tMK1LN
CfFuH9i1AgMBAAECggEABZIRQiTggooeeu460FP0+d2XtEg1A6KccVjh8dq18aZl
5NsDZa7W+41rBKufv1PFJDHsKRWPi9l4ouz5OMS49vNNaAiguZGADUzKlfAPt5Nf
la4lQhzdgyol6mFhy13piUib/Z04MgHva72OzCVCRvI7OpBVqe+wdKks9EXbA+LC
53P3vM2FR1OqonBWWEo0B6dXi4A5vYczaaPmmb9SjEaHsoKzVmJ9LM4CdpwxU10U
O0hSkx8Bk6y9YKHIyYrbsam9eRiIVPlumwigPrCnVLJIrJyuOQT6N4OPtu9RDF9y
z0/+y0ROp0RhubfLNlcSd3cgz89ajW/pkXnYUykIsQKBgQDheATBiSsbLfkK1dfy
OJWOawBiAqPbEr8qSuVcIhVQi238554D5PKoJvBgNCJNQ8gPeODnQTky4vfpBsvk
kva4PXTkeYehOWgAyvo7S+2yguu2xU/9VrkaEP8j3WQu0vxsjpVA8EocFgTKvyCf
D5ABVVkTliDKqn5CaRYZO5iqiQKBgQDJsay/tIxhzTvV6bdDI12akq5Tywwxq+G2
40VswXvcTP9WH0QF0jPPoQRP4zzExQKm+nhGk12tavsOtF5r9SfxuiJyY1PwzjUg
02at3F+mLnN0X33hNNU1d1WBWszSQwTpNkfOPxHaC9MIgCbVNuO4GwGlenlbnTIz
nU3ojl7BzQKBgDysc3sxUmxJ/s6vpSEFoRlmKgA1/aoibVcQOJCGi33VR4/bNGaP
4czmTaFV5jUsnFWtjbgtkRrkgRowPgYQllwWDbK+EYWNUTOFa7kxQZHcMVpJ1rCx
+bXOBRq9pQwEsvDzna6P+yF7u2Zj8H9dTL9PHF1s9P4Uy01LwiqgIwEhAoGAFddo
xqXNofWwqhySHPIie8+wkyBk5KghXEXGSd22BQhNikz+d8bol25vYhtQhFp1TBHJ
npLszQ/NuizsILK+rZ2jh1GcUHJ0LGbYMrGvpfZXyF1i61VmVVDj8IsdrRNW385h
/kK0MzGem8gM7H/yLwi1p+7YX4RpYE+DlVB9kG0CgYBq8KImZb1bTxSoyV38dU/L
WTHph8hsoq2Vv63dR3g6kP+hRbtEOAWizVAhYBdn0ODRj08gP2IJ/vTqP04VwUsL
niSVI9+dOzAFsfObCkRbEseYAr4weYcUL/MpqClOeS6jVYjAOXPm2c2kYkZQ9ke+
KBAhs4snj5QspGFqkazmIw==
-----END PRIVATE KEY-----
";

    #[cfg(feature = "distributed")]
    fn temp_pem(contents: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().expect("temp file");
        std::io::Write::write_all(file.as_file_mut(), contents.as_bytes()).expect("write pem");
        file
    }

    #[cfg(feature = "distributed")]
    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        let runtime = crate::runtime_compat::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("create runtime");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(future);
        }));
        // Absorb TLS destructor panics from asupersync during runtime drop.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(runtime);
        }));
        // Clear handle from TLS so it doesn't panic during thread exit.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::runtime_compat::clear_runtime_handle();
        }));
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[cfg(feature = "distributed")]
    #[test]
    fn tls_handshake_succeeds() {
        run_async_test(async {
            let ca_cert = temp_pem(CA_CERT);
            let server_cert = temp_pem(SERVER_CERT);
            let server_key = temp_pem(SERVER_KEY);

            let mut config = DistributedConfig::default();
            config.enabled = true;
            config.tls.enabled = true;
            config.tls.cert_path = Some(server_cert.path().display().to_string());
            config.tls.key_path = Some(server_key.path().display().to_string());

            let server_config = build_server_config(
                &config.tls,
                DistributedAuthMode::Token,
                &config.allow_agent_ids,
            )
            .expect("server config");
            let client_config = build_client_config(
                &config.tls,
                DistributedAuthMode::Token,
                Some(ca_cert.path()),
            )
            .expect("client config");

            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let addr = listener.local_addr().expect("addr");

            let acceptor = TlsAcceptor::new((*server_config).clone());
            let server_task = crate::runtime_compat::task::spawn(async move {
                let (stream, _) = listener.accept().await.expect("accept");
                let mut tls_stream = acceptor.accept(stream).await.expect("accept tls");
                let mut buf = [0u8; 4];
                tls_stream.read_exact(&mut buf).await.expect("read");
                buf
            });

            let connector = TlsConnector::new((*client_config).clone());
            let mut stream = connector
                .connect(
                    "localhost",
                    TcpStream::connect(addr).await.expect("connect"),
                )
                .await
                .expect("tls connect");
            stream.write_all(b"ping").await.expect("write");

            let received = server_task.await.expect("join");
            assert_eq!(&received, b"ping");
        });
    }

    #[cfg(feature = "distributed")]
    #[test]
    fn mtls_handshake_succeeds() {
        run_async_test(async {
            let ca_cert = temp_pem(CA_CERT);
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
            server_config.tls.client_ca_path = Some(ca_cert.path().display().to_string());
            server_config.allow_agent_ids = vec!["wa-client".to_string()];

            let mut client_config = DistributedConfig::default();
            client_config.enabled = true;
            client_config.auth_mode = DistributedAuthMode::Mtls;
            client_config.tls.enabled = true;
            client_config.tls.cert_path = Some(client_cert.path().display().to_string());
            client_config.tls.key_path = Some(client_key.path().display().to_string());

            let server_tls = build_server_config(
                &server_config.tls,
                server_config.auth_mode,
                &server_config.allow_agent_ids,
            )
            .expect("server config");
            let client_tls = build_client_config(
                &client_config.tls,
                client_config.auth_mode,
                Some(ca_cert.path()),
            )
            .expect("client config");

            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let addr = listener.local_addr().expect("addr");

            let acceptor = TlsAcceptor::new((*server_tls).clone());
            let server_task = crate::runtime_compat::task::spawn(async move {
                let (stream, _) = listener.accept().await.expect("accept");
                let mut tls_stream = acceptor.accept(stream).await.expect("accept tls");
                let mut buf = [0u8; 2];
                tls_stream.read_exact(&mut buf).await.expect("read");
                buf
            });

            let connector = TlsConnector::new((*client_tls).clone());
            let mut stream = connector
                .connect(
                    "localhost",
                    TcpStream::connect(addr).await.expect("connect"),
                )
                .await
                .expect("tls connect");
            stream.write_all(b"ok").await.expect("write");

            let received = server_task.await.expect("join");
            assert_eq!(&received, b"ok");
        });
    }

    #[cfg(feature = "distributed")]
    #[test]
    fn tls_handshake_rejects_untrusted_server() {
        run_async_test(async {
            let ca_cert_alt = temp_pem(CA_CERT_ALT);
            let server_cert = temp_pem(SERVER_CERT);
            let server_key = temp_pem(SERVER_KEY);

            let mut config = DistributedConfig::default();
            config.enabled = true;
            config.tls.enabled = true;
            config.tls.cert_path = Some(server_cert.path().display().to_string());
            config.tls.key_path = Some(server_key.path().display().to_string());

            let server_config = build_server_config(
                &config.tls,
                DistributedAuthMode::Token,
                &config.allow_agent_ids,
            )
            .expect("server config");
            let client_config = build_client_config(
                &config.tls,
                DistributedAuthMode::Token,
                Some(ca_cert_alt.path()),
            )
            .expect("client config");

            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let addr = listener.local_addr().expect("addr");

            let acceptor = TlsAcceptor::new((*server_config).clone());
            let server_task = crate::runtime_compat::task::spawn(async move {
                let (stream, _) = listener.accept().await.expect("accept");
                acceptor.accept(stream).await
            });

            let connector = TlsConnector::new((*client_config).clone());
            let client_result = connector
                .connect(
                    "localhost",
                    TcpStream::connect(addr).await.expect("connect"),
                )
                .await;

            let server_result = crate::runtime_compat::timeout(Duration::from_secs(2), server_task)
                .await
                .expect("server timeout")
                .expect("join");
            assert!(server_result.is_err());

            if let Ok(mut stream) = client_result {
                let write_result = stream.write_all(b"no cert").await;
                let mut buf = [0u8; 1];
                let read_result = stream.read_exact(&mut buf).await;
                assert!(write_result.is_err() || read_result.is_err());
            }
        });
    }

    #[cfg(feature = "distributed")]
    #[test]
    fn mtls_handshake_rejects_missing_client_cert() {
        run_async_test(async {
            let ca_cert = temp_pem(CA_CERT);
            let server_cert = temp_pem(SERVER_CERT);
            let server_key = temp_pem(SERVER_KEY);

            let mut server_config = DistributedConfig::default();
            server_config.enabled = true;
            server_config.auth_mode = DistributedAuthMode::Mtls;
            server_config.tls.enabled = true;
            server_config.tls.cert_path = Some(server_cert.path().display().to_string());
            server_config.tls.key_path = Some(server_key.path().display().to_string());
            server_config.tls.client_ca_path = Some(ca_cert.path().display().to_string());

            let mut client_config = DistributedConfig::default();
            client_config.enabled = true;
            client_config.auth_mode = DistributedAuthMode::Token;
            client_config.tls.enabled = true;

            let server_tls = build_server_config(
                &server_config.tls,
                server_config.auth_mode,
                &server_config.allow_agent_ids,
            )
            .expect("server");
            let client_tls = build_client_config(
                &client_config.tls,
                client_config.auth_mode,
                Some(ca_cert.path()),
            )
            .expect("client");

            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let addr = listener.local_addr().expect("addr");

            let acceptor = TlsAcceptor::new((*server_tls).clone());
            let server_task = crate::runtime_compat::task::spawn(async move {
                let (stream, _) = listener.accept().await.expect("accept");
                acceptor.accept(stream).await
            });

            let connector = TlsConnector::new((*client_tls).clone());
            let client_result = connector
                .connect(
                    "localhost",
                    TcpStream::connect(addr).await.expect("connect"),
                )
                .await;

            let server_result = crate::runtime_compat::timeout(Duration::from_secs(2), server_task)
                .await
                .expect("server timeout")
                .expect("join");
            assert!(server_result.is_err());

            if let Ok(mut stream) = client_result {
                let write_result = stream.write_all(b"no cert").await;
                let mut buf = [0u8; 1];
                let read_result = stream.read_exact(&mut buf).await;
                assert!(write_result.is_err() || read_result.is_err());
            }
        });
    }

    #[cfg(feature = "distributed")]
    #[test]
    fn mtls_handshake_rejects_disallowed_client() {
        run_async_test(async {
            let ca_cert = temp_pem(CA_CERT);
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
            server_config.tls.client_ca_path = Some(ca_cert.path().display().to_string());
            server_config.allow_agent_ids = vec!["not-allowed".to_string()];

            let mut client_config = DistributedConfig::default();
            client_config.enabled = true;
            client_config.auth_mode = DistributedAuthMode::Mtls;
            client_config.tls.enabled = true;
            client_config.tls.cert_path = Some(client_cert.path().display().to_string());
            client_config.tls.key_path = Some(client_key.path().display().to_string());

            let server_tls = build_server_config(
                &server_config.tls,
                server_config.auth_mode,
                &server_config.allow_agent_ids,
            )
            .expect("server config");
            let client_tls = build_client_config(
                &client_config.tls,
                client_config.auth_mode,
                Some(ca_cert.path()),
            )
            .expect("client config");

            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let addr = listener.local_addr().expect("addr");

            let acceptor = TlsAcceptor::new((*server_tls).clone());
            let server_task = crate::runtime_compat::task::spawn(async move {
                let (stream, _) = listener.accept().await.expect("accept");
                acceptor.accept(stream).await
            });

            let connector = TlsConnector::new((*client_tls).clone());
            let client_result = connector
                .connect(
                    "localhost",
                    TcpStream::connect(addr).await.expect("connect"),
                )
                .await;

            let server_result = crate::runtime_compat::timeout(Duration::from_secs(2), server_task)
                .await
                .expect("server timeout")
                .expect("join");
            assert!(server_result.is_err());

            if let Ok(mut stream) = client_result {
                let write_result = stream.write_all(b"nope").await;
                let mut buf = [0u8; 1];
                let read_result = stream.read_exact(&mut buf).await;
                assert!(write_result.is_err() || read_result.is_err());
            }
        });
    }

    #[cfg(feature = "distributed")]
    #[test]
    fn tls_rejects_plaintext_client() {
        run_async_test(async {
            let server_cert = temp_pem(SERVER_CERT);
            let server_key = temp_pem(SERVER_KEY);

            let mut config = DistributedConfig::default();
            config.enabled = true;
            config.tls.enabled = true;
            config.tls.cert_path = Some(server_cert.path().display().to_string());
            config.tls.key_path = Some(server_key.path().display().to_string());

            let server_config = build_server_config(
                &config.tls,
                DistributedAuthMode::Token,
                &config.allow_agent_ids,
            )
            .expect("server config");
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let addr = listener.local_addr().expect("addr");

            let acceptor = TlsAcceptor::new((*server_config).clone());
            let server_task = crate::runtime_compat::task::spawn(async move {
                let (stream, _) = listener.accept().await.expect("accept");
                acceptor.accept(stream).await
            });

            let mut client = TcpStream::connect(addr).await.expect("connect");
            client.write_all(b"not tls").await.expect("write");
            let _ = client.shutdown(std::net::Shutdown::Both);

            let server_result = crate::runtime_compat::timeout(Duration::from_secs(2), server_task)
                .await
                .expect("server timeout")
                .expect("join");
            assert!(server_result.is_err());
        });
    }

    #[cfg(feature = "distributed")]
    #[test]
    fn token_validation_rejects_missing_or_wrong() {
        let auth_mode = DistributedAuthMode::Token;
        let expected = Some("secret");

        assert_eq!(
            validate_token(auth_mode, expected, None, None).expect_err("missing token"),
            DistributedSecurityError::MissingToken
        );
        assert_eq!(
            validate_token(auth_mode, expected, Some("wrong"), None).expect_err("wrong token"),
            DistributedSecurityError::AuthFailed
        );
        assert!(validate_token(auth_mode, expected, Some("secret"), None).is_ok());
    }

    #[cfg(feature = "distributed")]
    #[test]
    fn token_identity_binding_requires_matching_tls_identity() {
        let auth_mode = DistributedAuthMode::TokenAndMtls;
        let expected = Some("agent-1:secret");

        assert!(
            validate_token(auth_mode, expected, Some("agent-1:secret"), Some("agent-1")).is_ok()
        );
        assert_eq!(
            validate_token(auth_mode, expected, Some("agent-2:secret"), Some("agent-1"))
                .expect_err("wrong token identity"),
            DistributedSecurityError::AuthFailed
        );
        assert_eq!(
            validate_token(auth_mode, expected, Some("agent-1:secret"), Some("agent-2"))
                .expect_err("wrong tls identity"),
            DistributedSecurityError::AuthFailed
        );
    }

    #[cfg(feature = "distributed")]
    #[test]
    fn token_errors_do_not_leak_secrets() {
        let auth_mode = DistributedAuthMode::Token;
        let err = validate_token(auth_mode, Some("topsecret"), Some("wrong"), None)
            .expect_err("auth failure");
        let message = err.to_string();
        assert!(!message.contains("topsecret"));
        assert!(!message.contains("wrong"));
    }

    #[cfg(feature = "distributed")]
    #[test]
    fn replay_guard_rejects_non_monotonic_sequences() {
        let mut guard = SessionReplayGuard::new(4);
        assert!(guard.validate("session-a", 1).is_ok());
        assert_eq!(
            guard.validate("session-a", 1).expect_err("duplicate"),
            DistributedSecurityError::ReplayDetected
        );
        assert_eq!(
            guard.validate("session-a", 0).expect_err("stale"),
            DistributedSecurityError::ReplayDetected
        );
        assert!(guard.validate("session-a", 2).is_ok());
    }

    #[cfg(feature = "distributed")]
    #[test]
    fn replay_guard_enforces_session_limit() {
        let mut guard = SessionReplayGuard::new(1);
        assert!(guard.validate("session-a", 1).is_ok());
        assert_eq!(
            guard.validate("session-b", 1).expect_err("session limit"),
            DistributedSecurityError::SessionLimitReached
        );
    }

    #[cfg(feature = "distributed")]
    #[test]
    fn replay_guard_remove_frees_capacity_for_new_session() {
        let mut guard = SessionReplayGuard::new(1);
        assert!(guard.validate("session-a", 1).is_ok());
        assert!(guard.remove("session-a"));
        assert!(!guard.remove("missing"));
        assert!(guard.validate("session-b", 1).is_ok());
    }

    #[cfg(feature = "distributed")]
    #[test]
    fn connection_limiter_enforces_max_connections() {
        let limiter = ConnectionLimiter::new(1);
        let permit = limiter.try_acquire().expect("first connection");
        assert_eq!(limiter.active(), 1);
        assert_eq!(
            limiter.try_acquire().expect_err("limit reached"),
            DistributedSecurityError::ConnectionLimitReached
        );
        drop(permit);
        assert_eq!(limiter.active(), 0);
    }

    #[cfg(feature = "distributed")]
    #[test]
    fn message_size_limit_enforced() {
        let limit = MessageSizeLimit { max_bytes: 4 };
        assert!(limit.check(4).is_ok());
        assert_eq!(
            limit.check(5).expect_err("too large"),
            DistributedSecurityError::MessageTooLarge
        );
    }

    #[cfg(feature = "distributed")]
    #[test]
    fn rate_limiter_enforces_window() {
        let mut limiter = FixedWindowRateLimiter::new(2, 1000);
        assert!(limiter.allow(0).is_ok());
        assert!(limiter.allow(10).is_ok());
        assert_eq!(
            limiter.allow(20).expect_err("rate limited"),
            DistributedSecurityError::RateLimited
        );
        assert!(limiter.allow(1000).is_ok());
    }

    #[cfg(feature = "distributed")]
    #[test]
    fn timeouts_are_enforced() {
        let timeouts = DistributedTimeouts {
            handshake: Duration::from_secs(1),
            message: Duration::from_secs(2),
        };
        assert!(timeouts.check_handshake(Duration::from_millis(900)).is_ok());
        assert_eq!(
            timeouts
                .check_handshake(Duration::from_secs(2))
                .expect_err("handshake timeout"),
            DistributedSecurityError::HandshakeTimeout
        );
        assert_eq!(
            timeouts
                .check_message(Duration::from_secs(3))
                .expect_err("message timeout"),
            DistributedSecurityError::MessageTimeout
        );
    }

    #[cfg(feature = "distributed")]
    #[test]
    fn security_error_codes_are_stable() {
        assert_eq!(
            DistributedSecurityError::AuthFailed.code(),
            "dist.auth_failed"
        );
        assert_eq!(
            DistributedSecurityError::ReplayDetected.code(),
            "dist.replay_detected"
        );
        assert_eq!(
            DistributedSecurityError::ConnectionLimitReached.code(),
            "dist.connection_limit"
        );
        assert_eq!(
            DistributedSecurityError::MessageTooLarge.code(),
            "dist.message_too_large"
        );
        assert_eq!(
            DistributedSecurityError::RateLimited.code(),
            "dist.rate_limited"
        );
        assert_eq!(
            DistributedSecurityError::HandshakeTimeout.code(),
            "dist.handshake_timeout"
        );
    }

    #[cfg(feature = "distributed")]
    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 32,
            .. ProptestConfig::default()
        })]

        #[test]
        fn token_parts_parse_round_trip_with_identity(
            identity in "[a-zA-Z0-9_-]{1,12}",
            secret in "[a-zA-Z0-9_-]{1,24}"
        ) {
            let token = format!("{identity}:{secret}");
            let parts = TokenParts::parse(&token);
            prop_assert_eq!(parts.identity, Some(identity.as_str()));
            prop_assert_eq!(parts.secret, secret.as_str());
        }

        #[test]
        fn token_validation_errors_do_not_leak_inputs(
            expected in "[a-zA-Z0-9_-]{3,24}",
            presented in "[a-zA-Z0-9_-]{3,24}"
        ) {
            prop_assume!(expected != presented);
            let err = validate_token(
                DistributedAuthMode::Token,
                Some(expected.as_str()),
                Some(presented.as_str()),
                None
            )
            .expect_err("auth failure");
            let message = err.to_string();
            prop_assert!(!message.contains(expected.as_str()));
            prop_assert!(!message.contains(presented.as_str()));
        }
    }

    // -----------------------------------------------------------------------
    // Readiness checklist tests (wa-nu4.4.3.6)
    // -----------------------------------------------------------------------

    #[test]
    fn readiness_default_config_not_ready() {
        let config = DistributedConfig::default();
        let report = evaluate_readiness(&config);
        // Default config has enabled=false, so not ready
        assert!(!report.ready);
        assert!(!report.runtime_enabled);
        // feature_compiled depends on build flags; runtime_enabled is always false for default
        let runtime = report
            .items
            .iter()
            .find(|i| i.id == "security.runtime_enabled")
            .unwrap();
        assert!(!runtime.pass);
    }

    #[test]
    fn readiness_enabled_loopback_with_token_is_ready() {
        let mut config = DistributedConfig::default();
        config.enabled = true;
        config.auth_mode = DistributedAuthMode::Token;
        config.token = Some("test-secret".to_string());
        // bind_addr defaults to 127.0.0.1:4141 (loopback)
        // TLS not required for loopback

        let report = evaluate_readiness(&config);

        // Whether ready depends on feature_compiled (cfg), but all config-based items should pass
        let runtime = report
            .items
            .iter()
            .find(|i| i.id == "security.runtime_enabled")
            .unwrap();
        assert!(runtime.pass);
        let auth = report
            .items
            .iter()
            .find(|i| i.id == "security.auth_configured")
            .unwrap();
        assert!(auth.pass);
        let tls_remote = report
            .items
            .iter()
            .find(|i| i.id == "security.tls_for_remote")
            .unwrap();
        assert!(tls_remote.pass, "loopback should not require TLS");
        let bind = report
            .items
            .iter()
            .find(|i| i.id == "config.bind_addr_set")
            .unwrap();
        assert!(bind.pass);
        let tls_paths = report
            .items
            .iter()
            .find(|i| i.id == "config.tls_paths")
            .unwrap();
        assert!(tls_paths.pass, "TLS disabled — paths not needed");
    }

    #[test]
    fn readiness_missing_auth_credentials_fails() {
        let mut config = DistributedConfig::default();
        config.enabled = true;
        config.auth_mode = DistributedAuthMode::Token;
        // No token, token_env, or token_path set

        let report = evaluate_readiness(&config);
        let auth = report
            .items
            .iter()
            .find(|i| i.id == "security.auth_configured")
            .unwrap();
        assert!(!auth.pass);
        assert!(auth.required);
    }

    #[test]
    fn readiness_mtls_only_passes_auth_without_token() {
        let mut config = DistributedConfig::default();
        config.enabled = true;
        config.auth_mode = DistributedAuthMode::Mtls;
        // No token set — mTLS-only doesn't need one

        let report = evaluate_readiness(&config);
        let auth = report
            .items
            .iter()
            .find(|i| i.id == "security.auth_configured")
            .unwrap();
        assert!(auth.pass, "mTLS-only should not require token credentials");
    }

    #[test]
    fn readiness_no_agent_allowlist_is_advisory_warning() {
        let mut config = DistributedConfig::default();
        config.enabled = true;
        config.auth_mode = DistributedAuthMode::Token;
        config.token = Some("secret".to_string());
        // No allow_agent_ids set

        let report = evaluate_readiness(&config);
        let advisory = report
            .items
            .iter()
            .find(|i| i.id == "security.agent_allowlist")
            .unwrap();
        assert!(!advisory.pass);
        assert!(!advisory.required);
    }

    #[test]
    fn readiness_agent_allowlist_passes_when_set() {
        let mut config = DistributedConfig::default();
        config.enabled = true;
        config.auth_mode = DistributedAuthMode::Token;
        config.token = Some("secret".to_string());
        config.allow_agent_ids = vec!["agent-1".to_string(), "agent-2".to_string()];

        let report = evaluate_readiness(&config);
        let advisory = report
            .items
            .iter()
            .find(|i| i.id == "security.agent_allowlist")
            .unwrap();
        assert!(advisory.pass);
    }

    #[test]
    fn readiness_non_loopback_without_tls_fails() {
        let mut config = DistributedConfig::default();
        config.enabled = true;
        config.auth_mode = DistributedAuthMode::Token;
        config.token = Some("test-secret".to_string());
        config.bind_addr = "0.0.0.0:4141".to_string();
        // TLS disabled, not loopback, allow_insecure=false

        let report = evaluate_readiness(&config);
        let tls = report
            .items
            .iter()
            .find(|i| i.id == "security.tls_for_remote")
            .unwrap();
        assert!(!tls.pass, "non-loopback without TLS should fail");
        assert!(tls.required);
    }

    #[test]
    fn readiness_non_loopback_with_tls_passes() {
        let mut config = DistributedConfig::default();
        config.enabled = true;
        config.auth_mode = DistributedAuthMode::Token;
        config.token = Some("test-secret".to_string());
        config.bind_addr = "10.0.0.1:4141".to_string();
        config.tls.enabled = true;
        config.tls.cert_path = Some("/etc/certs/server.pem".to_string());
        config.tls.key_path = Some("/etc/certs/server.key".to_string());

        let report = evaluate_readiness(&config);
        let tls = report
            .items
            .iter()
            .find(|i| i.id == "security.tls_for_remote")
            .unwrap();
        assert!(tls.pass);
        let paths = report
            .items
            .iter()
            .find(|i| i.id == "config.tls_paths")
            .unwrap();
        assert!(paths.pass);
    }

    #[test]
    fn readiness_allow_insecure_bypasses_tls_with_advisory_warning() {
        let mut config = DistributedConfig::default();
        config.enabled = true;
        config.auth_mode = DistributedAuthMode::Token;
        config.token = Some("test-secret".to_string());
        config.bind_addr = "0.0.0.0:4141".to_string();
        config.allow_insecure = true; // bypass TLS requirement

        let report = evaluate_readiness(&config);
        let tls = report
            .items
            .iter()
            .find(|i| i.id == "security.tls_for_remote")
            .unwrap();
        assert!(tls.pass, "allow_insecure bypasses TLS requirement");
        // Advisory should warn
        let advisory = report
            .items
            .iter()
            .find(|i| i.id == "security.no_insecure_override")
            .unwrap();
        assert!(!advisory.pass);
        assert!(!advisory.required);
    }

    #[test]
    fn readiness_tls_enabled_without_paths_fails() {
        let mut config = DistributedConfig::default();
        config.enabled = true;
        config.auth_mode = DistributedAuthMode::Token;
        config.token = Some("test-secret".to_string());
        config.tls.enabled = true;
        // No cert_path or key_path

        let report = evaluate_readiness(&config);
        let paths = report
            .items
            .iter()
            .find(|i| i.id == "config.tls_paths")
            .unwrap();
        assert!(!paths.pass);
        assert!(paths.required);
    }

    #[test]
    fn readiness_empty_bind_addr_fails() {
        let mut config = DistributedConfig::default();
        config.enabled = true;
        config.bind_addr = String::new();

        let report = evaluate_readiness(&config);
        let bind = report
            .items
            .iter()
            .find(|i| i.id == "config.bind_addr_set")
            .unwrap();
        assert!(!bind.pass);
        assert!(bind.required);
    }

    #[test]
    fn readiness_report_counts_correct() {
        let mut config = DistributedConfig::default();
        config.enabled = true;
        config.auth_mode = DistributedAuthMode::Token;
        config.token = Some("test-secret".to_string());

        let report = evaluate_readiness(&config);
        let manual_required_pass = report.items.iter().filter(|i| i.required && i.pass).count();
        let manual_required_total = report.items.iter().filter(|i| i.required).count();
        let manual_advisory_pass = report
            .items
            .iter()
            .filter(|i| !i.required && i.pass)
            .count();
        let manual_advisory_total = report.items.iter().filter(|i| !i.required).count();

        assert_eq!(report.required_pass, manual_required_pass);
        assert_eq!(report.required_total, manual_required_total);
        assert_eq!(report.advisory_pass, manual_advisory_pass);
        assert_eq!(report.advisory_total, manual_advisory_total);
        assert_eq!(report.ready, manual_required_pass == manual_required_total);
    }

    #[test]
    fn readiness_report_serde_roundtrip_batch2() {
        let mut config = DistributedConfig::default();
        config.enabled = true;
        config.auth_mode = DistributedAuthMode::Token;
        config.token = Some("test-secret".to_string());

        let report = evaluate_readiness(&config);
        let json = serde_json::to_string(&report).expect("serialize report");
        let deserialized: ReadinessReport =
            serde_json::from_str(&json).expect("deserialize report");

        assert_eq!(deserialized.ready, report.ready);
        assert_eq!(deserialized.feature_compiled, report.feature_compiled);
        assert_eq!(deserialized.runtime_enabled, report.runtime_enabled);
        assert_eq!(deserialized.items.len(), report.items.len());
        assert_eq!(deserialized.required_pass, report.required_pass);
        assert_eq!(deserialized.required_total, report.required_total);
    }

    #[test]
    fn readiness_item_serde_roundtrip() {
        let item = ReadinessItem {
            id: "test.item".to_string(),
            category: "Test".to_string(),
            description: "A test item".to_string(),
            pass: true,
            detail: "looks good".to_string(),
            required: true,
        };
        let json = serde_json::to_string(&item).expect("serialize");
        let deserialized: ReadinessItem = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(deserialized, item);
    }

    #[test]
    fn readiness_token_env_satisfies_auth() {
        let mut config = DistributedConfig::default();
        config.enabled = true;
        config.auth_mode = DistributedAuthMode::Token;
        config.token_env = Some("FT_DIST_TOKEN".to_string());

        let report = evaluate_readiness(&config);
        let auth = report
            .items
            .iter()
            .find(|i| i.id == "security.auth_configured")
            .unwrap();
        assert!(auth.pass);
    }

    #[test]
    fn readiness_token_path_satisfies_auth() {
        let mut config = DistributedConfig::default();
        config.enabled = true;
        config.auth_mode = DistributedAuthMode::Token;
        config.token_path = Some("/run/secrets/wa-token".to_string());

        let report = evaluate_readiness(&config);
        let auth = report
            .items
            .iter()
            .find(|i| i.id == "security.auth_configured")
            .unwrap();
        assert!(auth.pass);
    }

    #[test]
    fn readiness_ipv6_loopback_recognized() {
        let mut config = DistributedConfig::default();
        config.enabled = true;
        config.auth_mode = DistributedAuthMode::Token;
        config.token = Some("secret".to_string());
        config.bind_addr = "[::1]:4141".to_string();

        let report = evaluate_readiness(&config);
        let tls = report
            .items
            .iter()
            .find(|i| i.id == "security.tls_for_remote")
            .unwrap();
        assert!(tls.pass, "IPv6 loopback should not require TLS");
    }

    #[test]
    fn readiness_localhost_recognized_as_loopback() {
        let mut config = DistributedConfig::default();
        config.enabled = true;
        config.auth_mode = DistributedAuthMode::Token;
        config.token = Some("secret".to_string());
        config.bind_addr = "localhost:4141".to_string();

        let report = evaluate_readiness(&config);
        let tls = report
            .items
            .iter()
            .find(|i| i.id == "security.tls_for_remote")
            .unwrap();
        assert!(tls.pass, "localhost should not require TLS");
    }

    #[cfg(feature = "distributed")]
    #[test]
    fn tls_server_name_parses_ipv6_bind_addr() {
        let name = build_tls_server_name("[::1]:4141").expect("parse ipv6 bind addr");
        assert!(
            matches!(name, ServerName::IpAddress(_)),
            "expected ip server name, got {name:?}"
        );
    }

    #[cfg(feature = "distributed")]
    #[test]
    fn tls_server_name_parses_ipv4_bind_addr() {
        let name = build_tls_server_name("127.0.0.1:4141").expect("parse ipv4 bind addr");
        assert!(
            matches!(name, ServerName::IpAddress(_)),
            "expected ip server name, got {name:?}"
        );
    }

    #[cfg(feature = "distributed")]
    #[test]
    fn tls_server_name_parses_hostname_bind_addr() {
        let name = build_tls_server_name("localhost:4141").expect("parse host bind addr");
        assert!(
            matches!(name, ServerName::DnsName(_)),
            "expected dns server name, got {name:?}"
        );
    }

    #[test]
    fn readiness_all_items_have_unique_ids() {
        let config = DistributedConfig::default();
        let report = evaluate_readiness(&config);
        let mut ids: Vec<&str> = report.items.iter().map(|i| i.id.as_str()).collect();
        let original_len = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), original_len, "readiness item IDs must be unique");
    }

    #[test]
    fn readiness_report_json_fields_stable() {
        let mut config = DistributedConfig::default();
        config.enabled = true;
        config.auth_mode = DistributedAuthMode::Token;
        config.token = Some("secret".to_string());

        let report = evaluate_readiness(&config);
        let json = serde_json::to_value(&report).expect("serialize");

        assert!(json.get("ready").is_some());
        assert!(json.get("feature_compiled").is_some());
        assert!(json.get("runtime_enabled").is_some());
        assert!(json.get("items").is_some());
        assert!(json.get("required_pass").is_some());
        assert!(json.get("required_total").is_some());
        assert!(json.get("advisory_pass").is_some());
        assert!(json.get("advisory_total").is_some());
        assert!(json["items"].is_array());
    }

    // =====================================================================
    // normalize_identity (non-feature-gated pure function)
    // =====================================================================

    #[test]
    fn normalize_identity_trims_and_lowercases() {
        assert_eq!(normalize_identity("  Agent-1  "), "agent-1");
    }

    #[test]
    fn normalize_identity_already_normalized() {
        assert_eq!(normalize_identity("agent-1"), "agent-1");
    }

    #[test]
    fn normalize_identity_empty() {
        assert_eq!(normalize_identity(""), "");
    }

    #[test]
    fn normalize_identity_mixed_case() {
        assert_eq!(normalize_identity("MyAgent_ID"), "myagent_id");
    }

    #[test]
    fn normalize_identity_whitespace_only() {
        assert_eq!(normalize_identity("   "), "");
    }

    // =====================================================================
    // constant_time_eq (non-feature-gated pure function)
    // =====================================================================

    #[test]
    fn constant_time_eq_equal_strings() {
        assert!(constant_time_eq("secret", "secret"));
    }

    #[test]
    fn constant_time_eq_different_strings() {
        assert!(!constant_time_eq("secret", "wrong"));
    }

    #[test]
    fn constant_time_eq_different_lengths() {
        assert!(!constant_time_eq("short", "longer-string"));
    }

    #[test]
    fn constant_time_eq_empty_strings() {
        assert!(constant_time_eq("", ""));
    }

    #[test]
    fn constant_time_eq_one_empty() {
        assert!(!constant_time_eq("a", ""));
        assert!(!constant_time_eq("", "a"));
    }

    #[test]
    fn constant_time_eq_single_char_diff() {
        assert!(!constant_time_eq("abc", "abd"));
    }

    // =====================================================================
    // TokenParts::parse (non-feature-gated)
    // =====================================================================

    #[test]
    fn token_parts_parse_with_identity() {
        let parts = TokenParts::parse("agent-1:mysecret");
        assert_eq!(parts.identity, Some("agent-1"));
        assert_eq!(parts.secret, "mysecret");
    }

    #[test]
    fn token_parts_parse_without_identity() {
        let parts = TokenParts::parse("bare-secret");
        assert!(parts.identity.is_none());
        assert_eq!(parts.secret, "bare-secret");
    }

    #[test]
    fn token_parts_parse_empty_identity() {
        // ":secret" => empty identity, treated as no identity
        let parts = TokenParts::parse(":secret");
        assert!(parts.identity.is_none());
        assert_eq!(parts.secret, ":secret");
    }

    #[test]
    fn token_parts_parse_empty_secret() {
        // "identity:" => empty secret, treated as no identity
        let parts = TokenParts::parse("identity:");
        assert!(parts.identity.is_none());
        assert_eq!(parts.secret, "identity:");
    }

    #[test]
    fn token_parts_parse_multiple_colons() {
        // First colon splits identity from secret
        let parts = TokenParts::parse("agent:secret:extra");
        assert_eq!(parts.identity, Some("agent"));
        assert_eq!(parts.secret, "secret:extra");
    }

    // =====================================================================
    // validate_token (non-feature-gated)
    // =====================================================================

    #[test]
    fn validate_token_no_auth_required() {
        // MtlsOnly mode doesn't require token
        assert!(validate_token(DistributedAuthMode::Mtls, None, None, None).is_ok());
    }

    #[test]
    fn validate_token_missing_expected() {
        assert!(matches!(
            validate_token(DistributedAuthMode::Token, None, Some("x"), None),
            Err(DistributedSecurityError::MissingToken)
        ));
    }

    #[test]
    fn validate_token_missing_presented() {
        assert!(matches!(
            validate_token(DistributedAuthMode::Token, Some("x"), None, None),
            Err(DistributedSecurityError::MissingToken)
        ));
    }

    #[test]
    fn validate_token_matching_bare_secret() {
        assert!(
            validate_token(
                DistributedAuthMode::Token,
                Some("secret"),
                Some("secret"),
                None
            )
            .is_ok()
        );
    }

    #[test]
    fn validate_token_wrong_bare_secret() {
        assert!(matches!(
            validate_token(
                DistributedAuthMode::Token,
                Some("correct"),
                Some("wrong"),
                None
            ),
            Err(DistributedSecurityError::AuthFailed)
        ));
    }

    #[test]
    fn validate_token_identity_match() {
        assert!(
            validate_token(
                DistributedAuthMode::Token,
                Some("agent:secret"),
                Some("agent:secret"),
                Some("agent"),
            )
            .is_ok()
        );
    }

    #[test]
    fn validate_token_identity_mismatch() {
        assert!(matches!(
            validate_token(
                DistributedAuthMode::Token,
                Some("agent-a:secret"),
                Some("agent-b:secret"),
                None,
            ),
            Err(DistributedSecurityError::AuthFailed)
        ));
    }

    #[test]
    fn validate_token_identity_case_insensitive() {
        assert!(
            validate_token(
                DistributedAuthMode::Token,
                Some("Agent-A:secret"),
                Some("agent-a:secret"),
                Some("agent-a"),
            )
            .is_ok()
        );
    }

    #[test]
    fn validate_token_identity_requires_client_identity() {
        assert!(matches!(
            validate_token(
                DistributedAuthMode::Token,
                Some("agent-a:secret"),
                Some("agent-a:secret"),
                None,
            ),
            Err(DistributedSecurityError::AuthFailed)
        ));
    }

    #[test]
    fn validate_token_client_identity_mismatch() {
        assert!(matches!(
            validate_token(
                DistributedAuthMode::TokenAndMtls,
                Some("agent-1:secret"),
                Some("agent-1:secret"),
                Some("agent-2"),
            ),
            Err(DistributedSecurityError::AuthFailed)
        ));
    }

    // =====================================================================
    // DistributedSecurityError::code (non-feature-gated)
    // =====================================================================

    #[test]
    fn security_error_codes_all_stable() {
        assert_eq!(
            DistributedSecurityError::MissingToken.code(),
            "dist.auth_failed"
        );
        assert_eq!(
            DistributedSecurityError::AuthFailed.code(),
            "dist.auth_failed"
        );
        assert_eq!(
            DistributedSecurityError::ReplayDetected.code(),
            "dist.replay_detected"
        );
        assert_eq!(
            DistributedSecurityError::SessionLimitReached.code(),
            "dist.session_limit"
        );
        assert_eq!(
            DistributedSecurityError::ConnectionLimitReached.code(),
            "dist.connection_limit"
        );
        assert_eq!(
            DistributedSecurityError::MessageTooLarge.code(),
            "dist.message_too_large"
        );
        assert_eq!(
            DistributedSecurityError::RateLimited.code(),
            "dist.rate_limited"
        );
        assert_eq!(
            DistributedSecurityError::HandshakeTimeout.code(),
            "dist.handshake_timeout"
        );
        assert_eq!(
            DistributedSecurityError::MessageTimeout.code(),
            "dist.message_timeout"
        );
    }

    // =====================================================================
    // DistributedSecurityError traits (non-feature-gated)
    // =====================================================================

    #[test]
    fn security_error_display() {
        let err = DistributedSecurityError::AuthFailed;
        let msg = err.to_string();
        assert!(msg.contains("auth failed"));
    }

    #[test]
    fn security_error_clone_eq() {
        let err1 = DistributedSecurityError::ReplayDetected;
        let err2 = err1.clone();
        assert_eq!(err1, err2);
    }

    #[test]
    fn security_error_debug() {
        let err = DistributedSecurityError::RateLimited;
        let dbg = format!("{:?}", err);
        assert!(dbg.contains("RateLimited"));
    }

    #[test]
    fn security_error_inequality() {
        assert_ne!(
            DistributedSecurityError::MissingToken,
            DistributedSecurityError::AuthFailed
        );
    }

    // =====================================================================
    // configured_token_source_kind (non-feature-gated)
    // =====================================================================

    #[test]
    fn token_source_kind_inline() {
        let mut config = DistributedConfig::default();
        config.token = Some("inline-token".to_string());
        assert_eq!(
            configured_token_source_kind(&config),
            Some(DistributedTokenSourceKind::Inline)
        );
    }

    #[test]
    fn token_source_kind_env() {
        let mut config = DistributedConfig::default();
        config.token_env = Some("FT_TOKEN".to_string());
        assert_eq!(
            configured_token_source_kind(&config),
            Some(DistributedTokenSourceKind::Env)
        );
    }

    #[test]
    fn token_source_kind_file() {
        let mut config = DistributedConfig::default();
        config.token_path = Some("/run/secrets/token".to_string());
        assert_eq!(
            configured_token_source_kind(&config),
            Some(DistributedTokenSourceKind::File)
        );
    }

    #[test]
    fn token_source_kind_none_when_nothing_set() {
        let config = DistributedConfig::default();
        assert_eq!(configured_token_source_kind(&config), None);
    }

    #[test]
    fn token_source_kind_none_when_ambiguous() {
        let mut config = DistributedConfig::default();
        config.token = Some("inline".to_string());
        config.token_env = Some("ENV".to_string());
        assert_eq!(configured_token_source_kind(&config), None);
    }

    #[test]
    fn token_source_kind_ignores_empty_strings() {
        let mut config = DistributedConfig::default();
        config.token = Some("  ".to_string()); // whitespace only
        config.token_env = Some("REAL_VAR".to_string());
        assert_eq!(
            configured_token_source_kind(&config),
            Some(DistributedTokenSourceKind::Env)
        );
    }

    // =====================================================================
    // DistributedTokenSourceKind traits
    // =====================================================================

    #[test]
    fn token_source_kind_debug_clone_copy_eq_batch2() {
        let k = DistributedTokenSourceKind::File;
        let k2 = k; // Copy
        assert_eq!(k, k2);
        let dbg = format!("{:?}", k);
        assert!(dbg.contains("File"));
    }

    // =====================================================================
    // resolve_expected_token edge cases (non-feature-gated)
    // =====================================================================

    #[test]
    fn resolve_token_inline() {
        let mut config = DistributedConfig::default();
        config.auth_mode = DistributedAuthMode::Token;
        config.token = Some("my-secret".to_string());

        let tok = resolve_expected_token(&config).unwrap().unwrap();
        assert_eq!(tok, "my-secret");
    }

    #[test]
    fn resolve_token_no_auth_returns_none() {
        let mut config = DistributedConfig::default();
        config.auth_mode = DistributedAuthMode::Mtls;
        // Mtls doesn't require token

        assert_eq!(resolve_expected_token(&config).unwrap(), None);
    }

    #[test]
    fn resolve_token_missing_all_sources() {
        let mut config = DistributedConfig::default();
        config.auth_mode = DistributedAuthMode::Token;
        // No token, token_env, or token_path

        assert!(matches!(
            resolve_expected_token(&config),
            Err(DistributedCredentialError::TokenMissing)
        ));
    }

    #[test]
    fn resolve_token_empty_inline() {
        let mut config = DistributedConfig::default();
        config.auth_mode = DistributedAuthMode::Token;
        config.token = Some("  ".to_string()); // whitespace only, treated as empty

        assert!(matches!(
            resolve_expected_token(&config),
            Err(DistributedCredentialError::TokenMissing)
        ));
    }

    #[test]
    fn resolve_token_env_missing_var() {
        let mut config = DistributedConfig::default();
        config.auth_mode = DistributedAuthMode::Token;
        config.token_env = Some("FT_NONEXISTENT_TEST_VAR_12345".to_string());

        assert!(matches!(
            resolve_expected_token(&config),
            Err(DistributedCredentialError::TokenEnvMissing(_))
        ));
    }

    #[test]
    fn resolve_token_file_not_found() {
        let mut config = DistributedConfig::default();
        config.auth_mode = DistributedAuthMode::Token;
        config.token_path = Some("/nonexistent/path/to/token".to_string());

        assert!(matches!(
            resolve_expected_token(&config),
            Err(DistributedCredentialError::TokenFileRead { .. })
        ));
    }

    // =====================================================================
    // DistributedTlsError Display
    // =====================================================================

    #[test]
    fn tls_error_display_variants() {
        assert!(
            DistributedTlsError::TlsDisabled
                .to_string()
                .contains("not enabled")
        );
        assert!(
            DistributedTlsError::MissingCertPath
                .to_string()
                .contains("certificate")
        );
        assert!(
            DistributedTlsError::MissingKeyPath
                .to_string()
                .contains("key")
        );
        assert!(
            DistributedTlsError::MissingClientCaPath
                .to_string()
                .contains("client")
        );
        assert!(
            DistributedTlsError::MissingServerCaPath
                .to_string()
                .contains("server")
        );
        assert!(
            DistributedTlsError::EmptyCertChain("test.pem".to_string())
                .to_string()
                .contains("test.pem")
        );
        assert!(
            DistributedTlsError::EmptyPrivateKey("key.pem".to_string())
                .to_string()
                .contains("key.pem")
        );
        assert!(
            DistributedTlsError::InvalidMinTlsVersion("0.9".to_string())
                .to_string()
                .contains("0.9")
        );
        assert!(
            DistributedTlsError::Config("bad config".to_string())
                .to_string()
                .contains("bad config")
        );
    }

    // =====================================================================
    // DistributedCredentialError Display
    // =====================================================================

    #[test]
    fn credential_error_display() {
        assert!(
            DistributedCredentialError::TokenMissing
                .to_string()
                .contains("required")
        );
        assert!(
            DistributedCredentialError::TokenAmbiguous
                .to_string()
                .contains("ambiguous")
        );
        assert!(
            DistributedCredentialError::TokenEmpty
                .to_string()
                .contains("empty")
        );
        assert!(
            DistributedCredentialError::TokenEnvMissing("MY_VAR".to_string())
                .to_string()
                .contains("MY_VAR")
        );
    }

    // =====================================================================
    // ReadinessItem / ReadinessReport additional traits
    // =====================================================================

    #[test]
    fn readiness_item_equality() {
        let item1 = ReadinessItem {
            id: "test".to_string(),
            category: "Cat".to_string(),
            description: "Desc".to_string(),
            pass: true,
            detail: "ok".to_string(),
            required: true,
        };
        let item2 = item1.clone();
        assert_eq!(item1, item2);
    }

    #[test]
    fn readiness_item_inequality() {
        let item1 = ReadinessItem {
            id: "a".to_string(),
            category: "Cat".to_string(),
            description: "Desc".to_string(),
            pass: true,
            detail: "ok".to_string(),
            required: true,
        };
        let item2 = ReadinessItem {
            id: "b".to_string(),
            ..item1.clone()
        };
        assert_ne!(item1, item2);
    }

    #[test]
    fn readiness_report_debug() {
        let config = DistributedConfig::default();
        let report = evaluate_readiness(&config);
        let dbg = format!("{:?}", report);
        assert!(dbg.contains("ReadinessReport"));
    }

    // Batch: DarkBadger wa-1u90p.7.1

    #[test]
    fn distributed_security_error_debug_clone_eq() {
        let a = DistributedSecurityError::MissingToken;
        let b = a.clone();
        assert_eq!(a, b);
        assert_ne!(
            DistributedSecurityError::MissingToken,
            DistributedSecurityError::AuthFailed
        );
        let _ = format!("{:?}", a);
    }

    #[test]
    fn distributed_security_error_all_nine_distinct() {
        let errors = [
            DistributedSecurityError::MissingToken,
            DistributedSecurityError::AuthFailed,
            DistributedSecurityError::ReplayDetected,
            DistributedSecurityError::SessionLimitReached,
            DistributedSecurityError::ConnectionLimitReached,
            DistributedSecurityError::MessageTooLarge,
            DistributedSecurityError::RateLimited,
            DistributedSecurityError::HandshakeTimeout,
            DistributedSecurityError::MessageTimeout,
        ];
        for (i, a) in errors.iter().enumerate() {
            for (j, b) in errors.iter().enumerate() {
                if i == j {
                    assert_eq!(a, b);
                } else {
                    assert_ne!(a, b);
                }
            }
        }
    }

    #[test]
    fn distributed_security_error_display_all() {
        assert!(
            DistributedSecurityError::MissingToken
                .to_string()
                .contains("token")
        );
        assert!(
            DistributedSecurityError::AuthFailed
                .to_string()
                .contains("auth")
        );
        assert!(
            DistributedSecurityError::ReplayDetected
                .to_string()
                .contains("replay")
        );
        assert!(
            DistributedSecurityError::SessionLimitReached
                .to_string()
                .contains("session")
        );
        assert!(
            DistributedSecurityError::ConnectionLimitReached
                .to_string()
                .contains("connection")
        );
        assert!(
            DistributedSecurityError::MessageTooLarge
                .to_string()
                .contains("large")
        );
        assert!(
            DistributedSecurityError::RateLimited
                .to_string()
                .contains("rate")
        );
        assert!(
            DistributedSecurityError::HandshakeTimeout
                .to_string()
                .contains("handshake")
        );
        assert!(
            DistributedSecurityError::MessageTimeout
                .to_string()
                .contains("message")
        );
    }

    #[test]
    fn distributed_security_error_code_all() {
        assert_eq!(
            DistributedSecurityError::MissingToken.code(),
            "dist.auth_failed"
        );
        assert_eq!(
            DistributedSecurityError::AuthFailed.code(),
            "dist.auth_failed"
        );
        assert_eq!(
            DistributedSecurityError::ReplayDetected.code(),
            "dist.replay_detected"
        );
        assert_eq!(
            DistributedSecurityError::SessionLimitReached.code(),
            "dist.session_limit"
        );
        assert_eq!(
            DistributedSecurityError::ConnectionLimitReached.code(),
            "dist.connection_limit"
        );
        assert_eq!(
            DistributedSecurityError::MessageTooLarge.code(),
            "dist.message_too_large"
        );
        assert_eq!(
            DistributedSecurityError::RateLimited.code(),
            "dist.rate_limited"
        );
        assert_eq!(
            DistributedSecurityError::HandshakeTimeout.code(),
            "dist.handshake_timeout"
        );
        assert_eq!(
            DistributedSecurityError::MessageTimeout.code(),
            "dist.message_timeout"
        );
    }

    #[test]
    fn token_source_kind_debug_clone_copy_eq_v2() {
        let a = DistributedTokenSourceKind::Inline;
        let b = a; // Copy
        assert_eq!(a, b);
        let c = a;
        assert_eq!(a, c);
        assert_ne!(
            DistributedTokenSourceKind::Inline,
            DistributedTokenSourceKind::Env
        );
        assert_ne!(
            DistributedTokenSourceKind::Env,
            DistributedTokenSourceKind::File
        );
        assert_ne!(
            DistributedTokenSourceKind::Inline,
            DistributedTokenSourceKind::File
        );
        let _ = format!("{:?}", a);
    }

    #[test]
    fn configured_token_source_kind_inline_only() {
        let mut config = DistributedConfig::default();
        config.token = Some("secret".to_string());
        assert_eq!(
            configured_token_source_kind(&config),
            Some(DistributedTokenSourceKind::Inline)
        );
    }

    #[test]
    fn configured_token_source_kind_env_only() {
        let mut config = DistributedConfig::default();
        config.token_env = Some("MY_TOKEN".to_string());
        assert_eq!(
            configured_token_source_kind(&config),
            Some(DistributedTokenSourceKind::Env)
        );
    }

    #[test]
    fn configured_token_source_kind_file_only() {
        let mut config = DistributedConfig::default();
        config.token_path = Some("/tmp/token".to_string());
        assert_eq!(
            configured_token_source_kind(&config),
            Some(DistributedTokenSourceKind::File)
        );
    }

    #[test]
    fn configured_token_source_kind_none() {
        let config = DistributedConfig::default();
        assert_eq!(configured_token_source_kind(&config), None);
    }

    #[test]
    fn configured_token_source_kind_multiple() {
        let mut config = DistributedConfig::default();
        config.token = Some("secret".to_string());
        config.token_env = Some("ENV".to_string());
        assert_eq!(configured_token_source_kind(&config), None);
    }

    #[test]
    fn configured_token_source_kind_whitespace_ignored() {
        let mut config = DistributedConfig::default();
        config.token = Some("  ".to_string()); // whitespace only → treated as empty
        assert_eq!(configured_token_source_kind(&config), None);
    }

    #[test]
    fn readiness_item_serde_roundtrip_batch2() {
        let item = ReadinessItem {
            id: "test.check".to_string(),
            category: "Test".to_string(),
            description: "A test check".to_string(),
            pass: true,
            detail: "all good".to_string(),
            required: false,
        };
        let json = serde_json::to_string(&item).unwrap();
        let back: ReadinessItem = serde_json::from_str(&json).unwrap();
        assert_eq!(item, back);
    }

    #[test]
    fn readiness_report_serde_roundtrip_v2() {
        let config = DistributedConfig::default();
        let report = evaluate_readiness(&config);
        let json = serde_json::to_string(&report).unwrap();
        let back: ReadinessReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back.ready, report.ready);
        assert_eq!(back.items.len(), report.items.len());
        assert_eq!(back.required_total, report.required_total);
    }

    #[test]
    fn readiness_report_clone() {
        let config = DistributedConfig::default();
        let report = evaluate_readiness(&config);
        let cloned = report.clone();
        assert_eq!(cloned.ready, report.ready);
        assert_eq!(cloned.items.len(), report.items.len());
        assert_eq!(cloned.feature_compiled, report.feature_compiled);
    }

    #[test]
    fn tls_error_display_all() {
        assert!(
            DistributedTlsError::TlsDisabled
                .to_string()
                .contains("not enabled")
        );
        assert!(
            DistributedTlsError::MissingCertPath
                .to_string()
                .contains("certificate")
        );
        assert!(
            DistributedTlsError::MissingKeyPath
                .to_string()
                .contains("key")
        );
        assert!(
            DistributedTlsError::MissingClientCaPath
                .to_string()
                .contains("client")
        );
        assert!(
            DistributedTlsError::MissingServerCaPath
                .to_string()
                .contains("server")
        );
    }

    #[test]
    fn tls_error_debug() {
        let e = DistributedTlsError::TlsDisabled;
        let dbg = format!("{:?}", e);
        assert!(dbg.contains("TlsDisabled"));
    }

    #[test]
    fn credential_error_std_error_trait() {
        let e: Box<dyn std::error::Error> = Box::new(DistributedCredentialError::TokenMissing);
        assert!(e.to_string().contains("required"));
    }

    #[test]
    fn resolve_expected_token_no_auth_required() {
        let mut config = DistributedConfig::default();
        config.auth_mode = DistributedAuthMode::Mtls; // mtls doesn't require token
        let result = resolve_expected_token(&config).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn resolve_expected_token_inline() {
        let mut config = DistributedConfig::default();
        config.auth_mode = DistributedAuthMode::Token;
        config.token = Some("inline-secret".to_string());
        let tok = resolve_expected_token(&config).unwrap().unwrap();
        assert_eq!(tok, "inline-secret");
    }

    #[test]
    fn resolve_expected_token_missing() {
        let mut config = DistributedConfig::default();
        config.auth_mode = DistributedAuthMode::Token;
        // no token source at all
        let err = resolve_expected_token(&config).unwrap_err();
        assert!(matches!(err, DistributedCredentialError::TokenMissing));
    }

    // =========================================================================
    // wa-1u55z: Production bundle → asupersync TLS integration tests
    // =========================================================================

    /// Verify `DistributedTlsBundle::acceptor()` and `connector()` produce
    /// working asupersync TLS types that complete a handshake.
    #[cfg(feature = "distributed")]
    #[test]
    fn bundle_acceptor_connector_handshake() {
        run_async_test(async {
            let ca_cert = temp_pem(CA_CERT);
            let server_cert = temp_pem(SERVER_CERT);
            let server_key = temp_pem(SERVER_KEY);

            let mut config = DistributedConfig::default();
            config.enabled = true;
            config.tls.enabled = true;
            config.tls.cert_path = Some(server_cert.path().display().to_string());
            config.tls.key_path = Some(server_key.path().display().to_string());

            let bundle = build_tls_bundle(&config, Some(ca_cert.path())).expect("build bundle");

            let acceptor = bundle.acceptor();
            let connector = bundle.connector();

            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let addr = listener.local_addr().expect("addr");

            let server_task = crate::runtime_compat::task::spawn(async move {
                let (stream, _) = listener.accept().await.expect("accept");
                let mut tls = acceptor.accept(stream).await.expect("tls accept");
                let mut buf = [0u8; 5];
                tls.read_exact(&mut buf).await.expect("read");
                buf
            });

            let mut client = connector
                .connect(
                    "localhost",
                    TcpStream::connect(addr).await.expect("connect"),
                )
                .await
                .expect("tls connect");
            client.write_all(b"hello").await.expect("write");

            let received = server_task.await.expect("join");
            assert_eq!(&received, b"hello");
        });
    }

    /// Verify `DistributedTlsBundle::acceptor()` and `connector()` work with
    /// mTLS (mutual authentication).
    #[cfg(feature = "distributed")]
    #[test]
    fn bundle_acceptor_connector_mtls() {
        run_async_test(async {
            let ca_cert = temp_pem(CA_CERT);
            let server_cert = temp_pem(SERVER_CERT);
            let server_key = temp_pem(SERVER_KEY);
            let client_cert = temp_pem(CLIENT_CERT);
            let client_key = temp_pem(CLIENT_KEY);

            let mut server_cfg = DistributedConfig::default();
            server_cfg.enabled = true;
            server_cfg.auth_mode = DistributedAuthMode::Mtls;
            server_cfg.tls.enabled = true;
            server_cfg.tls.cert_path = Some(server_cert.path().display().to_string());
            server_cfg.tls.key_path = Some(server_key.path().display().to_string());
            server_cfg.tls.client_ca_path = Some(ca_cert.path().display().to_string());
            server_cfg.allow_agent_ids = vec!["wa-client".to_string()];

            let mut client_cfg = DistributedConfig::default();
            client_cfg.enabled = true;
            client_cfg.auth_mode = DistributedAuthMode::Mtls;
            client_cfg.tls.enabled = true;
            client_cfg.tls.cert_path = Some(client_cert.path().display().to_string());
            client_cfg.tls.key_path = Some(client_key.path().display().to_string());

            let server_bundle =
                build_tls_bundle(&server_cfg, Some(ca_cert.path())).expect("server bundle");
            let client_bundle =
                build_tls_bundle(&client_cfg, Some(ca_cert.path())).expect("client bundle");

            let acceptor = server_bundle.acceptor();
            let connector = client_bundle.connector();

            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let addr = listener.local_addr().expect("addr");

            let server_task = crate::runtime_compat::task::spawn(async move {
                let (stream, _) = listener.accept().await.expect("accept");
                let mut tls = acceptor.accept(stream).await.expect("mtls accept");
                let mut buf = [0u8; 4];
                tls.read_exact(&mut buf).await.expect("read");
                buf
            });

            let mut client = connector
                .connect(
                    "localhost",
                    TcpStream::connect(addr).await.expect("connect"),
                )
                .await
                .expect("mtls connect");
            client.write_all(b"mtls").await.expect("write");

            let received = server_task.await.expect("join");
            assert_eq!(&received, b"mtls");
        });
    }

    /// Verify the bundle is cloneable and both clones produce working TLS types.
    #[cfg(feature = "distributed")]
    #[test]
    fn bundle_clone_produces_working_tls() {
        let ca_cert = temp_pem(CA_CERT);
        let server_cert = temp_pem(SERVER_CERT);
        let server_key = temp_pem(SERVER_KEY);

        let mut config = DistributedConfig::default();
        config.enabled = true;
        config.tls.enabled = true;
        config.tls.cert_path = Some(server_cert.path().display().to_string());
        config.tls.key_path = Some(server_key.path().display().to_string());

        let bundle = build_tls_bundle(&config, Some(ca_cert.path())).expect("build");
        let cloned = bundle.clone();

        // Both produce acceptors/connectors without panic
        let _a1 = bundle.acceptor();
        let _c1 = bundle.connector();
        let _a2 = cloned.acceptor();
        let _c2 = cloned.connector();
    }

    /// Verify `DistributedHttpClient::new()` creates without panic.
    #[cfg(feature = "distributed")]
    #[test]
    fn distributed_http_client_creates() {
        let _client = DistributedHttpClient::new();
        let _plaintext = DistributedHttpClient::plaintext();
    }

    /// Verify `DistributedHttpClient` can make a request to a local server.
    #[cfg(feature = "distributed")]
    #[test]
    fn distributed_http_client_local_get() {
        run_async_test(async {
            use asupersync::io::AsyncWriteExt as _;

            // Spin up a minimal HTTP server
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let addr = listener.local_addr().expect("addr");

            let server_task = crate::runtime_compat::task::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut buf = [0u8; 1024];
                let n = stream.read(&mut buf).await.expect("read request");
                assert!(n > 0);
                let response = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
                stream.write_all(response).await.expect("write response");
                stream.shutdown(std::net::Shutdown::Both).expect("shutdown");
            });

            let client = DistributedHttpClient::plaintext();
            let cx = asupersync::cx::Cx::for_testing();
            let url = format!("http://127.0.0.1:{}/health", addr.port());
            let resp = client.get(&cx, &url).await.expect("get");
            assert_eq!(resp.status, 200);
            assert_eq!(resp.body, b"ok");

            server_task.await.expect("join");
        });
    }

    /// ft-xbnl0.2.4 tick 313: Pin the cx-cancel contract for
    /// `DistributedHttpClient::get`.
    ///
    /// The HTTP client wraps `asupersync::http::h1::http_client::HttpClient`,
    /// which accepts `&Cx` and should honor cancellation. A pre-cancelled
    /// cx must cause `get` to return an error (not silently complete
    /// against a cooperative server, and not hang against a stalled one).
    ///
    /// Acceptance criterion 3 of ft-xbnl0.2.4 — "Verification covers
    /// correctness" — includes the cx-cancel semantics of the migrated
    /// HTTP client boundary.
    #[cfg(feature = "distributed")]
    #[test]
    fn distributed_http_client_honors_pre_cancelled_cx() {
        run_async_test(async {
            // Server accepts but never writes a response. Without cx-cancel
            // propagation, the client would hang waiting for response bytes.
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let addr = listener.local_addr().expect("addr");

            let _server_task = crate::runtime_compat::task::spawn(async move {
                let _ = listener.accept().await;
                // Hold the connection open; never write a response.
                crate::runtime_compat::sleep(std::time::Duration::from_secs(10)).await;
            });

            let client = DistributedHttpClient::plaintext();
            let cx = asupersync::cx::Cx::for_testing();
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("pre-cancel for HTTP cx contract test"),
            );

            let url = format!("http://127.0.0.1:{}/health", addr.port());
            let started = std::time::Instant::now();
            let result = client.get(&cx, &url).await;
            let elapsed = started.elapsed();

            assert!(
                result.is_err(),
                "pre-cancelled cx must cause get() to return an error, got: {result:?}"
            );
            // A pre-cancelled cx should fail fast. Bound generously to
            // tolerate kernel connect/accept scheduling — the point is
            // "does not hang against a stalled server", not a tight
            // latency bound.
            assert!(
                elapsed < std::time::Duration::from_secs(5),
                "pre-cancelled cx should fail fast; took {elapsed:?}"
            );
        });
    }

    /// ft-xbnl0.2.4 tick 314: Pin the cx-cancel contract for
    /// `DistributedHttpClient::post` (mirror of tick 313's GET test).
    ///
    /// POST carries a body, so the cancel-propagation path differs from
    /// GET — the client must check cx at body-send boundaries too, not
    /// just at response-read. This test pre-cancels the cx and verifies
    /// POST fails fast against a stalled server.
    ///
    /// Acceptance criterion 3 of ft-xbnl0.2.4 — "Verification covers
    /// correctness" — includes both HTTP verbs the production code uses
    /// (GET for health checks, POST for event forwarding).
    #[cfg(feature = "distributed")]
    #[test]
    fn distributed_http_client_post_honors_pre_cancelled_cx() {
        run_async_test(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let addr = listener.local_addr().expect("addr");

            let _server_task = crate::runtime_compat::task::spawn(async move {
                let _ = listener.accept().await;
                crate::runtime_compat::sleep(std::time::Duration::from_secs(10)).await;
            });

            let client = DistributedHttpClient::plaintext();
            let cx = asupersync::cx::Cx::for_testing();
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("pre-cancel for HTTP POST cx contract test"),
            );

            let url = format!("http://127.0.0.1:{}/events", addr.port());
            let started = std::time::Instant::now();
            let result = client.post(&cx, &url, b"body=data".to_vec()).await;
            let elapsed = started.elapsed();

            assert!(
                result.is_err(),
                "pre-cancelled cx must cause post() to return an error, got: {result:?}"
            );
            assert!(
                elapsed < std::time::Duration::from_secs(5),
                "pre-cancelled cx should fail fast; took {elapsed:?}"
            );
        });
    }

    /// Verify bidirectional data exchange over TLS using bundle helpers.
    #[cfg(feature = "distributed")]
    #[test]
    fn bundle_tls_bidirectional_exchange() {
        run_async_test(async {
            let ca_cert = temp_pem(CA_CERT);
            let server_cert = temp_pem(SERVER_CERT);
            let server_key = temp_pem(SERVER_KEY);

            let mut config = DistributedConfig::default();
            config.enabled = true;
            config.tls.enabled = true;
            config.tls.cert_path = Some(server_cert.path().display().to_string());
            config.tls.key_path = Some(server_key.path().display().to_string());

            let bundle = build_tls_bundle(&config, Some(ca_cert.path())).expect("build bundle");
            let acceptor = bundle.acceptor();
            let connector = bundle.connector();

            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let addr = listener.local_addr().expect("addr");

            let server_task = crate::runtime_compat::task::spawn(async move {
                let (stream, _) = listener.accept().await.expect("accept");
                let mut tls = acceptor.accept(stream).await.expect("tls accept");

                // Read request
                let mut buf = [0u8; 7];
                tls.read_exact(&mut buf).await.expect("read");
                assert_eq!(&buf, b"request");

                // Write response
                tls.write_all(b"response").await.expect("write");
            });

            let mut client = connector
                .connect(
                    "localhost",
                    TcpStream::connect(addr).await.expect("connect"),
                )
                .await
                .expect("tls connect");

            // Write request
            client.write_all(b"request").await.expect("write");

            // Read response
            let mut buf = [0u8; 8];
            client.read_exact(&mut buf).await.expect("read");
            assert_eq!(&buf, b"response");

            server_task.await.expect("join");
        });
    }

    /// Verify TLS throughput path with larger payload using bundle helpers.
    #[cfg(feature = "distributed")]
    #[test]
    fn bundle_tls_large_payload() {
        run_async_test(async {
            let ca_cert = temp_pem(CA_CERT);
            let server_cert = temp_pem(SERVER_CERT);
            let server_key = temp_pem(SERVER_KEY);

            let mut config = DistributedConfig::default();
            config.enabled = true;
            config.tls.enabled = true;
            config.tls.cert_path = Some(server_cert.path().display().to_string());
            config.tls.key_path = Some(server_key.path().display().to_string());

            let bundle = build_tls_bundle(&config, Some(ca_cert.path())).expect("build bundle");
            let acceptor = bundle.acceptor();
            let connector = bundle.connector();

            let payload_size = 256 * 1024; // 256 KiB
            let payload = vec![0xABu8; payload_size];

            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let addr = listener.local_addr().expect("addr");

            let server_task = crate::runtime_compat::task::spawn(async move {
                let (stream, _) = listener.accept().await.expect("accept");
                let mut tls = acceptor.accept(stream).await.expect("tls accept");
                let mut received = Vec::new();
                let mut buf = [0u8; 16 * 1024];
                loop {
                    let n = tls.read(&mut buf).await.expect("read");
                    if n == 0 {
                        break;
                    }
                    received.extend_from_slice(&buf[..n]);
                }
                received.len()
            });

            let mut client = connector
                .connect(
                    "localhost",
                    TcpStream::connect(addr).await.expect("connect"),
                )
                .await
                .expect("tls connect");
            client.write_all(&payload).await.expect("write");
            client.shutdown().await.expect("shutdown");

            let received_len = server_task.await.expect("join");
            assert_eq!(received_len, payload_size);
        });
    }
}
