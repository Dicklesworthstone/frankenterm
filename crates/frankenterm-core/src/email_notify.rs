//! Bounded SMTP email notifications over the canonical async transport.
//!
//! STARTTLS is required by default; implicit TLS and explicitly configured
//! unauthenticated plaintext relays are also supported. A delivery is accepted
//! only after the server acknowledges DATA. Cancellation or connection loss
//! after DATA begins is indeterminate and is never automatically retried.

use base64::Engine;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::Duration;

use crate::notifications::{
    NotificationDelivery, NotificationDeliveryRecord, NotificationFuture, NotificationPayload,
    NotificationSender,
};
use crate::runtime_async::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use crate::runtime_async::net::TcpStream;
use crate::runtime_async::tls::TlsConnector;

const MAX_RECIPIENTS: usize = 32;
const MAX_CREDENTIAL_BYTES: usize = 1024;
const MAX_PAYLOAD_BYTES: usize = 128 * 1024;
const MAX_MESSAGE_BYTES: usize = 256 * 1024;
const MAX_REPLY_LINE_BYTES: usize = 512;
const MAX_REPLY_BYTES: usize = 16 * 1024;
const MAX_REPLY_LINES: usize = 64;

/// TLS mode for SMTP delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EmailTlsMode {
    /// No TLS (plain SMTP).
    None,
    /// Upgrade to TLS via STARTTLS (recommended).
    #[default]
    StartTls,
    /// Implicit TLS (SMTPS).
    Tls,
}

/// Email notification configuration.
///
/// ```toml
/// [notifications.email]
/// enabled = true
/// smtp_host = "smtp.example.com"
/// smtp_port = 587
/// tls = "start_tls"
/// username = "user@example.com"
/// password = "app-password"
/// from = "ft@example.com"
/// to = ["ops@example.com"]
/// subject_prefix = "[ft]"
/// timeout_secs = 10
/// ```
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EmailNotifyConfig {
    /// Enable email notifications.
    pub enabled: bool,

    /// SMTP server hostname.
    pub smtp_host: String,

    /// SMTP server port.
    pub smtp_port: u16,

    /// SMTP username (optional).
    pub username: Option<String>,

    /// SMTP password (optional).
    pub password: Option<String>,

    /// Sender email address.
    pub from: String,

    /// Recipient email addresses.
    pub to: Vec<String>,

    /// Optional subject prefix.
    pub subject_prefix: String,

    /// TLS mode for SMTP.
    pub tls: EmailTlsMode,

    /// Total SMTP transaction timeout, from 1 to 120 seconds.
    pub timeout_secs: u64,
}

impl Default for EmailNotifyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            smtp_host: String::new(),
            smtp_port: 587,
            username: None,
            password: None,
            from: String::new(),
            to: Vec::new(),
            subject_prefix: "[ft]".to_string(),
            tls: EmailTlsMode::StartTls,
            timeout_secs: 10,
        }
    }
}

impl fmt::Debug for EmailNotifyConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EmailNotifyConfig")
            .field("enabled", &self.enabled)
            .field("smtp_host", &"<configured>")
            .field("smtp_port", &self.smtp_port)
            .field("credentials_configured", &self.username.is_some())
            .field("from", &"<configured>")
            .field("recipient_count", &self.to.len())
            .field("subject_prefix", &"<configured>")
            .field("tls", &self.tls)
            .field("timeout_secs", &self.timeout_secs)
            .finish()
    }
}

impl EmailNotifyConfig {
    /// Validate the email configuration.
    pub fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }

        if self.smtp_host.trim().is_empty() {
            return Err("notifications.email.smtp_host must not be empty".to_string());
        }
        if !valid_smtp_host(&self.smtp_host) {
            return Err(
                "notifications.email.smtp_host must be a hostname or IP address".to_string(),
            );
        }

        if self.smtp_port == 0 {
            return Err("notifications.email.smtp_port must be >= 1".to_string());
        }

        validate_mailbox(&self.from, "notifications.email.from")?;

        if self.to.is_empty() {
            return Err("notifications.email.to must not be empty".to_string());
        }
        if self.to.len() > MAX_RECIPIENTS {
            return Err(format!(
                "notifications.email.to must contain at most {MAX_RECIPIENTS} recipients"
            ));
        }

        for (idx, addr) in self.to.iter().enumerate() {
            validate_mailbox(addr, &format!("notifications.email.to[{idx}]"))?;
        }

        let username_empty = self
            .username
            .as_ref()
            .map(|v| v.trim().is_empty())
            .unwrap_or(false);
        let password_empty = self
            .password
            .as_ref()
            .map(|v| v.trim().is_empty())
            .unwrap_or(false);

        if username_empty {
            return Err("notifications.email.username must not be empty".to_string());
        }
        if password_empty {
            return Err("notifications.email.password must not be empty".to_string());
        }

        if self.username.is_some() != self.password.is_some() {
            return Err(
                "notifications.email.username and notifications.email.password must be set together"
                    .to_string(),
            );
        }
        for (name, value) in [
            ("username", self.username.as_deref()),
            ("password", self.password.as_deref()),
        ] {
            if value.is_some_and(|value| value.len() > MAX_CREDENTIAL_BYTES || value.contains('\0'))
            {
                return Err(format!(
                    "notifications.email.{name} exceeds its bound or contains NUL"
                ));
            }
        }
        if self.username.is_some() && self.tls == EmailTlsMode::None {
            return Err("notifications.email credentials require TLS".to_string());
        }
        if self.timeout_secs == 0 || self.timeout_secs > 120 {
            return Err("notifications.email.timeout_secs must be between 1 and 120".to_string());
        }
        if self.subject_prefix.len() > 128 || self.subject_prefix.chars().any(char::is_control) {
            return Err(
                "notifications.email.subject_prefix must be at most 128 bytes without controls"
                    .to_string(),
            );
        }

        Ok(())
    }
}

fn looks_like_email(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return false;
    }

    let mut parts = trimmed.split('@');
    let local = parts.next().unwrap_or("");
    let domain = parts.next().unwrap_or("");
    if parts.next().is_some() {
        return false;
    }

    !local.is_empty() && !domain.is_empty() && domain.contains('.')
}

fn validate_mailbox(value: &str, field: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    if value.chars().any(char::is_control) || !looks_like_email(value) {
        return Err(format!("{field} must be a valid email address"));
    }
    let address = value.trim();
    if !address.is_ascii() {
        return Err(format!(
            "{field} uses an SMTPUTF8 mailbox, which is not supported; use an ASCII mailbox"
        ));
    }
    let (local, domain) = address
        .split_once('@')
        .expect("validated address separator");
    let valid = address.len() <= 254
        && !local.is_empty()
        && local.len() <= 64
        && !local.starts_with('.')
        && !local.ends_with('.')
        && !local.contains("..")
        && local
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-/=?^_`{|}~.".contains(&byte))
        && valid_dns_name(domain);
    if valid {
        Ok(())
    } else {
        Err(format!("{field} must be a valid email address"))
    }
}

fn valid_dns_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 253
        && value.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

fn valid_smtp_host(value: &str) -> bool {
    value.parse::<std::net::IpAddr>().is_ok() || valid_dns_name(value)
}

/// A reusable SMTP notification sender. All delivery errors and Debug output
/// are content-free: server replies, addresses, credentials and message text
/// never become diagnostic strings.
#[derive(Debug)]
pub struct EmailNotifier {
    config: EmailNotifyConfig,
    tls: Option<TlsConnector>,
    /// Skips an unreachable relay during its cooldown, so each notification
    /// does not wait out the full timeout inline.
    breaker: Option<std::sync::Mutex<crate::circuit_breaker::CircuitBreaker>>,
}

#[derive(Debug)]
struct SmtpFailure {
    code: &'static str,
    status: u16,
}

impl SmtpFailure {
    const fn new(code: &'static str) -> Self {
        Self { code, status: 0 }
    }

    fn rejected(code: &'static str, status: u16) -> Self {
        if (400..600).contains(&status) {
            Self { code, status }
        } else {
            Self::new("email.invalid_reply")
        }
    }
}

struct SmtpReply {
    code: u16,
    lines: Vec<String>,
}

impl SmtpReply {
    fn require(&self, code: u16, failure: &'static str) -> Result<(), SmtpFailure> {
        if self.code == code {
            Ok(())
        } else {
            Err(SmtpFailure::rejected(failure, self.code))
        }
    }

    fn supports(&self, extension: &str) -> bool {
        self.lines.iter().any(|line| {
            line.split_ascii_whitespace()
                .next()
                .is_some_and(|word| word.eq_ignore_ascii_case(extension))
        })
    }

    fn auth_supports(&self, mechanism: &str) -> bool {
        self.lines.iter().any(|line| {
            let mut words = line.split_ascii_whitespace();
            let Some(first) = words.next() else {
                return false;
            };
            if first.eq_ignore_ascii_case("AUTH") {
                return words.any(|word| word.eq_ignore_ascii_case(mechanism));
            }
            first.split_once('=').is_some_and(|(name, value)| {
                name.eq_ignore_ascii_case("AUTH")
                    && (value.eq_ignore_ascii_case(mechanism)
                        || words.any(|word| word.eq_ignore_ascii_case(mechanism)))
            })
        })
    }
}

struct SmtpConnection<'a, S> {
    stream: S,
    cx: &'a crate::cx::Cx,
}

impl<S: AsyncRead + AsyncWrite + Unpin> SmtpConnection<'_, S> {
    /// Cancellation can surface through the I/O call before the outer
    /// cancellation guard sees it; either way it is reported as cancellation.
    fn io_failure(&self) -> SmtpFailure {
        match self.cx.checkpoint() {
            Err(error) => smtp_context_failure(self.cx, &error),
            Ok(()) => SmtpFailure::new("email.transport_failed"),
        }
    }

    async fn write(&mut self, bytes: &[u8]) -> Result<(), SmtpFailure> {
        self.cx
            .checkpoint()
            .map_err(|error| smtp_context_failure(self.cx, &error))?;
        if self.stream.write_all(bytes).await.is_err() || self.stream.flush().await.is_err() {
            return Err(self.io_failure());
        }
        Ok(())
    }

    async fn reply(&mut self) -> Result<SmtpReply, SmtpFailure> {
        let invalid = || SmtpFailure::new("email.invalid_reply");
        let mut lines = Vec::new();
        let mut code = None;
        let mut total = 0;
        for _ in 0..MAX_REPLY_LINES {
            self.cx
                .checkpoint()
                .map_err(|error| smtp_context_failure(self.cx, &error))?;
            let mut line = Vec::new();
            loop {
                if line.len() >= MAX_REPLY_LINE_BYTES || total >= MAX_REPLY_BYTES {
                    return Err(SmtpFailure::new("email.reply_too_large"));
                }
                // Reading only through CRLF also makes the STARTTLS boundary
                // exact: no prefetched plaintext can survive the TLS upgrade.
                let mut byte = [0_u8; 1];
                let Ok(read) = crate::runtime_async::io::read(&mut self.stream, &mut byte).await
                else {
                    return Err(self.io_failure());
                };
                if read == 0 {
                    return Err(SmtpFailure::new("email.transport_closed"));
                }
                total += 1;
                line.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
            }
            if line.len() < 5 || !line.ends_with(b"\r\n") {
                return Err(invalid());
            }
            line.truncate(line.len() - 2);
            if !line[..3].iter().all(u8::is_ascii_digit) {
                return Err(invalid());
            }
            let parsed = u16::from(line[0] - b'0') * 100
                + u16::from(line[1] - b'0') * 10
                + u16::from(line[2] - b'0');
            if !(200..600).contains(&parsed) || code.is_some_and(|code| code != parsed) {
                return Err(invalid());
            }
            code = Some(parsed);
            let continued = match line.get(3) {
                None | Some(b' ') => false,
                Some(b'-') => true,
                _ => return Err(invalid()),
            };
            let text = line.get(4..).unwrap_or_default();
            if !text
                .iter()
                .all(|byte| byte.is_ascii_graphic() || *byte == b' ' || *byte == b'\t')
            {
                return Err(invalid());
            }
            lines.push(String::from_utf8(text.to_vec()).map_err(|_| invalid())?);
            if !continued {
                return Ok(SmtpReply {
                    code: parsed,
                    lines,
                });
            }
        }
        Err(SmtpFailure::new("email.reply_too_large"))
    }

    async fn command(&mut self, command: &[u8]) -> Result<SmtpReply, SmtpFailure> {
        self.write(command).await?;
        self.reply().await
    }

    async fn ehlo(&mut self) -> Result<SmtpReply, SmtpFailure> {
        let reply = self.command(b"EHLO frankenterm.local\r\n").await?;
        reply.require(250, "email.ehlo_rejected")?;
        Ok(reply)
    }
}

impl EmailNotifier {
    /// Validate configuration and prepare verified TLS before registration in
    /// a notification pipeline. No connection or email is sent here.
    pub fn new(mut config: EmailNotifyConfig) -> Result<Self, String> {
        config.validate()?;
        config.from = config.from.trim().to_string();
        config.to = config
            .to
            .iter()
            .map(|address| address.trim().to_string())
            .collect();
        let tls = if config.enabled && config.tls != EmailTlsMode::None {
            Some(
                TlsConnector::builder()
                    .with_webpki_roots()
                    .handshake_timeout(Duration::from_secs(config.timeout_secs))
                    .build()
                    .map_err(|_| "notifications.email TLS configuration unavailable".to_string())?,
            )
        } else {
            None
        };
        Ok(Self {
            config,
            tls,
            breaker: None,
        })
    }

    /// Put the relay behind a circuit breaker built from `config`. Only an
    /// unreachable or unresponsive relay counts as a failure; any SMTP reply,
    /// including a refusal, shows the relay is up.
    #[must_use]
    pub fn with_circuit_breaker(
        mut self,
        config: crate::circuit_breaker::CircuitBreakerConfig,
    ) -> Self {
        self.breaker = Some(std::sync::Mutex::new(
            crate::circuit_breaker::CircuitBreaker::with_name("notifications.email", config),
        ));
        self
    }

    fn relay_admitted(&self) -> bool {
        self.breaker.as_ref().is_none_or(|breaker| {
            breaker
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .allow()
        })
    }

    fn record_relay_health(&self, result: &Result<(), SmtpFailure>) {
        let Some(breaker) = &self.breaker else {
            return;
        };
        let mut breaker = breaker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match result {
            Ok(()) => breaker.record_success(),
            Err(failure)
                if matches!(
                    failure.code,
                    "email.connect_failed"
                        | "email.timeout"
                        | "email.tls_failed"
                        | "email.transport_failed"
                        | "email.transport_closed"
                ) =>
            {
                breaker.record_failure();
            }
            Err(failure) if failure.status != 0 => breaker.record_success(),
            // Cancellation, local refusals and indeterminate delivery say
            // nothing about the relay's health.
            Err(_) => {}
        }
    }

    async fn deliver(
        &self,
        cx: &crate::cx::Cx,
        payload: &NotificationPayload,
    ) -> NotificationDelivery {
        if !self.config.enabled {
            return email_delivery(Err(SmtpFailure::new("email.disabled")));
        }
        if let Err(error) = cx.checkpoint() {
            return email_delivery(Err(smtp_context_failure(cx, &error)));
        }
        if !self.relay_admitted() {
            return email_delivery(Err(SmtpFailure::new("email.circuit_open")));
        }
        let message = match self.render_message(payload) {
            Ok(message) => message,
            Err(error) => return email_delivery(Err(error)),
        };
        let mut data_started = false;
        let result = crate::runtime_async::timeout_with_cx(
            cx,
            Duration::from_secs(self.config.timeout_secs),
            crate::runtime_async::until_cancelled(
                cx,
                // The SMTP/TLS session state is ~20 KiB; keep it off the stack.
                Box::pin(self.transaction(cx, &message, &mut data_started)),
            ),
        )
        .await;
        let result = match result {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => Err(smtp_context_failure(cx, &error)),
            Err(_) => Err(SmtpFailure::new("email.timeout")),
        };
        let result = result.map_err(|failure| {
            if data_started && failure.status == 0 {
                SmtpFailure::new("email.delivery_indeterminate")
            } else {
                failure
            }
        });
        self.record_relay_health(&result);
        // A final 250 response is authoritative even if cancellation arrives
        // afterwards. There is no post-success checkpoint or automatic retry.
        email_delivery(result)
    }

    async fn transaction(
        &self,
        cx: &crate::cx::Cx,
        message: &[u8],
        data_started: &mut bool,
    ) -> Result<(), SmtpFailure> {
        cx.checkpoint()
            .map_err(|error| smtp_context_failure(cx, &error))?;
        let stream = TcpStream::connect((self.config.smtp_host.clone(), self.config.smtp_port))
            .await
            .map_err(|_| SmtpFailure::new("email.connect_failed"))?;
        let mut connection = SmtpConnection { stream, cx };
        match self.config.tls {
            EmailTlsMode::None => {
                connection
                    .reply()
                    .await?
                    .require(220, "email.greeting_rejected")?;
                self.submit(connection, message, data_started).await
            }
            EmailTlsMode::StartTls | EmailTlsMode::Tls => {
                if self.config.tls == EmailTlsMode::StartTls {
                    connection
                        .reply()
                        .await?
                        .require(220, "email.greeting_rejected")?;
                    if !connection.ehlo().await?.supports("STARTTLS") {
                        return Err(SmtpFailure::new("email.starttls_unavailable"));
                    }
                    connection
                        .command(b"STARTTLS\r\n")
                        .await?
                        .require(220, "email.starttls_rejected")?;
                }
                cx.checkpoint()
                    .map_err(|error| smtp_context_failure(cx, &error))?;
                let tls = self
                    .tls
                    .as_ref()
                    .ok_or_else(|| SmtpFailure::new("email.tls_unavailable"))?;
                let stream = tls
                    .connect(&self.config.smtp_host, connection.stream)
                    .await
                    .map_err(|_| SmtpFailure::new("email.tls_failed"))?;
                let mut connection = SmtpConnection { stream, cx };
                if self.config.tls == EmailTlsMode::Tls {
                    connection
                        .reply()
                        .await?
                        .require(220, "email.greeting_rejected")?;
                }
                // SMTP capabilities are renegotiated after STARTTLS. Never
                // authenticate from capabilities learned over plaintext.
                self.submit(connection, message, data_started).await
            }
        }
    }

    async fn submit<S: AsyncRead + AsyncWrite + Unpin>(
        &self,
        mut connection: SmtpConnection<'_, S>,
        message: &[u8],
        data_started: &mut bool,
    ) -> Result<(), SmtpFailure> {
        let capabilities = connection.ehlo().await?;
        if let (Some(username), Some(password)) = (&self.config.username, &self.config.password) {
            use zeroize::Zeroizing;
            let engine = base64::engine::general_purpose::STANDARD;
            if capabilities.auth_supports("PLAIN") {
                let credentials = Zeroizing::new(format!("\0{username}\0{password}"));
                let encoded = Zeroizing::new(engine.encode(credentials.as_bytes()));
                let command = Zeroizing::new(format!("AUTH PLAIN {}\r\n", encoded.as_str()));
                let mut reply = connection.command(command.as_bytes()).await?;
                if reply.code == 334 {
                    let response = Zeroizing::new(format!("{}\r\n", encoded.as_str()));
                    reply = connection.command(response.as_bytes()).await?;
                }
                reply.require(235, "email.authentication_rejected")?;
            } else if capabilities.auth_supports("LOGIN") {
                connection
                    .command(b"AUTH LOGIN\r\n")
                    .await?
                    .require(334, "email.authentication_rejected")?;
                for (value, expected) in [(username, 334), (password, 235)] {
                    let encoded = Zeroizing::new(engine.encode(value.as_bytes()));
                    let command = Zeroizing::new(format!("{}\r\n", encoded.as_str()));
                    connection
                        .command(command.as_bytes())
                        .await?
                        .require(expected, "email.authentication_rejected")?;
                }
            } else {
                return Err(SmtpFailure::new("email.authentication_unavailable"));
            }
        }
        connection
            .command(format!("MAIL FROM:<{}>\r\n", self.config.from).as_bytes())
            .await?
            .require(250, "email.sender_rejected")?;
        for recipient in &self.config.to {
            let reply = connection
                .command(format!("RCPT TO:<{recipient}>\r\n").as_bytes())
                .await?;
            if reply.code != 250 && reply.code != 251 && reply.code != 252 {
                // Drop the transaction before DATA if ANY recipient fails.
                // Accepted RCPTs alone are not delivered mail.
                return Err(SmtpFailure::rejected(
                    "email.recipient_rejected",
                    reply.code,
                ));
            }
        }
        connection
            .command(b"DATA\r\n")
            .await?
            .require(354, "email.data_rejected")?;
        connection
            .cx
            .checkpoint()
            .map_err(|error| smtp_context_failure(connection.cx, &error))?;
        *data_started = true;
        connection.write(message).await?;
        connection.write(b".\r\n").await?;
        let reply = connection.reply().await?;
        reply.require(250, "email.message_rejected")?;
        // Closing after the final 250 is safe. Waiting for QUIT could turn an
        // already accepted delivery into a timeout and invite duplicate mail.
        Ok(())
    }

    fn render_message(&self, payload: &NotificationPayload) -> Result<Vec<u8>, SmtpFailure> {
        let fields = [
            payload.event_type.as_str(),
            payload.timestamp.as_str(),
            payload.summary.as_str(),
            payload.description.as_str(),
            payload.severity.as_str(),
            payload.agent_type.as_str(),
            payload.quick_fix.as_deref().unwrap_or_default(),
        ];
        let total = fields
            .iter()
            .try_fold(0_usize, |total, field| total.checked_add(field.len()));
        if total.is_none_or(|total| total > MAX_PAYLOAD_BYTES) {
            return Err(SmtpFailure::new("email.payload_too_large"));
        }
        let redactor = crate::policy::Redactor::new();
        let clean = |text: &str| {
            redactor.redact(&crate::output::normalize_terminal_text_for_redaction(text))
        };
        let summary = clean(&payload.summary);
        let mut subject: String = format!("{} {summary}", clean(&self.config.subject_prefix))
            .chars()
            .map(|character| {
                if character.is_control() {
                    ' '
                } else {
                    character
                }
            })
            .collect();
        let mut boundary = subject.len().min(512);
        while !subject.is_char_boundary(boundary) {
            boundary -= 1;
        }
        subject.truncate(boundary);
        let body = format!(
            "{summary}\n\n{}\n\nSeverity: {}\nAgent: {}\nPane: {}\nEvent: {}\nDetected: {}\nSuppressed: {}\n{}",
            clean(&payload.description),
            clean(&payload.severity),
            clean(&payload.agent_type),
            payload.pane_id,
            clean(&payload.event_type),
            clean(&payload.timestamp),
            payload.suppressed_since_last,
            payload
                .quick_fix
                .as_deref()
                .map(|fix| format!("Suggested action: {}\n", clean(fix)))
                .unwrap_or_default(),
        );
        if body.len() > MAX_PAYLOAD_BYTES {
            return Err(SmtpFailure::new("email.payload_too_large"));
        }
        // Some providers reject mail without a Message-ID, and not every
        // submission relay adds one. The sender's validated domain scopes it.
        let id_domain = self
            .config
            .from
            .rsplit_once('@')
            .map_or("frankenterm.invalid", |(_, domain)| domain);
        let mut message = format!(
            "From: <{}>\r\nTo: <{}>\r\nDate: {}\r\nMessage-ID: <{:032x}@{}>\r\nSubject: {}\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=UTF-8\r\nContent-Transfer-Encoding: base64\r\n\r\n",
            self.config.from, self.config.to.join(">,\r\n <"),
            chrono::Utc::now().to_rfc2822(), rand::random::<u128>(), id_domain,
            encoded_subject(&subject),
        ).into_bytes();
        let encoded = base64::engine::general_purpose::STANDARD.encode(body.as_bytes());
        for line in encoded.as_bytes().chunks(76) {
            message.extend_from_slice(line);
            message.extend_from_slice(b"\r\n");
        }
        if message.len() > MAX_MESSAGE_BYTES {
            return Err(SmtpFailure::new("email.payload_too_large"));
        }
        Ok(message)
    }
}

fn smtp_context_failure(
    cx: &crate::cx::Cx,
    error: &crate::runtime_async::ContextError,
) -> SmtpFailure {
    use crate::runtime_async::ContextErrorKind;

    let code = match error.kind() {
        ContextErrorKind::DeadlineExceeded => "email.timeout",
        ContextErrorKind::CancelTimeout => "email.cancellation_cleanup_timeout",
        ContextErrorKind::PollQuotaExhausted => "email.poll_budget_exhausted",
        ContextErrorKind::CostQuotaExhausted => "email.cost_budget_exhausted",
        ContextErrorKind::Cancelled => {
            match cx.cancel_reason().map(|reason| reason.root_cause().kind) {
                Some(
                    crate::outcome::CancelKind::Timeout | crate::outcome::CancelKind::Deadline,
                ) => "email.timeout",
                Some(crate::outcome::CancelKind::PollQuota) => "email.poll_budget_exhausted",
                Some(crate::outcome::CancelKind::CostBudget) => "email.cost_budget_exhausted",
                _ => "email.cancelled",
            }
        }
        _ => "email.context_failed",
    };
    SmtpFailure::new(code)
}

fn encoded_subject(mut subject: &str) -> String {
    let mut words = Vec::new();
    while !subject.is_empty() {
        // 42 UTF-8 bytes produce at most 56 base64 characters; each RFC 2047
        // word stays under 75 characters and never splits a Unicode scalar.
        let mut boundary = subject.len().min(42);
        while !subject.is_char_boundary(boundary) {
            boundary -= 1;
        }
        let word =
            base64::engine::general_purpose::STANDARD.encode(&subject.as_bytes()[..boundary]);
        words.push(format!("=?UTF-8?B?{word}?="));
        subject = &subject[boundary..];
    }
    words.join("\r\n ")
}

fn email_delivery(result: Result<(), SmtpFailure>) -> NotificationDelivery {
    let (success, status_code, error) = match result {
        Ok(()) => (true, 250, None),
        Err(failure) => (false, failure.status, Some(failure.code.to_string())),
    };
    NotificationDelivery {
        sender: "email".to_string(),
        success,
        rate_limited: false,
        error: error.clone(),
        records: vec![NotificationDeliveryRecord {
            target: "email".to_string(),
            accepted: success,
            status_code,
            error,
        }],
    }
}

impl NotificationSender for EmailNotifier {
    fn name(&self) -> &'static str {
        "email"
    }

    fn send<'a>(&'a self, payload: &'a NotificationPayload) -> NotificationFuture<'a> {
        Box::pin(async move {
            let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
            self.deliver(&cx, payload).await
        })
    }

    fn send_with_cx<'a>(
        &'a self,
        cx: &'a crate::cx::Cx,
        payload: &'a NotificationPayload,
    ) -> NotificationFuture<'a> {
        Box::pin(self.deliver(cx, payload))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_config_disabled_is_ok() {
        let config = EmailNotifyConfig::default();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn email_config_requires_host_and_recipients() {
        let config = EmailNotifyConfig {
            enabled: true,
            from: "wa@example.com".to_string(),
            to: vec!["ops@example.com".to_string()],
            ..Default::default()
        };

        let err = config.validate().unwrap_err();
        assert!(err.contains("smtp_host"));
    }

    #[test]
    fn email_config_rejects_invalid_addresses() {
        let config = EmailNotifyConfig {
            enabled: true,
            smtp_host: "smtp.example.com".to_string(),
            from: "invalid".to_string(),
            to: vec!["ops@example.com".to_string()],
            ..Default::default()
        };

        let err = config.validate().unwrap_err();
        assert!(err.contains("from"));
    }

    // -----------------------------------------------------------------------
    // Batch 14 — PearlHeron wa-1u90p.7.1
    // -----------------------------------------------------------------------

    // ---- looks_like_email ----

    #[test]
    fn looks_like_email_valid_address() {
        assert!(looks_like_email("user@example.com"));
        assert!(looks_like_email("a@b.c"));
        assert!(looks_like_email("complex+tag@sub.domain.org"));
    }

    #[test]
    fn looks_like_email_empty_string() {
        assert!(!looks_like_email(""));
    }

    #[test]
    fn looks_like_email_whitespace_only() {
        assert!(!looks_like_email("   "));
        assert!(!looks_like_email("\t\n"));
    }

    #[test]
    fn looks_like_email_no_at_sign() {
        assert!(!looks_like_email("userexample.com"));
    }

    #[test]
    fn looks_like_email_multiple_at_signs() {
        assert!(!looks_like_email("user@@example.com"));
        assert!(!looks_like_email("a@b@c.com"));
    }

    #[test]
    fn looks_like_email_no_local_part() {
        assert!(!looks_like_email("@example.com"));
    }

    #[test]
    fn looks_like_email_no_domain() {
        assert!(!looks_like_email("user@"));
    }

    #[test]
    fn looks_like_email_domain_without_dot() {
        assert!(!looks_like_email("user@localhost"));
    }

    #[test]
    fn looks_like_email_leading_trailing_whitespace_tolerated() {
        // The function trims, so padded valid addresses pass
        assert!(looks_like_email("  user@example.com  "));
    }

    // ---- validate: valid full config ----

    #[test]
    fn email_config_valid_full_passes() {
        let config = EmailNotifyConfig {
            enabled: true,
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 587,
            username: Some("user@example.com".to_string()),
            password: Some("app-password".to_string()),
            from: "wa@example.com".to_string(),
            to: vec!["ops@example.com".to_string()],
            subject_prefix: "[ft]".to_string(),
            tls: EmailTlsMode::StartTls,
            timeout_secs: 10,
        };
        assert!(config.validate().is_ok());
    }

    // ---- validate: port zero ----

    #[test]
    fn email_config_rejects_port_zero() {
        let config = EmailNotifyConfig {
            enabled: true,
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 0,
            username: Some("user@example.com".to_string()),
            password: Some("pass".to_string()),
            from: "wa@example.com".to_string(),
            to: vec!["ops@example.com".to_string()],
            ..EmailNotifyConfig::default()
        };
        let err = config.validate().unwrap_err();
        assert!(err.contains("smtp_port"));
    }

    // ---- validate: empty to list ----

    #[test]
    fn email_config_rejects_empty_to_list() {
        let config = EmailNotifyConfig {
            enabled: true,
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 587,
            username: Some("user@example.com".to_string()),
            password: Some("pass".to_string()),
            from: "wa@example.com".to_string(),
            to: vec![],
            ..EmailNotifyConfig::default()
        };
        let err = config.validate().unwrap_err();
        assert!(err.contains("to"));
    }

    // ---- validate: invalid recipient in to list ----

    #[test]
    fn email_config_rejects_invalid_recipient() {
        let config = EmailNotifyConfig {
            enabled: true,
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 587,
            username: Some("user@example.com".to_string()),
            password: Some("pass".to_string()),
            from: "wa@example.com".to_string(),
            to: vec!["valid@example.com".to_string(), "not-an-email".to_string()],
            ..EmailNotifyConfig::default()
        };
        let err = config.validate().unwrap_err();
        assert!(err.contains("to[1]"));
    }

    // ---- validate: empty string in to list ----

    #[test]
    fn email_config_rejects_empty_string_in_to() {
        let config = EmailNotifyConfig {
            enabled: true,
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 587,
            username: Some("user@example.com".to_string()),
            password: Some("pass".to_string()),
            from: "wa@example.com".to_string(),
            to: vec![String::new()],
            ..EmailNotifyConfig::default()
        };
        let err = config.validate().unwrap_err();
        assert!(err.contains("to[0]"));
    }

    // ---- validate: username without password ----

    #[test]
    fn email_config_rejects_username_without_password() {
        let config = EmailNotifyConfig {
            enabled: true,
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 587,
            username: Some("user@example.com".to_string()),
            password: None,
            from: "wa@example.com".to_string(),
            to: vec!["ops@example.com".to_string()],
            ..EmailNotifyConfig::default()
        };
        let err = config.validate().unwrap_err();
        assert!(err.contains("username") || err.contains("password"));
    }

    // ---- validate: password without username ----

    #[test]
    fn email_config_rejects_password_without_username() {
        let config = EmailNotifyConfig {
            enabled: true,
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 587,
            username: None,
            password: Some("pass".to_string()),
            from: "wa@example.com".to_string(),
            to: vec!["ops@example.com".to_string()],
            ..EmailNotifyConfig::default()
        };
        let err = config.validate().unwrap_err();
        assert!(err.contains("username") || err.contains("password"));
    }

    // ---- validate: empty username string ----

    #[test]
    fn email_config_rejects_empty_username() {
        let config = EmailNotifyConfig {
            enabled: true,
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 587,
            username: Some("  ".to_string()),
            password: Some("pass".to_string()),
            from: "wa@example.com".to_string(),
            to: vec!["ops@example.com".to_string()],
            ..EmailNotifyConfig::default()
        };
        let err = config.validate().unwrap_err();
        assert!(err.contains("username"));
    }

    // ---- validate: empty password string ----

    #[test]
    fn email_config_rejects_empty_password() {
        let config = EmailNotifyConfig {
            enabled: true,
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 587,
            username: Some("user@example.com".to_string()),
            password: Some("  ".to_string()),
            from: "wa@example.com".to_string(),
            to: vec!["ops@example.com".to_string()],
            ..EmailNotifyConfig::default()
        };
        let err = config.validate().unwrap_err();
        assert!(err.contains("password"));
    }

    // ---- validate: no credentials is valid (anonymous SMTP) ----

    #[test]
    fn email_config_no_credentials_is_valid() {
        let config = EmailNotifyConfig {
            enabled: true,
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 25,
            username: None,
            password: None,
            from: "wa@example.com".to_string(),
            to: vec!["ops@example.com".to_string()],
            ..EmailNotifyConfig::default()
        };
        assert!(config.validate().is_ok());
    }

    // ---- TLS mode default ----

    #[test]
    fn email_tls_mode_default_is_starttls() {
        assert_eq!(EmailTlsMode::default(), EmailTlsMode::StartTls);
    }

    // ---- Default config values ----

    #[test]
    fn email_config_default_values() {
        let config = EmailNotifyConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.smtp_host, "");
        assert_eq!(config.smtp_port, 587);
        assert!(config.username.is_none());
        assert!(config.password.is_none());
        assert_eq!(config.from, "");
        assert_eq!(config.to, [] as [std::string::String; 0]);
        assert_eq!(config.subject_prefix, "[ft]");
        assert_eq!(config.tls, EmailTlsMode::StartTls);
        assert_eq!(config.timeout_secs, 10);
    }

    #[test]
    fn ft_wn3yc_email_default_subject_prefix_is_ft_branded() {
        let config = EmailNotifyConfig::default();
        assert_eq!(config.subject_prefix, "[ft]");
    }

    // ---- Serde roundtrip ----

    #[test]
    fn email_tls_mode_serde_roundtrip() {
        for mode in [
            EmailTlsMode::None,
            EmailTlsMode::StartTls,
            EmailTlsMode::Tls,
        ] {
            let json = serde_json::to_string(&mode).unwrap();
            let restored: EmailTlsMode = serde_json::from_str(&json).unwrap();
            assert_eq!(mode, restored);
        }
    }

    #[test]
    fn email_config_serde_roundtrip() {
        let config = EmailNotifyConfig {
            enabled: true,
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 465,
            username: Some("user".to_string()),
            password: Some("pass".to_string()),
            from: "wa@example.com".to_string(),
            to: vec!["a@b.com".to_string(), "c@d.com".to_string()],
            subject_prefix: "[test]".to_string(),
            tls: EmailTlsMode::Tls,
            timeout_secs: 30,
        };
        let json = serde_json::to_string(&config).unwrap();
        let restored: EmailNotifyConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.smtp_host, "smtp.example.com");
        assert_eq!(restored.smtp_port, 465);
        assert_eq!(restored.to.len(), 2);
        assert_eq!(restored.timeout_secs, 30);
    }

    // ---- validate: whitespace-only smtp_host ----

    #[test]
    fn email_config_rejects_whitespace_only_host() {
        let config = EmailNotifyConfig {
            enabled: true,
            smtp_host: "   ".to_string(),
            smtp_port: 587,
            from: "wa@example.com".to_string(),
            to: vec!["ops@example.com".to_string()],
            ..EmailNotifyConfig::default()
        };
        let err = config.validate().unwrap_err();
        assert!(err.contains("smtp_host"));
    }

    // ---- TLS mode serde values ----

    #[test]
    fn email_tls_mode_serde_snake_case() {
        assert_eq!(
            serde_json::to_string(&EmailTlsMode::None).unwrap(),
            "\"none\""
        );
        assert_eq!(
            serde_json::to_string(&EmailTlsMode::StartTls).unwrap(),
            "\"start_tls\""
        );
        assert_eq!(
            serde_json::to_string(&EmailTlsMode::Tls).unwrap(),
            "\"tls\""
        );
    }

    #[test]
    fn email_tls_mode_copy_semantics() {
        let a = EmailTlsMode::StartTls;
        let b = a; // Copy
        assert_eq!(a, b);
        // a is still usable after copy
        assert_eq!(a, EmailTlsMode::StartTls);
    }

    #[test]
    fn email_tls_mode_debug_format() {
        let dbg_none = format!("{:?}", EmailTlsMode::None);
        let dbg_start = format!("{:?}", EmailTlsMode::StartTls);
        let dbg_tls = format!("{:?}", EmailTlsMode::Tls);
        assert!(dbg_none.contains("None"), "Debug for None: {}", dbg_none);
        assert!(
            dbg_start.contains("StartTls"),
            "Debug for StartTls: {}",
            dbg_start
        );
        assert!(dbg_tls.contains("Tls"), "Debug for Tls: {}", dbg_tls);
    }

    #[test]
    fn email_config_clone_independence() {
        let original = EmailNotifyConfig {
            enabled: true,
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 587,
            from: "sender@example.com".to_string(),
            to: vec!["a@example.com".to_string()],
            ..EmailNotifyConfig::default()
        };
        let mut cloned = original.clone();
        cloned.smtp_host = "other.host.com".to_string();
        cloned.smtp_port = 465;
        cloned.to.push("b@example.com".to_string());

        // Original is unchanged
        assert_eq!(original.smtp_host, "smtp.example.com");
        assert_eq!(original.smtp_port, 587);
        assert_eq!(original.to.len(), 1);

        // Clone reflects mutations
        assert_eq!(cloned.smtp_host, "other.host.com");
        assert_eq!(cloned.smtp_port, 465);
        assert_eq!(cloned.to.len(), 2);
    }

    #[test]
    fn email_config_debug_contains_fields() {
        let config = EmailNotifyConfig {
            enabled: true,
            smtp_host: "smtp.test.com".to_string(),
            smtp_port: 465,
            from: "me@test.com".to_string(),
            to: vec!["you@test.com".to_string()],
            subject_prefix: "[alert]".to_string(),
            ..EmailNotifyConfig::default()
        };
        let dbg = format!("{:?}", config);
        assert!(dbg.contains("enabled"), "missing 'enabled' in: {}", dbg);
        assert!(dbg.contains("smtp_host"), "missing 'smtp_host' in: {}", dbg);
        assert!(dbg.contains("smtp_port"), "missing 'smtp_port' in: {}", dbg);
        assert!(dbg.contains("from"), "missing 'from' in: {}", dbg);
        assert!(
            dbg.contains("subject_prefix"),
            "missing 'subject_prefix' in: {}",
            dbg
        );
        assert!(dbg.contains("tls"), "missing 'tls' in: {}", dbg);
        assert!(
            dbg.contains("timeout_secs"),
            "missing 'timeout_secs' in: {}",
            dbg
        );
    }

    #[test]
    fn looks_like_email_numeric_local() {
        assert!(looks_like_email("123@example.com"));
    }

    #[test]
    fn looks_like_email_dots_in_local() {
        assert!(looks_like_email("user.name@example.com"));
    }

    #[test]
    fn looks_like_email_hyphen_in_domain() {
        assert!(looks_like_email("user@my-server.com"));
    }

    #[test]
    fn looks_like_email_just_at_and_dot() {
        assert!(looks_like_email("a@b.c"));
    }

    #[test]
    fn looks_like_email_unicode_local() {
        // The function only checks structure (@ and .), not charset
        assert!(looks_like_email("\u{00FC}ser@example.com"));
    }

    #[test]
    fn email_config_serde_missing_fields_get_defaults() {
        let json = r#"{"enabled": true}"#;
        let config: EmailNotifyConfig = serde_json::from_str(json).unwrap();
        assert!(config.enabled);
        assert_eq!(config.smtp_host, "");
        assert_eq!(config.smtp_port, 587);
        assert!(config.username.is_none());
        assert!(config.password.is_none());
        assert_eq!(config.from, "");
        assert_eq!(config.to, [] as [std::string::String; 0]);
        assert_eq!(config.subject_prefix, "[ft]");
        assert_eq!(config.tls, EmailTlsMode::StartTls);
        assert_eq!(config.timeout_secs, 10);
    }

    #[test]
    fn email_config_validate_max_port() {
        let config = EmailNotifyConfig {
            enabled: true,
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: u16::MAX,
            from: "wa@example.com".to_string(),
            to: vec!["ops@example.com".to_string()],
            ..EmailNotifyConfig::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn email_config_validate_multiple_valid_recipients() {
        let config = EmailNotifyConfig {
            enabled: true,
            smtp_host: "smtp.example.com".to_string(),
            from: "wa@example.com".to_string(),
            to: vec![
                "a@example.com".to_string(),
                "b@example.com".to_string(),
                "c@example.com".to_string(),
                "d@example.com".to_string(),
                "e@example.com".to_string(),
            ],
            ..EmailNotifyConfig::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn email_config_validate_from_with_whitespace_passes() {
        let config = EmailNotifyConfig {
            enabled: true,
            smtp_host: "smtp.example.com".to_string(),
            from: "  user@example.com  ".to_string(),
            to: vec!["ops@example.com".to_string()],
            ..EmailNotifyConfig::default()
        };
        // looks_like_email trims, so this should pass validation
        assert!(config.validate().is_ok());
    }

    #[test]
    fn email_tls_mode_all_variants_distinct() {
        assert_ne!(EmailTlsMode::None, EmailTlsMode::StartTls);
        assert_ne!(EmailTlsMode::StartTls, EmailTlsMode::Tls);
        assert_ne!(EmailTlsMode::None, EmailTlsMode::Tls);
    }

    #[test]
    fn email_config_default_not_enabled() {
        let config = EmailNotifyConfig::default();
        assert!(!config.enabled);
    }
}

#[cfg(test)]
mod transport_tests {
    use super::*;
    use std::io::{Read, Write};

    const SENTINEL: &str = "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

    fn config(port: u16, tls: EmailTlsMode) -> EmailNotifyConfig {
        EmailNotifyConfig {
            enabled: true,
            smtp_host: "127.0.0.1".to_string(),
            smtp_port: port,
            from: "sender@example.com".to_string(),
            to: vec!["ops@example.com".to_string()],
            tls,
            timeout_secs: 2,
            ..Default::default()
        }
    }

    fn payload() -> NotificationPayload {
        NotificationPayload {
            event_type: "test.email".to_string(),
            pane_id: 7,
            timestamp: "2026-11-24T12:00:00Z".to_string(),
            summary: "Agent needs attention".to_string(),
            description: format!("Résumé ready.\n.\nCredential: {SENTINEL}"),
            severity: "warning".to_string(),
            agent_type: "codex".to_string(),
            confidence: 1.0,
            quick_fix: Some(format!("inspect --token {SENTINEL}")),
            suppressed_since_last: 3,
        }
    }

    fn run_smtp_test(future: impl std::future::Future<Output = ()>) {
        use crate::runtime_async::CompatRuntime;
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("SMTP test runtime");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(future);
        }));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(runtime)));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::runtime_async::clear_runtime_handle();
        }));
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    // Loopback only, with finite accept and I/O waits even on a failing client.
    fn fixture<T: Send + 'static>(
        serve: impl FnOnce(std::net::TcpStream) -> T + Send + 'static,
    ) -> (u16, std::thread::JoinHandle<T>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let task = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(4);
            let stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "SMTP fixture accept timeout"
                        );
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("SMTP fixture accept failed: {error}"),
                }
            };
            // BSD-derived systems (macOS) hand the accepted socket the
            // listener's non-blocking mode; the fixture's reads must block.
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(4)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(4)))
                .unwrap();
            serve(stream)
        });
        (port, task)
    }

    fn wire_line(stream: &mut impl Read) -> String {
        let mut line = Vec::new();
        loop {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            line.push(byte[0]);
            assert!(line.len() <= MAX_MESSAGE_BYTES);
            if byte[0] == b'\n' {
                return String::from_utf8(line).unwrap();
            }
        }
    }

    fn reply(stream: &mut impl Write, response: &[u8]) {
        stream.write_all(response).unwrap();
        stream.flush().unwrap();
    }

    fn expect_command(stream: &mut (impl Read + Write), expected: &str, response: &[u8]) {
        assert_eq!(wire_line(stream), expected);
        reply(stream, response);
    }

    fn read_message(stream: &mut impl Read) -> String {
        let mut message = String::new();
        loop {
            let line = wire_line(stream);
            if line == ".\r\n" {
                return message;
            }
            message.push_str(&line);
            assert!(message.len() <= MAX_MESSAGE_BYTES);
        }
    }

    fn accept_envelope(stream: &mut (impl Read + Write)) {
        expect_command(
            stream,
            "MAIL FROM:<sender@example.com>\r\n",
            b"250 sender ok\r\n",
        );
        expect_command(
            stream,
            "RCPT TO:<ops@example.com>\r\n",
            b"250 recipient ok\r\n",
        );
        expect_command(stream, "DATA\r\n", b"354 send message\r\n");
    }

    fn expect_closed(stream: &mut impl Read) {
        let mut byte = [0];
        match stream.read(&mut byte) {
            Ok(0) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe
                ) => {}
            other => panic!("SMTP client did not close cleanly: {other:?}"),
        }
    }

    fn decoded_body(message: &str) -> String {
        let (_, body) = message.split_once("\r\n\r\n").unwrap();
        let encoded = body.replace("\r\n", "");
        String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn ft_9b41l_plain_relay_delivers_redacted_mime_after_all_recipients() {
        let (port, server) = fixture(|mut stream| {
            reply(&mut stream, b"220 loopback fixture\r\n");
            expect_command(
                &mut stream,
                "EHLO frankenterm.local\r\n",
                b"250-fixture\r\n250 SIZE 262144\r\n",
            );
            expect_command(
                &mut stream,
                "MAIL FROM:<sender@example.com>\r\n",
                b"250 ok\r\n",
            );
            expect_command(&mut stream, "RCPT TO:<ops@example.com>\r\n", b"250 ok\r\n");
            expect_command(
                &mut stream,
                "RCPT TO:<backup@example.com>\r\n",
                b"251 forwarded\r\n",
            );
            expect_command(&mut stream, "DATA\r\n", b"354 continue\r\n");
            let message = read_message(&mut stream);
            reply(&mut stream, b"250 accepted\r\n");
            expect_closed(&mut stream);
            message
        });
        run_smtp_test(async {
            let mut config = config(port, EmailTlsMode::None);
            config.to.push("backup@example.com".to_string());
            let delivery = EmailNotifier::new(config).unwrap().send(&payload()).await;
            assert!(delivery.success, "{delivery:?}");
            assert_eq!(delivery.records[0].status_code, 250);
            assert_eq!(delivery.records[0].target, "email");
        });
        let message = server.join().unwrap();
        assert!(message.contains("To: <ops@example.com>,\r\n <backup@example.com>"));
        let body = decoded_body(&message);
        assert!(body.contains("Résumé ready.\n.\n"));
        assert!(body.contains("Suggested action:"));
        assert!(body.contains("Detected: 2026-11-24T12:00:00Z"));
        assert!(!body.contains(SENTINEL));
    }

    #[test]
    fn ft_9b41l_recipient_rejection_prevents_data_and_hides_server_text() {
        let (port, server) = fixture(|mut stream| {
            reply(&mut stream, b"220 fixture\r\n");
            expect_command(
                &mut stream,
                "EHLO frankenterm.local\r\n",
                b"250 fixture\r\n",
            );
            expect_command(
                &mut stream,
                "MAIL FROM:<sender@example.com>\r\n",
                b"250 ok\r\n",
            );
            expect_command(&mut stream, "RCPT TO:<ops@example.com>\r\n", b"250 ok\r\n");
            expect_command(
                &mut stream,
                "RCPT TO:<denied@example.com>\r\n",
                format!("550 private server detail {SENTINEL}\r\n").as_bytes(),
            );
            expect_closed(&mut stream);
        });
        run_smtp_test(async {
            let mut config = config(port, EmailTlsMode::None);
            config.to.push("denied@example.com".to_string());
            let delivery = EmailNotifier::new(config).unwrap().send(&payload()).await;
            assert!(!delivery.success);
            assert_eq!(delivery.error.as_deref(), Some("email.recipient_rejected"));
            assert_eq!(delivery.records[0].status_code, 550);
            let diagnostics = format!("{delivery:?}");
            assert!(!diagnostics.contains(SENTINEL));
            assert!(!diagnostics.contains("denied@example.com"));
        });
        server.join().unwrap();
    }

    #[test]
    fn ft_9b41l_starttls_cannot_downgrade_to_plaintext_authentication() {
        let (port, server) = fixture(|mut stream| {
            reply(&mut stream, b"220 fixture\r\n");
            expect_command(
                &mut stream,
                "EHLO frankenterm.local\r\n",
                b"250-fixture\r\n250 AUTH PLAIN LOGIN\r\n",
            );
            expect_closed(&mut stream);
        });
        run_smtp_test(async {
            let mut config = config(port, EmailTlsMode::StartTls);
            config.username = Some("synthetic-user".to_string());
            config.password = Some("synthetic-password".to_string());
            let delivery = EmailNotifier::new(config).unwrap().send(&payload()).await;
            assert_eq!(
                delivery.error.as_deref(),
                Some("email.starttls_unavailable")
            );
        });
        server.join().unwrap();
    }

    #[test]
    fn ft_9b41l_smtp_replies_have_strict_finite_bounds() {
        for (response, expected) in [
            (
                b"220-first\r\n250 wrong-code\r\n".to_vec(),
                "email.invalid_reply",
            ),
            (b"220 bare-LF\n".to_vec(), "email.invalid_reply"),
            (
                format!("220 {}\r\n", "x".repeat(MAX_REPLY_LINE_BYTES)).into_bytes(),
                "email.reply_too_large",
            ),
            (
                b"220-continuation\r\n".repeat(MAX_REPLY_LINES + 1),
                "email.reply_too_large",
            ),
        ] {
            let (port, server) = fixture(move |mut stream| {
                reply(&mut stream, &response);
                expect_closed(&mut stream);
            });
            run_smtp_test(async {
                let notifier = EmailNotifier::new(config(port, EmailTlsMode::None)).unwrap();
                let delivery = notifier.send(&payload()).await;
                assert_eq!(delivery.error.as_deref(), Some(expected));
            });
            server.join().unwrap();
        }
    }

    #[test]
    fn ft_9b41l_lost_or_invalid_data_acknowledgment_is_indeterminate() {
        for final_reply in [
            None,
            Some("334 unexpected challenge\r\n"),
            Some("550 rejected\r\n"),
        ] {
            let (port, server) = fixture(move |mut stream| {
                reply(&mut stream, b"220 fixture\r\n");
                expect_command(
                    &mut stream,
                    "EHLO frankenterm.local\r\n",
                    b"250 fixture\r\n",
                );
                accept_envelope(&mut stream);
                let message = read_message(&mut stream);
                if let Some(final_reply) = final_reply {
                    reply(&mut stream, final_reply.as_bytes());
                    expect_closed(&mut stream);
                }
                message
            });
            run_smtp_test(async {
                let notifier = EmailNotifier::new(config(port, EmailTlsMode::None)).unwrap();
                let delivery = notifier.send(&payload()).await;
                let expected = if final_reply == Some("550 rejected\r\n") {
                    "email.message_rejected"
                } else {
                    "email.delivery_indeterminate"
                };
                assert_eq!(delivery.error.as_deref(), Some(expected));
                assert!(!delivery.success);
            });
            assert!(!server.join().unwrap().is_empty());
        }
    }

    #[test]
    fn ft_9b41l_total_timeout_drops_a_stalled_connection() {
        let (port, server) = fixture(|mut stream| expect_closed(&mut stream));
        run_smtp_test(async {
            let mut config = config(port, EmailTlsMode::None);
            config.timeout_secs = 1;
            let started = std::time::Instant::now();
            let delivery = EmailNotifier::new(config).unwrap().send(&payload()).await;
            assert_eq!(delivery.error.as_deref(), Some("email.timeout"));
            assert!(started.elapsed() < Duration::from_secs(3));
        });
        server.join().unwrap();
    }

    #[test]
    fn ft_9b41l_direct_cancellation_interrupts_stalled_io_without_leaking_reason() {
        run_smtp_test(async {
            let cx = crate::cx::for_request();
            let cancel = cx.clone();
            let (port, server) = fixture(move |mut stream| {
                cancel.cancel_with(crate::outcome::CancelKind::User, Some(SENTINEL));
                expect_closed(&mut stream);
            });
            let notifier = EmailNotifier::new(config(port, EmailTlsMode::None)).unwrap();
            let delivery = notifier.send_with_cx(&cx, &payload()).await;
            assert_eq!(delivery.error.as_deref(), Some("email.cancelled"));
            assert!(!format!("{delivery:?}").contains(SENTINEL));
            server.join().unwrap();
        });
    }

    #[test]
    fn ft_9b41l_cancellation_after_data_does_not_claim_a_definitive_failure() {
        run_smtp_test(async {
            let cx = crate::cx::for_request();
            let cancel = cx.clone();
            let (port, server) = fixture(move |mut stream| {
                reply(&mut stream, b"220 fixture\r\n");
                expect_command(
                    &mut stream,
                    "EHLO frankenterm.local\r\n",
                    b"250 fixture\r\n",
                );
                accept_envelope(&mut stream);
                let message = read_message(&mut stream);
                cancel.cancel_with(crate::outcome::CancelKind::User, Some(SENTINEL));
                expect_closed(&mut stream);
                message
            });
            let notifier = EmailNotifier::new(config(port, EmailTlsMode::None)).unwrap();
            let delivery = notifier.send_with_cx(&cx, &payload()).await;
            assert_eq!(
                delivery.error.as_deref(),
                Some("email.delivery_indeterminate")
            );
            assert!(!format!("{delivery:?}").contains(SENTINEL));
            assert!(!server.join().unwrap().is_empty());
        });
    }

    #[test]
    fn ft_9b41l_precancelled_or_exhausted_context_never_connects() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let notifier = EmailNotifier::new(config(
            listener.local_addr().unwrap().port(),
            EmailTlsMode::None,
        ))
        .unwrap();
        run_smtp_test(async {
            let cx = crate::cx::for_request();
            cx.cancel_with(crate::outcome::CancelKind::User, Some(SENTINEL));
            let delivery = notifier.send_with_cx(&cx, &payload()).await;
            assert_eq!(delivery.error.as_deref(), Some("email.cancelled"));
            let exhausted =
                crate::cx::Cx::for_request_with_budget(crate::cx::Budget::new().with_poll_quota(0));
            let delivery = notifier.send_with_cx(&exhausted, &payload()).await;
            assert_eq!(
                delivery.error.as_deref(),
                Some("email.poll_budget_exhausted")
            );
        });
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn ft_9b41l_configuration_rejects_injection_plaintext_credentials_and_unbounded_values() {
        for mailbox in [
            "a@example.com\r\nBcc: stolen@example.com",
            "a b@example.com",
            "a@example.com>",
            "a..b@example.com",
        ] {
            let mut config = config(2525, EmailTlsMode::None);
            config.from = mailbox.to_string();
            assert!(config.validate().is_err());
        }
        let mut plaintext_auth = config(2525, EmailTlsMode::None);
        plaintext_auth.username = Some("user".to_string());
        plaintext_auth.password = Some("password".to_string());
        assert!(plaintext_auth.validate().unwrap_err().contains("TLS"));
        for timeout in [0, 121, u64::MAX] {
            let mut config = config(2525, EmailTlsMode::None);
            config.timeout_secs = timeout;
            assert!(config.validate().unwrap_err().contains("timeout_secs"));
        }
        let mut recipients = config(2525, EmailTlsMode::None);
        recipients.to = vec!["ops@example.com".to_string(); MAX_RECIPIENTS + 1];
        assert!(recipients.validate().is_err());
        recipients.to = vec!["üser@example.com".to_string()];
        assert!(recipients.validate().unwrap_err().contains("SMTPUTF8"));
        for host in [
            "https://mail.example.com",
            "mail.example.com:25",
            "mail.example.com\r\n",
        ] {
            let mut config = config(2525, EmailTlsMode::None);
            config.smtp_host = host.to_string();
            assert!(config.validate().is_err());
        }
    }

    #[test]
    fn ft_9b41l_configuration_debug_never_exposes_sensitive_values() {
        let mut config = config(2525, EmailTlsMode::Tls);
        config.username = Some("synthetic-private-user".to_string());
        config.password = Some(SENTINEL.to_string());
        let diagnostics = format!("{config:?}");
        for hidden in [
            SENTINEL,
            "synthetic-private-user",
            "sender@example.com",
            "ops@example.com",
            "127.0.0.1",
        ] {
            assert!(!diagnostics.contains(hidden));
        }
    }

    #[test]
    fn ft_9b41l_mime_bounds_unicode_and_header_injection() {
        let notifier = EmailNotifier::new(config(2525, EmailTlsMode::None)).unwrap();
        let mut payload = payload();
        payload.summary = "Résumé\r\nBcc: stolen@example.net".repeat(30);
        let message = String::from_utf8(notifier.render_message(&payload).unwrap()).unwrap();
        assert!(!message.contains("\r\nBcc:"));
        assert!(!decoded_body(&message).contains(SENTINEL));
        let message_id = |message: &str| {
            let ids: Vec<String> = message
                .split("\r\n")
                .filter_map(|line| line.strip_prefix("Message-ID: "))
                .map(str::to_owned)
                .collect();
            assert_eq!(ids.len(), 1, "exactly one Message-ID header");
            ids.into_iter().next().unwrap()
        };
        let first = message_id(&message);
        assert!(
            first.starts_with('<') && first.ends_with("@example.com>"),
            "{first}"
        );
        let again = String::from_utf8(notifier.render_message(&payload).unwrap()).unwrap();
        assert_ne!(first, message_id(&again), "each message gets its own ID");
        let subject = encoded_subject(&"Résumé".repeat(30));
        let decoded: Vec<u8> = subject
            .split("\r\n ")
            .flat_map(|word| {
                assert!(word.len() <= 75);
                let encoded = word
                    .strip_prefix("=?UTF-8?B?")
                    .unwrap()
                    .strip_suffix("?=")
                    .unwrap();
                base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .unwrap()
            })
            .collect();
        assert_eq!(String::from_utf8(decoded).unwrap(), "Résumé".repeat(30));
        payload.description = "x".repeat(MAX_PAYLOAD_BYTES + 1);
        assert_eq!(
            notifier.render_message(&payload).unwrap_err().code,
            "email.payload_too_large"
        );
    }

    #[test]
    fn ft_9b41l_only_an_unreachable_relay_opens_the_breaker() {
        let (port, task) = fixture(|mut stream| reply(&mut stream, b"554 no service\r\n"));
        let notifier = EmailNotifier::new(config(port, EmailTlsMode::None))
            .unwrap()
            .with_circuit_breaker(crate::circuit_breaker::CircuitBreakerConfig::new(
                1,
                1,
                Duration::from_secs(300),
            ));
        run_smtp_test(async move {
            let cx = crate::cx::for_testing();
            let payload = payload();
            // A refusal is an answer: the relay is up and the breaker stays
            // closed even at a threshold of one.
            let refused = notifier.deliver(&cx, &payload).await;
            assert_eq!(refused.error.as_deref(), Some("email.greeting_rejected"));
            task.join().unwrap();
            // The fixture has closed its listener, so the relay is unreachable.
            let unreachable = notifier.deliver(&cx, &payload).await;
            assert_eq!(unreachable.error.as_deref(), Some("email.connect_failed"));
            // Now open: skipped without connecting, until the cooldown ends.
            let skipped = notifier.deliver(&cx, &payload).await;
            assert_eq!(skipped.error.as_deref(), Some("email.circuit_open"));
            assert!(!skipped.success);
        });
    }

    #[cfg(feature = "distributed")]
    fn fixture_tls(name: &str) -> (std::sync::Arc<rustls::ServerConfig>, TlsConnector) {
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec![name.to_string()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        let private_key = rustls::pki_types::PrivatePkcs8KeyDer::from(key.serialize_der());
        let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
        let server = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert.der().clone()], private_key.into())
            .unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert.der().clone()).unwrap();
        let client = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        (std::sync::Arc::new(server), TlsConnector::new(client))
    }

    #[cfg(feature = "distributed")]
    #[test]
    fn ft_9b41l_verified_tls_supports_starttls_plain_and_implicit_login() {
        for (mode, mechanism, challenge) in [
            (EmailTlsMode::StartTls, "PLAIN", false),
            (EmailTlsMode::StartTls, "PLAIN", true),
            (EmailTlsMode::Tls, "LOGIN", false),
        ] {
            let (server_config, connector) = fixture_tls("127.0.0.1");
            let (port, server) = fixture(move |mut stream| {
                if mode == EmailTlsMode::StartTls {
                    reply(&mut stream, b"220 fixture\r\n");
                    // Post-TLS EHLO must replace these LOGIN capabilities.
                    expect_command(
                        &mut stream,
                        "EHLO frankenterm.local\r\n",
                        b"250-fixture\r\n250-STARTTLS\r\n250 AUTH LOGIN\r\n",
                    );
                    expect_command(&mut stream, "STARTTLS\r\n", b"220 begin TLS\r\n");
                }
                let mut stream = rustls::StreamOwned::new(
                    rustls::ServerConnection::new(server_config).unwrap(),
                    stream,
                );
                if mode == EmailTlsMode::Tls {
                    reply(&mut stream, b"220 TLS fixture\r\n");
                }
                expect_command(
                    &mut stream,
                    "EHLO frankenterm.local\r\n",
                    format!("250-fixture\r\n250 AUTH {mechanism}\r\n").as_bytes(),
                );
                let engine = base64::engine::general_purpose::STANDARD;
                if mechanism == "PLAIN" {
                    let encoded = engine.encode(b"\0synthetic-user\0synthetic-password");
                    assert_eq!(wire_line(&mut stream), format!("AUTH PLAIN {encoded}\r\n"));
                    if challenge {
                        reply(&mut stream, b"334 \r\n");
                        assert_eq!(wire_line(&mut stream), format!("{encoded}\r\n"));
                    }
                    reply(&mut stream, b"235 authenticated\r\n");
                } else {
                    expect_command(&mut stream, "AUTH LOGIN\r\n", b"334 VXNlcm5hbWU6\r\n");
                    expect_command(
                        &mut stream,
                        &format!("{}\r\n", engine.encode(b"synthetic-user")),
                        b"334 UGFzc3dvcmQ6\r\n",
                    );
                    expect_command(
                        &mut stream,
                        &format!("{}\r\n", engine.encode(b"synthetic-password")),
                        b"235 authenticated\r\n",
                    );
                }
                accept_envelope(&mut stream);
                let message = read_message(&mut stream);
                reply(&mut stream, b"250 queued\r\n");
                message
            });
            run_smtp_test(async {
                let mut config = config(port, mode);
                config.username = Some("synthetic-user".to_string());
                config.password = Some("synthetic-password".to_string());
                let mut notifier = EmailNotifier::new(config).unwrap();
                // Trust only the loopback fixture certificate for this test.
                notifier.tls = Some(connector);
                let delivery = notifier.send(&payload()).await;
                assert!(delivery.success, "{delivery:?}");
            });
            assert!(!decoded_body(&server.join().unwrap()).contains(SENTINEL));
        }
    }

    #[cfg(feature = "distributed")]
    #[test]
    fn ft_9b41l_tls_rejects_untrusted_certificates_and_wrong_server_names_before_auth() {
        for wrong_name in [false, true] {
            let name = if wrong_name { "localhost" } else { "127.0.0.1" };
            let (server_config, trusted_connector) = fixture_tls(name);
            let (port, server) = fixture(move |stream| {
                let mut stream = rustls::StreamOwned::new(
                    rustls::ServerConnection::new(server_config).unwrap(),
                    stream,
                );
                let mut byte = [0];
                assert!(
                    !matches!(stream.read(&mut byte), Ok(1)),
                    "TLS rejection must precede any SMTP command"
                );
            });
            run_smtp_test(async {
                let mut config = config(port, EmailTlsMode::Tls);
                config.username = Some("synthetic-user".to_string());
                config.password = Some(SENTINEL.to_string());
                let mut notifier = EmailNotifier::new(config).unwrap();
                if wrong_name {
                    notifier.tls = Some(trusted_connector);
                }
                let delivery = notifier.send(&payload()).await;
                assert_eq!(delivery.error.as_deref(), Some("email.tls_failed"));
                assert!(!format!("{delivery:?}").contains(SENTINEL));
            });
            server.join().unwrap();
        }
    }

    #[cfg(feature = "distributed")]
    #[test]
    fn ft_9b41l_tls_authentication_rejection_is_content_free_and_stops_before_mail() {
        let (server_config, connector) = fixture_tls("127.0.0.1");
        let (port, server) = fixture(move |stream| {
            let mut stream = rustls::StreamOwned::new(
                rustls::ServerConnection::new(server_config).unwrap(),
                stream,
            );
            reply(&mut stream, b"220 TLS fixture\r\n");
            expect_command(
                &mut stream,
                "EHLO frankenterm.local\r\n",
                b"250-fixture\r\n250 AUTH PLAIN\r\n",
            );
            assert!(wire_line(&mut stream).starts_with("AUTH PLAIN "));
            reply(
                &mut stream,
                format!("535 private authentication detail {SENTINEL}\r\n").as_bytes(),
            );
            let mut byte = [0];
            // TLS peers may close TCP without close_notify after failure.
            assert!(!matches!(stream.read(&mut byte), Ok(1)));
        });
        run_smtp_test(async {
            let mut config = config(port, EmailTlsMode::Tls);
            config.username = Some("synthetic-user".to_string());
            config.password = Some(SENTINEL.to_string());
            let mut notifier = EmailNotifier::new(config).unwrap();
            notifier.tls = Some(connector);
            let delivery = notifier.send(&payload()).await;
            assert_eq!(
                delivery.error.as_deref(),
                Some("email.authentication_rejected")
            );
            assert_eq!(delivery.records[0].status_code, 535);
            assert!(!format!("{delivery:?}").contains(SENTINEL));
        });
        server.join().unwrap();
    }
}
