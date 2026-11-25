//! Opt-in operator sessions for the web viewer's loopback listener.
//!
//! An explicit HTTPS origin allows this same operator authority through a
//! TLS-terminating proxy that preserves Host. Forwarded headers never grant
//! access. This is not a multi-user sharing or pane grant system. FastAPI's
//! session middleware uses different request/handler
//! types; its generic store also cannot notify an already-open SSE body of
//! revocation. Keep this small store bounded and use the existing runtime's
//! lifecycle/timer primitives rather than introducing another web runtime.

use super::error::{json_err, json_ok};
use crate::web_framework::{
    BoxFuture, ControlFlow, FrameworkRequestBody, Method, Middleware, PreBodyValidator, Request,
    RequestContext, Response, StatusCode, WebStreamLifecycle,
};
use hmac::{Hmac, KeyInit, Mac};
use rand::{TryRng, rngs::SysRng};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::future::{Future, poll_fn};
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::Poll;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use zeroize::{ZeroizeOnDrop, Zeroizing};

const COOKIE_NAME: &str = "ft_viewer_session";
// Browsers only accept a `__Host-` cookie that is Secure, host-only and
// Path=/. Other hosts under the same site, such as tailnet peers, cannot plant
// one. Without the prefix, a planted Domain cookie turns into a duplicate that
// makes every route refuse with 401, which the server cannot clear.
const HTTPS_COOKIE_NAME: &str = "__Host-ft_viewer_session";
const LOGIN_BODY_LIMIT: usize = 1024;
const CREDENTIAL_DOMAIN: &[u8] = b"frankenterm.local-web-bootstrap.v1";
// Both SSE routes share these budgets. Eight streams allow four viewer tabs
// (events + deltas each) without letting one session occupy the whole server.
// Legacy loopback mode is one local operator and shares the same viewer cap.
const MAX_SERVER_STREAMS: usize = 64;
const MAX_VIEWER_STREAMS: usize = 8;
type CredentialMac = Hmac<Sha256>;

/// Finite configuration failures that never include a credential.
#[derive(Debug, thiserror::Error)]
pub enum WebSessionAuthError {
    #[error("web bootstrap token must contain 32 to 256 visible ASCII characters")]
    InvalidCredential,
    #[error("web session lifetime must be between one second and 24 hours")]
    InvalidLifetime,
    #[error("web session capacity must be between 1 and 256")]
    InvalidCapacity,
    #[error(
        "web HTTPS origin must be one canonical HTTPS origin without credentials, a path, query or fragment"
    )]
    InvalidHttpsOrigin,
    #[error("secure randomness for web sessions is unavailable")]
    EntropyUnavailable,
}

struct CredentialVerifier {
    key: Zeroizing<[u8; 32]>,
    tag: Zeroizing<[u8; 32]>,
}

/// Shared, bounded, process-local session authority. Restart revokes all
/// sessions. The bootstrap credential is retained only as a keyed digest.
#[derive(Clone)]
pub struct WebSessionAuth {
    verifier: Arc<CredentialVerifier>,
    sessions: Arc<Mutex<HashMap<[u8; 32], SessionGrant>>>,
    session_ttl: Duration,
    max_sessions: usize,
    https_origin: Option<String>,
}

impl std::fmt::Debug for WebSessionAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSessionAuth")
            .field("session_ttl", &self.session_ttl)
            .field("max_sessions", &self.max_sessions)
            .field("https_origin", &self.https_origin)
            .finish_non_exhaustive()
    }
}

impl WebSessionAuth {
    /// Create local operator authentication with a one-hour session lifetime
    /// and at most sixteen simultaneous sessions. Supply a randomly generated
    /// token through an environment variable, never a URL or command argument.
    pub fn new(credential: &str) -> Result<Self, WebSessionAuthError> {
        if !valid_credential(credential) {
            return Err(WebSessionAuthError::InvalidCredential);
        }
        let mut key = Zeroizing::new([0_u8; 32]);
        SysRng
            .try_fill_bytes(key.as_mut())
            .map_err(|_| WebSessionAuthError::EntropyUnavailable)?;
        let mut mac = CredentialMac::new_from_slice(key.as_ref())
            .map_err(|_| WebSessionAuthError::EntropyUnavailable)?;
        mac.update(CREDENTIAL_DOMAIN);
        mac.update(credential.as_bytes());
        let tag = Zeroizing::new(mac.finalize().into_bytes().into());
        Ok(Self {
            verifier: Arc::new(CredentialVerifier { key, tag }),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            session_ttl: Duration::from_secs(3600),
            max_sessions: 16,
            https_origin: None,
        })
    }

    /// Admit this operator's viewer through one explicitly configured HTTPS
    /// origin, for example `https://workstation.example.ts.net` behind
    /// Tailscale Serve. The proxy must terminate TLS and preserve Host while
    /// forwarding to the loopback listener. No forwarded or identity header
    /// establishes authority, and the loopback browser origin is not also
    /// admitted in this mode. Issued and cleared cookies require HTTPS.
    pub fn with_https_origin(mut self, origin: &str) -> Result<Self, WebSessionAuthError> {
        if origin.len() > 256
            || !origin.is_ascii()
            || origin
                .bytes()
                .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
        {
            return Err(WebSessionAuthError::InvalidHttpsOrigin);
        }
        let parsed =
            url::Url::parse(origin).map_err(|_| WebSessionAuthError::InvalidHttpsOrigin)?;
        if parsed.scheme() != "https"
            || parsed.host().is_none()
            || parsed
                .host_str()
                .is_some_and(|host| host.ends_with('.') || host.contains('*'))
            || parsed.port_or_known_default() == Some(0)
            || parsed.origin().ascii_serialization() != origin
        {
            return Err(WebSessionAuthError::InvalidHttpsOrigin);
        }
        self.https_origin = Some(origin.to_owned());
        Ok(self)
    }

    pub(super) fn https_origin(&self) -> Option<&str> {
        self.https_origin.as_deref()
    }

    fn cookie_name(&self) -> &'static str {
        if self.https_origin.is_some() {
            HTTPS_COOKIE_NAME
        } else {
            COOKIE_NAME
        }
    }

    /// Override the lifetime of newly issued sessions. Existing grants retain
    /// their original monotonic deadline.
    pub fn with_session_ttl(mut self, ttl: Duration) -> Result<Self, WebSessionAuthError> {
        if !(Duration::from_secs(1)..=Duration::from_secs(86_400)).contains(&ttl) {
            return Err(WebSessionAuthError::InvalidLifetime);
        }
        self.session_ttl = ttl;
        Ok(self)
    }

    /// Override the maximum number of resident sessions. At capacity login is
    /// refused; an unrelated active viewer is never silently evicted.
    pub fn with_max_sessions(mut self, capacity: usize) -> Result<Self, WebSessionAuthError> {
        if !(1..=256).contains(&capacity) {
            return Err(WebSessionAuthError::InvalidCapacity);
        }
        self.max_sessions = capacity;
        Ok(self)
    }

    fn verifies(&self, credential: &str) -> bool {
        if !valid_credential(credential) {
            return false;
        }
        let Ok(mut mac) = CredentialMac::new_from_slice(self.verifier.key.as_ref()) else {
            return false;
        };
        mac.update(CREDENTIAL_DOMAIN);
        mac.update(credential.as_bytes());
        // The crypto library compares the digest in constant time.
        mac.verify_slice(self.verifier.tag.as_ref()).is_ok()
    }

    fn lookup(&self, digest: Option<[u8; 32]>) -> Result<Option<SessionGrant>, Response> {
        let mut sessions = self.sessions.lock().map_err(|_| unavailable())?;
        sessions.retain(|_, session| {
            if session.is_valid() {
                true
            } else {
                session.revoke();
                false
            }
        });
        Ok(digest.and_then(|digest| sessions.get(&digest).cloned()))
    }

    fn issue(
        &self,
        replace: Option<[u8; 32]>,
        origin: String,
    ) -> Result<(Zeroizing<String>, SessionGrant), Response> {
        let mut bytes = Zeroizing::new([0_u8; 48]);
        SysRng
            .try_fill_bytes(bytes.as_mut())
            .map_err(|_| unavailable())?;
        let token = Zeroizing::new(hex::encode(&bytes[..32]));
        // This independent random epoch identifies viewer state, never a
        // credential. Other tabs can recognize rotation without learning the
        // HttpOnly token. It differs across process restarts as well.
        let session_epoch = hex::encode(&bytes[32..]);
        let digest = session_digest(token.as_str());
        let now = Instant::now();
        let deadline = now.checked_add(self.session_ttl).ok_or_else(unavailable)?;
        // This timestamp is only a UI hint. Authorization uses Instant, so
        // wall-clock adjustment cannot extend a session's lifetime.
        let expires_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|elapsed| elapsed.checked_add(self.session_ttl))
            .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok())
            .ok_or_else(unavailable)?;
        let grant = SessionGrant(Arc::new(SessionLease {
            deadline,
            expires_at_ms,
            revoked: AtomicBool::new(false),
            lifecycle: WebStreamLifecycle::new(),
            authority: Arc::downgrade(&self.sessions),
            origin,
            session_epoch,
            streams: Arc::new(StreamCapacity::new(MAX_VIEWER_STREAMS)),
        }));
        let mut sessions = self.sessions.lock().map_err(|_| unavailable())?;
        sessions.retain(|_, session| {
            if session.is_valid() {
                true
            } else {
                session.revoke();
                false
            }
        });
        if sessions.contains_key(&digest) {
            return Err(unavailable());
        }
        let replacing = replace.is_some_and(|old| sessions.contains_key(&old));
        if sessions.len() >= self.max_sessions && !replacing {
            return Err(json_err(
                StatusCode::TOO_MANY_REQUESTS,
                "web_session_capacity",
                "All local viewer session slots are in use; sign out another viewer or wait for expiry",
            ));
        }
        if let Some(old) = replace.and_then(|old| sessions.remove(&old)) {
            old.revoke();
        }
        sessions.insert(digest, grant.clone());
        Ok((token, grant))
    }

    fn revoke(&self, digest: Option<[u8; 32]>) -> Result<(), Response> {
        let mut sessions = self.sessions.lock().map_err(|_| unavailable())?;
        if let Some(session) = digest.and_then(|digest| sessions.remove(&digest)) {
            session.revoke();
        }
        Ok(())
    }
}

fn valid_credential(credential: &str) -> bool {
    (32..=256).contains(&credential.len()) && credential.bytes().all(|byte| byte.is_ascii_graphic())
}

fn session_digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn unavailable() -> Response {
    json_err(
        StatusCode::SERVICE_UNAVAILABLE,
        "web_session_unavailable",
        "Local viewer session authority is unavailable",
    )
}

fn unauthorized() -> Response {
    json_err(
        StatusCode::UNAUTHORIZED,
        "web_session_required",
        "Sign in to the local viewer",
    )
}

struct SessionLease {
    deadline: Instant,
    expires_at_ms: u64,
    revoked: AtomicBool,
    lifecycle: WebStreamLifecycle,
    authority: Weak<Mutex<HashMap<[u8; 32], SessionGrant>>>,
    origin: String,
    session_epoch: String,
    streams: Arc<StreamCapacity>,
}

/// An admitted request retains this exact lease, never a newly issued session
/// that happens to reuse its browser's cookie name.
#[derive(Clone)]
pub(super) struct SessionGrant(Arc<SessionLease>);

impl SessionGrant {
    fn is_valid(&self) -> bool {
        !self.0.revoked.load(Ordering::Acquire)
            && Instant::now() < self.0.deadline
            && self
                .0
                .authority
                .upgrade()
                .is_some_and(|store| !store.is_poisoned())
    }

    fn revoke(&self) {
        self.0.revoked.store(true, Ordering::Release);
        self.0.lifecycle.signal_shutdown();
    }
}

impl std::fmt::Debug for SessionGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionGrant").finish_non_exhaustive()
    }
}

/// Non-waiting admission: a full stream budget never parks a request or creates
/// another producer. The response body owns each reservation until its queued
/// output and producer are disposed, including unpolled-body cancellation.
#[derive(Debug)]
struct StreamCapacity {
    active: AtomicUsize,
    limit: usize,
}

impl StreamCapacity {
    fn new(limit: usize) -> Self {
        Self {
            active: AtomicUsize::new(0),
            limit,
        }
    }

    fn try_acquire(self: &Arc<Self>) -> Option<StreamReservation> {
        self.active
            .try_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < self.limit).then(|| active + 1)
            })
            .ok()
            .map(|_| StreamReservation(Arc::clone(self)))
    }
}

#[derive(Debug)]
struct StreamReservation(Arc<StreamCapacity>);

impl Drop for StreamReservation {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Intentionally not Clone: one response owns one server slot and one viewer
/// slot. Releasing either on admission failure rolls back the other as well.
#[derive(Debug)]
pub(super) struct StreamPermit {
    _server: StreamReservation,
    _viewer: StreamReservation,
}

fn stream_capacity_response(server_full: bool) -> Response {
    let (status, code, message) = if server_full {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "web_stream_capacity",
            "All viewer stream slots are in use; retry after a stream closes",
        )
    } else {
        (
            StatusCode::TOO_MANY_REQUESTS,
            "web_viewer_stream_capacity",
            "Too many streams for this viewer; close a stream and retry",
        )
    };
    private_response(json_err(status, code, message)).header("retry-after", b"1".to_vec())
}

pub(super) fn reserve_stream(req: &Request) -> Result<StreamPermit, Response> {
    let admission = req
        .get_extension::<SessionAuthMiddleware>()
        .ok_or_else(|| {
            json_err(
                StatusCode::SERVICE_UNAVAILABLE,
                "stream_admission_unavailable",
                "Stream admission is unavailable",
            )
        })?;
    admission.reserve_stream(req.get_extension::<SessionGrant>())
}

/// Race the server's stop signal with revocation and the monotonic session
/// deadline. SseByteStream polls this before production and before dequeue,
/// dropping both its producer and queued output when either authority ends.
pub(super) fn stream_shutdown(
    mut shutdown: Pin<Box<dyn Future<Output = ()> + Send>>,
    grant: Option<SessionGrant>,
    cx: &crate::cx::Cx,
) -> Pin<Box<dyn Future<Output = ()> + Send>> {
    let Some(grant) = grant else {
        return shutdown;
    };
    let waiter = grant.0.lifecycle.shutdown_waiter(cx);
    let deadline = grant.0.deadline;
    let cx = cx.clone();
    let mut ended = Box::pin(async move {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let _ = crate::runtime_async::timeout_with_cx(&cx, remaining, waiter).await;
    });
    Box::pin(poll_fn(move |task| {
        if !grant.is_valid()
            || shutdown.as_mut().poll(task).is_ready()
            || ended.as_mut().poll(task).is_ready()
        {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }))
}

#[derive(Serialize)]
struct SessionStatus {
    required: bool,
    authenticated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_epoch: Option<String>,
}

fn status(auth: Option<&WebSessionAuth>, grant: Option<&SessionGrant>) -> Response {
    let grant = grant.filter(|grant| grant.is_valid());
    json_ok(SessionStatus {
        required: auth.is_some(),
        authenticated: auth.is_none() || grant.is_some_and(SessionGrant::is_valid),
        expires_at_ms: grant.map(|grant| grant.0.expires_at_ms),
        session_epoch: grant.map(|grant| grant.0.session_epoch.clone()),
    })
}

/// Authentication defaults to deny for every route except the deliberately
/// public status, login and verified static assets. Generated OpenAPI routes
/// pass through this same middleware in the pinned framework.
#[derive(Clone)]
pub struct SessionAuthMiddleware {
    auth: Option<WebSessionAuth>,
    public_assets: HashSet<String>,
    listener_port: Arc<AtomicU16>,
    streams: Arc<StreamCapacity>,
    anonymous_streams: Arc<StreamCapacity>,
}

impl SessionAuthMiddleware {
    pub(super) fn new(auth: Option<WebSessionAuth>, assets: impl Iterator<Item = String>) -> Self {
        Self {
            auth,
            public_assets: assets.collect(),
            listener_port: Arc::new(AtomicU16::new(0)),
            streams: Arc::new(StreamCapacity::new(MAX_SERVER_STREAMS)),
            anonymous_streams: Arc::new(StreamCapacity::new(MAX_VIEWER_STREAMS)),
        }
    }

    pub(super) fn reserve_stream(
        &self,
        session: Option<&SessionGrant>,
    ) -> Result<StreamPermit, Response> {
        let viewer = if let Some(auth) = self.auth.as_ref() {
            let grant = session.filter(|grant| {
                grant.is_valid()
                    && grant
                        .0
                        .authority
                        .upgrade()
                        .is_some_and(|authority| Arc::ptr_eq(&authority, &auth.sessions))
            });
            &grant.ok_or_else(unauthorized)?.0.streams
        } else {
            &self.anonymous_streams
        };
        let viewer = viewer
            .try_acquire()
            .ok_or_else(|| stream_capacity_response(false))?;
        let server = self
            .streams
            .try_acquire()
            .ok_or_else(|| stream_capacity_response(true))?;
        // Logout/expiry may race the reservations. A revoked lease cannot
        // acquire a fresh producer; the body also checks revocation before
        // producing and before exposing any queued output.
        if self.auth.is_some() && session.is_none_or(|grant| !grant.is_valid()) {
            return Err(unauthorized());
        }
        Ok(StreamPermit {
            _server: server,
            _viewer: viewer,
        })
    }

    /// Set from the actual validated listener before it accepts requests.
    /// An unbound authenticated application cannot admit requests.
    pub(crate) fn bind_listener(&self, address: SocketAddr) {
        if address.ip().is_loopback() {
            self.listener_port.store(address.port(), Ordering::Release);
        }
    }

    fn public_read(&self, req: &Request) -> bool {
        matches!(req.method(), Method::Get | Method::Head)
            && (matches!(req.path(), "/health" | "/auth/status")
                || self.public_assets.contains(req.path()))
    }

    fn login(req: &Request) -> bool {
        req.method() == Method::Post && req.path() == "/auth/session"
    }

    fn check_request(&self, req: &Request) -> Result<Option<SessionGrant>, Response> {
        let Some(auth) = self.auth.as_ref() else {
            check_unauthenticated_boundary(req)?;
            return Ok(None);
        };
        let origin = check_browser_boundary_for_route(
            req,
            self.listener_port.load(Ordering::Acquire),
            self.public_read(req),
            auth.https_origin.as_deref(),
        )?;
        if Self::login(req) {
            check_login_body_size(req)?;
        }
        let digest = cookie_digest(req, auth.cookie_name())?;
        let grant = auth
            .lookup(digest)?
            .filter(|grant| grant.0.origin == origin);
        if grant.is_none() && !self.public_read(req) && !Self::login(req) {
            return Err(unauthorized());
        }
        if let Some(grant) = grant.as_ref() {
            check_session_epoch(req, grant)?;
        }
        Ok(grant)
    }

    fn admit(&self, req: &mut Request) -> Result<(), Response> {
        let grant = self.check_request(req)?;
        req.insert_extension(self.clone());
        if let Some(grant) = grant {
            req.insert_extension(grant);
        }
        Ok(())
    }

    fn finish(&self, req: &Request, response: Response) -> Response {
        // A storage query can outlive logout or expiry. Never hand its result
        // to the transport based only on the admission-time decision.
        let response = if !self.public_read(req)
            && !Self::login(req)
            && !(req.method() == Method::Delete && req.path() == "/auth/session")
            && req
                .get_extension::<SessionGrant>()
                .is_some_and(|grant| !grant.is_valid())
        {
            unauthorized()
        } else {
            response
        };
        private_response(response)
    }
}

impl PreBodyValidator for SessionAuthMiddleware {
    fn validate(&self, req: &Request) -> Result<(), Response> {
        self.check_request(req)
            .map(|_| ())
            .map_err(|response| private_response(response).header("connection", b"close".to_vec()))
    }

    fn name(&self) -> &'static str {
        "SessionAuthMiddleware"
    }
}

impl Middleware for SessionAuthMiddleware {
    fn before<'a>(
        &'a self,
        _ctx: &'a RequestContext,
        req: &'a mut Request,
    ) -> BoxFuture<'a, ControlFlow> {
        let outcome = match self.admit(req) {
            Ok(()) => ControlFlow::Continue,
            Err(response) => ControlFlow::Break(private_response(response)),
        };
        Box::pin(async move { outcome })
    }

    fn after<'a>(
        &'a self,
        _ctx: &'a RequestContext,
        req: &'a Request,
        response: Response,
    ) -> BoxFuture<'a, Response> {
        let response = self.finish(req, response);
        Box::pin(async move { response })
    }

    fn name(&self) -> &'static str {
        "SessionAuthMiddleware"
    }
}

fn private_response(response: Response) -> Response {
    response
        .header("cache-control", b"no-store".to_vec())
        .header("referrer-policy", b"no-referrer".to_vec())
}

fn boundary_error() -> Response {
    json_err(
        StatusCode::FORBIDDEN,
        "web_origin_refused",
        "Use the viewer's configured origin",
    )
}

fn check_session_epoch(req: &Request, grant: &SessionGrant) -> Result<(), Response> {
    let expected = grant.0.session_epoch.as_bytes();
    if req
        .headers()
        .get("x-ft-session-epoch")
        .is_some_and(|epoch| epoch != expected)
    {
        return Err(unauthorized());
    }
    if matches!(req.path(), "/stream/events" | "/stream/deltas") {
        // Native EventSource reconnects with the browser's current cookie.
        // Bind its URL to the viewer that opened it so a newly issued cookie
        // cannot silently revive an old tab's cursor and terminal model.
        // The epoch is independent random metadata, never a credential.
        let mut epochs = url::form_urlencoded::parse(req.query().unwrap_or("").as_bytes())
            .filter(|(name, _)| name == "session_epoch");
        let matches = epochs
            .next()
            .is_some_and(|(_, epoch)| epoch.as_bytes() == expected);
        if !matches || epochs.next().is_some() {
            return Err(unauthorized());
        }
    }
    Ok(())
}

fn check_browser_boundary(
    req: &Request,
    listener_port: u16,
    https_origin: Option<&str>,
) -> Result<String, Response> {
    check_browser_boundary_for_route(req, listener_port, false, https_origin)
}

/// The request's Host header, if it is one plausible authority.
fn request_host(req: &Request) -> Result<&str, Response> {
    req.headers()
        .get("host")
        .and_then(|value| std::str::from_utf8(value).ok())
        .filter(|host| {
            !host.is_empty()
                && host.len() <= 256
                && !host.bytes().any(|byte| {
                    byte.is_ascii_whitespace()
                        || byte.is_ascii_control()
                        || matches!(byte, b'/' | b'\\' | b'@' | b',' | b'#' | b'?')
                })
        })
        .ok_or_else(boundary_error)
}

/// `http://{host}` and its effective port, if `host` is a loopback IP literal
/// or exactly `localhost` in canonical syntax: no shorthand integer or octal
/// IP authority, trailing dot, empty port, userinfo or URL parser
/// normalization.
fn canonical_loopback_origin(host: &str) -> Option<(String, u16)> {
    let origin = format!("http://{host}");
    let parsed = url::Url::parse(&origin).ok()?;
    let loopback = match parsed.host() {
        Some(url::Host::Domain(name)) => name == "localhost",
        Some(url::Host::Ipv4(ip)) => IpAddr::V4(ip).is_loopback(),
        Some(url::Host::Ipv6(ip)) => IpAddr::V6(ip).is_loopback(),
        None => false,
    };
    if !loopback || parsed.origin().ascii_serialization() != origin {
        return None;
    }
    let port = parsed.port_or_known_default()?;
    Some((origin, port))
}

/// Fetch Metadata: refuse a request a browser marks as coming from another
/// site, except a top-level document navigation where the caller allows one.
fn check_fetch_site(req: &Request, navigation_allowed: bool) -> Result<(), Response> {
    if let Some(site) = req.headers().get("sec-fetch-site")
        && site != b"same-origin"
        && site != b"none"
        && !(navigation_allowed
            && req.method() == Method::Get
            && req.headers().get("sec-fetch-mode") == Some(b"navigate".as_slice())
            && req.headers().get("sec-fetch-dest") == Some(b"document".as_slice()))
    {
        return Err(boundary_error());
    }
    Ok(())
}

/// Unauthenticated mode has no credential, so Host is its defence against
/// DNS rebinding: a page whose hostname later resolves to 127.0.0.1 reaches
/// the loopback listener as same-origin, and names itself in Host. Admit
/// only a loopback IP literal or exactly `localhost`. Any port is accepted,
/// so an SSH `-L` tunnel to a different local port keeps working; the
/// listener itself is loopback-only. A presented Origin must name the same
/// host. Cross-site subresource requests cannot read a response, but they
/// could hold the anonymous stream budget, so Fetch Metadata refuses them.
fn check_unauthenticated_boundary(req: &Request) -> Result<(), Response> {
    let host = request_host(req)?;
    let (origin, _port) = canonical_loopback_origin(host).ok_or_else(boundary_error)?;
    if req
        .headers()
        .get("origin")
        .is_some_and(|presented| presented != origin.as_bytes())
    {
        return Err(boundary_error());
    }
    check_fetch_site(req, true)
}

fn check_browser_boundary_for_route(
    req: &Request,
    listener_port: u16,
    public_navigation: bool,
    https_origin: Option<&str>,
) -> Result<String, Response> {
    // No forwarded header establishes authority. Host must name the exact
    // configured HTTPS origin, or a canonical loopback origin in local mode.
    // The listener itself remains loopback in both modes.
    let host = request_host(req)?;
    if listener_port == 0 {
        return Err(boundary_error());
    }
    let origin = if let Some(configured) = https_origin {
        // The builder validated this origin once. Never infer HTTPS or the
        // expected hostname from proxy-supplied headers on a request.
        if configured.strip_prefix("https://") != Some(host) {
            return Err(boundary_error());
        }
        configured.to_owned()
    } else {
        match canonical_loopback_origin(host) {
            Some((origin, port)) if port == listener_port => origin,
            _ => return Err(boundary_error()),
        }
    };
    if let Some(presented) = req.headers().get("origin") {
        if presented != origin.as_bytes() {
            return Err(boundary_error());
        }
    } else if !matches!(req.method(), Method::Get | Method::Head | Method::Options) {
        // Explicit Origin is also required for a native login client. This
        // prevents a form POST or opaque/null origin from minting a session.
        return Err(boundary_error());
    }
    check_fetch_site(req, public_navigation)?;
    // This surface has no bearer-token mode; accepting a second credential
    // channel would make request authority ambiguous.
    if req.headers().contains("authorization") {
        return Err(boundary_error());
    }
    Ok(origin)
}

fn cookie_digest(req: &Request, cookie_name: &str) -> Result<Option<[u8; 32]>, Response> {
    let Some(cookie) = req.headers().get("cookie") else {
        return Ok(None);
    };
    let cookie = std::str::from_utf8(cookie)
        .ok()
        .filter(|value| {
            value.len() <= 8192
                && !value
                    .bytes()
                    .any(|byte| byte.is_ascii_control() || byte == b',')
        })
        .ok_or_else(unauthorized)?;
    let mut found = None;
    for part in cookie.split(';') {
        let Some((name, value)) = part.trim().split_once('=') else {
            continue;
        };
        if name.trim() == cookie_name {
            if found.is_some()
                || name != cookie_name
                || value.len() != 64
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err(unauthorized());
            }
            found = Some(session_digest(value));
        }
    }
    Ok(found)
}

fn check_login_body_size(req: &Request) -> Result<(), Response> {
    let declared = req
        .headers()
        .get("content-length")
        .and_then(|value| std::str::from_utf8(value).ok())
        .and_then(|value| value.parse::<usize>().ok());
    let buffered = match req.body() {
        FrameworkRequestBody::Bytes(bytes) => Some(bytes.len()),
        FrameworkRequestBody::Stream { content_length, .. } => *content_length,
        FrameworkRequestBody::Empty => Some(0),
    };
    if declared
        .into_iter()
        .chain(buffered)
        .any(|len| len > LOGIN_BODY_LIMIT)
    {
        return Err(json_err(
            StatusCode::PAYLOAD_TOO_LARGE,
            "web_login_too_large",
            "Viewer login body exceeds its limit",
        ));
    }
    Ok(())
}

pub(super) fn handle_status(req: &Request) -> Response {
    let auth = req
        .get_extension::<SessionAuthMiddleware>()
        .and_then(|middleware| middleware.auth.as_ref());
    status(auth, req.get_extension::<SessionGrant>())
}

#[derive(Deserialize, ZeroizeOnDrop)]
#[serde(deny_unknown_fields)]
struct LoginBody {
    token: String,
}

pub(super) fn handle_login(req: &mut Request) -> Response {
    let Some(auth) = req
        .get_extension::<SessionAuthMiddleware>()
        .and_then(|middleware| middleware.auth.clone())
    else {
        return json_err(
            StatusCode::BAD_REQUEST,
            "web_auth_disabled",
            "Local operator authentication is not enabled",
        );
    };
    let origin = match req.get_extension::<SessionAuthMiddleware>() {
        Some(middleware) => {
            match check_browser_boundary(
                req,
                middleware.listener_port.load(Ordering::Acquire),
                auth.https_origin.as_deref(),
            ) {
                Ok(origin) => origin,
                Err(response) => return response,
            }
        }
        None => return unavailable(),
    };
    let content_type = req.headers().get("content-type").unwrap_or_default();
    if content_type != b"application/json" {
        return json_err(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "web_login_content_type",
            "Viewer login requires application/json",
        );
    }
    let replace = match cookie_digest(req, auth.cookie_name()) {
        Ok(digest) => digest,
        Err(response) => return response,
    };
    let FrameworkRequestBody::Bytes(bytes) = req.take_body() else {
        return unauthorized();
    };
    let bytes = Zeroizing::new(bytes);
    if bytes.len() > LOGIN_BODY_LIMIT {
        return unauthorized();
    }
    let Ok(login) = serde_json::from_slice::<LoginBody>(&bytes) else {
        return unauthorized();
    };
    if !auth.verifies(&login.token) {
        return unauthorized();
    }
    match auth.issue(replace, origin) {
        Ok((token, grant)) => status(Some(&auth), Some(&grant)).header(
            "set-cookie",
            format!(
                "{}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{}",
                auth.cookie_name(),
                token.as_str(),
                auth.session_ttl.as_secs(),
                if auth.https_origin.is_some() {
                    "; Secure"
                } else {
                    ""
                },
            )
            .into_bytes(),
        ),
        Err(response) => response,
    }
}

pub(super) fn handle_logout(req: &Request) -> Response {
    let auth = req
        .get_extension::<SessionAuthMiddleware>()
        .and_then(|middleware| middleware.auth.as_ref());
    if let Some(auth) = auth {
        let digest = match cookie_digest(req, auth.cookie_name()) {
            Ok(digest) => digest,
            Err(response) => return response,
        };
        if let Err(response) = auth.revoke(digest) {
            return response;
        }
    }
    status(auth, None).header(
        "set-cookie",
        format!(
            "{}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0{}",
            auth.map_or(COOKIE_NAME, WebSessionAuth::cookie_name),
            if auth.is_some_and(|auth| auth.https_origin.is_some()) {
                "; Secure"
            } else {
                ""
            },
        )
        .into_bytes(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web_framework::ResponseBody;
    use std::task::{Context, Waker};

    const SECRET: &str = "local-test-bootstrap-0123456789abcdef";
    const ORIGIN: &str = "http://localhost:4321";
    const HTTPS_ORIGIN: &str = "https://workstation.example.ts.net";

    fn authority() -> WebSessionAuth {
        WebSessionAuth::new(SECRET).expect("synthetic credential")
    }

    fn middleware(auth: WebSessionAuth) -> SessionAuthMiddleware {
        let guard = SessionAuthMiddleware::new(
            Some(auth),
            ["/ui/".to_owned(), "/ui/app.mjs".to_owned()].into_iter(),
        );
        guard.bind_listener("127.0.0.1:4321".parse().unwrap());
        guard
    }

    fn request(method: Method, path: &str) -> Request {
        let mut req = Request::new(method, path);
        req.headers_mut().insert("host", b"localhost:4321".to_vec());
        req.headers_mut()
            .insert("origin", ORIGIN.as_bytes().to_vec());
        req
    }

    fn set_cookie(req: &mut Request, token: &str) {
        req.headers_mut()
            .insert("cookie", format!("{COOKIE_NAME}={token}").into_bytes());
    }

    fn set_https_cookie(req: &mut Request, token: &str) {
        req.headers_mut().insert(
            "cookie",
            format!("{HTTPS_COOKIE_NAME}={token}").into_bytes(),
        );
    }

    fn https_request(method: Method, path: &str) -> Request {
        let mut req = request(method, path);
        req.headers_mut()
            .insert("host", b"workstation.example.ts.net".to_vec());
        req.headers_mut()
            .insert("origin", HTTPS_ORIGIN.as_bytes().to_vec());
        req
    }

    fn issue(auth: &WebSessionAuth) -> (Zeroizing<String>, SessionGrant) {
        auth.issue(None, ORIGIN.to_owned())
            .unwrap_or_else(|_| panic!("issue synthetic session"))
    }

    fn body(response: &Response) -> serde_json::Value {
        match response.body_ref() {
            ResponseBody::Bytes(bytes) => serde_json::from_slice(bytes).unwrap(),
            _ => panic!("expected JSON response"),
        }
    }

    fn header<'a>(response: &'a Response, name: &str) -> Option<&'a [u8]> {
        response
            .headers()
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_slice())
    }

    #[test]
    fn credential_and_limit_validation_fail_closed_without_secret_diagnostics() {
        for credential in [
            "",
            "too-short",
            "                                ",
            "0123456789abcdef0123456789abcdef\n",
        ] {
            assert!(WebSessionAuth::new(credential).is_err());
        }
        assert!(WebSessionAuth::new(&"a".repeat(257)).is_err());
        let auth = authority();
        assert!(auth.verifies(SECRET));
        assert!(!auth.verifies("local-test-bootstrap-0123456789abcdeg"));
        assert!(!format!("{auth:?}").contains(SECRET));
        assert!(auth.clone().with_session_ttl(Duration::ZERO).is_err());
        assert!(
            auth.clone()
                .with_session_ttl(Duration::from_secs(86_401))
                .is_err()
        );
        assert!(auth.clone().with_max_sessions(0).is_err());
        assert!(auth.with_max_sessions(257).is_err());
    }

    #[test]
    fn sessions_are_bounded_and_rotation_revokes_the_exact_old_grant() {
        let auth = authority().with_max_sessions(1).unwrap();
        let (token, first) = issue(&auth);
        assert!(first.is_valid());
        assert_eq!(
            auth.issue(None, ORIGIN.to_owned()).unwrap_err().status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        assert!(
            first.is_valid(),
            "capacity refusal must preserve the existing viewer"
        );
        let (next_token, second) = auth
            .issue(Some(session_digest(&token)), ORIGIN.to_owned())
            .unwrap();
        assert_ne!(token.as_str(), next_token.as_str());
        assert_ne!(first.0.session_epoch, second.0.session_epoch);
        assert!(!first.is_valid());
        assert!(second.is_valid());
        assert!(auth.lookup(Some(session_digest(&token))).unwrap().is_none());
        assert_eq!(auth.sessions.lock().unwrap().len(), 1);
    }

    #[test]
    fn https_origin_configuration_rejects_ambiguous_and_non_https_origins() {
        for origin in [
            HTTPS_ORIGIN,
            "https://workstation.example.ts.net:8443",
            "https://192.0.2.10",
            "https://[2001:db8::1]:8443",
        ] {
            let auth = authority().with_https_origin(origin).unwrap();
            assert_eq!(auth.https_origin.as_deref(), Some(origin));
        }
        for origin in [
            "",
            "null",
            "http://workstation.example.ts.net",
            "HTTPS://workstation.example.ts.net",
            "https://WORKSTATION.example.ts.net",
            "https://workstation.example.ts.net.",
            "https://*.example.ts.net",
            "https://workstation.example.ts.net/",
            "https://workstation.example.ts.net/ui",
            "https://workstation.example.ts.net?credential=sentinel-secret",
            "https://workstation.example.ts.net#fragment",
            "https://user:sentinel-secret@workstation.example.ts.net",
            "https://workstation.example.ts.net:443",
            "https://workstation.example.ts.net:",
            "https://workstation.example.ts.net:0",
            "https://workstation.example.ts.net:08443",
            " https://workstation.example.ts.net",
            "https://workstation.example.ts.net\n",
            "https://workstation.example.ts.net https://other.invalid",
            "https://wörkstation.example.ts.net",
            "https://%77orkstation.example.ts.net",
            "https://127.1",
        ] {
            let error = authority().with_https_origin(origin).unwrap_err();
            assert!(matches!(&error, WebSessionAuthError::InvalidHttpsOrigin));
            assert!(!error.to_string().contains("sentinel-secret"));
        }
        assert!(authority().with_https_origin(&"x".repeat(257)).is_err());
        assert!(
            authority().https_origin.is_none(),
            "local mode is the default"
        );
    }

    #[test]
    fn https_origin_admission_uses_only_configured_host_and_origin() {
        let auth = authority().with_https_origin(HTTPS_ORIGIN).unwrap();
        let guard = middleware(auth.clone());
        let req = https_request(Method::Get, "/auth/status");
        assert!(guard.check_request(&req).is_ok());
        assert_eq!(
            middleware(authority())
                .check_request(&req)
                .unwrap_err()
                .status(),
            StatusCode::FORBIDDEN,
            "the same proxy request must be refused without explicit opt-in"
        );
        assert_eq!(
            guard
                .check_request(&request(Method::Get, "/auth/status"))
                .unwrap_err()
                .status(),
            StatusCode::FORBIDDEN,
            "HTTPS mode must not also admit an HTTP loopback alias"
        );
        for path in [
            "/panes",
            "/search",
            "/openapi.json",
            "/stream/events",
            "/stream/deltas",
        ] {
            assert_eq!(
                guard
                    .check_request(&https_request(Method::Get, path))
                    .unwrap_err()
                    .status(),
                StatusCode::UNAUTHORIZED,
                "proxy access still requires the operator session: {path}"
            );
        }
        let unbound = SessionAuthMiddleware::new(Some(auth), std::iter::empty());
        assert_eq!(
            unbound.check_request(&req).unwrap_err().status(),
            StatusCode::FORBIDDEN
        );
        for host in [
            "localhost:4321",
            "workstation.example.ts.net:443",
            "workstation.example.ts.net:8443",
            "workstation.example.ts.net.",
            "WORKSTATION.example.ts.net",
            "workstation.example.ts.net.evil.invalid",
        ] {
            let mut req = https_request(Method::Get, "/auth/status");
            req.headers_mut().insert("host", host.as_bytes().to_vec());
            req.headers_mut()
                .insert("x-forwarded-host", b"workstation.example.ts.net".to_vec());
            req.headers_mut()
                .insert("x-forwarded-proto", b"https".to_vec());
            req.headers_mut().insert(
                "forwarded",
                b"host=workstation.example.ts.net;proto=https".to_vec(),
            );
            assert_eq!(
                guard.check_request(&req).unwrap_err().status(),
                StatusCode::FORBIDDEN,
                "{host}"
            );
        }
        let mut req = https_request(Method::Get, "/auth/status");
        req.headers_mut()
            .insert("x-forwarded-host", b"evil.invalid".to_vec());
        req.headers_mut()
            .insert("x-forwarded-proto", b"http".to_vec());
        req.headers_mut()
            .insert("forwarded", b"host=evil.invalid;proto=http".to_vec());
        req.headers_mut().insert(
            "tailscale-user-login",
            b"untrusted@example.invalid".to_vec(),
        );
        assert!(
            guard.check_request(&req).is_ok(),
            "forwarded and identity headers cannot select authority"
        );
        req.headers_mut()
            .insert("origin", b"https://other.example.ts.net".to_vec());
        assert_eq!(
            guard.check_request(&req).unwrap_err().status(),
            StatusCode::FORBIDDEN
        );
    }

    #[test]
    fn https_origin_keeps_csrf_and_public_navigation_boundaries() {
        let guard = middleware(authority().with_https_origin(HTTPS_ORIGIN).unwrap());
        for method in [Method::Post, Method::Delete] {
            let mut req = https_request(method, "/auth/session");
            req.headers_mut().remove("origin");
            assert_eq!(
                guard.check_request(&req).unwrap_err().status(),
                StatusCode::FORBIDDEN
            );
            for origin in [
                "null",
                ORIGIN,
                "http://workstation.example.ts.net",
                "https://other.invalid",
            ] {
                req.headers_mut()
                    .insert("origin", origin.as_bytes().to_vec());
                assert_eq!(
                    guard.check_request(&req).unwrap_err().status(),
                    StatusCode::FORBIDDEN
                );
            }
        }
        for site in ["same-site", "cross-site"] {
            let mut req = https_request(Method::Get, "/panes");
            req.headers_mut()
                .insert("sec-fetch-site", site.as_bytes().to_vec());
            assert_eq!(
                guard.check_request(&req).unwrap_err().status(),
                StatusCode::FORBIDDEN
            );
        }
        let mut navigation = https_request(Method::Get, "/ui/");
        navigation.headers_mut().remove("origin");
        navigation
            .headers_mut()
            .insert("sec-fetch-site", b"cross-site".to_vec());
        navigation
            .headers_mut()
            .insert("sec-fetch-mode", b"navigate".to_vec());
        navigation
            .headers_mut()
            .insert("sec-fetch-dest", b"document".to_vec());
        assert!(guard.check_request(&navigation).is_ok());
        navigation
            .headers_mut()
            .insert("sec-fetch-dest", b"iframe".to_vec());
        assert_eq!(
            guard.check_request(&navigation).unwrap_err().status(),
            StatusCode::FORBIDDEN
        );
    }

    #[test]
    fn https_login_and_logout_use_secure_cookies_and_revoke_exact_stream_authority() {
        let auth = authority().with_https_origin(HTTPS_ORIGIN).unwrap();
        let guard = middleware(auth.clone());
        let mut login = https_request(Method::Post, "/auth/session");
        login
            .headers_mut()
            .insert("content-type", b"application/json".to_vec());
        login.set_body(FrameworkRequestBody::Bytes(
            format!("{{\"token\":\"{SECRET}\"}}").into_bytes(),
        ));
        guard.admit(&mut login).unwrap();
        let response = handle_login(&mut login);
        assert_eq!(response.status(), StatusCode::OK);
        let cookie = std::str::from_utf8(header(&response, "set-cookie").unwrap()).unwrap();
        assert!(cookie.contains("; Path=/; HttpOnly; SameSite=Strict; Max-Age=3600; Secure"));
        assert!(!cookie.contains("Domain="));
        assert!(!cookie.contains(SECRET));
        let token = cookie
            .split(';')
            .next()
            .unwrap()
            .strip_prefix("__Host-ft_viewer_session=")
            .unwrap();
        let epoch = body(&response)["data"]["session_epoch"]
            .as_str()
            .unwrap()
            .to_owned();
        let mut read = https_request(Method::Get, "/panes");
        set_https_cookie(&mut read, token);
        guard.admit(&mut read).unwrap();
        let grant = read.get_extension::<SessionGrant>().unwrap().clone();
        assert_eq!(grant.0.origin, HTTPS_ORIGIN);

        let mut stream = https_request(Method::Get, "/stream/events");
        set_https_cookie(&mut stream, token);
        assert_eq!(
            guard.check_request(&stream).unwrap_err().status(),
            StatusCode::UNAUTHORIZED
        );
        stream.set_query(Some(format!("session_epoch={epoch}")));
        guard.admit(&mut stream).unwrap();
        let permit = reserve_stream(&stream).unwrap();

        let other = middleware(
            auth.clone()
                .with_https_origin("https://other.example.ts.net")
                .unwrap(),
        );
        let mut moved = https_request(Method::Get, "/panes");
        set_https_cookie(&mut moved, token);
        moved
            .headers_mut()
            .insert("host", b"other.example.ts.net".to_vec());
        moved
            .headers_mut()
            .insert("origin", b"https://other.example.ts.net".to_vec());
        assert_eq!(
            other.check_request(&moved).unwrap_err().status(),
            StatusCode::UNAUTHORIZED,
            "even shared session state cannot retarget a grant to another origin"
        );

        let mut logout = https_request(Method::Delete, "/auth/session");
        set_https_cookie(&mut logout, token);
        guard.admit(&mut logout).unwrap();
        let response = handle_logout(&logout);
        assert_eq!(response.status(), StatusCode::OK);
        let cleared = std::str::from_utf8(header(&response, "set-cookie").unwrap()).unwrap();
        assert_eq!(
            cleared,
            "__Host-ft_viewer_session=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0; Secure"
        );
        assert!(!grant.is_valid());
        assert_eq!(
            guard.check_request(&read).unwrap_err().status(),
            StatusCode::UNAUTHORIZED
        );
        assert!(
            reserve_stream(&stream).is_err(),
            "revoked stream cannot reserve more output"
        );
        let late = guard.finish(&read, json_ok("late-private-content"));
        assert_eq!(late.status(), StatusCode::UNAUTHORIZED);
        assert!(!body(&late).to_string().contains("late-private-content"));
        drop(permit);
    }

    #[test]
    fn https_login_rechecks_origin_even_if_a_handler_is_called_directly() {
        let auth = authority().with_https_origin(HTTPS_ORIGIN).unwrap();
        let guard = middleware(auth.clone());
        let mut login = request(Method::Post, "/auth/session");
        login.insert_extension(guard);
        login
            .headers_mut()
            .insert("content-type", b"application/json".to_vec());
        login.set_body(FrameworkRequestBody::Bytes(
            format!("{{\"token\":\"{SECRET}\"}}").into_bytes(),
        ));
        assert_eq!(handle_login(&mut login).status(), StatusCode::FORBIDDEN);
        assert!(auth.sessions.lock().unwrap().is_empty());
    }

    #[test]
    fn both_stream_routes_share_the_legacy_viewer_budget_and_cloned_admission() {
        let guard = SessionAuthMiddleware::new(None, std::iter::empty());
        let mut requests = [
            request(Method::Get, "/stream/events"),
            request(Method::Get, "/stream/deltas"),
        ];
        for req in &mut requests {
            guard.clone().admit(req).unwrap();
        }
        let mut permits = Vec::new();
        for index in 0..MAX_VIEWER_STREAMS {
            permits.push(reserve_stream(&requests[index % requests.len()]).unwrap());
        }
        for req in &requests {
            let denied = reserve_stream(req).unwrap_err();
            assert_eq!(denied.status(), StatusCode::TOO_MANY_REQUESTS);
            assert_eq!(body(&denied)["error_code"], "web_viewer_stream_capacity");
            assert_eq!(header(&denied, "retry-after"), Some(b"1".as_slice()));
        }
        assert_eq!(
            guard.streams.active.load(Ordering::Acquire),
            MAX_VIEWER_STREAMS
        );
        drop(permits.pop());
        let replacement = reserve_stream(&requests[1]).unwrap();
        assert!(reserve_stream(&requests[0]).is_err());
        drop(replacement);
        drop(permits);
        assert_eq!(guard.streams.active.load(Ordering::Acquire), 0);
        assert_eq!(guard.anonymous_streams.active.load(Ordering::Acquire), 0);
    }

    #[test]
    fn all_sessions_share_server_capacity_and_denial_rolls_back_viewer_reservation() {
        let auth = authority();
        let guard = middleware(auth.clone());
        let mut sessions = Vec::new();
        let mut permits = Vec::new();
        for _ in 0..MAX_SERVER_STREAMS.div_ceil(MAX_VIEWER_STREAMS) {
            let (_, grant) = issue(&auth);
            for _ in 0..MAX_VIEWER_STREAMS {
                permits.push(guard.reserve_stream(Some(&grant)).unwrap());
            }
            sessions.push(grant);
        }
        let (_, next) = issue(&auth);
        let denied = guard.reserve_stream(Some(&next)).unwrap_err();
        assert_eq!(denied.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body(&denied)["error_code"], "web_stream_capacity");
        assert_eq!(header(&denied, "retry-after"), Some(b"1".as_slice()));
        assert_eq!(next.0.streams.active.load(Ordering::Acquire), 0);
        assert_eq!(
            guard.streams.active.load(Ordering::Acquire),
            MAX_SERVER_STREAMS
        );

        drop(permits.pop());
        let replacement = guard.reserve_stream(Some(&next)).unwrap();
        assert_eq!(next.0.streams.active.load(Ordering::Acquire), 1);
        assert!(guard.reserve_stream(Some(&next)).is_err());
        assert_eq!(next.0.streams.active.load(Ordering::Acquire), 1);
        drop(replacement);
        drop(permits);
        assert_eq!(guard.streams.active.load(Ordering::Acquire), 0);
        assert!(
            sessions
                .iter()
                .all(|grant| grant.0.streams.active.load(Ordering::Acquire) == 0)
        );
    }

    #[test]
    fn stream_admission_refuses_missing_revoked_and_foreign_session_authority() {
        let auth = authority();
        let guard = middleware(auth.clone());
        let (_, live) = issue(&auth);
        let foreign_auth = authority();
        let (_, foreign) = issue(&foreign_auth);
        for session in [None, Some(&foreign)] {
            assert_eq!(
                guard.reserve_stream(session).unwrap_err().status(),
                StatusCode::UNAUTHORIZED
            );
        }
        live.revoke();
        assert_eq!(
            guard.reserve_stream(Some(&live)).unwrap_err().status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(guard.streams.active.load(Ordering::Acquire), 0);
        assert_eq!(live.0.streams.active.load(Ordering::Acquire), 0);
        assert_eq!(foreign.0.streams.active.load(Ordering::Acquire), 0);
    }

    #[test]
    fn concurrent_stream_admission_never_exceeds_a_viewers_budget() {
        let auth = authority();
        let guard = middleware(auth.clone());
        let (_, grant) = issue(&auth);
        let attempted = Arc::new(std::sync::Barrier::new(17));
        let release = Arc::new(std::sync::Barrier::new(17));
        std::thread::scope(|scope| {
            for _ in 0..16 {
                let guard = guard.clone();
                let grant = grant.clone();
                let attempted = Arc::clone(&attempted);
                let release = Arc::clone(&release);
                scope.spawn(move || {
                    let permit = guard.reserve_stream(Some(&grant));
                    attempted.wait();
                    release.wait();
                    drop(permit);
                });
            }
            attempted.wait();
            let viewer_active = grant.0.streams.active.load(Ordering::Acquire);
            let server_active = guard.streams.active.load(Ordering::Acquire);
            release.wait();
            assert_eq!(viewer_active, MAX_VIEWER_STREAMS);
            assert_eq!(server_active, MAX_VIEWER_STREAMS);
        });
        assert_eq!(grant.0.streams.active.load(Ordering::Acquire), 0);
        assert_eq!(guard.streams.active.load(Ordering::Acquire), 0);
    }

    #[test]
    fn session_status_contains_an_independent_epoch_and_never_the_cookie() {
        let auth = authority();
        let (token, grant) = issue(&auth);
        let value = body(&status(Some(&auth), Some(&grant)));
        assert_eq!(value["data"]["required"], true);
        assert_eq!(value["data"]["authenticated"], true);
        assert!(value["data"]["expires_at_ms"].as_u64().unwrap() > 0);
        let epoch = value["data"]["session_epoch"].as_str().unwrap();
        assert_eq!(epoch.len(), 32);
        assert!(epoch.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(!value.to_string().contains(token.as_str()));
        assert!(!value.to_string().contains(SECRET));
        assert!(!format!("{grant:?}").contains(token.as_str()));
        let private = body(&status(None, None));
        assert_eq!(private["data"]["required"], false);
        assert_eq!(private["data"]["authenticated"], true);
        assert!(private["data"].get("session_epoch").is_none());
    }

    #[test]
    fn all_data_and_future_routes_default_to_authenticated_admission() {
        let guard = middleware(authority());
        for path in [
            "/panes",
            "/search",
            "/events",
            "/stream/events",
            "/stream/deltas",
            "/openapi.json",
            "/new-sensitive-route",
            "/ui/private-data",
        ] {
            let req = request(Method::Get, path);
            assert_eq!(
                guard.check_request(&req).unwrap_err().status(),
                StatusCode::UNAUTHORIZED,
                "{path}"
            );
        }
        for path in ["/health", "/auth/status", "/ui/", "/ui/app.mjs"] {
            assert!(
                guard.check_request(&request(Method::Get, path)).is_ok(),
                "{path}"
            );
        }
        assert!(
            guard
                .check_request(&request(Method::Post, "/auth/session"))
                .is_ok()
        );
        assert_eq!(
            guard
                .check_request(&request(Method::Post, "/ui/app.mjs"))
                .unwrap_err()
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn exact_loopback_host_port_and_origin_are_required() {
        for host in [
            "evil.invalid:4321",
            "localhost.evil.invalid:4321",
            "localhost:4322",
            "localhost.",
            "localhost:4321,evil.invalid",
            "127.1:4321",
            "user@localhost:4321",
            "localhost:4321/",
        ] {
            let mut req = request(Method::Get, "/health");
            req.headers_mut().insert("host", host.as_bytes().to_vec());
            assert!(check_browser_boundary(&req, 4321, None).is_err(), "{host}");
        }
        let mut req = request(Method::Post, "/auth/session");
        for origin in [
            "null",
            "http://evil.invalid",
            "http://localhost:4322",
            "http://localhost:4321/",
            "http://localhost:4321 http://evil.invalid",
        ] {
            req.headers_mut()
                .insert("origin", origin.as_bytes().to_vec());
            assert!(
                check_browser_boundary(&req, 4321, None).is_err(),
                "{origin}"
            );
        }
        req.headers_mut().remove("origin");
        assert!(check_browser_boundary(&req, 4321, None).is_err());
        req.headers_mut()
            .insert("origin", ORIGIN.as_bytes().to_vec());
        assert!(check_browser_boundary(&req, 4321, None).is_ok());
        assert!(
            check_browser_boundary(&req, 0, None).is_err(),
            "unbound applications cannot admit"
        );
        req.headers_mut()
            .insert("sec-fetch-site", b"same-site".to_vec());
        assert!(check_browser_boundary(&req, 4321, None).is_err());
        req.headers_mut().remove("sec-fetch-site");
        req.headers_mut()
            .insert("authorization", b"Bearer second-credential".to_vec());
        assert!(check_browser_boundary(&req, 4321, None).is_err());
    }

    #[test]
    fn unauthenticated_mode_admits_only_loopback_hosts_on_any_port() {
        // ft-mgy5t2.27.1: with no credential, Host is the DNS-rebinding
        // boundary. A rebound page is same-origin and names itself in Host.
        let guard = SessionAuthMiddleware::new(None, std::iter::empty());
        let host_request = |host: &str| {
            let mut req = Request::new(Method::Get, "/panes");
            req.headers_mut().insert("host", host.as_bytes().to_vec());
            req
        };
        for host in [
            "localhost",
            "localhost:4321",
            "localhost:8080",
            "127.0.0.1:9999",
            "127.1.2.3:9999",
            "[::1]:9999",
        ] {
            assert!(guard.check_request(&host_request(host)).is_ok(), "{host}");
        }
        for host in [
            "attacker.example",
            "attacker.example:4321",
            "localhost.attacker.example:4321",
            "localhost.:4321",
            "LOCALHOST:4321",
            "127.1:4321",
            "2130706433:4321",
            "0x7f.0.0.1:4321",
            "192.168.1.5:4321",
            "[::ffff:127.0.0.1]:4321",
            "user@localhost:4321",
            "localhost:4321,attacker.example",
            "localhost:4321/",
            "",
        ] {
            let refused = guard.check_request(&host_request(host)).unwrap_err();
            assert_eq!(refused.status(), StatusCode::FORBIDDEN, "{host}");
            assert_eq!(body(&refused)["error_code"], "web_origin_refused", "{host}");
        }
        let missing = Request::new(Method::Get, "/panes");
        assert!(guard.check_request(&missing).is_err(), "Host is required");

        let mut req = host_request("localhost:4321");
        for origin in [
            "null",
            "http://attacker.example",
            "http://localhost:4322",
            "http://127.0.0.1:4321",
            "http://localhost:4321/",
        ] {
            req.headers_mut()
                .insert("origin", origin.as_bytes().to_vec());
            assert!(guard.check_request(&req).is_err(), "{origin}");
        }
        req.headers_mut()
            .insert("origin", b"http://localhost:4321".to_vec());
        assert!(guard.check_request(&req).is_ok());

        // A cross-site subresource cannot read the response but could hold
        // the anonymous stream budget. A top-level navigation still loads.
        req.headers_mut().remove("origin");
        req.headers_mut()
            .insert("sec-fetch-site", b"cross-site".to_vec());
        assert!(guard.check_request(&req).is_err());
        req.headers_mut()
            .insert("sec-fetch-mode", b"navigate".to_vec());
        req.headers_mut()
            .insert("sec-fetch-dest", b"document".to_vec());
        assert!(guard.check_request(&req).is_ok());
        req.headers_mut()
            .insert("sec-fetch-site", b"same-origin".to_vec());
        req.headers_mut().insert("sec-fetch-mode", b"cors".to_vec());
        req.headers_mut()
            .insert("sec-fetch-dest", b"empty".to_vec());
        assert!(guard.check_request(&req).is_ok());

        // The pre-body validator applies the same boundary and closes.
        let refused = guard
            .validate(&host_request("attacker.example:4321"))
            .unwrap_err();
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);
        assert_eq!(header(&refused, "connection"), Some(b"close".as_slice()));
    }

    #[test]
    fn ipv6_localhost_origin_and_port_are_admitted_without_forwarded_trust() {
        let mut req = request(Method::Get, "/health");
        req.headers_mut().insert("host", b"[::1]:4321".to_vec());
        req.headers_mut()
            .insert("origin", b"http://[::1]:4321".to_vec());
        req.headers_mut()
            .insert("x-forwarded-host", b"evil.invalid".to_vec());
        req.headers_mut()
            .insert("x-forwarded-proto", b"https".to_vec());
        assert!(check_browser_boundary(&req, 4321, None).is_ok());
        req.headers_mut()
            .insert("host", b"evil.invalid:4321".to_vec());
        req.headers_mut()
            .insert("x-forwarded-host", b"localhost:4321".to_vec());
        assert!(check_browser_boundary(&req, 4321, None).is_err());
    }

    #[test]
    fn linked_public_document_navigation_does_not_admit_cross_site_data_reads() {
        let guard = middleware(authority());
        let mut req = request(Method::Get, "/ui/");
        req.headers_mut().remove("origin");
        req.headers_mut()
            .insert("sec-fetch-site", b"cross-site".to_vec());
        req.headers_mut()
            .insert("sec-fetch-mode", b"navigate".to_vec());
        req.headers_mut()
            .insert("sec-fetch-dest", b"document".to_vec());
        assert!(guard.check_request(&req).is_ok());
        req.headers_mut()
            .insert("sec-fetch-dest", b"iframe".to_vec());
        assert!(guard.check_request(&req).is_err());
        let mut data = request(Method::Get, "/panes");
        data.headers_mut().remove("origin");
        data.headers_mut()
            .insert("sec-fetch-site", b"cross-site".to_vec());
        data.headers_mut()
            .insert("sec-fetch-mode", b"navigate".to_vec());
        data.headers_mut()
            .insert("sec-fetch-dest", b"document".to_vec());
        assert_eq!(
            guard.check_request(&data).unwrap_err().status(),
            StatusCode::FORBIDDEN
        );
    }

    #[test]
    fn stream_epoch_fences_native_reconnect_and_optional_read_epoch_is_checked() {
        let auth = authority();
        let guard = middleware(auth.clone());
        let (token, grant) = issue(&auth);
        let mut req = request(Method::Get, "/stream/events");
        set_cookie(&mut req, &token);
        assert_eq!(
            guard.check_request(&req).unwrap_err().status(),
            StatusCode::UNAUTHORIZED
        );
        req.set_query(Some(format!("session_epoch={}", grant.0.session_epoch)));
        assert!(guard.check_request(&req).is_ok());
        req.set_query(Some(format!(
            "session_epoch={0}&session_epoch={0}",
            grant.0.session_epoch
        )));
        assert!(guard.check_request(&req).is_err());
        req.set_query(Some(format!("session_epoch={}", grant.0.session_epoch)));
        let (replacement, _) = auth
            .issue(Some(session_digest(&token)), ORIGIN.to_owned())
            .unwrap();
        set_cookie(&mut req, &replacement);
        assert_eq!(
            guard.check_request(&req).unwrap_err().status(),
            StatusCode::UNAUTHORIZED,
            "new cookie must not resume the old viewer's stream"
        );
        let mut read = request(Method::Get, "/panes");
        set_cookie(&mut read, &replacement);
        assert!(guard.check_request(&read).is_ok());
        read.headers_mut().insert(
            "x-ft-session-epoch",
            grant.0.session_epoch.as_bytes().to_vec(),
        );
        assert_eq!(
            guard.check_request(&read).unwrap_err().status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn duplicate_and_noncanonical_cookie_values_cannot_select_authority() {
        let mut req = request(Method::Get, "/panes");
        let token = "a".repeat(64);
        for value in [
            format!("{COOKIE_NAME}={token}; {COOKIE_NAME}={token}"),
            format!("{COOKIE_NAME}={token}, {COOKIE_NAME}={token}"),
            format!("{COOKIE_NAME} = {token}"),
            format!("{COOKIE_NAME}=\"{token}\""),
            format!("{COOKIE_NAME}=short"),
        ] {
            req.headers_mut().insert("cookie", value.into_bytes());
            assert!(cookie_digest(&req, COOKIE_NAME).is_err());
        }
        req.headers_mut().insert(
            "cookie",
            format!("theme=dark; {COOKIE_NAME}={token}").into_bytes(),
        );
        assert_eq!(
            cookie_digest(&req, COOKIE_NAME).unwrap(),
            Some(session_digest(&token))
        );
    }

    /// Another host under the same site, such as a tailnet peer, can plant
    /// Domain cookies under the plain name. In HTTPS mode the server reads only
    /// the host-only `__Host-` cookie. Planted ones can neither lock out sign-in
    /// nor select authority, and duplicate prefixed cookies are still refused.
    #[test]
    fn https_mode_ignores_planted_unprefixed_cookies() {
        let auth = authority().with_https_origin(HTTPS_ORIGIN).unwrap();
        let guard = middleware(auth.clone());
        let (token, _) = auth
            .issue(None, HTTPS_ORIGIN.to_owned())
            .unwrap_or_else(|_| panic!("issue synthetic session"));
        let planted = format!("{COOKIE_NAME}={}; {COOKIE_NAME}=junk", "a".repeat(64));

        let mut status = https_request(Method::Get, "/auth/status");
        status
            .headers_mut()
            .insert("cookie", planted.clone().into_bytes());
        assert!(
            guard.check_request(&status).is_ok(),
            "planted cookies must not lock out sign-in"
        );

        let mut read = https_request(Method::Get, "/panes");
        read.headers_mut().insert(
            "cookie",
            format!("{planted}; {HTTPS_COOKIE_NAME}={}", token.as_str()).into_bytes(),
        );
        assert!(matches!(guard.check_request(&read), Ok(Some(_))));

        read.headers_mut().insert(
            "cookie",
            format!(
                "{HTTPS_COOKIE_NAME}={0}; {HTTPS_COOKIE_NAME}={0}",
                token.as_str()
            )
            .into_bytes(),
        );
        assert_eq!(
            guard.check_request(&read).unwrap_err().status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn grant_is_bound_to_its_original_origin_and_late_response_is_fenced() {
        let auth = authority();
        let guard = middleware(auth.clone());
        let (token, grant) = issue(&auth);
        let mut req = request(Method::Get, "/panes");
        set_cookie(&mut req, &token);
        guard.admit(&mut req).unwrap();
        let mut alias = request(Method::Get, "/panes");
        set_cookie(&mut alias, &token);
        alias
            .headers_mut()
            .insert("host", b"127.0.0.1:4321".to_vec());
        alias
            .headers_mut()
            .insert("origin", b"http://127.0.0.1:4321".to_vec());
        assert_eq!(
            guard.check_request(&alias).unwrap_err().status(),
            StatusCode::UNAUTHORIZED
        );
        auth.revoke(Some(session_digest(&token))).unwrap();
        assert!(!grant.is_valid());
        let response = guard.finish(&req, json_ok("private-query-result"));
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(!body(&response).to_string().contains("private-query-result"));
        assert_eq!(
            header(&response, "cache-control"),
            Some(b"no-store".as_slice())
        );
    }

    #[test]
    fn declared_and_buffered_login_limits_run_before_credential_parsing() {
        let mut req = request(Method::Post, "/auth/session");
        req.headers_mut().insert("content-length", b"1025".to_vec());
        assert_eq!(
            check_login_body_size(&req).unwrap_err().status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        req.headers_mut().remove("content-length");
        req.set_body(FrameworkRequestBody::Bytes(vec![b'a'; 1025]));
        assert_eq!(
            check_login_body_size(&req).unwrap_err().status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }

    #[test]
    fn duplicate_json_credentials_are_refused_and_login_has_private_cookie_flags() {
        let auth = authority();
        let guard = middleware(auth);
        let mut req = request(Method::Post, "/auth/session");
        req.headers_mut()
            .insert("content-type", b"application/json".to_vec());
        req.set_body(FrameworkRequestBody::Bytes(
            format!("{{\"token\":\"{SECRET}\",\"token\":\"{SECRET}\"}}").into_bytes(),
        ));
        guard.admit(&mut req).unwrap();
        assert_eq!(handle_login(&mut req).status(), StatusCode::UNAUTHORIZED);
        req.set_body(FrameworkRequestBody::Bytes(
            format!("{{\"token\":\"{SECRET}\"}}").into_bytes(),
        ));
        let response = handle_login(&mut req);
        assert_eq!(response.status(), StatusCode::OK);
        let cookie = std::str::from_utf8(header(&response, "set-cookie").unwrap()).unwrap();
        assert!(cookie.contains("; Path=/; HttpOnly; SameSite=Strict; Max-Age=3600"));
        assert!(!cookie.contains("Domain="));
        assert!(!cookie.contains(SECRET));
        assert!(matches!(req.body(), FrameworkRequestBody::Empty));
    }

    #[test]
    fn expiry_reclaims_capacity_and_cannot_be_extended_by_wall_clock_hints() {
        let auth = authority().with_max_sessions(1).unwrap();
        let (token, grant) = issue(&auth);
        let digest = session_digest(&token);
        drop(grant);
        {
            let mut sessions = auth.sessions.lock().unwrap();
            let grant = sessions.get_mut(&digest).unwrap();
            let lease = Arc::get_mut(&mut grant.0).unwrap();
            lease.deadline = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
            lease.expires_at_ms = u64::MAX;
        }
        assert!(auth.lookup(Some(digest)).unwrap().is_none());
        let (_, replacement) = issue(&auth);
        assert!(replacement.is_valid());
        assert_eq!(auth.sessions.lock().unwrap().len(), 1);
    }

    #[test]
    fn revoked_stream_authority_ends_before_any_runtime_wait_is_polled() {
        let auth = authority();
        let (token, grant) = issue(&auth);
        let mut end = stream_shutdown(
            Box::pin(std::future::pending()),
            Some(grant),
            &crate::cx::for_request(),
        );
        auth.revoke(Some(session_digest(&token))).unwrap();
        let mut task = Context::from_waker(Waker::noop());
        assert!(end.as_mut().poll(&mut task).is_ready());
    }

    #[test]
    fn server_stream_stop_does_not_revoke_other_responses_session() {
        let auth = authority();
        let (_, grant) = issue(&auth);
        let mut end = stream_shutdown(
            Box::pin(std::future::ready(())),
            Some(grant.clone()),
            &crate::cx::for_request(),
        );
        assert!(
            end.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_ready()
        );
        assert!(grant.is_valid());
    }

    #[test]
    fn poisoned_session_authority_fences_existing_and_new_requests() {
        let auth = authority();
        let (_, grant) = issue(&auth);
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _sessions = auth.sessions.lock().unwrap();
            panic!("synthetic session authority failure");
        }));
        assert!(poisoned.is_err());
        assert!(!grant.is_valid());
        assert_eq!(
            auth.lookup(None).unwrap_err().status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        let mut end = stream_shutdown(
            Box::pin(std::future::pending()),
            Some(grant),
            &crate::cx::for_request(),
        );
        assert!(
            end.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_ready()
        );
    }
}
