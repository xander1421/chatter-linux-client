//! Android camera capture via CameraX (Kotlin) bridged to chatter-core.
//!
//! chatter-core owns the LiveKit video sink but can't touch the camera; this
//! crate can reach Kotlin over JNI but has no livekit dep on mobile. So we
//! implement chatter-core's `PlatformCamera` trait here: `start`/`stop` drive
//! `MainActivity.startCamera/stopCamera`, and `nativeOnCameraFrame` copies the
//! packed CameraX bytes directly into libwebrtc's native planes.

use chatter_core::calls::android_camera::{self, CameraFormat, PlatformCamera};
use jni::objects::{JByteArray, JObject, JValue};
use jni::sys::{jint, jlong};

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
                JValue::Bool(front),
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
    activity::with_jni_env(|env| {
        let act = activity::activity()?;
        let res = env.call_method(
            act,
            jni::strings::JNIString::from(method),
            jni::signature::RuntimeMethodSignature::from_str(sig)
                .map_err(|e| e.to_string())?
                .method_signature(),
            args,
        );
        if res.is_err() {
            activity::clear_exception(env);
        }
        res.map_err(|e| e.to_string()).map(|_| ())
    })
}

/// JNI callback from `MainActivity` — one tightly-packed I420 or NV12 frame.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_MainActivity_nativeOnCameraFrame(
    mut env: jni::EnvUnowned<'_>,
    _this: JObject,
    data: JByteArray,
    width: jint,
    height: jint,
    rotation: jint,
    timestamp_us: jlong,
    format: jint,
) {
    env.with_env(|env| -> jni::errors::Result<_> {
        if width <= 0 || height <= 0 { return Ok(()); }
        let frame_format = match format {
            0 => CameraFormat::I420,
            1 => CameraFormat::Nv12,
            _ => return Ok(()),
        };
        android_camera::push_frame(frame_format, width as u32, height as u32, rotation, timestamp_us,
            |y, chroma, v| {
                let required = y.len() + chroma.len() + v.as_ref().map_or(0, |plane| plane.len());
                if data.len(env).map_or(true, |len| len < required) {
                    tracing::warn!("Android camera: short frame array");
                    return false;
                }
                let mut offset = 0;
                let mut read = |plane: &mut [u8]| {
                    // Java byte[] is signed, but JNI copies its bit pattern unchanged.
                    let signed = unsafe {
                        std::slice::from_raw_parts_mut(plane.as_mut_ptr() as *mut i8, plane.len())
                    };
                    let result = data.get_region(env, offset, signed);
                    offset += plane.len() as i32;
                    result.is_ok()
                };
                if !read(y) || !read(chroma) { return false; }
                v.map_or(true, read)
            });
        Ok(())
    })
    .resolve::<jni::errors::LogErrorAndDefault>()
}
