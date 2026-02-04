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
}
