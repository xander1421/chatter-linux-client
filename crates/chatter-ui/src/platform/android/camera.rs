//! Android camera capture via CameraX (Kotlin) bridged to chatter-core.
//!
//! chatter-core owns the LiveKit video sink but can't touch the camera; this
//! crate can reach Kotlin over JNI but has no livekit dep on mobile. So we
//! implement chatter-core's `PlatformCamera` trait here: `start`/`stop` drive
//! `MainActivity.startCamera/stopCamera`, and `nativeOnCameraFrame` forwards the
//! I420 bytes CameraX produced into `chatter_core::calls::android_camera`.

use chatter_core::calls::android_camera::{self, PlatformCamera};
use jni::objects::{JByteArray, JObject, JValue};
use jni::sys::{jint, jlong};
use jni::JNIEnv;

use super::activity;

struct CameraX;

impl PlatformCamera for CameraX {
    fn start(&self, width: u32, height: u32, fps: u32, front: bool) {
        if let Err(e) = call_void(
            "startCamera",
            "(IIIZ)V",
            &[
                JValue::Int(width as jint),
                JValue::Int(height as jint),
                JValue::Int(fps as jint),
                JValue::Bool(front as u8),
            ],
        ) {
            tracing::error!("Android camera: startCamera failed: {e}");
        }
    }

    fn stop(&self) {
        if let Err(e) = call_void("stopCamera", "()V", &[]) {
            tracing::error!("Android camera: stopCamera failed: {e}");
        }
    }
}

/// Register CameraX as chatter-core's platform camera. Call once at startup.
pub fn init() {
    android_camera::register_platform_camera(Box::new(CameraX));
}

fn call_void(method: &str, sig: &str, args: &[JValue]) -> Result<(), String> {
    let mut env = activity::jni_env()?;
    let act = activity::activity()?;
    let res = env.call_method(act, method, sig, args);
    if res.is_err() {
        activity::clear_exception(&mut env);
    }
    res.map_err(|e| e.to_string()).map(|_| ())
}

/// JNI callback from `MainActivity` — one CameraX frame as tightly-packed I420.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_MainActivity_nativeOnCameraFrame(
    env: JNIEnv,
    _this: JObject,
    data: JByteArray,
    width: jint,
    height: jint,
    rotation: jint,
    timestamp_us: jlong,
) {
    let bytes = match env.convert_byte_array(&data) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!("Android camera: bad frame array: {e}");
            return;
        }
    };
    android_camera::push_frame_i420(&bytes, width as u32, height as u32, rotation, timestamp_us);
}
