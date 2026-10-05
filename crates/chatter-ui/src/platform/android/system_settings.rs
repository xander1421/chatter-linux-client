//! Reading and opening the OS's own settings.

use jni::objects::JValue;

use super::activity::{activity, clear_exception, jni_env};

/// `Configuration.fontScale` — Settings › Display › Font size. 1.0 when it
/// cannot be read.
///
/// Read once at startup: `fontScale` is not in the manifest's
/// `configChanges`, so Android recreates the Activity when it changes rather
/// than telling a running one.
pub fn font_scale() -> f32 {
    let result = (|| -> Result<f32, String> {
        let mut env = jni_env()?;
        // The Application context: it carries the same configuration and,
        // unlike the Activity, exists before `onCreate` registers one.
        let ctx = ndk_context::android_context();
        let context = unsafe { jni::objects::JObject::from_raw(ctx.context().cast()) };
        let res = (|| -> Result<f32, jni::errors::Error> {
            let resources = env
                .call_method(
                    &context,
                    "getResources",
                    "()Landroid/content/res/Resources;",
                    &[],
                )?
                .l()?;
            let config = env
                .call_method(
                    &resources,
                    "getConfiguration",
                    "()Landroid/content/res/Configuration;",
                    &[],
                )?
                .l()?;
            env.get_field(&config, "fontScale", "F")?.f()
        })();
        if res.is_err() {
            clear_exception(&mut env);
        }
        res.map_err(|e| e.to_string())
    })();
    result.unwrap_or_else(|e| {
        tracing::warn!("Android: reading fontScale failed ({e}); using 1.0");
        1.0
    })
}

/// Open the system page for the message notification channel, where the user
/// picks its sound and vibration (`Settings.ACTION_CHANNEL_NOTIFICATION_SETTINGS`,
/// API 26 = our minSdk). The channel exists from startup (`init_channel`).
pub fn open_message_notification_settings() {
    let result = (|| -> Result<(), String> {
        let act = activity()?;
        let mut env = jni_env()?;
        let res = (|| -> Result<(), jni::errors::Error> {
            let action = env.new_string("android.settings.CHANNEL_NOTIFICATION_SETTINGS")?;
            let intent = env.new_object(
                "android/content/Intent",
                "(Ljava/lang/String;)V",
                &[JValue::Object(&action)],
            )?;
            let package = env
                .call_method(act, "getPackageName", "()Ljava/lang/String;", &[])?
                .l()?;
            let put = "(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;";
            // Settings.EXTRA_APP_PACKAGE / Settings.EXTRA_CHANNEL_ID.
            let k_pkg = env.new_string("android.provider.extra.APP_PACKAGE")?;
            env.call_method(
                &intent,
                "putExtra",
                put,
                &[JValue::Object(&k_pkg), JValue::Object(&package)],
            )?;
            let k_chan = env.new_string("android.provider.extra.CHANNEL_ID")?;
            let chan = env.new_string(super::notifications::CHANNEL_ID)?;
            env.call_method(
                &intent,
                "putExtra",
                put,
                &[JValue::Object(&k_chan), JValue::Object(&chan)],
            )?;
            env.call_method(
                act,
                "startActivity",
                "(Landroid/content/Intent;)V",
                &[JValue::Object(&intent)],
            )?;
            Ok(())
        })();
        if res.is_err() {
            clear_exception(&mut env);
        }
        res.map_err(|e| e.to_string())
    })();
    if let Err(e) = result {
        tracing::error!("Android: opening notification channel settings failed: {e}");
    }
}
