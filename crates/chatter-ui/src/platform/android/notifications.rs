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

const CHANNEL_ID: &str = "chatter_messages";
const CHANNEL_NAME: &str = "Messages";

const IMPORTANCE_HIGH: i32 = 4;

pub struct AndroidNotifications {
    channel_initialized: AtomicBool,
    next_id: AtomicI32,
}

impl AndroidNotifications {
    pub fn new() -> Self {
        Self {
            channel_initialized: AtomicBool::new(false),
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

    fn show_raw(&self, title: &str, body: &str, priority: u8) -> Result<(), SystemError> {
        if !self.channel_initialized.load(Ordering::Relaxed) {
            self.init_channel()?;
        }

        let notification_id = self.next_id.fetch_add(1, Ordering::Relaxed);

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

        env.call_method(
            &manager,
            "notify",
            "(ILandroid/app/Notification;)V",
            &[
                JValue::Int(notification_id),
                JValue::Object(&notification),
            ],
        ).map_err(|e| SystemError::NotificationSend(format!("notify: {}", e)))?;

        tracing::debug!(notification_id, title, "Android notification shown");
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
        let p = match priority {
            NotificationPriority::Urgent => 5,
            NotificationPriority::High => 4,
            NotificationPriority::Low => 2,
            NotificationPriority::Normal => 3,
        };
        self.show_raw(title, body, p)
    }

    fn init(&self) -> Result<(), SystemError> {
        self.init_channel()
    }
}

fn get_jni_env() -> Result<JNIEnv<'static>, SystemError> {
    let ctx = unsafe { ndk_context::android_context() };
    // JavaVM lives for the process lifetime, so leaking is safe.
    let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }
        .map_err(|e| SystemError::NotificationInit(format!("JavaVM: {}", e)))?;
    let vm = Box::leak(Box::new(vm));

    vm.get_env()
        .or_else(|_| vm.attach_current_thread_permanently())
        .map_err(|e| SystemError::NotificationInit(format!("JNIEnv: {}", e)))
}

fn get_app_context<'a>(env: &mut JNIEnv<'a>) -> Result<JObject<'a>, SystemError> {
    let ctx = unsafe { ndk_context::android_context() };
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
