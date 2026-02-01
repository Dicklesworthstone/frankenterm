//! Desktop notification delivery via native OS notification tools.
//!
//! Delivers event notifications as native desktop alerts on macOS
//! (osascript), Linux (notify-send), and Windows (PowerShell toast).
//!
//! # Platform detection
//!
//! The notifier auto-detects the platform at construction time and
//! selects the appropriate command. If the tool is not available, the
//! notifier returns a graceful fallback error instead of panicking.

use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::event_templates::RenderedEvent;
use crate::notifications::{
    NotificationDelivery, NotificationFuture, NotificationPayload, NotificationSender,
};
use crate::patterns::Detection;

// ============================================================================
// Urgency mapping
// ============================================================================

/// Urgency level for desktop notifications.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Urgency {
    Low,
    Normal,
    Critical,
}

impl std::fmt::Display for Urgency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Low => write!(f, "low"),
            Self::Normal => write!(f, "normal"),
            Self::Critical => write!(f, "critical"),
        }
    }
}

/// Map a detection's severity to a desktop notification urgency.
#[must_use]
pub fn severity_to_urgency(severity: crate::patterns::Severity) -> Urgency {
    match severity {
        crate::patterns::Severity::Info => Urgency::Low,
        crate::patterns::Severity::Warning => Urgency::Normal,
        crate::patterns::Severity::Critical => Urgency::Critical,
    }
}

fn urgency_from_str(severity: &str) -> Urgency {
    match severity {
        "critical" => Urgency::Critical,
        "warning" => Urgency::Normal,
        _ => Urgency::Low,
    }
}

// ============================================================================
// Platform backend
// ============================================================================

/// Which platform notification backend to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotifyBackend {
    /// macOS: uses `osascript` for native Notification Center.
    MacOs,
    /// Linux: uses `notify-send` (libnotify).
    Linux,
    /// Windows: uses PowerShell toast notifications.
    Windows,
    /// No suitable backend found — notifications will be no-ops.
    None,
}

impl NotifyBackend {
    /// Auto-detect the backend for the current platform.
    #[must_use]
    pub fn detect() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else {
            Self::None
        }
    }
}

impl std::fmt::Display for NotifyBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MacOs => write!(f, "macos (osascript)"),
            Self::Linux => write!(f, "linux (notify-send)"),
            Self::Windows => write!(f, "windows (powershell)"),
            Self::None => write!(f, "none"),
        }
    }
}

// ============================================================================
// Desktop notification config
// ============================================================================

/// Desktop notification configuration.
///
/// ```toml
/// [notifications.desktop]
/// enabled = true
/// sound = false
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DesktopNotifyConfig {
    /// Enable desktop notifications.
    pub enabled: bool,

    /// Play a sound with the notification (platform-dependent).
    pub sound: bool,
}

impl Default for DesktopNotifyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            sound: false,
        }
    }
}

// ============================================================================
// Notification command builder
// ============================================================================

/// A platform-specific notification command ready for execution.
#[derive(Debug, Clone)]
pub struct NotifyCommand {
    /// The program to run.
    pub program: String,
    /// Command-line arguments.
    pub args: Vec<String>,
}

/// Build the notification command for the given backend.
///
/// Returns `None` if the backend is `None` (no suitable tool).
#[must_use]
pub fn build_command(
    backend: NotifyBackend,
    title: &str,
    body: &str,
    urgency: Urgency,
    sound: bool,
) -> Option<NotifyCommand> {
    match backend {
        NotifyBackend::MacOs => Some(build_macos_command(title, body, sound)),
        NotifyBackend::Linux => Some(build_linux_command(title, body, urgency)),
        NotifyBackend::Windows => Some(build_windows_command(title, body)),
        NotifyBackend::None => None,
    }
}

fn build_macos_command(title: &str, body: &str, sound: bool) -> NotifyCommand {
    // osascript -e 'display notification "body" with title "title" [sound name "default"]'
    let sound_clause = if sound { " sound name \"default\"" } else { "" };
    let script = format!(
        "display notification \"{}\" with title \"{}\"{}",
        escape_applescript(body),
        escape_applescript(title),
        sound_clause
    );
    NotifyCommand {
        program: "osascript".to_string(),
        args: vec!["-e".to_string(), script],
    }
}

fn build_linux_command(title: &str, body: &str, urgency: Urgency) -> NotifyCommand {
    let urgency_str = match urgency {
        Urgency::Low => "low",
        Urgency::Normal => "normal",
        Urgency::Critical => "critical",
    };
    NotifyCommand {
        program: "notify-send".to_string(),
        args: vec![
            title.to_string(),
            body.to_string(),
            format!("--urgency={urgency_str}"),
            "--app-name=wa".to_string(),
        ],
    }
}

fn build_windows_command(title: &str, body: &str) -> NotifyCommand {
    let script = format!(
        "[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null; \
         $xml = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02); \
         $text = $xml.GetElementsByTagName('text'); \
         $text.Item(0).AppendChild($xml.CreateTextNode('{title}')) | Out-Null; \
         $text.Item(1).AppendChild($xml.CreateTextNode('{body}')) | Out-Null; \
         $toast = [Windows.UI.Notifications.ToastNotification]::new($xml); \
         [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('wa').Show($toast)",
        title = escape_powershell(title),
        body = escape_powershell(body)
    );
    NotifyCommand {
        program: "powershell".to_string(),
        args: vec!["-Command".to_string(), script],
    }
}

/// Escape characters for AppleScript string literals.
fn escape_applescript(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Escape characters for PowerShell string interpolation.
fn escape_powershell(s: &str) -> String {
    s.replace('\'', "''")
}

// ============================================================================
// Desktop notifier
// ============================================================================

/// Result of a desktop notification attempt.
#[derive(Debug, Clone, Serialize)]
pub struct DesktopDeliveryResult {
    /// Backend used.
    pub backend: String,
    /// Whether the notification was sent successfully.
    pub success: bool,
    /// Error message (if failed).
    pub error: Option<String>,
}

/// Desktop notification sender.
///
/// Builds and executes platform-specific notification commands.
#[derive(Debug, Clone)]
pub struct DesktopNotifier {
    backend: NotifyBackend,
    config: DesktopNotifyConfig,
}

impl DesktopNotifier {
    /// Create a notifier with auto-detected backend.
    #[must_use]
    pub fn new(config: DesktopNotifyConfig) -> Self {
        Self {
            backend: NotifyBackend::detect(),
            config,
        }
    }

    /// Create a notifier with a specific backend (useful for testing).
    #[must_use]
    pub fn with_backend(backend: NotifyBackend, config: DesktopNotifyConfig) -> Self {
        Self { backend, config }
    }

    /// The detected (or configured) backend.
    #[must_use]
    pub fn backend(&self) -> NotifyBackend {
        self.backend
    }

    /// Whether desktop notifications are enabled and a backend is available.
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.config.enabled && self.backend != NotifyBackend::None
    }

    /// Send a desktop notification for a detection event.
    ///
    /// Returns `Ok(result)` with delivery info, or `Err` if command
    /// building failed (e.g., no backend).
    pub fn notify(
        &self,
        detection: &Detection,
        pane_id: u64,
        rendered: &RenderedEvent,
        suppressed_since_last: u64,
    ) -> DesktopDeliveryResult {
        if !self.config.enabled {
            return DesktopDeliveryResult {
                backend: self.backend.to_string(),
                success: false,
                error: Some("desktop notifications disabled".to_string()),
            };
        }

        let payload = NotificationPayload::from_detection(
            detection,
            pane_id,
            rendered,
            suppressed_since_last,
        );
        let urgency = severity_to_urgency(detection.severity);

        let title = format!("wa: {}", payload.summary);
        let mut body = format!(
            "[{}] {} (pane {})",
            payload.severity, payload.event_type, payload.pane_id
        );
        if payload.suppressed_since_last > 0 {
            body.push_str(&format!(" (+{} suppressed)", payload.suppressed_since_last));
        }

        let Some(cmd) = build_command(self.backend, &title, &body, urgency, self.config.sound)
        else {
            return DesktopDeliveryResult {
                backend: self.backend.to_string(),
                success: false,
                error: Some("no notification backend available".to_string()),
            };
        };

        tracing::debug!(
            backend = %self.backend,
            program = %cmd.program,
            rule_id = %detection.rule_id,
            pane_id,
            "sending desktop notification"
        );

        match Command::new(&cmd.program).args(&cmd.args).output() {
            Ok(output) if output.status.success() => {
                tracing::info!(
                    backend = %self.backend,
                    rule_id = %detection.rule_id,
                    "desktop notification sent"
                );
                DesktopDeliveryResult {
                    backend: self.backend.to_string(),
                    success: true,
                    error: None,
                }
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                tracing::warn!(
                    backend = %self.backend,
                    status = ?output.status,
                    stderr = %stderr,
                    "desktop notification command failed"
                );
                DesktopDeliveryResult {
                    backend: self.backend.to_string(),
                    success: false,
                    error: Some(format!(
                        "exit {}: {}",
                        output.status.code().unwrap_or(-1),
                        stderr.trim()
                    )),
                }
            }
            Err(e) => {
                tracing::warn!(
                    backend = %self.backend,
                    error = %e,
                    "desktop notification command not found"
                );
                DesktopDeliveryResult {
                    backend: self.backend.to_string(),
                    success: false,
                    error: Some(format!("command not found: {e}")),
                }
            }
        }
    }

    /// Send a desktop notification with a custom title/body.
    pub fn notify_message(
        &self,
        title: &str,
        body: &str,
        urgency: Urgency,
    ) -> DesktopDeliveryResult {
        if !self.config.enabled {
            return DesktopDeliveryResult {
                backend: self.backend.to_string(),
                success: false,
                error: Some("desktop notifications disabled".to_string()),
            };
        }

        let Some(cmd) = build_command(self.backend, title, body, urgency, self.config.sound) else {
            return DesktopDeliveryResult {
                backend: self.backend.to_string(),
                success: false,
                error: Some("no notification backend available".to_string()),
            };
        };

        tracing::debug!(
            backend = %self.backend,
            program = %cmd.program,
            "sending desktop notification"
        );

        match Command::new(&cmd.program).args(&cmd.args).output() {
            Ok(output) if output.status.success() => DesktopDeliveryResult {
                backend: self.backend.to_string(),
                success: true,
                error: None,
            },
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                DesktopDeliveryResult {
                    backend: self.backend.to_string(),
                    success: false,
                    error: Some(format!(
                        "exit {}: {}",
                        output.status.code().unwrap_or(-1),
                        stderr.trim()
                    )),
                }
            }
            Err(e) => DesktopDeliveryResult {
                backend: self.backend.to_string(),
                success: false,
                error: Some(format!("command not found: {e}")),
            },
        }
    }
}

impl NotificationSender for DesktopNotifier {
    fn name(&self) -> &'static str {
        "desktop"
    }

    fn send<'a>(&'a self, payload: &'a NotificationPayload) -> NotificationFuture<'a> {
        Box::pin(async move {
            if !self.config.enabled {
                return NotificationDelivery {
                    sender: self.name().to_string(),
                    success: false,
                    rate_limited: false,
                    error: Some("desktop notifications disabled".to_string()),
                    records: Vec::new(),
                };
            }

            let urgency = urgency_from_str(&payload.severity);
            let title = format!("wa: {}", payload.summary);
            let mut body = format!(
                "[{}] {} (pane {})",
                payload.severity, payload.event_type, payload.pane_id
            );
            if payload.suppressed_since_last > 0 {
                body.push_str(&format!(" (+{} suppressed)", payload.suppressed_since_last));
            }

            let Some(cmd) = build_command(self.backend, &title, &body, urgency, self.config.sound)
            else {
                return NotificationDelivery {
                    sender: self.name().to_string(),
                    success: false,
                    rate_limited: false,
                    error: Some("no notification backend available".to_string()),
                    records: Vec::new(),
                };
            };

            tracing::debug!(
                backend = %self.backend,
                program = %cmd.program,
                "sending desktop notification"
            );

            let result = match Command::new(&cmd.program).args(&cmd.args).output() {
                Ok(output) if output.status.success() => NotificationDelivery {
                    sender: self.name().to_string(),
                    success: true,
                    rate_limited: false,
                    error: None,
                    records: Vec::new(),
                },
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    NotificationDelivery {
                        sender: self.name().to_string(),
                        success: false,
                        rate_limited: false,
                        error: Some(format!(
                            "exit {}: {}",
                            output.status.code().unwrap_or(-1),
                            stderr.trim()
                        )),
                        records: Vec::new(),
                    }
                }
                Err(e) => NotificationDelivery {
                    sender: self.name().to_string(),
                    success: false,
                    rate_limited: false,
                    error: Some(format!("command not found: {e}")),
                    records: Vec::new(),
                },
            };

            if result.success {
                tracing::info!(backend = %self.backend, "desktop notification sent");
            } else if let Some(ref err) = result.error {
                tracing::warn!(backend = %self.backend, error = %err, "desktop notification failed");
            }

            result
        })
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patterns::{AgentType, Severity};

    fn test_detection() -> Detection {
        Detection {
            rule_id: "core.codex:usage_reached".to_string(),
            agent_type: AgentType::Codex,
            event_type: "usage_reached".to_string(),
            severity: Severity::Warning,
            confidence: 0.95,
            extracted: serde_json::json!({}),
            matched_text: "Rate limit exceeded".to_string(),
            span: (0, 19),
        }
    }

    fn test_rendered() -> RenderedEvent {
        RenderedEvent {
            summary: "Codex hit usage limit on Pane 3".to_string(),
            description: "The Codex CLI reported a usage limit.".to_string(),
            suggestions: Vec::new(),
            severity: Severity::Warning,
        }
    }

    // ---- Urgency mapping ----

    #[test]
    fn severity_maps_to_correct_urgency() {
        assert_eq!(severity_to_urgency(Severity::Info), Urgency::Low);
        assert_eq!(severity_to_urgency(Severity::Warning), Urgency::Normal);
        assert_eq!(severity_to_urgency(Severity::Critical), Urgency::Critical);
    }

    // ---- Backend detection ----

    #[test]
    fn backend_detect_returns_platform() {
        let backend = NotifyBackend::detect();
        // On Linux CI, should be Linux; on macOS, MacOs
        assert_ne!(backend, NotifyBackend::None);
    }

    #[test]
    fn backend_display() {
        assert_eq!(format!("{}", NotifyBackend::MacOs), "macos (osascript)");
        assert_eq!(format!("{}", NotifyBackend::Linux), "linux (notify-send)");
        assert_eq!(
            format!("{}", NotifyBackend::Windows),
            "windows (powershell)"
        );
        assert_eq!(format!("{}", NotifyBackend::None), "none");
    }

    // ---- Command building ----

    #[test]
    fn build_macos_command_structure() {
        let cmd = build_command(
            NotifyBackend::MacOs,
            "wa: test",
            "Event body",
            Urgency::Normal,
            false,
        )
        .unwrap();

        assert_eq!(cmd.program, "osascript");
        assert_eq!(cmd.args.len(), 2);
        assert_eq!(cmd.args[0], "-e");
        assert!(cmd.args[1].contains("display notification"));
        assert!(cmd.args[1].contains("Event body"));
        assert!(cmd.args[1].contains("wa: test"));
        assert!(!cmd.args[1].contains("sound name"));
    }

    #[test]
    fn build_macos_command_with_sound() {
        let cmd = build_command(
            NotifyBackend::MacOs,
            "wa: test",
            "body",
            Urgency::Normal,
            true,
        )
        .unwrap();
        assert!(cmd.args[1].contains("sound name \"default\""));
    }

    #[test]
    fn build_linux_command_structure() {
        let cmd = build_command(
            NotifyBackend::Linux,
            "wa: test",
            "Event body",
            Urgency::Critical,
            false,
        )
        .unwrap();

        assert_eq!(cmd.program, "notify-send");
        assert!(cmd.args.contains(&"wa: test".to_string()));
        assert!(cmd.args.contains(&"Event body".to_string()));
        assert!(cmd.args.contains(&"--urgency=critical".to_string()));
        assert!(cmd.args.contains(&"--app-name=wa".to_string()));
    }

    #[test]
    fn build_linux_urgency_levels() {
        let low = build_command(NotifyBackend::Linux, "t", "b", Urgency::Low, false).unwrap();
        assert!(low.args.contains(&"--urgency=low".to_string()));

        let normal = build_command(NotifyBackend::Linux, "t", "b", Urgency::Normal, false).unwrap();
        assert!(normal.args.contains(&"--urgency=normal".to_string()));

        let crit = build_command(NotifyBackend::Linux, "t", "b", Urgency::Critical, false).unwrap();
        assert!(crit.args.contains(&"--urgency=critical".to_string()));
    }

    #[test]
    fn build_windows_command_structure() {
        let cmd = build_command(
            NotifyBackend::Windows,
            "wa: test",
            "Event body",
            Urgency::Normal,
            false,
        )
        .unwrap();

        assert_eq!(cmd.program, "powershell");
        assert_eq!(cmd.args[0], "-Command");
        assert!(cmd.args[1].contains("ToastNotification"));
        assert!(cmd.args[1].contains("wa: test"));
        assert!(cmd.args[1].contains("Event body"));
    }

    #[test]
    fn build_none_backend_returns_none() {
        let cmd = build_command(NotifyBackend::None, "t", "b", Urgency::Normal, false);
        assert!(cmd.is_none());
    }

    // ---- Escaping ----

    #[test]
    fn escape_applescript_quotes() {
        assert_eq!(escape_applescript(r#"say "hello""#), r#"say \"hello\""#);
        assert_eq!(escape_applescript(r"back\slash"), r"back\\slash");
    }

    #[test]
    fn escape_powershell_quotes() {
        assert_eq!(escape_powershell("it's"), "it''s");
    }

    // ---- Config ----

    #[test]
    fn config_defaults() {
        let c = DesktopNotifyConfig::default();
        assert!(!c.enabled); // disabled by default
        assert!(!c.sound);
    }

    #[test]
    fn config_toml_roundtrip() {
        let toml_str = r"
enabled = true
sound = true
";
        let c: DesktopNotifyConfig = toml::from_str(toml_str).expect("parse");
        assert!(c.enabled);
        assert!(c.sound);
    }

    // ---- Notifier ----

    #[test]
    fn notifier_disabled_returns_error() {
        let notifier = DesktopNotifier::new(DesktopNotifyConfig::default());
        let result = notifier.notify(&test_detection(), 3, &test_rendered(), 0);
        assert!(!result.success);
        assert!(result.error.unwrap().contains("disabled"));
    }

    #[test]
    fn notifier_none_backend_returns_error() {
        let notifier = DesktopNotifier::with_backend(
            NotifyBackend::None,
            DesktopNotifyConfig {
                enabled: true,
                sound: false,
            },
        );
        let result = notifier.notify(&test_detection(), 3, &test_rendered(), 0);
        assert!(!result.success);
        assert!(result.error.unwrap().contains("no notification backend"));
    }

    #[test]
    fn notifier_is_available() {
        // Enabled + real backend → available
        let n1 = DesktopNotifier::with_backend(
            NotifyBackend::Linux,
            DesktopNotifyConfig {
                enabled: true,
                sound: false,
            },
        );
        assert!(n1.is_available());

        // Disabled → not available
        let n2 = DesktopNotifier::with_backend(
            NotifyBackend::Linux,
            DesktopNotifyConfig {
                enabled: false,
                sound: false,
            },
        );
        assert!(!n2.is_available());

        // None backend → not available
        let n3 = DesktopNotifier::with_backend(
            NotifyBackend::None,
            DesktopNotifyConfig {
                enabled: true,
                sound: false,
            },
        );
        assert!(!n3.is_available());
    }

    #[test]
    fn notifier_backend_accessor() {
        let n = DesktopNotifier::with_backend(NotifyBackend::Linux, DesktopNotifyConfig::default());
        assert_eq!(n.backend(), NotifyBackend::Linux);
    }

    #[test]
    fn urgency_display() {
        assert_eq!(format!("{}", Urgency::Low), "low");
        assert_eq!(format!("{}", Urgency::Normal), "normal");
        assert_eq!(format!("{}", Urgency::Critical), "critical");
    }

    #[test]
    fn delivery_result_serde() {
        let r = DesktopDeliveryResult {
            backend: "linux (notify-send)".to_string(),
            success: true,
            error: None,
        };
        let json = serde_json::to_string(&r).expect("serialize");
        assert!(json.contains("linux"));
        assert!(json.contains("true"));
    }
}
