mod dbus;
mod macos;
mod windows;

use std::fmt;
use std::sync::Arc;

#[derive(Clone)]
pub struct ToastNotificationAction {
    label: String,
    callback: Arc<dyn Fn() + Send + Sync + 'static>,
}

impl ToastNotificationAction {
    pub fn new<F>(label: impl Into<String>, callback: F) -> Self
    where
        F: Fn() + Send + Sync + 'static,
    {
        Self {
            label: label.into(),
            callback: Arc::new(callback),
        }
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn invoke(&self) {
        (self.callback)();
    }
}

impl fmt::Debug for ToastNotificationAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToastNotificationAction")
            .field("label", &self.label)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub struct ToastNotification {
    pub title: String,
    pub message: String,
    pub url: Option<String>,
    pub action: Option<ToastNotificationAction>,
    pub timeout: Option<std::time::Duration>,
}

impl ToastNotification {
    pub fn show(self) {
        show(self)
    }

    pub fn has_activation_action(&self) -> bool {
        self.url.is_some() || self.action.is_some()
    }

    pub fn activation_label(&self) -> &str {
        self.action
            .as_ref()
            .map(ToastNotificationAction::label)
            .unwrap_or("Show")
    }

    pub fn activate(&self) {
        if let Some(action) = self.action.as_ref() {
            action.invoke();
        } else if let Some(url) = self.url.as_ref() {
            frankenterm_open_url::open_url(url);
        }
    }
}

#[cfg(windows)]
use crate::windows as backend;
#[cfg(all(not(target_os = "macos"), not(windows)))]
use dbus as backend;
#[cfg(target_os = "macos")]
use macos as backend;

mod nop {
    use super::*;

    #[allow(dead_code)]
    pub fn show_notif(_: ToastNotification) -> Result<(), Box<dyn std::error::Error>> {
        Ok(())
    }
}

pub fn show(notif: ToastNotification) {
    if let Err(err) = backend::show_notif(notif) {
        log::error!("Failed to show notification: {}", err);
    }
}

pub fn persistent_toast_notification_with_click_to_open_url(title: &str, message: &str, url: &str) {
    show(ToastNotification {
        title: title.to_string(),
        message: message.to_string(),
        url: Some(url.to_string()),
        action: None,
        timeout: None,
    });
}

pub fn persistent_toast_notification_with_action(
    title: &str,
    message: &str,
    action: ToastNotificationAction,
) {
    show(ToastNotification {
        title: title.to_string(),
        message: message.to_string(),
        url: None,
        action: Some(action),
        timeout: None,
    });
}

pub fn persistent_toast_notification(title: &str, message: &str) {
    show(ToastNotification {
        title: title.to_string(),
        message: message.to_string(),
        url: None,
        action: None,
        timeout: None,
    });
}

#[cfg(target_os = "macos")]
pub use macos::initialize as macos_initialize;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn passive_notification_has_no_activation_action() {
        let notif = ToastNotification {
            title: "Title".to_string(),
            message: "Body".to_string(),
            url: None,
            action: None,
            timeout: None,
        };

        assert!(!notif.has_activation_action());
        assert_eq!(notif.activation_label(), "Show");
    }

    #[test]
    fn url_notification_uses_default_show_activation() {
        let notif = ToastNotification {
            title: "Title".to_string(),
            message: "Body".to_string(),
            url: Some("https://example.invalid".to_string()),
            action: None,
            timeout: None,
        };

        assert!(notif.has_activation_action());
        assert_eq!(notif.activation_label(), "Show");
    }

    #[test]
    fn callback_notification_carries_label_and_invokes_action() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_action = calls.clone();
        let action = ToastNotificationAction::new("Focus", move || {
            calls_for_action.fetch_add(1, Ordering::SeqCst);
        });
        let notif = ToastNotification {
            title: "Title".to_string(),
            message: "Body".to_string(),
            url: None,
            action: Some(action),
            timeout: None,
        };

        assert!(notif.has_activation_action());
        assert_eq!(notif.activation_label(), "Focus");
        notif.activate();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
