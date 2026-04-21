//! Platform-neutral notification sink abstraction.
//!
//! Core defines the trait; platform impls live where they belong:
//! - Desktop impl: `system::notifications::DesktopNotifications` (uses notify-rust)
//! - Android impl: `chatter-ui/src/platform/android/notifications.rs` (uses JNI)
//!
//! The UI crate injects the right impl via `ClientBuilder::notification_sink`.

use crate::domain::notification::{Notification, NotificationPriority};
use crate::error::SystemError;

pub trait NotificationSink: Send + Sync {
    fn show(
        &self,
        title: &str,
        body: &str,
        priority: NotificationPriority,
    ) -> Result<(), SystemError>;

    fn show_domain(&self, notification: &Notification) -> Result<(), SystemError> {
        self.show(&notification.title, &notification.body, NotificationPriority::Normal)
    }

    fn init(&self) -> Result<(), SystemError> {
        Ok(())
    }
}

/// No-op sink. Used when no platform sink is injected (headless / tests / Android before UI wires it).
pub struct NoopNotificationSink;

impl NotificationSink for NoopNotificationSink {
    fn show(
        &self,
        _title: &str,
        _body: &str,
        _priority: NotificationPriority,
    ) -> Result<(), SystemError> {
        Ok(())
    }
}
