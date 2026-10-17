//! Platform-neutral notification sink abstraction.
//!
//! Core defines the trait; platform impls live where they belong:
//! - Desktop impl with `desktop-native`: `system::notifications::DesktopNotifications` (uses notify-rust)
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
        self.show(
            &notification.title,
            &notification.body,
            NotificationPriority::Normal,
        )
    }

    /// Show the notification for one conversation (Signal: one per thread).
    /// `conversation` is a stable key; showing it again replaces the earlier
    /// one, and [`Self::cancel_conversation`] dismisses it. The default can
    /// replace nothing and falls back to a plain notification.
    fn show_conversation(
        &self,
        _conversation: &str,
        title: &str,
        body: &str,
        priority: NotificationPriority,
    ) -> Result<(), SystemError> {
        self.show(title, body, priority)
    }

    /// Dismiss `conversation`'s notification: it was read, here or on another
    /// of our devices. No-op by default and when none is showing.
    fn cancel_conversation(&self, _conversation: &str) -> Result<(), SystemError> {
        Ok(())
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

    /// Whether message notifications should make a sound (Settings ›
    /// Notifications › Sound). Only desktop sinks honour it; Android's sound
    /// and vibration belong to the OS notification channel, which the user
    /// edits in the system settings instead. No-op by default.
    fn set_sound(&self, _enabled: bool) {}

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
