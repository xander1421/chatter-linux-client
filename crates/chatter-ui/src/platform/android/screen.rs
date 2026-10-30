//! JNI bridge for Android MediaProjection screen capture.

use chatter_core::calls::android_screen::{self, PlatformScreen};
use jni::objects::{JByteArray, JObject, JValue};
use jni::sys::{jint, jlong};

use super::activity;

struct MediaProjection;

impl PlatformScreen for MediaProjection {
    fn start(&self, token: u64) -> Result<(), String> {
        call("startScreenShare", token)
    }

    fn stop(&self, token: u64) {
        if let Err(error) = call("stopScreenShare", token) {
            tracing::warn!("Android screen stop failed: {error}");
        }
    }
}

fn call(method: &str, token: u64) -> Result<(), String> {
    activity::with_jni_env(|env| {
        let act = activity::activity()?;
        let result = env.call_method(
            act,
            jni::strings::JNIString::from(method),
            jni::signature::RuntimeMethodSignature::from_str("(J)V")
                .map_err(|error| error.to_string())?
                .method_signature(),
            &[JValue::Long(token as jlong)],
        );
        if result.is_err() {
            activity::clear_exception(env);
        }
        result.map(|_| ()).map_err(|error| error.to_string())
    })
}

pub fn init() {
    android_screen::register_platform_screen(Box::new(MediaProjection));
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_ScreenShareService_nativeOnScreenFrame(
    mut env: jni::EnvUnowned<'_>,
    _this: JObject,
    token: jlong,
    data: JByteArray,
    width: jint,
    height: jint,
    timestamp_us: jlong,
) {
    env.with_env(|env| -> jni::errors::Result<_> {
        if let Ok(bytes) = env.convert_byte_array(&data) {
            android_screen::push_frame(
                token as u64,
                &bytes,
                width as u32,
                height as u32,
                timestamp_us,
            );
        }
        Ok(())
    })
    .resolve::<jni::errors::LogErrorAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_ScreenShareService_nativeOnScreenStarted(
    _env: jni::EnvUnowned<'_>,
    _this: JObject,
    token: jlong,
    width: jint,
    height: jint,
) {
    android_screen::started(token as u64, width as u32, height as u32);
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_ScreenShareService_nativeOnScreenStopped(
    _env: jni::EnvUnowned<'_>,
    _this: JObject,
    token: jlong,
) {
    android_screen::stopped(token as u64);
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_MainActivity_nativeOnScreenShareDenied(
    _env: jni::EnvUnowned<'_>,
    _this: JObject,
    token: jlong,
) {
    android_screen::stopped(token as u64);
}
