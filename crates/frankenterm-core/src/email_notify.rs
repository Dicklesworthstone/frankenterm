//! SMTP email notification configuration.

use serde::{Deserialize, Serialize};

/// TLS mode for SMTP delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmailTlsMode {
    /// No TLS (plain SMTP).
    None,
    /// Upgrade to TLS via STARTTLS (recommended).
    StartTls,
    /// Implicit TLS (SMTPS).
    Tls,
}

impl Default for EmailTlsMode {
    fn default() -> Self {
        Self::StartTls
    }
}

/// Email notification configuration.
///
/// ```toml
/// [notifications.email]
/// enabled = true
/// smtp_host = "smtp.example.com"
/// smtp_port = 587
/// tls = "starttls"
/// username = "user@example.com"
/// password = "app-password"
/// from = "wa@example.com"
/// to = ["ops@example.com"]
/// subject_prefix = "[wa]"
/// timeout_secs = 10
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
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

    /// SMTP timeout in seconds.
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
            subject_prefix: "[wa]".to_string(),
            tls: EmailTlsMode::StartTls,
            timeout_secs: 10,
        }
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

        if self.smtp_port == 0 {
            return Err("notifications.email.smtp_port must be >= 1".to_string());
        }

        if self.from.trim().is_empty() {
            return Err("notifications.email.from must not be empty".to_string());
        }

        if !looks_like_email(&self.from) {
            return Err("notifications.email.from must be a valid email address".to_string());
        }

        if self.to.is_empty() {
            return Err("notifications.email.to must not be empty".to_string());
        }

        for (idx, addr) in self.to.iter().enumerate() {
            if addr.trim().is_empty() {
                return Err(format!("notifications.email.to[{idx}] must not be empty"));
            }
            if !looks_like_email(addr) {
                return Err(format!(
                    "notifications.email.to[{idx}] must be a valid email address"
                ));
            }
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
        let mut config = EmailNotifyConfig::default();
        config.enabled = true;
        config.from = "wa@example.com".to_string();
        config.to = vec!["ops@example.com".to_string()];

        let err = config.validate().unwrap_err();
        assert!(err.contains("smtp_host"));
    }

    #[test]
    fn email_config_rejects_invalid_addresses() {
        let mut config = EmailNotifyConfig::default();
        config.enabled = true;
        config.smtp_host = "smtp.example.com".to_string();
        config.from = "invalid".to_string();
        config.to = vec!["ops@example.com".to_string()];

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
            subject_prefix: "[wa]".to_string(),
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
        assert!(config.smtp_host.is_empty());
        assert_eq!(config.smtp_port, 587);
        assert!(config.username.is_none());
        assert!(config.password.is_none());
        assert!(config.from.is_empty());
        assert!(config.to.is_empty());
        assert_eq!(config.subject_prefix, "[wa]");
        assert_eq!(config.tls, EmailTlsMode::StartTls);
        assert_eq!(config.timeout_secs, 10);
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
}
