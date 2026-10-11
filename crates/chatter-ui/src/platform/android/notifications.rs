//! Android system notifications via JNI.
//!
//! Calls Android's `NotificationManager` directly from Rust using JNI.
//! No Java/Kotlin code needed — pure Rust JNI calls.
//!
//! ## Notification Channel
//!
//! Android 8+ (API 26, our min SDK) requires a notification channel.
//! We create one channel "chatter_messages" at app startup via `init_channel()`.
//!
//! ## Privacy
//!
//! The caller (`NotificationService` in core) controls what title/body are
//! passed. This module just displays whatever it receives — privacy filtering
//! happens upstream.

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use chatter_core::domain::notification::NotificationPriority;
use chatter_core::error::SystemError;
use chatter_core::system::notification_sink::NotificationSink;
use jni::Env;
use jni::objects::{JObject, JString, JValue};

#[path = "call_notification_consent.rs"]
pub(crate) mod call_consent;
#[path = "call_notification_identity.rs"]
mod call_identity;

static CALL_CONSENT: std::sync::Mutex<call_consent::Consent> =
    std::sync::Mutex::new(call_consent::Consent::new());

pub(crate) fn consume_call_consent(call_id: &str, action: &str, token: &str) -> bool {
    CALL_CONSENT
        .lock()
        .map(|mut state| state.consume(call_id, action, token, std::time::Instant::now()))
        .unwrap_or(false)
}

pub(crate) fn consume_and_cancel_call_consent(call_id: &str, action: &str, token: &str) -> bool {
    let Ok(mut state) = CALL_CONSENT.lock() else {
        return false;
    };
    if !state.consume(call_id, action, token, std::time::Instant::now()) {
        return false;
    }
    // Keep the publication lock through cancellation so a replacement cannot
    // be posted between capability consumption and dismissal.
    let _ = AndroidNotifications::new().cancel_call_platform(call_id);
    true
}

pub(crate) const CHANNEL_ID: &str = "chatter_messages";
const CHANNEL_NAME: &str = "Messages";

/// Dedicated channel for incoming calls — high importance, ringtone + vibration
/// + full-screen intent. Separate from messages so calls actually ring (channel
/// importance, not per-notification priority, drives sound/heads-up on API 26+).
const CHANNEL_CALLS_ID: &str = "chatter_calls";
const CHANNEL_CALLS_NAME: &str = "Calls";

const IMPORTANCE_HIGH: i32 = 4;

/// Id of every conversation notification; the conversation id is its tag, so
/// the (tag, id) pair is per conversation and never meets an untagged id.
const CONVERSATION_NOTIFICATION_ID: i32 = 1;

// PendingIntent flags.
const FLAG_UPDATE_CURRENT: i32 = 0x0800_0000; // 134217728
const FLAG_IMMUTABLE: i32 = 0x0400_0000; // 67108864

// android.R.drawable icons.
const IC_CALL: i32 = 17301507; // ic_menu_call
const IC_CLOSE: i32 = 17301560; // ic_menu_close_clear_cancel

pub struct AndroidNotifications {
    channel_initialized: AtomicBool,
    calls_channel_initialized: AtomicBool,
    next_id: AtomicI32,
}

impl AndroidNotifications {
    pub fn new() -> Self {
        Self {
            channel_initialized: AtomicBool::new(false),
            calls_channel_initialized: AtomicBool::new(false),
            next_id: AtomicI32::new(1),
        }
    }

    /// Initialize the notification channel (required Android 8+, API 26).
    /// Safe to call multiple times — only initializes once.
    pub fn init_channel(&self) -> Result<(), SystemError> {
        if self.channel_initialized.load(Ordering::Relaxed) {
            return Ok(());
        }

        with_jni_env(|env| {
            let context = get_app_context(env)?;

            let channel_id = env
                .new_string(CHANNEL_ID)
                .map_err(|e| SystemError::NotificationInit(format!("channel_id string: {}", e)))?;
            let channel_name = env.new_string(CHANNEL_NAME).map_err(|e| {
                SystemError::NotificationInit(format!("channel_name string: {}", e))
            })?;

            let channel = env
                .new_object(
                    jni::jni_str!("android/app/NotificationChannel"),
                    jni::jni_sig!("(Ljava/lang/String;Ljava/lang/CharSequence;I)V"),
                    &[
                        JValue::Object(&channel_id),
                        JValue::Object(&channel_name),
                        JValue::Int(IMPORTANCE_HIGH),
                    ],
                )
                .map_err(|e| {
                    SystemError::NotificationInit(format!("NotificationChannel: {}", e))
                })?;

            let manager = get_notification_manager(env, &context)?;

            env.call_method(
                &manager,
                jni::jni_str!("createNotificationChannel"),
                jni::jni_sig!("(Landroid/app/NotificationChannel;)V"),
                &[JValue::Object(&channel)],
            )
            .map_err(|e| {
                SystemError::NotificationInit(format!("createNotificationChannel: {}", e))
            })?;

            self.channel_initialized.store(true, Ordering::Relaxed);
            tracing::info!("Android notification channel initialized");

            Ok(())
        })
    }

    /// Initialize the dedicated calls channel: high importance + ringtone +
    /// vibration. Channel sound/importance are fixed at creation (Android
    /// ignores later edits), so this uses a distinct channel id from messages.
    fn init_calls_channel(&self) -> Result<(), SystemError> {
        if self.calls_channel_initialized.load(Ordering::Relaxed) {
            return Ok(());
        }
        with_jni_env(|env| {
            let context = get_app_context(env)?;

            let channel_id = env.new_string(CHANNEL_CALLS_ID).map_err(nerr)?;
            let channel_name = env.new_string(CHANNEL_CALLS_NAME).map_err(nerr)?;
            let channel = env
                .new_object(
                    jni::jni_str!("android/app/NotificationChannel"),
                    jni::jni_sig!("(Ljava/lang/String;Ljava/lang/CharSequence;I)V"),
                    &[
                        JValue::Object(&channel_id),
                        JValue::Object(&channel_name),
                        JValue::Int(IMPORTANCE_HIGH),
                    ],
                )
                .map_err(nerr)?;

            env.call_method(
                &channel,
                jni::jni_str!("enableVibration"),
                jni::jni_sig!("(Z)V"),
                &[JValue::Bool(true)],
            )
            .map_err(nerr)?;

            // Ring with the default ringtone (USAGE_NOTIFICATION_RINGTONE) so it
            // sounds like a call, not a message ping.
            // RingtoneManager.getDefaultUri(TYPE_RINGTONE = 1)
            let ringtone_uri = env
                .call_static_method(
                    jni::jni_str!("android/media/RingtoneManager"),
                    jni::jni_str!("getDefaultUri"),
                    jni::jni_sig!("(I)Landroid/net/Uri;"),
                    &[JValue::Int(1)],
                )
                .map_err(nerr)?
                .l()
                .map_err(nerr)?;
            // AudioAttributes.Builder().setUsage(6 = USAGE_NOTIFICATION_RINGTONE)
            //   .setContentType(4 = CONTENT_TYPE_SONIFICATION).build()
            let ab = env
                .new_object(
                    jni::jni_str!("android/media/AudioAttributes$Builder"),
                    jni::jni_sig!("()V"),
                    &[],
                )
                .map_err(nerr)?;
            let ab = env
                .call_method(
                    &ab,
                    jni::jni_str!("setUsage"),
                    jni::jni_sig!("(I)Landroid/media/AudioAttributes$Builder;"),
                    &[JValue::Int(6)],
                )
                .map_err(nerr)?
                .l()
                .map_err(nerr)?;
            let ab = env
                .call_method(
                    &ab,
                    jni::jni_str!("setContentType"),
                    jni::jni_sig!("(I)Landroid/media/AudioAttributes$Builder;"),
                    &[JValue::Int(4)],
                )
                .map_err(nerr)?
                .l()
                .map_err(nerr)?;
            let audio_attrs = env
                .call_method(
                    &ab,
                    jni::jni_str!("build"),
                    jni::jni_sig!("()Landroid/media/AudioAttributes;"),
                    &[],
                )
                .map_err(nerr)?
                .l()
                .map_err(nerr)?;
            env.call_method(
                &channel,
                jni::jni_str!("setSound"),
                jni::jni_sig!("(Landroid/net/Uri;Landroid/media/AudioAttributes;)V"),
                &[JValue::Object(&ringtone_uri), JValue::Object(&audio_attrs)],
            )
            .map_err(nerr)?;

            let manager = get_notification_manager(env, &context)?;
            env.call_method(
                &manager,
                jni::jni_str!("createNotificationChannel"),
                jni::jni_sig!("(Landroid/app/NotificationChannel;)V"),
                &[JValue::Object(&channel)],
            )
            .map_err(nerr)?;

            self.calls_channel_initialized
                .store(true, Ordering::Relaxed);
            tracing::info!("Android calls notification channel initialized");
            Ok(())
        })
    }

    /// Post a ringing notification with capability-authenticated actions.
    /// Accept launches MainActivity; Decline uses the nonexported receiver.
    /// Body/full-screen intents only open the in-app call screen.
    fn show_incoming_call_raw(
        &self,
        call_id: &str,
        title: &str,
        body: &str,
    ) -> Result<(), SystemError> {
        self.init_calls_channel()?;
        // Serialize publication, replacement and revocation: an older build may
        // never overwrite a newer notification's proof. Poisoning fails closed.
        let mut consent = CALL_CONSENT
            .lock()
            .map_err(|_| SystemError::NotificationSend("Call consent unavailable".into()))?;
        let tokens = consent.issue(
            call_id,
            std::time::Instant::now(),
            chatter_core::services::call_worker::RING_TIMEOUT,
        );

        let id = call_identity::request_code(call_id);
        with_jni_env(|env| {
            let context = get_app_context(env)?;

            let j_title = env.new_string(title).map_err(nerr)?;
            let j_body = env.new_string(body).map_err(nerr)?;
            let j_channel = env.new_string(CHANNEL_CALLS_ID).map_err(nerr)?;

            let builder = env
                .new_object(
                    jni::jni_str!("android/app/Notification$Builder"),
                    jni::jni_sig!("(Landroid/content/Context;Ljava/lang/String;)V"),
                    &[JValue::Object(&context), JValue::Object(&j_channel)],
                )
                .map_err(nerr)?;

            env.call_method(
                &builder,
                jni::jni_str!("setSmallIcon"),
                jni::jni_sig!("(I)Landroid/app/Notification$Builder;"),
                &[JValue::Int(IC_CALL)],
            )
            .map_err(nerr)?;
            env.call_method(
                &builder,
                jni::jni_str!("setContentTitle"),
                jni::jni_sig!("(Ljava/lang/CharSequence;)Landroid/app/Notification$Builder;"),
                &[JValue::Object(&j_title)],
            )
            .map_err(nerr)?;
            env.call_method(
                &builder,
                jni::jni_str!("setContentText"),
                jni::jni_sig!("(Ljava/lang/CharSequence;)Landroid/app/Notification$Builder;"),
                &[JValue::Object(&j_body)],
            )
            .map_err(nerr)?;
            private_lockscreen_version(
                env,
                &context,
                &builder,
                CHANNEL_CALLS_ID,
                IC_CALL,
                "Incoming call",
            )?;
            // PRIORITY_MAX (2) for heads-up on pre-26; category=call; ongoing so it
            // can't be swiped away while ringing; auto-timeout if unanswered,
            // when core stops ringing it too (setTimeoutAfter: API 26 = minSdk).
            env.call_method(
                &builder,
                jni::jni_str!("setPriority"),
                jni::jni_sig!("(I)Landroid/app/Notification$Builder;"),
                &[JValue::Int(2)],
            )
            .map_err(nerr)?;
            let j_cat = env.new_string("call").map_err(nerr)?;
            env.call_method(
                &builder,
                jni::jni_str!("setCategory"),
                jni::jni_sig!("(Ljava/lang/String;)Landroid/app/Notification$Builder;"),
                &[JValue::Object(&j_cat)],
            )
            .map_err(nerr)?;
            env.call_method(
                &builder,
                jni::jni_str!("setOngoing"),
                jni::jni_sig!("(Z)Landroid/app/Notification$Builder;"),
                &[JValue::Bool(true)],
            )
            .map_err(nerr)?;
            env.call_method(
                &builder,
                jni::jni_str!("setAutoCancel"),
                jni::jni_sig!("(Z)Landroid/app/Notification$Builder;"),
                &[JValue::Bool(true)],
            )
            .map_err(nerr)?;
            env.call_method(
                &builder,
                jni::jni_str!("setTimeoutAfter"),
                jni::jni_sig!("(J)Landroid/app/Notification$Builder;"),
                &[JValue::Long(
                    chatter_core::services::call_worker::RING_TIMEOUT.as_millis() as i64,
                )],
            )
            .map_err(nerr)?;

            // Full-screen + content intent → open the in-app ring screen (no auto-accept).
            let fs_pi = build_activity_intent(
                env,
                &context,
                call_id,
                id,
                call_consent::FULL_SCREEN,
                &tokens[2],
            )?;
            let open_pi =
                build_activity_intent(env, &context, call_id, id, call_consent::OPEN, &tokens[1])?;
            env.call_method(
                &builder,
                jni::jni_str!("setFullScreenIntent"),
                jni::jni_sig!("(Landroid/app/PendingIntent;Z)Landroid/app/Notification$Builder;"),
                &[JValue::Object(&fs_pi), JValue::Bool(true)],
            )
            .map_err(nerr)?;
            env.call_method(
                &builder,
                jni::jni_str!("setContentIntent"),
                jni::jni_sig!("(Landroid/app/PendingIntent;)Landroid/app/Notification$Builder;"),
                &[JValue::Object(&open_pi)],
            )
            .map_err(nerr)?;

            // Accept = activity intent: foregrounds the app (a broadcast receiver
            // can't reliably start an activity on Android 14+) and auto-accepts.
            // Decline = headless broadcast.
            // PendingIntent request codes only need to differ from each other and
            // from the content intent's; `id` is any positive i32, so `id * 2`
            // overflowed (a panic that killed the notification bridge for every
            // later call). Wrapping keeps them distinct without overflow.
            let accept_pi = build_activity_intent(
                env,
                &context,
                call_id,
                id.wrapping_mul(2),
                call_consent::ACCEPT,
                &tokens[0],
            )?;
            let accept = make_action(env, IC_CALL, "Accept", &accept_pi)?;
            let decline = build_action(
                env,
                &context,
                "com.chatter.client.CALL_DECLINE",
                "Decline",
                call_id,
                IC_CLOSE,
                id.wrapping_mul(2).wrapping_add(1),
                &tokens[3],
            )?;
            env.call_method(
                &builder,
                jni::jni_str!("addAction"),
                jni::jni_sig!(
                    "(Landroid/app/Notification$Action;)Landroid/app/Notification$Builder;"
                ),
                &[JValue::Object(&accept)],
            )
            .map_err(nerr)?;
            env.call_method(
                &builder,
                jni::jni_str!("addAction"),
                jni::jni_sig!(
                    "(Landroid/app/Notification$Action;)Landroid/app/Notification$Builder;"
                ),
                &[JValue::Object(&decline)],
            )
            .map_err(nerr)?;

            let notification = env
                .call_method(
                    &builder,
                    jni::jni_str!("build"),
                    jni::jni_sig!("()Landroid/app/Notification;"),
                    &[],
                )
                .map_err(nerr)?
                .l()
                .map_err(nerr)?;
            let manager = get_notification_manager(env, &context)?;
            let tag = env.new_string(call_identity::tag(call_id)).map_err(nerr)?;
            env.call_method(
                &manager,
                jni::jni_str!("notify"),
                jni::jni_sig!("(Ljava/lang/String;ILandroid/app/Notification;)V"),
                &[
                    JValue::Object(&tag),
                    JValue::Int(id),
                    JValue::Object(&notification),
                ],
            )
            .map_err(nerr)?;

            tracing::info!(call_id, id, "Posted ringing incoming-call notification");
            Ok(())
        })
    }

    fn cancel_call_raw(&self, call_id: &str) -> Result<(), SystemError> {
        let mut consent = CALL_CONSENT
            .lock()
            .map_err(|_| SystemError::NotificationSend("Call consent unavailable".into()))?;
        consent.revoke(call_id);
        self.cancel_call_platform(call_id)
    }

    fn cancel_call_platform(&self, call_id: &str) -> Result<(), SystemError> {
        let id = call_identity::request_code(call_id);
        with_jni_env(|env| {
            let context = get_app_context(env)?;
            let manager = get_notification_manager(env, &context)?;
            let tag = env.new_string(call_identity::tag(call_id)).map_err(nerr)?;
            env.call_method(
                &manager,
                jni::jni_str!("cancel"),
                jni::jni_sig!("(Ljava/lang/String;I)V"),
                &[JValue::Object(&tag), JValue::Int(id)],
            )
            .map_err(nerr)?;
            Ok(())
        })
    }

    /// Post a notification. With a `conversation` tag it is posted as
    /// (tag, `CONVERSATION_NOTIFICATION_ID`), so the conversation's next one
    /// replaces it and `cancel_conversation_raw` dismisses it; untagged ones
    /// get a fresh id each.
    fn show_raw(
        &self,
        conversation: Option<&str>,
        title: &str,
        body: &str,
        priority: u8,
    ) -> Result<(), SystemError> {
        if !self.channel_initialized.load(Ordering::Relaxed) {
            self.init_channel()?;
        }

        let notification_id = match conversation {
            Some(_) => CONVERSATION_NOTIFICATION_ID,
            None => self.next_id.fetch_add(1, Ordering::Relaxed),
        };

        with_jni_env(|env| {
            let context = get_app_context(env)?;

            let j_title = env
                .new_string(title)
                .map_err(|e| SystemError::NotificationSend(format!("title string: {}", e)))?;
            let j_body = env
                .new_string(body)
                .map_err(|e| SystemError::NotificationSend(format!("body string: {}", e)))?;
            let j_channel_id = env
                .new_string(CHANNEL_ID)
                .map_err(|e| SystemError::NotificationSend(format!("channel_id string: {}", e)))?;

            let builder = env
                .new_object(
                    jni::jni_str!("android/app/Notification$Builder"),
                    jni::jni_sig!("(Landroid/content/Context;Ljava/lang/String;)V"),
                    &[JValue::Object(&context), JValue::Object(&j_channel_id)],
                )
                .map_err(|e| {
                    SystemError::NotificationSend(format!("Notification.Builder: {}", e))
                })?;

            // android.R.drawable.ic_dialog_info
            const IC_DIALOG_INFO: i32 = 17301659;

            env.call_method(
                &builder,
                jni::jni_str!("setSmallIcon"),
                jni::jni_sig!("(I)Landroid/app/Notification$Builder;"),
                &[JValue::Int(IC_DIALOG_INFO)],
            )
            .map_err(|e| SystemError::NotificationSend(format!("setSmallIcon: {}", e)))?;

            env.call_method(
                &builder,
                jni::jni_str!("setContentTitle"),
                jni::jni_sig!("(Ljava/lang/CharSequence;)Landroid/app/Notification$Builder;"),
                &[JValue::Object(&j_title)],
            )
            .map_err(|e| SystemError::NotificationSend(format!("setContentTitle: {}", e)))?;

            env.call_method(
                &builder,
                jni::jni_str!("setContentText"),
                jni::jni_sig!("(Ljava/lang/CharSequence;)Landroid/app/Notification$Builder;"),
                &[JValue::Object(&j_body)],
            )
            .map_err(|e| SystemError::NotificationSend(format!("setContentText: {}", e)))?;

            env.call_method(
                &builder,
                jni::jni_str!("setAutoCancel"),
                jni::jni_sig!("(Z)Landroid/app/Notification$Builder;"),
                &[JValue::Bool(true)],
            )
            .map_err(|e| SystemError::NotificationSend(format!("setAutoCancel: {}", e)))?;

            private_lockscreen_version(
                env,
                &context,
                &builder,
                CHANNEL_ID,
                IC_DIALOG_INFO,
                "New message",
            )?;
            // pre-API 26 compat priority
            let android_priority = match priority {
                5 => 2,
                4 => 1,
                2 => -1,
                1 => -2,
                _ => 0,
            };
            env.call_method(
                &builder,
                jni::jni_str!("setPriority"),
                jni::jni_sig!("(I)Landroid/app/Notification$Builder;"),
                &[JValue::Int(android_priority)],
            )
            .map_err(|e| SystemError::NotificationSend(format!("setPriority: {}", e)))?;

            let notification = env
                .call_method(
                    &builder,
                    jni::jni_str!("build"),
                    jni::jni_sig!("()Landroid/app/Notification;"),
                    &[],
                )
                .map_err(|e| SystemError::NotificationSend(format!("build: {}", e)))?
                .l()
                .map_err(|e| SystemError::NotificationSend(format!("build result: {}", e)))?;

            let manager = get_notification_manager(env, &context)?;

            match conversation {
                Some(tag) => {
                    let j_tag = env.new_string(tag).map_err(nerr)?;
                    env.call_method(
                        &manager,
                        jni::jni_str!("notify"),
                        jni::jni_sig!("(Ljava/lang/String;ILandroid/app/Notification;)V"),
                        &[
                            JValue::Object(&j_tag),
                            JValue::Int(notification_id),
                            JValue::Object(&notification),
                        ],
                    )
                }
                None => env.call_method(
                    &manager,
                    jni::jni_str!("notify"),
                    jni::jni_sig!("(ILandroid/app/Notification;)V"),
                    &[JValue::Int(notification_id), JValue::Object(&notification)],
                ),
            }
            .map_err(|e| SystemError::NotificationSend(format!("notify: {}", e)))?;

            tracing::debug!(notification_id, title, "Android notification shown");
            Ok(())
        })
    }

    fn cancel_conversation_raw(&self, conversation: &str) -> Result<(), SystemError> {
        with_jni_env(|env| {
            let context = get_app_context(env)?;
            let manager = get_notification_manager(env, &context)?;
            let j_tag = env.new_string(conversation).map_err(nerr)?;
            env.call_method(
                &manager,
                jni::jni_str!("cancel"),
                jni::jni_sig!("(Ljava/lang/String;I)V"),
                &[
                    JValue::Object(&j_tag),
                    JValue::Int(CONVERSATION_NOTIFICATION_ID),
                ],
            )
            .map_err(nerr)?;
            Ok(())
        })
    }
}

impl Default for AndroidNotifications {
    fn default() -> Self {
        Self::new()
    }
}

impl NotificationSink for AndroidNotifications {
    fn show(
        &self,
        title: &str,
        body: &str,
        priority: NotificationPriority,
    ) -> Result<(), SystemError> {
        self.show_raw(None, title, body, android_priority_level(priority))
    }

    fn show_conversation(
        &self,
        conversation: &str,
        title: &str,
        body: &str,
        priority: NotificationPriority,
    ) -> Result<(), SystemError> {
        self.show_raw(
            Some(conversation),
            title,
            body,
            android_priority_level(priority),
        )
    }

    fn cancel_conversation(&self, conversation: &str) -> Result<(), SystemError> {
        self.cancel_conversation_raw(conversation)
    }

    fn show_incoming_call(
        &self,
        call_id: &str,
        title: &str,
        body: &str,
    ) -> Result<(), SystemError> {
        self.show_incoming_call_raw(call_id, title, body)
    }

    fn cancel_incoming_call(&self, call_id: &str) -> Result<(), SystemError> {
        self.cancel_call_raw(call_id)
    }

    fn init(&self) -> Result<(), SystemError> {
        self.init_channel()?;
        self.init_calls_channel()
    }
}

fn android_priority_level(priority: NotificationPriority) -> u8 {
    match priority {
        NotificationPriority::Urgent => 5,
        NotificationPriority::High => 4,
        NotificationPriority::Low => 2,
        NotificationPriority::Normal => 3,
    }
}

/// Map any JNI / display error into a `SystemError::NotificationSend`.
fn nerr<E: std::fmt::Display>(e: E) -> SystemError {
    SystemError::NotificationSend(e.to_string())
}

/// Best-effort dismissal of a call notification by call_id, callable from
/// outside the sink (e.g. the `CallActionReceiver` JNI bridge after the user
/// taps Accept/Decline).
pub fn cancel_call_notification(call_id: &str) {
    // Use the same full-identity dismissal as the notification sink.
    let _ = AndroidNotifications::new().cancel_call_raw(call_id);
}

/// Build a `Notification.Action` whose PendingIntent broadcasts `action` (with
/// the `call_id` extra) to `CallActionReceiver`.
fn build_action<'a>(
    env: &mut Env<'a>,
    context: &JObject<'a>,
    action: &str,
    label: &str,
    call_id: &str,
    icon: i32,
    request_code: i32,
    capability: &str,
) -> Result<JObject<'a>, SystemError> {
    // Target the receiver by class NAME via setClassName, not find_class +
    // Intent(Context, Class): this notification is built on the JNI poller
    // thread, whose classloader can't resolve app classes, so find_class throws.
    let intent = env
        .new_object(
            jni::jni_str!("android/content/Intent"),
            jni::jni_sig!("()V"),
            &[],
        )
        .map_err(nerr)?;
    let cls_name = env
        .new_string("com.chatter.client.CallActionReceiver")
        .map_err(nerr)?;
    env.call_method(
        &intent,
        jni::jni_str!("setClassName"),
        jni::jni_sig!("(Landroid/content/Context;Ljava/lang/String;)Landroid/content/Intent;"),
        &[JValue::Object(context), JValue::Object(&cls_name)],
    )
    .map_err(nerr)?;
    let j_action = env.new_string(action).map_err(nerr)?;
    env.call_method(
        &intent,
        jni::jni_str!("setAction"),
        jni::jni_sig!("(Ljava/lang/String;)Landroid/content/Intent;"),
        &[JValue::Object(&j_action)],
    )
    .map_err(nerr)?;
    add_call_intent_identity(env, &intent, call_id, action)?;
    add_call_capability(env, &intent, capability)?;
    let k = env.new_string("call_id").map_err(nerr)?;
    let v = env.new_string(call_id).map_err(nerr)?;
    env.call_method(
        &intent,
        jni::jni_str!("putExtra"),
        jni::jni_sig!("(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;"),
        &[JValue::Object(&k), JValue::Object(&v)],
    )
    .map_err(nerr)?;

    let pi = env
        .call_static_method(
            jni::jni_str!("android/app/PendingIntent"),
            jni::jni_str!("getBroadcast"),
            jni::jni_sig!(
                "(Landroid/content/Context;ILandroid/content/Intent;I)Landroid/app/PendingIntent;"
            ),
            &[
                JValue::Object(context),
                JValue::Int(request_code),
                JValue::Object(&intent),
                JValue::Int(FLAG_IMMUTABLE | FLAG_UPDATE_CURRENT),
            ],
        )
        .map_err(nerr)?
        .l()
        .map_err(nerr)?;

    make_action(env, icon, label, &pi)
}

/// Build a `Notification.Action` from an already-constructed PendingIntent.
fn make_action<'a>(
    env: &mut Env<'a>,
    icon: i32,
    label: &str,
    pi: &JObject<'a>,
) -> Result<JObject<'a>, SystemError> {
    let j_label = env.new_string(label).map_err(nerr)?;
    let action_builder = env
        .new_object(
            jni::jni_str!("android/app/Notification$Action$Builder"),
            jni::jni_sig!("(ILjava/lang/CharSequence;Landroid/app/PendingIntent;)V"),
            &[
                JValue::Int(icon),
                JValue::Object(&j_label),
                JValue::Object(pi),
            ],
        )
        .map_err(nerr)?;
    env.call_method(
        &action_builder,
        jni::jni_str!("build"),
        jni::jni_sig!("()Landroid/app/Notification$Action;"),
        &[],
    )
    .map_err(nerr)?
    .l()
    .map_err(nerr)
}

/// Build a PendingIntent that launches `MainActivity` (the in-app call screen),
/// carrying a call/action-bound capability. Only authenticated Accept invokes
/// acceptance; body and full-screen open have independent one-shot proofs.
/// Launched via an activity
/// PendingIntent so the notification can foreground the app — a broadcast
/// receiver can't reliably start an activity on Android 14+.
fn build_activity_intent<'a>(
    env: &mut Env<'a>,
    context: &JObject<'a>,
    call_id: &str,
    request_code: i32,
    action: &str,
    capability: &str,
) -> Result<JObject<'a>, SystemError> {
    // setClassName (string) instead of find_class — see build_action.
    let intent = env
        .new_object(
            jni::jni_str!("android/content/Intent"),
            jni::jni_sig!("()V"),
            &[],
        )
        .map_err(nerr)?;
    let cls_name = env
        .new_string("com.chatter.client.MainActivity")
        .map_err(nerr)?;
    env.call_method(
        &intent,
        jni::jni_str!("setClassName"),
        jni::jni_sig!("(Landroid/content/Context;Ljava/lang/String;)Landroid/content/Intent;"),
        &[JValue::Object(context), JValue::Object(&cls_name)],
    )
    .map_err(nerr)?;
    add_call_intent_identity(env, &intent, call_id, action)?;
    add_call_capability(env, &intent, capability)?;
    let j_action = env.new_string(action).map_err(nerr)?;
    env.call_method(
        &intent,
        jni::jni_str!("setAction"),
        jni::jni_sig!("(Ljava/lang/String;)Landroid/content/Intent;"),
        &[JValue::Object(&j_action)],
    )
    .map_err(nerr)?;
    // FLAG_ACTIVITY_NEW_TASK | FLAG_ACTIVITY_SINGLE_TOP
    env.call_method(
        &intent,
        jni::jni_str!("addFlags"),
        jni::jni_sig!("(I)Landroid/content/Intent;"),
        &[JValue::Int(0x1000_0000 | 0x2000_0000)],
    )
    .map_err(nerr)?;
    let k = env.new_string("incoming_call_id").map_err(nerr)?;
    let v = env.new_string(call_id).map_err(nerr)?;
    env.call_method(
        &intent,
        jni::jni_str!("putExtra"),
        jni::jni_sig!("(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;"),
        &[JValue::Object(&k), JValue::Object(&v)],
    )
    .map_err(nerr)?;
    if action == call_consent::ACCEPT {
        let ak = env.new_string("auto_accept").map_err(nerr)?;
        env.call_method(
            &intent,
            jni::jni_str!("putExtra"),
            jni::jni_sig!("(Ljava/lang/String;Z)Landroid/content/Intent;"),
            &[JValue::Object(&ak), JValue::Bool(true)],
        )
        .map_err(nerr)?;
    }

    let pi = env
        .call_static_method(
            jni::jni_str!("android/app/PendingIntent"),
            jni::jni_str!("getActivity"),
            jni::jni_sig!(
                "(Landroid/content/Context;ILandroid/content/Intent;I)Landroid/app/PendingIntent;"
            ),
            &[
                JValue::Object(context),
                JValue::Int(request_code),
                JValue::Object(&intent),
                JValue::Int(FLAG_IMMUTABLE | FLAG_UPDATE_CURRENT),
            ],
        )
        .map_err(nerr)?
        .l()
        .map_err(nerr)?;
    Ok(pi)
}

fn add_call_capability(
    env: &mut Env<'_>,
    intent: &JObject<'_>,
    capability: &str,
) -> Result<(), SystemError> {
    let key = env
        .new_string("call_notification_capability")
        .map_err(nerr)?;
    let value = env.new_string(capability).map_err(nerr)?;
    env.call_method(
        intent,
        jni::jni_str!("putExtra"),
        jni::jni_sig!("(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;"),
        &[JValue::Object(&key), JValue::Object(&value)],
    )
    .map_err(nerr)?;
    Ok(())
}

/// Extras do not distinguish Android PendingIntents. Explicit components stay
/// unchanged, while this category distinguishes the full call and action on
/// every supported API level, including when request-code hashes collide.
fn add_call_intent_identity(
    env: &mut Env<'_>,
    intent: &JObject<'_>,
    call_id: &str,
    action: &str,
) -> Result<(), SystemError> {
    let category = env
        .new_string(call_identity::category(call_id, action))
        .map_err(nerr)?;
    env.call_method(
        intent,
        jni::jni_str!("addCategory"),
        jni::jni_sig!("(Ljava/lang/String;)Landroid/content/Intent;"),
        &[JValue::Object(&category)],
    )
    .map_err(nerr)?;
    Ok(())
}

fn with_jni_env<T>(
    body: impl FnOnce(&mut Env<'_>) -> Result<T, SystemError>,
) -> Result<T, SystemError> {
    super::activity::with_jni_env(|env| Ok(body(env))).map_err(SystemError::NotificationInit)?
}

fn get_app_context<'a>(env: &mut Env<'a>) -> Result<JObject<'a>, SystemError> {
    let ctx = ndk_context::android_context();
    let activity = unsafe { JObject::from_raw(&env, ctx.context().cast()) };

    let context = env
        .call_method(
            &activity,
            jni::jni_str!("getApplicationContext"),
            jni::jni_sig!("()Landroid/content/Context;"),
            &[],
        )
        .map_err(|e| SystemError::NotificationInit(format!("getApplicationContext: {}", e)))?
        .l()
        .map_err(|e| SystemError::NotificationInit(format!("context cast: {}", e)))?;

    Ok(context)
}

fn get_notification_manager<'a>(
    env: &mut Env<'a>,
    context: &JObject<'a>,
) -> Result<JObject<'a>, SystemError> {
    let service_name: JString<'a> = env
        .new_string("notification")
        .map_err(|e| SystemError::NotificationInit(format!("service name: {}", e)))?;

    let manager = env
        .call_method(
            context,
            jni::jni_str!("getSystemService"),
            jni::jni_sig!("(Ljava/lang/String;)Ljava/lang/Object;"),
            &[JValue::Object(&service_name)],
        )
        .map_err(|e| SystemError::NotificationInit(format!("getSystemService: {}", e)))?
        .l()
        .map_err(|e| SystemError::NotificationInit(format!("manager cast: {}", e)))?;

    Ok(manager)
}

/// Android may show a public version on the lock screen even when the private
/// notification follows the user's chosen detail level. Never put identifiers
/// or call action capabilities in that public projection.
fn private_lockscreen_version(
    env: &mut Env<'_>,
    context: &JObject<'_>,
    builder: &JObject<'_>,
    channel: &str,
    icon: i32,
    body: &str,
) -> Result<(), SystemError> {
    let channel = env.new_string(channel).map_err(nerr)?;
    let public = env
        .new_object(
            jni::jni_str!("android/app/Notification$Builder"),
            jni::jni_sig!("(Landroid/content/Context;Ljava/lang/String;)V"),
            &[JValue::Object(context), JValue::Object(&channel)],
        )
        .map_err(nerr)?;
    let title = env.new_string("Chatter").map_err(nerr)?;
    let body = env.new_string(body).map_err(nerr)?;
    env.call_method(
        &public,
        jni::jni_str!("setSmallIcon"),
        jni::jni_sig!("(I)Landroid/app/Notification$Builder;"),
        &[JValue::Int(icon)],
    )
    .map_err(nerr)?;
    env.call_method(
        &public,
        jni::jni_str!("setContentTitle"),
        jni::jni_sig!("(Ljava/lang/CharSequence;)Landroid/app/Notification$Builder;"),
        &[JValue::Object(&title)],
    )
    .map_err(nerr)?;
    env.call_method(
        &public,
        jni::jni_str!("setContentText"),
        jni::jni_sig!("(Ljava/lang/CharSequence;)Landroid/app/Notification$Builder;"),
        &[JValue::Object(&body)],
    )
    .map_err(nerr)?;
    let notification = env
        .call_method(
            &public,
            jni::jni_str!("build"),
            jni::jni_sig!("()Landroid/app/Notification;"),
            &[],
        )
        .map_err(nerr)?
        .l()
        .map_err(nerr)?;
    env.call_method(
        builder,
        jni::jni_str!("setVisibility"),
        jni::jni_sig!("(I)Landroid/app/Notification$Builder;"),
        &[JValue::Int(0)],
    )
    .map_err(nerr)?; // VISIBILITY_PRIVATE
    env.call_method(
        builder,
        jni::jni_str!("setPublicVersion"),
        jni::jni_sig!("(Landroid/app/Notification;)Landroid/app/Notification$Builder;"),
        &[JValue::Object(&notification)],
    )
    .map_err(nerr)?;
    Ok(())
}
