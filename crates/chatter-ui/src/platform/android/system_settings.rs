//! Reading and opening the OS's own settings.

use jni::objects::{JObject, JValue};
use slint::ComponentHandle;
use std::sync::atomic::{AtomicI32, Ordering};

use super::activity::{activity, clear_exception, with_jni_env};

static FULL_SCREEN_CALL_STATE: AtomicI32 = AtomicI32::new(0);

pub fn apply_full_screen_call_state(window: &crate::MainWindow) {
    window
        .global::<crate::FullScreenCallPermission>()
        .set_state(FULL_SCREEN_CALL_STATE.load(Ordering::Relaxed));
}

/// `Configuration.fontScale` — Settings › Display › Font size. 1.0 when it
/// cannot be read.
///
/// Read once at startup: `fontScale` is not in the manifest's
/// `configChanges`, so Android recreates the Activity when it changes rather
/// than telling a running one.
pub fn font_scale() -> f32 {
    let result = (|| -> Result<f32, String> {
        with_jni_env(|env| {
            // The Application context: it carries the same configuration and,
            // unlike the Activity, exists before `onCreate` registers one.
            let ctx = ndk_context::android_context();
            let context = unsafe { jni::objects::JObject::from_raw(&env, ctx.context().cast()) };
            let res = (|| -> Result<f32, jni::errors::Error> {
                let resources = env
                    .call_method(
                        &context,
                        jni::jni_str!("getResources"),
                        jni::jni_sig!("()Landroid/content/res/Resources;"),
                        &[],
                    )?
                    .l()?;
                let config = env
                    .call_method(
                        &resources,
                        jni::jni_str!("getConfiguration"),
                        jni::jni_sig!("()Landroid/content/res/Configuration;"),
                        &[],
                    )?
                    .l()?;
                env.get_field(&config, jni::jni_str!("fontScale"), jni::jni_sig!("F"))?
                    .f()
            })();
            if res.is_err() {
                clear_exception(env);
            }
            res.map_err(|e| e.to_string())
        })
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
        with_jni_env(|env| {
            let res = (|| -> Result<(), jni::errors::Error> {
                let action = env.new_string("android.settings.CHANNEL_NOTIFICATION_SETTINGS")?;
                let intent = env.new_object(
                    jni::jni_str!("android/content/Intent"),
                    jni::jni_sig!("(Ljava/lang/String;)V"),
                    &[JValue::Object(&action)],
                )?;
                let package = env
                    .call_method(
                        &act,
                        jni::jni_str!("getPackageName"),
                        jni::jni_sig!("()Ljava/lang/String;"),
                        &[],
                    )?
                    .l()?;
                let put =
                    jni::jni_sig!("(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;");
                // Settings.EXTRA_APP_PACKAGE / Settings.EXTRA_CHANNEL_ID.
                let k_pkg = env.new_string("android.provider.extra.APP_PACKAGE")?;
                env.call_method(
                    &intent,
                    jni::jni_str!("putExtra"),
                    &put,
                    &[JValue::Object(&k_pkg), JValue::Object(&package)],
                )?;
                let k_chan = env.new_string("android.provider.extra.CHANNEL_ID")?;
                let chan = env.new_string(super::notifications::CHANNEL_ID)?;
                env.call_method(
                    &intent,
                    jni::jni_str!("putExtra"),
                    &put,
                    &[JValue::Object(&k_chan), JValue::Object(&chan)],
                )?;
                env.call_method(
                    &act,
                    jni::jni_str!("startActivity"),
                    jni::jni_sig!("(Landroid/content/Intent;)V"),
                    &[JValue::Object(&intent)],
                )?;
                Ok(())
            })();
            if res.is_err() {
                clear_exception(env);
            }
            res.map_err(|e| e.to_string())
        })
    })();
    if let Err(e) = result {
        tracing::error!("Android: opening notification channel settings failed: {e}");
    }
}

/// Set up the one-time explanation alongside Android's startup permission flow.
pub fn prepare_full_screen_call_prompt() {
    let result = with_jni_env(|env| {
        let act = activity()?;
        let message = env.new_string(chatter_i18n::tr(
            "Allow incoming calls to show on the lock screen.",
        )).map_err(|e| e.to_string())?;
        let allow = env.new_string(chatter_i18n::tr("Allow")).map_err(|e| e.to_string())?;
        let later = env.new_string(chatter_i18n::tr("Not now")).map_err(|e| e.to_string())?;
        let result = env.call_method(
            &act,
            jni::jni_str!("prepareFullScreenCallPrompt"),
            jni::jni_sig!("(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V"),
            &[JValue::Object(&message), JValue::Object(&allow), JValue::Object(&later)],
        );
        if result.is_err() {
            clear_exception(env);
        }
        result.map(|_| ()).map_err(|e| e.to_string())
    });
    if let Err(error) = result {
        tracing::warn!(%error, "Android full-screen call permission setup failed");
    }
}

pub fn open_full_screen_call_settings() {
    let result = with_jni_env(|env| {
        let act = activity()?;
        let result = env.call_method(
            &act,
            jni::jni_str!("openFullScreenCallSettings"),
            jni::jni_sig!("()V"),
            &[],
        );
        if result.is_err() {
            clear_exception(env);
        }
        result.map(|_| ()).map_err(|e| e.to_string())
    });
    if let Err(error) = result {
        tracing::warn!(%error, "Could not open Android full-screen call settings");
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_MainActivity_nativeFullScreenCallState(
    mut env: jni::EnvUnowned<'_>,
    _this: JObject,
    state: jni::sys::jint,
) {
    env.with_env(|_| -> jni::errors::Result<()> {
        FULL_SCREEN_CALL_STATE.store(state, Ordering::Relaxed);
        if let Some(weak) = super::call_actions::main_window() {
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak.upgrade() {
                    apply_full_screen_call_state(&window);
                }
            });
        }
        Ok(())
    })
    .resolve::<jni::errors::LogErrorAndDefault>();
}
