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
//! The caller (NotificationService) controls what title/body are passed.
//! This module just displays whatever it receives — privacy filtering
//! happens upstream in `notification_service.rs`.

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use jni::objects::{JObject, JString, JValue};
use jni::JNIEnv;

use crate::error::SystemError;

/// Notification channel ID (Android 8+ requirement).
const CHANNEL_ID: &str = "chatter_messages";

/// Notification channel name (user-visible in Android Settings).
const CHANNEL_NAME: &str = "Messages";

/// Android notification priority constants.
/// Maps to android.app.NotificationManager.IMPORTANCE_*
const IMPORTANCE_DEFAULT: i32 = 3;
const IMPORTANCE_HIGH: i32 = 4;
const IMPORTANCE_LOW: i32 = 2;
const IMPORTANCE_MAX: i32 = 5;

/// Android notification display via JNI.
///
/// Calls `NotificationManager.notify()` to show system notifications.
/// Must call `init_channel()` once before showing any notifications.
pub struct AndroidNotifications {
    channel_initialized: AtomicBool,
    next_id: AtomicI32,
}

impl AndroidNotifications {
    /// Create a new Android notifications instance.
    pub fn new() -> Self {
        Self {
            channel_initialized: AtomicBool::new(false),
            next_id: AtomicI32::new(1),
        }
    }

    /// Initialize the notification channel (required Android 8+, API 26).
    ///
    /// Must be called once at app startup before any `show()` calls.
    /// Safe to call multiple times — only initializes once.
    pub fn init_channel(&self) -> Result<(), SystemError> {
        if self.channel_initialized.load(Ordering::Relaxed) {
            return Ok(());
        }

        let mut env = get_jni_env()?;
        let context = get_app_context(&mut env)?;

        // Create NotificationChannel
        let channel_id = env.new_string(CHANNEL_ID)
            .map_err(|e| SystemError::NotificationInit(format!("channel_id string: {}", e)))?;
        let channel_name = env.new_string(CHANNEL_NAME)
            .map_err(|e| SystemError::NotificationInit(format!("channel_name string: {}", e)))?;

        // new NotificationChannel(channelId, channelName, IMPORTANCE_HIGH)
        let channel = env.new_object(
            "android/app/NotificationChannel",
            "(Ljava/lang/String;Ljava/lang/CharSequence;I)V",
            &[
                JValue::Object(&channel_id),
                JValue::Object(&channel_name),
                JValue::Int(IMPORTANCE_HIGH),
            ],
        ).map_err(|e| SystemError::NotificationInit(format!("NotificationChannel: {}", e)))?;

        // Get NotificationManager
        let manager = get_notification_manager(&mut env, &context)?;

        // manager.createNotificationChannel(channel)
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

    /// Show an Android system notification.
    ///
    /// # Arguments
    ///
    /// * `title` - Notification title (already privacy-filtered by caller)
    /// * `body` - Notification body (already privacy-filtered by caller)
    /// * `priority` - 1-5 scale (maps to Android importance)
    pub fn show(&self, title: &str, body: &str, priority: u8) -> Result<(), SystemError> {
        // Ensure channel is initialized
        if !self.channel_initialized.load(Ordering::Relaxed) {
            self.init_channel()?;
        }

        let notification_id = self.next_id.fetch_add(1, Ordering::Relaxed);

        let mut env = get_jni_env()?;
        let context = get_app_context(&mut env)?;

        // Create strings
        let j_title = env.new_string(title)
            .map_err(|e| SystemError::NotificationSend(format!("title string: {}", e)))?;
        let j_body = env.new_string(body)
            .map_err(|e| SystemError::NotificationSend(format!("body string: {}", e)))?;
        let j_channel_id = env.new_string(CHANNEL_ID)
            .map_err(|e| SystemError::NotificationSend(format!("channel_id string: {}", e)))?;

        // new Notification.Builder(context, channelId)
        let builder = env.new_object(
            "android/app/Notification$Builder",
            "(Landroid/content/Context;Ljava/lang/String;)V",
            &[
                JValue::Object(&context),
                JValue::Object(&j_channel_id),
            ],
        ).map_err(|e| SystemError::NotificationSend(format!("Notification.Builder: {}", e)))?;

        // Use built-in Android icon (ic_dialog_info)
        // android.R.drawable.ic_dialog_info = 17301659
        const IC_DIALOG_INFO: i32 = 17301659;

        // builder.setSmallIcon(iconResId)
        env.call_method(
            &builder,
            "setSmallIcon",
            "(I)Landroid/app/Notification$Builder;",
            &[JValue::Int(IC_DIALOG_INFO)],
        ).map_err(|e| SystemError::NotificationSend(format!("setSmallIcon: {}", e)))?;

        // builder.setContentTitle(title)
        env.call_method(
            &builder,
            "setContentTitle",
            "(Ljava/lang/CharSequence;)Landroid/app/Notification$Builder;",
            &[JValue::Object(&j_title)],
        ).map_err(|e| SystemError::NotificationSend(format!("setContentTitle: {}", e)))?;

        // builder.setContentText(body)
        env.call_method(
            &builder,
            "setContentText",
            "(Ljava/lang/CharSequence;)Landroid/app/Notification$Builder;",
            &[JValue::Object(&j_body)],
        ).map_err(|e| SystemError::NotificationSend(format!("setContentText: {}", e)))?;

        // builder.setAutoCancel(true)
        env.call_method(
            &builder,
            "setAutoCancel",
            "(Z)Landroid/app/Notification$Builder;",
            &[JValue::Bool(1)],
        ).map_err(|e| SystemError::NotificationSend(format!("setAutoCancel: {}", e)))?;

        // builder.setPriority(priority) — for pre-API 26 compat
        let android_priority = match priority {
            5 => 2,  // PRIORITY_MAX
            4 => 1,  // PRIORITY_HIGH
            2 => -1, // PRIORITY_LOW
            1 => -2, // PRIORITY_MIN
            _ => 0,  // PRIORITY_DEFAULT
        };
        env.call_method(
            &builder,
            "setPriority",
            "(I)Landroid/app/Notification$Builder;",
            &[JValue::Int(android_priority)],
        ).map_err(|e| SystemError::NotificationSend(format!("setPriority: {}", e)))?;

        // Notification notification = builder.build()
        let notification = env.call_method(
            &builder,
            "build",
            "()Landroid/app/Notification;",
            &[],
        ).map_err(|e| SystemError::NotificationSend(format!("build: {}", e)))?
        .l()
        .map_err(|e| SystemError::NotificationSend(format!("build result: {}", e)))?;

        // Get NotificationManager and notify
        let manager = get_notification_manager(&mut env, &context)?;

        // manager.notify(id, notification)
        env.call_method(
            &manager,
            "notify",
            "(ILandroid/app/Notification;)V",
            &[
                JValue::Int(notification_id),
                JValue::Object(&notification),
            ],
        ).map_err(|e| SystemError::NotificationSend(format!("notify: {}", e)))?;

        tracing::debug!(
            notification_id = notification_id,
            title = title,
            "Android notification shown"
        );

        Ok(())
    }
}

impl Default for AndroidNotifications {
    fn default() -> Self {
        Self::new()
    }
}

/// Get the JNI environment for the current thread.
fn get_jni_env() -> Result<JNIEnv<'static>, SystemError> {
    let ctx = unsafe { ndk_context::android_context() };
    // JavaVM lives for the process lifetime, so leaking is safe
    let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }
        .map_err(|e| SystemError::NotificationInit(format!("JavaVM: {}", e)))?;
    let vm = Box::leak(Box::new(vm));

    vm.get_env()
        .or_else(|_| vm.attach_current_thread_permanently())
        .map_err(|e| SystemError::NotificationInit(format!("JNIEnv: {}", e)))
}

/// Get the Android application context.
fn get_app_context<'a>(env: &mut JNIEnv<'a>) -> Result<JObject<'a>, SystemError> {
    let ctx = unsafe { ndk_context::android_context() };
    let activity = unsafe { JObject::from_raw(ctx.context().cast()) };

    // activity.getApplicationContext()
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

/// Get the Android NotificationManager system service.
fn get_notification_manager<'a>(
    env: &mut JNIEnv<'a>,
    context: &JObject<'a>,
) -> Result<JObject<'a>, SystemError> {
    // "notification" = Context.NOTIFICATION_SERVICE
    let service_name = env.new_string("notification")
        .map_err(|e| SystemError::NotificationInit(format!("service name: {}", e)))?;

    // context.getSystemService("notification")
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
