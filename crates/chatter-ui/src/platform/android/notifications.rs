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
use jni::objects::{JObject, JString, JValue};
use jni::JNIEnv;

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

/// Stable notification id for a call: all events for one call share this id so
/// the ring can be cancelled / replaced. Derived from the call_id string.
fn call_notification_id(call_id: &str) -> i32 {
    // FNV-1a → positive i32. Stable across show/cancel for the same call_id.
    let mut h: u32 = 2166136261;
    for b in call_id.as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(16777619);
    }
    (h & 0x7fff_ffff) as i32
}

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

        let mut env = get_jni_env()?;
        let context = get_app_context(&mut env)?;

        let channel_id = env.new_string(CHANNEL_ID)
            .map_err(|e| SystemError::NotificationInit(format!("channel_id string: {}", e)))?;
        let channel_name = env.new_string(CHANNEL_NAME)
            .map_err(|e| SystemError::NotificationInit(format!("channel_name string: {}", e)))?;

        let channel = env.new_object(
            "android/app/NotificationChannel",
            "(Ljava/lang/String;Ljava/lang/CharSequence;I)V",
            &[
                JValue::Object(&channel_id),
                JValue::Object(&channel_name),
                JValue::Int(IMPORTANCE_HIGH),
            ],
        ).map_err(|e| SystemError::NotificationInit(format!("NotificationChannel: {}", e)))?;

        let manager = get_notification_manager(&mut env, &context)?;

        env.call_method(
            &manager,
            "createNotificationChannel",
            "(Landroid/app/NotificationChannel;)V",
            &[JValue::Object(&channel)],
        ).map_err(|e| SystemError::NotificationInit(format!("createNotificationChannel: {}", e)))?;

        self.channel_initialized.store(true, Ordering::Relaxed);
        tracing::info!("Android notification channel initialized");

        Ok(())
    }

    /// Initialize the dedicated calls channel: high importance + ringtone +
    /// vibration. Channel sound/importance are fixed at creation (Android
    /// ignores later edits), so this uses a distinct channel id from messages.
    fn init_calls_channel(&self) -> Result<(), SystemError> {
        if self.calls_channel_initialized.load(Ordering::Relaxed) {
            return Ok(());
        }
        let mut env = get_jni_env()?;
        let context = get_app_context(&mut env)?;

        let channel_id = env.new_string(CHANNEL_CALLS_ID).map_err(nerr)?;
        let channel_name = env.new_string(CHANNEL_CALLS_NAME).map_err(nerr)?;
        let channel = env
            .new_object(
                "android/app/NotificationChannel",
                "(Ljava/lang/String;Ljava/lang/CharSequence;I)V",
                &[
                    JValue::Object(&channel_id),
                    JValue::Object(&channel_name),
                    JValue::Int(IMPORTANCE_HIGH),
                ],
            )
            .map_err(nerr)?;

        env.call_method(&channel, "enableVibration", "(Z)V", &[JValue::Bool(1)])
            .map_err(nerr)?;

        // Ring with the default ringtone (USAGE_NOTIFICATION_RINGTONE) so it
        // sounds like a call, not a message ping.
        // RingtoneManager.getDefaultUri(TYPE_RINGTONE = 1)
        let ringtone_uri = env
            .call_static_method(
                "android/media/RingtoneManager",
                "getDefaultUri",
                "(I)Landroid/net/Uri;",
                &[JValue::Int(1)],
            )
            .map_err(nerr)?
            .l()
            .map_err(nerr)?;
        // AudioAttributes.Builder().setUsage(6 = USAGE_NOTIFICATION_RINGTONE)
        //   .setContentType(4 = CONTENT_TYPE_SONIFICATION).build()
        let ab = env
            .new_object("android/media/AudioAttributes$Builder", "()V", &[])
            .map_err(nerr)?;
        let ab = env
            .call_method(&ab, "setUsage", "(I)Landroid/media/AudioAttributes$Builder;", &[JValue::Int(6)])
            .map_err(nerr)?
            .l()
            .map_err(nerr)?;
        let ab = env
            .call_method(&ab, "setContentType", "(I)Landroid/media/AudioAttributes$Builder;", &[JValue::Int(4)])
            .map_err(nerr)?
            .l()
            .map_err(nerr)?;
        let audio_attrs = env
            .call_method(&ab, "build", "()Landroid/media/AudioAttributes;", &[])
            .map_err(nerr)?
            .l()
            .map_err(nerr)?;
        env.call_method(
            &channel,
            "setSound",
            "(Landroid/net/Uri;Landroid/media/AudioAttributes;)V",
            &[JValue::Object(&ringtone_uri), JValue::Object(&audio_attrs)],
        )
        .map_err(nerr)?;

        let manager = get_notification_manager(&mut env, &context)?;
        env.call_method(
            &manager,
            "createNotificationChannel",
            "(Landroid/app/NotificationChannel;)V",
            &[JValue::Object(&channel)],
        )
        .map_err(nerr)?;

        self.calls_channel_initialized.store(true, Ordering::Relaxed);
        tracing::info!("Android calls notification channel initialized");
        Ok(())
    }

    /// Post a ringing, full-screen incoming-call notification with Accept /
    /// Decline action buttons. Tapping a button broadcasts to
    /// `CallActionReceiver`, which calls back into Rust to accept/decline
    /// `call_id` without opening the app. The body/full-screen intent opens the
    /// in-app call screen.
    fn show_incoming_call_raw(&self, call_id: &str, title: &str, body: &str) -> Result<(), SystemError> {
        self.init_calls_channel()?;

        let id = call_notification_id(call_id);
        let mut env = get_jni_env()?;
        let context = get_app_context(&mut env)?;

        let j_title = env.new_string(title).map_err(nerr)?;
        let j_body = env.new_string(body).map_err(nerr)?;
        let j_channel = env.new_string(CHANNEL_CALLS_ID).map_err(nerr)?;

        let builder = env
            .new_object(
                "android/app/Notification$Builder",
                "(Landroid/content/Context;Ljava/lang/String;)V",
                &[JValue::Object(&context), JValue::Object(&j_channel)],
            )
            .map_err(nerr)?;

        env.call_method(&builder, "setSmallIcon", "(I)Landroid/app/Notification$Builder;", &[JValue::Int(IC_CALL)]).map_err(nerr)?;
        env.call_method(&builder, "setContentTitle", "(Ljava/lang/CharSequence;)Landroid/app/Notification$Builder;", &[JValue::Object(&j_title)]).map_err(nerr)?;
        env.call_method(&builder, "setContentText", "(Ljava/lang/CharSequence;)Landroid/app/Notification$Builder;", &[JValue::Object(&j_body)]).map_err(nerr)?;
        // PRIORITY_MAX (2) for heads-up on pre-26; category=call; ongoing so it
        // can't be swiped away while ringing; auto-timeout if unanswered,
        // when core stops ringing it too (setTimeoutAfter: API 26 = minSdk).
        env.call_method(&builder, "setPriority", "(I)Landroid/app/Notification$Builder;", &[JValue::Int(2)]).map_err(nerr)?;
        let j_cat = env.new_string("call").map_err(nerr)?;
        env.call_method(&builder, "setCategory", "(Ljava/lang/String;)Landroid/app/Notification$Builder;", &[JValue::Object(&j_cat)]).map_err(nerr)?;
        env.call_method(&builder, "setOngoing", "(Z)Landroid/app/Notification$Builder;", &[JValue::Bool(1)]).map_err(nerr)?;
        env.call_method(&builder, "setAutoCancel", "(Z)Landroid/app/Notification$Builder;", &[JValue::Bool(1)]).map_err(nerr)?;
        env.call_method(&builder, "setTimeoutAfter", "(J)Landroid/app/Notification$Builder;", &[JValue::Long(chatter_core::services::call_worker::RING_TIMEOUT.as_millis() as i64)]).map_err(nerr)?;

        // Full-screen + content intent → open the in-app ring screen (no auto-accept).
        let fs_pi = build_activity_intent(&mut env, &context, call_id, id, false)?;
        env.call_method(&builder, "setFullScreenIntent", "(Landroid/app/PendingIntent;Z)Landroid/app/Notification$Builder;", &[JValue::Object(&fs_pi), JValue::Bool(1)]).map_err(nerr)?;
        env.call_method(&builder, "setContentIntent", "(Landroid/app/PendingIntent;)Landroid/app/Notification$Builder;", &[JValue::Object(&fs_pi)]).map_err(nerr)?;

        // Accept = activity intent: foregrounds the app (a broadcast receiver
        // can't reliably start an activity on Android 14+) and auto-accepts.
        // Decline = headless broadcast.
        // PendingIntent request codes only need to differ from each other and
        // from the content intent's; `id` is any positive i32, so `id * 2`
        // overflowed (a panic that killed the notification bridge for every
        // later call). Wrapping keeps them distinct without overflow.
        let accept_pi = build_activity_intent(&mut env, &context, call_id, id.wrapping_mul(2), true)?;
        let accept = make_action(&mut env, IC_CALL, "Accept", &accept_pi)?;
        let decline = build_action(&mut env, &context, "com.chatter.client.CALL_DECLINE", "Decline", call_id, IC_CLOSE, id.wrapping_mul(2).wrapping_add(1))?;
        env.call_method(&builder, "addAction", "(Landroid/app/Notification$Action;)Landroid/app/Notification$Builder;", &[JValue::Object(&accept)]).map_err(nerr)?;
        env.call_method(&builder, "addAction", "(Landroid/app/Notification$Action;)Landroid/app/Notification$Builder;", &[JValue::Object(&decline)]).map_err(nerr)?;

        let notification = env.call_method(&builder, "build", "()Landroid/app/Notification;", &[]).map_err(nerr)?.l().map_err(nerr)?;
        let manager = get_notification_manager(&mut env, &context)?;
        env.call_method(&manager, "notify", "(ILandroid/app/Notification;)V", &[JValue::Int(id), JValue::Object(&notification)]).map_err(nerr)?;

        tracing::info!(call_id, id, "Posted ringing incoming-call notification");
        Ok(())
    }

    fn cancel_call_raw(&self, call_id: &str) -> Result<(), SystemError> {
        let id = call_notification_id(call_id);
        let mut env = get_jni_env()?;
        let context = get_app_context(&mut env)?;
        let manager = get_notification_manager(&mut env, &context)?;
        env.call_method(&manager, "cancel", "(I)V", &[JValue::Int(id)]).map_err(nerr)?;
        Ok(())
    }

    /// Post a notification. With a `conversation` tag it is posted as
    /// (tag, `CONVERSATION_NOTIFICATION_ID`), so the conversation's next one
    /// replaces it and `cancel_conversation_raw` dismisses it; untagged ones
    /// get a fresh id each.
    fn show_raw(&self, conversation: Option<&str>, title: &str, body: &str, priority: u8) -> Result<(), SystemError> {
        if !self.channel_initialized.load(Ordering::Relaxed) {
            self.init_channel()?;
        }

        let notification_id = match conversation {
            Some(_) => CONVERSATION_NOTIFICATION_ID,
            None => self.next_id.fetch_add(1, Ordering::Relaxed),
        };

        let mut env = get_jni_env()?;
        let context = get_app_context(&mut env)?;

        let j_title = env.new_string(title)
            .map_err(|e| SystemError::NotificationSend(format!("title string: {}", e)))?;
        let j_body = env.new_string(body)
            .map_err(|e| SystemError::NotificationSend(format!("body string: {}", e)))?;
        let j_channel_id = env.new_string(CHANNEL_ID)
            .map_err(|e| SystemError::NotificationSend(format!("channel_id string: {}", e)))?;

        let builder = env.new_object(
            "android/app/Notification$Builder",
            "(Landroid/content/Context;Ljava/lang/String;)V",
            &[
                JValue::Object(&context),
                JValue::Object(&j_channel_id),
            ],
        ).map_err(|e| SystemError::NotificationSend(format!("Notification.Builder: {}", e)))?;

        // android.R.drawable.ic_dialog_info
        const IC_DIALOG_INFO: i32 = 17301659;

        env.call_method(
            &builder,
            "setSmallIcon",
            "(I)Landroid/app/Notification$Builder;",
            &[JValue::Int(IC_DIALOG_INFO)],
        ).map_err(|e| SystemError::NotificationSend(format!("setSmallIcon: {}", e)))?;

        env.call_method(
            &builder,
            "setContentTitle",
            "(Ljava/lang/CharSequence;)Landroid/app/Notification$Builder;",
            &[JValue::Object(&j_title)],
        ).map_err(|e| SystemError::NotificationSend(format!("setContentTitle: {}", e)))?;

        env.call_method(
            &builder,
            "setContentText",
            "(Ljava/lang/CharSequence;)Landroid/app/Notification$Builder;",
            &[JValue::Object(&j_body)],
        ).map_err(|e| SystemError::NotificationSend(format!("setContentText: {}", e)))?;

        env.call_method(
            &builder,
            "setAutoCancel",
            "(Z)Landroid/app/Notification$Builder;",
            &[JValue::Bool(1)],
        ).map_err(|e| SystemError::NotificationSend(format!("setAutoCancel: {}", e)))?;

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
            "setPriority",
            "(I)Landroid/app/Notification$Builder;",
            &[JValue::Int(android_priority)],
        ).map_err(|e| SystemError::NotificationSend(format!("setPriority: {}", e)))?;

        let notification = env.call_method(
            &builder,
            "build",
            "()Landroid/app/Notification;",
            &[],
        ).map_err(|e| SystemError::NotificationSend(format!("build: {}", e)))?
        .l()
        .map_err(|e| SystemError::NotificationSend(format!("build result: {}", e)))?;

        let manager = get_notification_manager(&mut env, &context)?;

        match conversation {
            Some(tag) => {
                let j_tag = env.new_string(tag).map_err(nerr)?;
                env.call_method(
                    &manager,
                    "notify",
                    "(Ljava/lang/String;ILandroid/app/Notification;)V",
                    &[
                        JValue::Object(&j_tag),
                        JValue::Int(notification_id),
                        JValue::Object(&notification),
                    ],
                )
            }
            None => env.call_method(
                &manager,
                "notify",
                "(ILandroid/app/Notification;)V",
                &[
                    JValue::Int(notification_id),
                    JValue::Object(&notification),
                ],
            ),
        }
        .map_err(|e| SystemError::NotificationSend(format!("notify: {}", e)))?;

        tracing::debug!(notification_id, title, "Android notification shown");
        Ok(())
    }

    fn cancel_conversation_raw(&self, conversation: &str) -> Result<(), SystemError> {
        let mut env = get_jni_env()?;
        let context = get_app_context(&mut env)?;
        let manager = get_notification_manager(&mut env, &context)?;
        let j_tag = env.new_string(conversation).map_err(nerr)?;
        env.call_method(
            &manager,
            "cancel",
            "(Ljava/lang/String;I)V",
            &[JValue::Object(&j_tag), JValue::Int(CONVERSATION_NOTIFICATION_ID)],
        )
        .map_err(nerr)?;
        Ok(())
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
        self.show_raw(Some(conversation), title, body, android_priority_level(priority))
    }

    fn cancel_conversation(&self, conversation: &str) -> Result<(), SystemError> {
        self.cancel_conversation_raw(conversation)
    }

    fn show_incoming_call(&self, call_id: &str, title: &str, body: &str) -> Result<(), SystemError> {
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
    let id = call_notification_id(call_id);
    if let Ok(mut env) = get_jni_env() {
        if let Ok(context) = get_app_context(&mut env) {
            if let Ok(manager) = get_notification_manager(&mut env, &context) {
                let _ = env.call_method(&manager, "cancel", "(I)V", &[JValue::Int(id)]);
            }
        }
    }
}

/// Build a `Notification.Action` whose PendingIntent broadcasts `action` (with
/// the `call_id` extra) to `CallActionReceiver`.
fn build_action<'a>(
    env: &mut JNIEnv<'a>,
    context: &JObject<'a>,
    action: &str,
    label: &str,
    call_id: &str,
    icon: i32,
    request_code: i32,
) -> Result<JObject<'a>, SystemError> {
    // Target the receiver by class NAME via setClassName, not find_class +
    // Intent(Context, Class): this notification is built on the JNI poller
    // thread, whose classloader can't resolve app classes, so find_class throws.
    let intent = env
        .new_object("android/content/Intent", "()V", &[])
        .map_err(nerr)?;
    let cls_name = env.new_string("com.chatter.client.CallActionReceiver").map_err(nerr)?;
    env.call_method(
        &intent,
        "setClassName",
        "(Landroid/content/Context;Ljava/lang/String;)Landroid/content/Intent;",
        &[JValue::Object(context), JValue::Object(&cls_name)],
    )
    .map_err(nerr)?;
    let j_action = env.new_string(action).map_err(nerr)?;
    env.call_method(&intent, "setAction", "(Ljava/lang/String;)Landroid/content/Intent;", &[JValue::Object(&j_action)]).map_err(nerr)?;
    let k = env.new_string("call_id").map_err(nerr)?;
    let v = env.new_string(call_id).map_err(nerr)?;
    env.call_method(&intent, "putExtra", "(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;", &[JValue::Object(&k), JValue::Object(&v)]).map_err(nerr)?;

    let pi = env
        .call_static_method(
            "android/app/PendingIntent",
            "getBroadcast",
            "(Landroid/content/Context;ILandroid/content/Intent;I)Landroid/app/PendingIntent;",
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
    env: &mut JNIEnv<'a>,
    icon: i32,
    label: &str,
    pi: &JObject<'a>,
) -> Result<JObject<'a>, SystemError> {
    let j_label = env.new_string(label).map_err(nerr)?;
    let action_builder = env
        .new_object(
            "android/app/Notification$Action$Builder",
            "(ILjava/lang/CharSequence;Landroid/app/PendingIntent;)V",
            &[JValue::Int(icon), JValue::Object(&j_label), JValue::Object(pi)],
        )
        .map_err(nerr)?;
    env.call_method(&action_builder, "build", "()Landroid/app/Notification$Action;", &[])
        .map_err(nerr)?
        .l()
        .map_err(nerr)
}

/// Build a PendingIntent that launches `MainActivity` (the in-app call screen),
/// carrying the incoming call_id. When `auto_accept` is set, MainActivity
/// accepts the call on arrival (the Accept button); otherwise it just opens the
/// ring screen (full-screen / content intent). Launched via an activity
/// PendingIntent so the notification can foreground the app — a broadcast
/// receiver can't reliably start an activity on Android 14+.
fn build_activity_intent<'a>(
    env: &mut JNIEnv<'a>,
    context: &JObject<'a>,
    call_id: &str,
    request_code: i32,
    auto_accept: bool,
) -> Result<JObject<'a>, SystemError> {
    // setClassName (string) instead of find_class — see build_action.
    let intent = env
        .new_object("android/content/Intent", "()V", &[])
        .map_err(nerr)?;
    let cls_name = env.new_string("com.chatter.client.MainActivity").map_err(nerr)?;
    env.call_method(
        &intent,
        "setClassName",
        "(Landroid/content/Context;Ljava/lang/String;)Landroid/content/Intent;",
        &[JValue::Object(context), JValue::Object(&cls_name)],
    )
    .map_err(nerr)?;
    // FLAG_ACTIVITY_NEW_TASK | FLAG_ACTIVITY_SINGLE_TOP
    env.call_method(&intent, "addFlags", "(I)Landroid/content/Intent;", &[JValue::Int(0x1000_0000 | 0x2000_0000)]).map_err(nerr)?;
    let k = env.new_string("incoming_call_id").map_err(nerr)?;
    let v = env.new_string(call_id).map_err(nerr)?;
    env.call_method(&intent, "putExtra", "(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;", &[JValue::Object(&k), JValue::Object(&v)]).map_err(nerr)?;
    if auto_accept {
        let ak = env.new_string("auto_accept").map_err(nerr)?;
        env.call_method(&intent, "putExtra", "(Ljava/lang/String;Z)Landroid/content/Intent;", &[JValue::Object(&ak), JValue::Bool(1)]).map_err(nerr)?;
    }

    let pi = env
        .call_static_method(
            "android/app/PendingIntent",
            "getActivity",
            "(Landroid/content/Context;ILandroid/content/Intent;I)Landroid/app/PendingIntent;",
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

fn get_jni_env() -> Result<JNIEnv<'static>, SystemError> {
    let ctx = ndk_context::android_context();
    // JavaVM lives for the process lifetime, so leaking is safe.
    let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }
        .map_err(|e| SystemError::NotificationInit(format!("JavaVM: {}", e)))?;
    let vm = Box::leak(Box::new(vm));

    vm.get_env()
        .or_else(|_| vm.attach_current_thread_permanently())
        .map_err(|e| SystemError::NotificationInit(format!("JNIEnv: {}", e)))
}

fn get_app_context<'a>(env: &mut JNIEnv<'a>) -> Result<JObject<'a>, SystemError> {
    let ctx = ndk_context::android_context();
    let activity = unsafe { JObject::from_raw(ctx.context().cast()) };

    let context = env.call_method(
        &activity,
        "getApplicationContext",
        "()Landroid/content/Context;",
        &[],
    ).map_err(|e| SystemError::NotificationInit(format!("getApplicationContext: {}", e)))?
    .l()
    .map_err(|e| SystemError::NotificationInit(format!("context cast: {}", e)))?;

    Ok(context)
}

fn get_notification_manager<'a>(
    env: &mut JNIEnv<'a>,
    context: &JObject<'a>,
) -> Result<JObject<'a>, SystemError> {
    let service_name: JString<'a> = env.new_string("notification")
        .map_err(|e| SystemError::NotificationInit(format!("service name: {}", e)))?;

    let manager = env.call_method(
        context,
        "getSystemService",
        "(Ljava/lang/String;)Ljava/lang/Object;",
        &[JValue::Object(&service_name)],
    ).map_err(|e| SystemError::NotificationInit(format!("getSystemService: {}", e)))?
    .l()
    .map_err(|e| SystemError::NotificationInit(format!("manager cast: {}", e)))?;

    Ok(manager)
}
