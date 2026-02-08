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
        let presented_norm = presented_parts.identity.map(normalize_identity);
        if presented_norm.as_deref() != Some(expected_norm.as_str()) {
            return Err(DistributedSecurityError::AuthFailed);
        }
        if let Some(client_identity) = client_identity {
            if normalize_identity(client_identity) != expected_norm {
                return Err(DistributedSecurityError::AuthFailed);
            }
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
/// rotation by updating the token file content without changing `wa.toml`.
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
                .compare_exchange(current, current + 1, Ordering::SeqCst, Ordering::SeqCst)
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
fn build_server_config(
    tls: &DistributedTlsConfig,
    auth_mode: DistributedAuthMode,
    allow_agent_ids: &[String],
) -> Result<Arc<ServerConfig>, DistributedTlsError> {
    if !tls.enabled {
        return Err(DistributedTlsError::TlsDisabled);
    }

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
    let host = bind_addr.split(':').next().unwrap_or(bind_addr).trim();
    let name = if host.is_empty() { "localhost" } else { host };
    ServerName::try_from(name.to_string())
        .map_err(|_| DistributedTlsError::Config("invalid server name".to_string()))
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
//   - Runtime: `[distributed] enabled = true` in wa.toml
//
// This dual gate ensures operators consciously enable both the code path
// and the runtime behavior. The default binary ships without distributed
// networking capabilities.
//
// ## Rollout Steps
//
// 1. Build with `cargo build --features distributed`
// 2. Run `wa doctor` to verify security posture
// 3. Configure `[distributed]` in wa.toml (see distributed-security-spec.md)
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
            "Set distributed.enabled = true in wa.toml".to_string()
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
        detail: "wa emits tracing spans for all distributed operations".to_string(),
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
    use proptest::prelude::*;
    #[cfg(feature = "distributed")]
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    #[cfg(feature = "distributed")]
    use tokio::net::TcpListener;
    #[cfg(feature = "distributed")]
    use tokio::time::{Duration, timeout};
    #[cfg(feature = "distributed")]
    use tokio_rustls::{TlsAcceptor, TlsConnector};

    #[cfg(feature = "distributed")]
    const CA_CERT: &str = "-----BEGIN CERTIFICATE-----\nMIIDGzCCAgOgAwIBAgIUR8JHXom3tZxZAwXcBF09FctZBXUwDQYJKoZIhvcNAQEL\nBQAwFTETMBEGA1UEAwwKd2EtdGVzdC1jYTAeFw0yNjAxMzExOTUwNDFaFw0yNjAz\nMDIxOTUwNDFaMBUxEzARBgNVBAMMCndhLXRlc3QtY2EwggEiMA0GCSqGSIb3DQEB\nAQUAA4IBDwAwggEKAoIBAQCLsfmpPVqsXx4W3mJhOSonFeARj9j9jZ2z7HKq5DwF\nt40XW9aBTJ3tAyEf+96so/196v2dwNL/GF2c/NLFDYblpVKWKEBpbIxsFeimquz/\nBP+biMAXHK18/r2Sotad5FNb3jLGmeZ5q9jjC2T+Mvw7KFc0ptz/m7yivBgECQgS\n3qfaKfeYwdPVtRT9BHLXtVi0y1r7E+7bvfnWBkIJ5Jz/LIDOQBoEd/ofwuvWx/as\n3Pnz4jbN8Rz5/x8GmgVni5ryaoJv0nmNavoZScIGgVOua3Cro8Nf47lW67HQ7QTl\ngWbTURQzjRznD2KWQKclNt8LMfhaTPWCwWv5m99wibDDAgMBAAGjYzBhMB0GA1Ud\nDgQWBBRuIqT4PRnABam0DRoUTFnTmT0rozAfBgNVHSMEGDAWgBRuIqT4PRnABam0\nDRoUTFnTmT0rozAPBgNVHRMBAf8EBTADAQH/MA4GA1UdDwEB/wQEAwIBBjANBgkq\nhkiG9w0BAQsFAAOCAQEAIrtQ1+ykRNoqpYuvcuMa5s3inzpCkmtXfrhXAIclroAW\nhxkZ8YobU381HSjq9CoOmcEwvj/SESqCD21u3qH4iqAPXEMSdi7sfXznc41Xmm+Z\nK5gXwmeqmO+VX7t2XtSvAeBEhOTpgtFcOCt2UoSVD38Qq8yJGcE7zS5d2B2rncTz\nhtHaFr21HeGSpn+Jz91CgPBCdhHuVrruZOr61lhfHfaNH8E7pPS63GXbo58yrOfX\nw/w5gkbPZVMkxLFn1OQt2Ah4uud4VbJ76JOylfyKwWJH3VrYw8ZE98M3CWRh6mGq\nhLXdOswkuXOAIL5kTVIpJzkXRxW+owwW5pHvCs0DiA==\n-----END CERTIFICATE-----\n";
    #[cfg(feature = "distributed")]
    const CA_CERT_ALT: &str = "-----BEGIN CERTIFICATE-----\nMIIDIzCCAgugAwIBAgIUZEO9mhldKaM+vYQlBxRzbx4NDOYwDQYJKoZIhvcNAQEL\nBQAwGTEXMBUGA1UEAwwOd2EtdGVzdC1jYS1hbHQwHhcNMjYwMTMxMTk1MTIwWhcN\nMjYwMzAyMTk1MTIwWjAZMRcwFQYDVQQDDA53YS10ZXN0LWNhLWFsdDCCASIwDQYJ\nKoZIhvcNAQEBBQADggEPADCCAQoCggEBAKfmzBFOOLB68UYCpAkvLuFebPm8vi5g\nFOAFTNA15bSOOHV1NAidEnvRxRr1BBbSeZDkiL3ucCaApMWZUfceOY+qkbiRSQdv\nLWRLt8b4UhuU/jV5wYbVrLaQ6+v6AneVMAHEdto3rcth/lZH/snRGzkReFF+uWG2\nat+GcyGHGQkpseK6bYaE/NgjawVqU4UdCf9OlgFHdrbKKjpnOwULv2t6THeqv36X\nm0G2m6aaFLG/23VWA/l0wKHP2slpBcLizZEwuQL4vY3SQYEI9Iw53tb8fh6hEANj\n9scTDoyW0AO/KSH8adPnX6KoJg6c2I7jkWXxbBlVXJtU9wfkd1D0RikCAwEAAaNj\nMGEwHQYDVR0OBBYEFBmwJJCWc0HPjfJkWiOq0/9038ySMB8GA1UdIwQYMBaAFBmw\nJJCWc0HPjfJkWiOq0/9038ySMA8GA1UdEwEB/wQFMAMBAf8wDgYDVR0PAQH/BAQD\nAgEGMA0GCSqGSIb3DQEBCwUAA4IBAQB0s7vQNAudWKupjWP97II5X31y8GUKKgAh\nQqoCl9OUhqTvmaWLSj1d4+8YSO6F34ZW0QNuHQZ/6gzuHIyLpaOUC2V/PMaFuC3O\nZJv3K/udxXsMH2otFo4iT0FFFUigFynXu/0//iD850/g6jHk8YMLeOGWZQkDKOae\nTlfh3IYE7kWZQUBUYPzuLZc4gYvPYVMdIfY8+5IPxOJxC7brFrViRMcbp4xW7Jfu\nkZz8vfzmY+hjQFgOsdcFVzQenRtTxr8eMdowJ++phHJs4gtQyEY15+zkYpg7B5iZ\nIX6nxMJcVfMJb4OPECWPjjwJTPSH8yiIOmw24/dbJZ4ZKjcpP3FH\n-----END CERTIFICATE-----\n";
    #[cfg(feature = "distributed")]
    const SERVER_CERT: &str = "-----BEGIN CERTIFICATE-----\nMIIDSjCCAjKgAwIBAgIUJCkA/YZgClbfb2uy8x2u/esjLQswDQYJKoZIhvcNAQEL\nBQAwFTETMBEGA1UEAwwKd2EtdGVzdC1jYTAeFw0yNjAxMzExOTUwNDFaFw0yNjAz\nMDIxOTUwNDFaMBQxEjAQBgNVBAMMCWxvY2FsaG9zdDCCASIwDQYJKoZIhvcNAQEB\nBQADggEPADCCAQoCggEBAJCazMUTdFnCMXolx/7uXzPMWX5CVxXTKL/tFuisXo3m\nPuxdT+gbaHOsDSwuOAm1jojUtQblCr1NSHNdvJoIMdOmZ2Z4wOexaqb+d25p6QcZ\n2yyILjmEWUhGu/OKT95rxH0t+rwidMnfh4MT7qkrE/ybjzaYuxH18qLIRAbKy/xp\nsrOO7loBCS3PUqrXwj9eDXqm7WzzN1PcqqVqGzEJCOJJVJGN4qW3F7xXrVZQ3UYo\n25Ve/W3w27qOF7szrGpdT3j6ZBeDuCkzVba1jbTfwDJ+azo5Hc4wtuFkb1izQItd\no+D3ChXP4kF1fxb7MLIHJ4ICpNNjsAeaWzY5wkEXskkCAwEAAaOBkjCBjzAMBgNV\nHRMBAf8EAjAAMA4GA1UdDwEB/wQEAwIFoDATBgNVHSUEDDAKBggrBgEFBQcDATAa\nBgNVHREEEzARgglsb2NhbGhvc3SHBH8AAAEwHQYDVR0OBBYEFHB089XTOjeLi+KX\niGzgJbz6vyUXMB8GA1UdIwQYMBaAFG4ipPg9GcAFqbQNGhRMWdOZPSujMA0GCSqG\nSIb3DQEBCwUAA4IBAQBRXt2g280K7U5bsLUO5rMhTgDw3OfaGul6FYCH0Cfah1jC\n/DlTQ+bWHnK+zz2Jqvh2zYw8wHEUGD+aCWIK2B9+9B6oOUAMIzWhQovIro11AAut\n8FKYpdNT32UWbWSv0hKU5H5HBetfM+7ZEA3ZAdGgblBvnW3h6LZfmCMgUAuzbsdq\n4WrgpDiNArSxLC+ZFdsNWfIztntg4IDRGnbpd59dnuL3sznB2ggXJq6MW9wnfbtu\njzteJfIE4m2SU7zlsZY6mDGLx8u7Hz22WfCrdhxq6vomYyrxlDJTNR1kudOcwwFB\nquZGgDxcDu64rrmVno3xYqfPMUeA8/NpwKYI2y2+\n-----END CERTIFICATE-----\n";
    #[cfg(feature = "distributed")]
    const SERVER_KEY: &str = "-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQCQmszFE3RZwjF6\nJcf+7l8zzFl+QlcV0yi/7RborF6N5j7sXU/oG2hzrA0sLjgJtY6I1LUG5Qq9TUhz\nXbyaCDHTpmdmeMDnsWqm/nduaekHGdssiC45hFlIRrvzik/ea8R9Lfq8InTJ34eD\nE+6pKxP8m482mLsR9fKiyEQGysv8abKzju5aAQktz1Kq18I/Xg16pu1s8zdT3Kql\nahsxCQjiSVSRjeKltxe8V61WUN1GKNuVXv1t8Nu6jhe7M6xqXU94+mQXg7gpM1W2\ntY2038Ayfms6OR3OMLbhZG9Ys0CLXaPg9woVz+JBdX8W+zCyByeCAqTTY7AHmls2\nOcJBF7JJAgMBAAECggEAHnAnODiPHjGtPnvjbDr62SljkRsfv51SD4w1bUaTJKVZ\ni2Fc54uVYfvOTgVwkEKiPRUhAdGGgDBbVsVdZMLi0h1N2JkEagDDZWFc/GXYwkDk\nDKyhpkPAk2EoQOxVQYlHs93Q0HckRDYEDUhNzVge/eY0sBZYEkDGERO8lf1sELZS\nAkgUNl+jwsGkpTuDXd87dN0cQ5DgORsj8LiCbCMSMyL/sFv58CUgiwzQyi6hQSTw\ngBvLe8snAf65B+M63WTs5UBoD5U52Lpr98jqdY/U+B0SRB0xluQfYeMegJkab+H8\nOy+/nWeih6gtWXvco+OlUAabPCOUpwaETxx4QIUjPQKBgQDBFYDnq22wHuW15kBS\nKoK9kXtYGxiJ+nAbtRYorres+fd6VFH9CBUslUDpHfiEZ4qI1FBRhrx0mMDHs/hS\nQdCnUhZaDAOjmNLwNImPwZM9YEVRDwWlmzy/0/l4O/HM+1Rs2dakASoH+/+PDrLZ\nFd0+RawX34drfILHWeZsS2p/twKBgQC/uUulbrjeWVuHcp7QBC5VAyihWdmRTzEx\nNSruxFrHqq/P5WOkN5C4upOt/QJYBSietXjT4i6w26jrxQOXdetZoc9JRTVqbh1R\nJapFWb/HsFreps2+O7eqtPa21aad37a+WHbX0QBXBxN0ACtHafqkOgUY3KYCd7JI\n6fzoMUtd/wKBgEKGWid31Q79Vj/Z2Qd2Rh1yZoDwtP+1HbMuLThPGlGqvi2Tp7v6\ncPEva3HmNZ3I3t5N6G5ucbfqeWFVDJWqv20mxzS3NvnCycqhD1RMaaKX7MoE1vk8\nBy5Apo9ad/EcFvZ6B43yKL0fgemUMuLAub2e27BN/6Z0+8obm1xsj4D5AoGBALyf\nc4IN3cm7xiYLKZ3kDyVKV0XvHPMuI2qTMWr5OYrpLdFukEp29GYaAcMSgaTRZnZG\nedqT03Xill1nVjJELEjhvgsLERNlxGgak1tpghnXMn+NQivfmsJTCcs1hZgbCjJY\n3ItVr2zvpD7jD7FR3eqGvo8IPjd9RaUgt9ZE8S5HAoGALZDIV3SPPBPAY0ihfYWa\nJvqq4q+r44NMxk3yksr6yypuX3oZZM6HDERlRvhARYhIA+LIY5uK9tlZRsBmL7Ka\nVbhuUjmV7CF3lfyni4cvVM3D8fv05gSc5v4fnhrzAI2WZ53Vr/6f8k5avXYEocjn\nkxlgLg6xndsSmoukN3i0FrI=\n-----END PRIVATE KEY-----\n";
    #[cfg(feature = "distributed")]
    const CLIENT_CERT: &str = "-----BEGIN CERTIFICATE-----\nMIIDLDCCAhSgAwIBAgIUJCkA/YZgClbfb2uy8x2u/esjLQwwDQYJKoZIhvcNAQEL\nBQAwFTETMBEGA1UEAwwKd2EtdGVzdC1jYTAeFw0yNjAxMzExOTUwNDFaFw0yNjAz\nMDIxOTUwNDFaMBQxEjAQBgNVBAMMCXdhLWNsaWVudDCCASIwDQYJKoZIhvcNAQEB\nBQADggEPADCCAQoCggEBAKgARf2gerf4yMQqHoZ0YfaRbYTjL6HEoyC3ZHrMLmLx\nUsHt7ELB/KiX+mYLQ7J+JW+ZYyOBETq9vqBZCT8+pGc/8c2KuUasVldzTpU7JneT\ny6x0Pld9TvoXZVqFDHA+O4yqwsmPWqm57XWTcTFjLyrWaEAdTSD0NdsxStlv2xgN\nbjelUl/1CNhYGeOVmYNZnz0tx4KGdO85LkafDltc3C55tTe3U0yitKS14GrKe/Xz\no0VGB5htkxQbGSMhVSmt5VnpheERiQ+mLDc9U2KlJ2euSDVvmFiMZ3w9ehshL1xp\n6H6P3cxX9ocEVritzLczV7aBkepLnCCNpqS5cqIBiQ8CAwEAAaN1MHMwDAYDVR0T\nAQH/BAIwADAOBgNVHQ8BAf8EBAMCBaAwEwYDVR0lBAwwCgYIKwYBBQUHAwIwHQYD\nVR0OBBYEFJhYZvekIWexWSegWXOIguWJmS2WMB8GA1UdIwQYMBaAFG4ipPg9GcAF\nqbQNGhRMWdOZPSujMA0GCSqGSIb3DQEBCwUAA4IBAQB8++cVKFRc7vz/dEL4qQGA\n9m4Ss06Mw+e2x7Ns4bc0HjxJSe/2XeARUmFTJknwJA9e3+tLz9a3M1turL5PZTCA\n3+NnNZUeFChsMIV07xa60KdFbd6lkV+Z8y2gw365j4twJLoibw6Rkfd9P+tGJT4w\nNDKmVotOPBbCCaiUANX7TVUxrB9FL+h044fNj3x8R5mFy06D3HxOErbSTJalnPd9\nfJDMZD6lVqm8tskKFbCSQ0clgrlOEv6gsL9cHsjwlyLAJs17BE4PT3cvZKlHZ5Ai\nX0B5sDGWLSmhKl+9eECJt0trrjuT/NOr4UsiN6StyMJwnaC7Bucy+o+iO5Z8cOl6\n-----END CERTIFICATE-----\n";
    #[cfg(feature = "distributed")]
    const CLIENT_KEY: &str = "-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQCoAEX9oHq3+MjE\nKh6GdGH2kW2E4y+hxKMgt2R6zC5i8VLB7exCwfyol/pmC0OyfiVvmWMjgRE6vb6g\nWQk/PqRnP/HNirlGrFZXc06VOyZ3k8usdD5XfU76F2VahQxwPjuMqsLJj1qpue11\nk3ExYy8q1mhAHU0g9DXbMUrZb9sYDW43pVJf9QjYWBnjlZmDWZ89LceChnTvOS5G\nnw5bXNwuebU3t1NMorSkteBqynv186NFRgeYbZMUGxkjIVUpreVZ6YXhEYkPpiw3\nPVNipSdnrkg1b5hYjGd8PXobIS9caeh+j93MV/aHBFa4rcy3M1e2gZHqS5wgjaak\nuXKiAYkPAgMBAAECggEAERQ6CU8zupk1m8+mW8fgH6doKV7JPFpXtR8/vUYdnxxm\na+Wqo5zB+Ue+Anq5rp8pYh+HVxgrbrvUccurZ30QTJjRFbK5JCin/Grx/bTOM9DY\nH1eP8OgBy+Xt/VZSTeTdu+6uL7x9nIyUyeOr2bf6FxJF9eKksSlygi6QK+u1q8uj\njY0l2HG18BQLDgvfsTa92aSPVTiJ/gnK3/SmPt60TFUjtSPJ4Yzhx++5sijuUq9L\nNe3yDXefBJjj4y8Xdx0grnXjHh6wI96pdBWd+uuQpt7GQGz3ApQwugzYBaVMEKa6\nEc2dSYqzxUXB1JgLhBc8PaqEQgwk5RQdcTsgcL2sCQKBgQDV8uD780Y/4WgaWp3W\nkoYa90ehJtjEgTN/PIPT04ictqxzEpYRj8s0LrKCsvzO5bGOk73UC9h6jyKh0rLy\nwEE7ISn4pijh62dm8EkHGN9OvzH1eUEBkwwY7s693ivOfxxNPDhc2Zf3AHhPg5mS\nsgE5SU4SiRm9qWjW2CrepLLAuQKBgQDJBXmRhGNh5nk5dK7EEiR0VN5esjbazvlp\nHhETs86rg8/K9lRhDzZ5Je/wCoGY3gOlVQUtGOZ1jgXga5QcbwzODHZBPxDpSUsm\nYmfRO9ySRJEbG8+gYDUyA24UTm3eNKE1akbJKQFOlX4sHoxREcoI394kPEXoyvwP\n70U4VYZkBwKBgCErzAgkOsMSvqI/ZHNtOk+aAUgSDs/AvGxAxKumA2tQw0IAIrZM\nVhQcHV84QwwM/s99RpRG1eSCprryQP50Imj5hllf4bzNU7XZEWmBSLYb3LITf6mv\n09NVy0YS2TXl7UxoRtDWh8IrF3w0ii39XUU1gV5MVWpbhr6wu0zTukc5AoGBAIZg\n1I2ENHNjgDH6YEHN5vSlLymadLT8mxm78ap8DnH1YVjKJknjw4Rk6epK+6tW7pT9\nKsKk3JpE4ITPJWmEisjK59ph8Eoipsv4CHKEU8SrdVzr0HXjGmxegp2seCGMiR+N\n9dfPQ4JmyLtxiFdBTw9zp6oNaKZf2vRD/L/V3ErNAoGAfIbZxO9HAKxhx1IdtzmF\nnYq5UBDjz+dMD2O0CYOpkm6qQGtObEL0u+mkHn7QU1ojatI2XHV2yqei/eJZ3yHr\n0AdZ9rdtgqH7q1gU6GMjj/97me5SVmW+kMizR0PGf3aj5+3FDSzf1DiYshHEL3hd\nq7BEO+XYA2PpWEpAroXhMbQ=\n-----END PRIVATE KEY-----\n";

    #[cfg(feature = "distributed")]
    fn temp_pem(contents: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().expect("temp file");
        std::io::Write::write_all(file.as_file_mut(), contents.as_bytes()).expect("write pem");
        file
    }

    #[cfg(feature = "distributed")]
    #[tokio::test]
    async fn tls_handshake_succeeds() {
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

        let acceptor = TlsAcceptor::from(server_config);
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let mut tls_stream = acceptor.accept(stream).await.expect("accept tls");
            let mut buf = [0u8; 4];
            tls_stream.read_exact(&mut buf).await.expect("read");
            buf
        });

        let connector = TlsConnector::from(client_config);
        let server_name = ServerName::try_from("localhost").expect("server name");
        let mut stream = connector
            .connect(
                server_name,
                tokio::net::TcpStream::connect(addr).await.expect("connect"),
            )
            .await
            .expect("tls connect");
        stream.write_all(b"ping").await.expect("write");

        let received = server_task.await.expect("join");
        assert_eq!(&received, b"ping");
    }

    #[cfg(feature = "distributed")]
    #[tokio::test]
    async fn mtls_handshake_succeeds() {
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

        let acceptor = TlsAcceptor::from(server_tls);
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let mut tls_stream = acceptor.accept(stream).await.expect("accept tls");
            let mut buf = [0u8; 2];
            tls_stream.read_exact(&mut buf).await.expect("read");
            buf
        });

        let connector = TlsConnector::from(client_tls);
        let server_name = ServerName::try_from("localhost").expect("server name");
        let mut stream = connector
            .connect(
                server_name,
                tokio::net::TcpStream::connect(addr).await.expect("connect"),
            )
            .await
            .expect("tls connect");
        stream.write_all(b"ok").await.expect("write");

        let received = server_task.await.expect("join");
        assert_eq!(&received, b"ok");
    }

    #[cfg(feature = "distributed")]
    #[tokio::test]
    async fn tls_handshake_rejects_untrusted_server() {
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

        let acceptor = TlsAcceptor::from(server_config);
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            acceptor.accept(stream).await
        });

        let connector = TlsConnector::from(client_config);
        let server_name = ServerName::try_from("localhost").expect("server name");
        let client_result = connector
            .connect(
                server_name,
                tokio::net::TcpStream::connect(addr).await.expect("connect"),
            )
            .await;

        let server_result = timeout(Duration::from_secs(2), server_task)
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
    }

    #[cfg(feature = "distributed")]
    #[tokio::test]
    async fn mtls_handshake_rejects_missing_client_cert() {
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

        let acceptor = TlsAcceptor::from(server_tls);
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            acceptor.accept(stream).await
        });

        let connector = TlsConnector::from(client_tls);
        let server_name = ServerName::try_from("localhost").expect("server name");
        let client_result = connector
            .connect(
                server_name,
                tokio::net::TcpStream::connect(addr).await.expect("connect"),
            )
            .await;

        let server_result = timeout(Duration::from_secs(2), server_task)
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
    }

    #[cfg(feature = "distributed")]
    #[tokio::test]
    async fn mtls_handshake_rejects_disallowed_client() {
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

        let acceptor = TlsAcceptor::from(server_tls);
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            acceptor.accept(stream).await
        });

        let connector = TlsConnector::from(client_tls);
        let server_name = ServerName::try_from("localhost").expect("server name");
        let client_result = connector
            .connect(
                server_name,
                tokio::net::TcpStream::connect(addr).await.expect("connect"),
            )
            .await;

        let server_result = timeout(Duration::from_secs(2), server_task)
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
    }

    #[cfg(feature = "distributed")]
    #[tokio::test]
    async fn tls_rejects_plaintext_client() {
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

        let acceptor = TlsAcceptor::from(server_config);
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            acceptor.accept(stream).await
        });

        let mut client = tokio::net::TcpStream::connect(addr).await.expect("connect");
        client.write_all(b"not tls").await.expect("write");
        let _ = client.shutdown().await;

        let server_result = timeout(Duration::from_secs(2), server_task)
            .await
            .expect("server timeout")
            .expect("join");
        assert!(server_result.is_err());
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
            expected in "[a-zA-Z0-9_-]{1,24}",
            presented in "[a-zA-Z0-9_-]{1,24}"
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
    fn readiness_report_serde_roundtrip() {
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
        config.token_env = Some("WA_DIST_TOKEN".to_string());

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
}
