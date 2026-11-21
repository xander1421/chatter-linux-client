//! Notification service.
//!
//! Privacy-aware notification delivery across platforms:
//! - **Desktop (Linux/Windows/macOS)**: notify-rust → OS notification daemon
//! - **Android**: JNI → NotificationManager.notify()
//!
//! # Privacy Levels (Signal Desktop approach)
//!
//! | Level              | Title               | Body                | Metadata leaked |
//! |--------------------|---------------------|---------------------|-----------------|
//! | `none` (default)   | "Chatter"           | "New message"       | Nothing         |
//! | `name_only`        | "Alice"             | "New message"       | Sender identity |
//! | `name_and_message` | "Alice"             | "hey are you free?" | Sender + content|
//!
//! For sealed sender messages and anything in a secret chat (anonymous DM or
//! group) — messages and reactions alike — the notification is always the
//! generic one: no sender, no text, no emoji, whatever the detail level. See
//! [`listener::is_private_notification`].
mod account_keys;
mod listener;

pub use account_keys::is_opaque_notification_key;
#[cfg(feature = "mesh")]
pub use listener::spawn_mesh_notification_listener;
pub(crate) use listener::{is_private_notification, should_notify_message};

use crate::domain::notification::NotificationPriority;
use crate::error::SystemError;
use crate::system::notification_sink::{NoopNotificationSink, NotificationSink};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

/// How much detail to show in OS notifications.
///
/// Default: `None` — maximum privacy, same as Signal Desktop default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationDetailLevel {
    /// "Chatter: New message" — no sender, no content.
    /// Nothing leaks to OS notification system.
    None,
    /// "Alice: New message" — sender name shown, no content.
    /// Sender identity leaks to OS.
    NameOnly,
    /// "Alice: hey are you free?" — sender name + message preview.
    /// Sender identity + message content leak to OS.
    NameAndMessage,
}

impl NotificationDetailLevel {
    /// Parse from settings string value.
    pub fn from_str(s: &str) -> Self {
        match s {
            "name_only" => Self::NameOnly,
            "name_and_message" => Self::NameAndMessage,
            _ => Self::None,
        }
    }

    /// Convert to settings string value.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::NameOnly => "name_only",
            Self::NameAndMessage => "name_and_message",
        }
    }
}

/// Notification service — privacy-aware cross-platform notification delivery.
///
/// Business logic (privacy filtering, sealed-sender handling) lives here.
/// Platform delivery is delegated to a `NotificationSink` injected at construction.
pub struct NotificationService {
    sink: Arc<dyn NotificationSink>,
    detail_level: std::sync::RwLock<NotificationDetailLevel>,
    account_keys: Option<AccountNotificationKeys>,
    message_notifications: Mutex<MessageNotifications>,
    /// While it reports locked, every notification is the generic one.
    app_lock: std::sync::OnceLock<Arc<crate::services::app_lock::AppLock>>,
}

#[derive(Default)]
struct MessageNotifications {
    entries: HashMap<String, Vec<MessageNotification>>,
    lock_generation: u64,
}

struct MessageNotification {
    id: crate::domain::ids::MessageId,
    sender_name: Option<String>,
    message_preview: Option<String>,
    private: bool,
    priority: NotificationPriority,
}

struct AccountNotificationKeys {
    settings: Arc<crate::storage::repositories::settings_repo::SettingsRepository>,
    auth: crate::state::AuthState,
    creation_lock: Mutex<()>,
}

impl NotificationService {
    /// Create a notification service with the given platform sink.
    pub fn new(sink: Arc<dyn NotificationSink>) -> Self {
        Self {
            sink,
            detail_level: std::sync::RwLock::new(NotificationDetailLevel::None),
            account_keys: None,
            message_notifications: Mutex::default(),
            app_lock: std::sync::OnceLock::new(),
        }
    }

    pub fn with_account_keys(
        sink: Arc<dyn NotificationSink>,
        settings: Arc<crate::storage::repositories::settings_repo::SettingsRepository>,
        auth: crate::state::AuthState,
    ) -> Self {
        let mut service = Self::new(sink);
        service.account_keys = Some(AccountNotificationKeys {
            settings,
            auth,
            creation_lock: Mutex::new(()),
        });
        service
    }

    /// Create a notification service with a no-op sink (tests / headless).
    pub fn noop() -> Self {
        Self::new(Arc::new(NoopNotificationSink))
    }

    /// Set the notification detail level.
    pub fn set_detail_level(&self, level: NotificationDetailLevel) {
        *self.detail_level.write().unwrap() = level;
    }

    /// Apply the notification-sound preference to the platform sink.
    pub fn set_sound_enabled(&self, enabled: bool) {
        self.sink.set_sound(enabled);
    }

    /// Get the current detail level setting.
    pub fn detail_level(&self) -> NotificationDetailLevel {
        *self.detail_level.read().unwrap()
    }

    /// Gate notifications on the app lock.
    pub fn set_app_lock(self: &Arc<Self>, lock: Arc<crate::services::app_lock::AppLock>) {
        let mut changes = lock.subscribe_to_changes();
        if self.app_lock.set(lock).is_err() {
            return;
        }
        let service = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                let Some(service) = service.upgrade() else {
                    break;
                };
                let remaining = service.with_message_privacy(|_, _, remaining| remaining);
                drop(service);
                if let Some(remaining) = remaining {
                    tokio::select! {
                        _ = wait_for_lock_check(remaining) => {}
                        result = changes.changed() => if result.is_err() { break; },
                    }
                } else if changes.changed().await.is_err() {
                    break;
                }
            }
        });
    }

    fn message_posts(&self) -> MutexGuard<'_, MessageNotifications> {
        self.message_notifications
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn with_app_lock_state<T>(&self, f: impl FnOnce((bool, u64, Option<Duration>)) -> T) -> T {
        match self.app_lock.get() {
            Some(lock) => lock.with_notification_lock_state(f),
            None => f((false, 0, None)),
        }
    }

    fn with_message_privacy<T>(
        &self,
        f: impl FnOnce(&mut MessageNotifications, NotificationDetailLevel, Option<Duration>) -> T,
    ) -> T {
        let mut posts = self.message_posts();
        // Keep the lock state stable through the sink call and preserve the
        // same mutex order in both posting and the cancellation watcher.
        self.with_app_lock_state(|(locked, generation, remaining)| {
            let remaining = self.cancel_locked_previews(&mut posts, generation, remaining);
            f(&mut posts, self.detail_for_lock_state(locked), remaining)
        })
    }

    fn cancel_locked_previews(
        &self,
        posts: &mut MessageNotifications,
        generation: u64,
        mut remaining: Option<Duration>,
    ) -> Option<Duration> {
        if posts.lock_generation != generation {
            match self.deliver(|sink| sink.cancel_message_notifications()) {
                Ok(cancelled) => {
                    if cancelled {
                        posts.entries.clear();
                    }
                    posts.lock_generation = generation;
                }
                Err(error) => {
                    tracing::warn!(%error, "Could not clear message notifications on app lock");
                    let retry = Duration::from_secs(5);
                    remaining = Some(remaining.map_or(retry, |deadline| deadline.min(retry)));
                }
            }
        }
        remaining
    }

    /// Whether the app lock is engaged right now.
    pub fn app_locked(&self) -> bool {
        self.app_lock.get().is_some_and(|lock| lock.is_locked())
    }

    /// The level notifications are rendered at: the setting, or `None` while
    /// the app is locked (Signal-Android `NotificationItem.getPrimaryTextActual`
    /// shows "Locked message" while `KeyCachingService.isLocked`). Re-read on
    /// every post, so a lock that expires in the background applies at once.
    pub fn effective_detail_level(&self) -> NotificationDetailLevel {
        self.detail_for_lock_state(self.app_locked())
    }

    fn detail_for_lock_state(&self, locked: bool) -> NotificationDetailLevel {
        if locked {
            NotificationDetailLevel::None
        } else {
            self.detail_level()
        }
    }

    /// Show `conversation_id`'s notification for an incoming message,
    /// replacing the conversation's earlier one.
    ///
    /// Respects privacy settings; `private` (see [`listener::is_private_notification`])
    /// forces the generic notification at every level.
    pub fn show_message_notification(
        &self,
        conversation_id: &crate::domain::ids::ChannelId,
        sender_name: Option<&str>,
        message_preview: Option<&str>,
        private: bool,
        priority: NotificationPriority,
    ) -> Result<(), SystemError> {
        let key = self.current_key(conversation_id)?;
        self.with_message_privacy(|notifications, level, _| {
            let (title, body) = Self::message_text(level, sender_name, message_preview, private);
            self.deliver(|sink| sink.show_conversation(&key, &title, &body, priority))?;
            // Requests and reactions replace this conversation's OS notification.
            // Its earlier message entries can no longer be restored.
            notifications.entries.remove(&key);
            Ok(())
        })
    }

    fn message_text(
        level: NotificationDetailLevel,
        sender_name: Option<&str>,
        message_preview: Option<&str>,
        private: bool,
    ) -> (String, String) {
        let new_message = chatter_i18n::tr("New message");
        let someone = || chatter_i18n::tr("Someone");
        match (level, private) {
            // Sealed sender / secret chat: nothing but "New message", ever.
            // "Chatter" is the product name.
            (_, true) => ("Chatter".to_string(), new_message),
            // None: maximum privacy
            (NotificationDetailLevel::None, false) => ("Chatter".to_string(), new_message),
            // Name only: show sender, hide content
            (NotificationDetailLevel::NameOnly, false) => (
                sender_name.map(str::to_owned).unwrap_or_else(someone),
                new_message,
            ),
            // Name and message: show both. The preview is message content.
            (NotificationDetailLevel::NameAndMessage, false) => (
                sender_name.map(str::to_owned).unwrap_or_else(someone),
                message_preview.map(str::to_owned).unwrap_or(new_message),
            ),
        }
    }

    /// Post an incoming message and retain enough state to restore the
    /// preceding notification if this message disappears. Display text is
    /// filtered against the current privacy setting each time it is posted.
    pub fn show_tracked_message_notification(
        &self,
        message_id: crate::domain::ids::MessageId,
        conversation_id: &crate::domain::ids::ChannelId,
        sender_name: Option<&str>,
        message_preview: Option<&str>,
        private: bool,
        priority: NotificationPriority,
    ) -> Result<(), SystemError> {
        let key = self.current_key(conversation_id)?;
        self.with_message_privacy(|notifications, level, _| {
            let (title, body) = Self::message_text(level, sender_name, message_preview, private);
            self.deliver(|sink| sink.show_conversation(&key, &title, &body, priority))?;
            let entries = notifications.entries.entry(key).or_default();
            entries.retain(|entry| entry.id != message_id);
            entries.push(MessageNotification {
                id: message_id,
                sender_name: sender_name.map(str::to_owned),
                message_preview: message_preview.map(str::to_owned),
                private,
                priority,
            });
            Ok(())
        })
    }

    /// Remove deleted or expired messages from whichever conversations posted
    /// them. A prior message remains visible when the newest one is removed.
    pub fn remove_message_notifications(&self, message_ids: &[crate::domain::ids::MessageId]) {
        self.with_message_privacy(|notifications, level, _| {
            for (key, entries) in &mut notifications.entries {
                let was_latest = entries
                    .last()
                    .is_some_and(|entry| message_ids.contains(&entry.id));
                let old_len = entries.len();
                entries.retain(|entry| !message_ids.contains(&entry.id));
                if entries.len() == old_len || !was_latest {
                    continue;
                }
                let result = if let Some(entry) = entries.last() {
                    let (title, body) = Self::message_text(
                        level,
                        entry.sender_name.as_deref(),
                        entry.message_preview.as_deref(),
                        entry.private,
                    );
                    self.deliver(|sink| sink.show_conversation(key, &title, &body, entry.priority))
                } else {
                    self.deliver(|sink| sink.cancel_conversation(key))
                };
                if let Err(error) = result {
                    tracing::warn!(%error, "Could not update notification for removed message");
                }
            }
            notifications
                .entries
                .retain(|_, entries| !entries.is_empty());
        })
    }

    /// Dismiss `conversation_id`'s notification (it was read).
    pub fn cancel_conversation(
        &self,
        conversation_id: &crate::domain::ids::ChannelId,
    ) -> Result<(), SystemError> {
        let key = self.current_key(conversation_id)?;
        let mut notifications = self.message_posts();
        self.deliver(|sink| sink.cancel_conversation(&key))?;
        notifications.entries.remove(&key);
        Ok(())
    }

    // `show_push_event` was removed alongside push_listener's
    // show_push_notification. Rationale: ntfy SSE is a wake-up transport
    // only; user-facing toasts flow via DomainEvent from the typed gRPC
    // stream consumer (see services::notification_stream) so server can
    // run ntfy in content-free mode without losing notification
    // fidelity.

    /// Show a simple text notification (caller controls content).
    pub fn show_message(
        &self,
        title: &str,
        body: &str,
        priority: NotificationPriority,
    ) -> Result<(), SystemError> {
        self.show_platform_notification(title, body, priority)
    }

    /// Show an actionable incoming-call notification (Accept/Decline) for
    /// `call_id`. On Android this is a full-screen ringing notification with
    /// buttons; elsewhere it falls back to a plain urgent toast.
    pub fn show_incoming_call(
        &self,
        call_id: &str,
        title: &str,
        body: &str,
    ) -> Result<(), SystemError> {
        self.with_app_lock_state(|(locked, _, _)| match self.detail_for_lock_state(locked) {
            NotificationDetailLevel::None => {
                let body = chatter_i18n::tr("Incoming call");
                self.deliver(|sink| sink.show_incoming_call(call_id, "Chatter", &body))
            }
            _ => self.deliver(|sink| sink.show_incoming_call(call_id, title, body)),
        })
    }

    /// Dismiss the incoming-call notification for `call_id` (accepted,
    /// declined, withdrawn, or ended).
    pub fn cancel_incoming_call(&self, call_id: &str) -> Result<(), SystemError> {
        self.deliver(|sink| sink.cancel_incoming_call(call_id))
    }

    /// Platform-neutral notification display via the injected sink.
    fn show_platform_notification(
        &self,
        title: &str,
        body: &str,
        priority: NotificationPriority,
    ) -> Result<(), SystemError> {
        self.deliver(|sink| sink.show(title, body, priority))
    }

    /// Every sink call goes through here. The sinks are long-lived event
    /// loops' only contact with platform code (JNI on Android), and a panic
    /// there used to end the loop: one call whose id overflowed a request-code
    /// multiply killed the ring-notification bridge, so every later call rang
    /// silently. A panic is one failed notification, reported as an error.
    fn deliver<T>(
        &self,
        f: impl FnOnce(&dyn NotificationSink) -> Result<T, SystemError>,
    ) -> Result<T, SystemError> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(self.sink.as_ref())))
            .unwrap_or_else(|_| {
                Err(SystemError::NotificationSend(
                    "notification sink panicked".into(),
                ))
            })
    }
}

impl Default for NotificationService {
    fn default() -> Self {
        Self::noop()
    }
}

async fn wait_for_lock_check(delay: Duration) {
    #[cfg(any(target_os = "android", target_os = "linux"))]
    if wait_including_suspend(delay).await.is_ok() {
        return;
    }
    tokio::time::sleep(delay).await;
}

#[cfg(any(target_os = "android", target_os = "linux"))]
async fn wait_including_suspend(delay: Duration) -> std::io::Result<()> {
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    // A Tokio timer pauses during suspend. CLOCK_BOOTTIME includes that time,
    // so an elapsed lock deadline is ready as soon as the device wakes.
    let fd = unsafe {
        libc::timerfd_create(libc::CLOCK_BOOTTIME, libc::TFD_CLOEXEC | libc::TFD_NONBLOCK)
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: timerfd_create returned a new, valid file descriptor.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let timer = libc::itimerspec {
        it_interval: libc::timespec { tv_sec: 0, tv_nsec: 0 },
        it_value: libc::timespec {
            tv_sec: delay.as_secs() as libc::time_t,
            tv_nsec: delay.subsec_nanos() as _,
        },
    };
    // SAFETY: fd is open, timer is a valid input and the old value is unused.
    if unsafe { libc::timerfd_settime(fd.as_raw_fd(), 0, &timer, std::ptr::null_mut()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let timer = tokio::io::unix::AsyncFd::new(fd)?;
    let _ready = timer.readable().await?;
    Ok(())
}

#[cfg(test)]
mod tests;
