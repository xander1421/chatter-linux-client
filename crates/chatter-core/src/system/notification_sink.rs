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

    /// Show an incoming-call notification with Accept/Decline actions.
    ///
    /// Platforms that support actionable call notifications (Android) override
    /// this to post a full-screen, ringing notification with buttons that
    /// accept/decline `call_id` without opening the app. The default falls back
    /// to a plain urgent notification (desktop relies on the in-app ring UI).
    fn show_incoming_call(
        &self,
        _call_id: &str,
        title: &str,
        body: &str,
    ) -> Result<(), SystemError> {
        self.show(title, body, NotificationPriority::Urgent)
    }

    /// Dismiss a previously shown incoming-call notification (call was
    /// accepted, declined, withdrawn, or ended). No-op by default.
    fn cancel_incoming_call(&self, _call_id: &str) -> Result<(), SystemError> {
        Ok(())
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
