//! Thumbnail decoder source is independent of the active playback source.
use super::activity;
use crate::platform::video_source::{ThumbnailSource, VideoSource};
use chatter_core::services::attachment::{CbcRangeDecryptor, ImageDecodeResult, ThumbnailOwner};
use jni::{
    JNIEnv,
    objects::{JByteArray, JClass, JObject, JValue},
    sys::{jint, jlong},
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicI64, Ordering},
};
use std::time::{Duration, Instant};
use zeroize::Zeroize;

struct Source {
    token: i64,
    video: ThumbnailSource,
}
static SOURCE: Mutex<Option<Source>> = Mutex::new(None);
static NEXT: AtomicI64 = AtomicI64::new(1);

pub fn init() {
    chatter_core::services::attachment::register_video_thumbnail_extractor(extract);
}
fn extract(source: Arc<CbcRangeDecryptor>) -> Result<ImageDecodeResult, String> {
    let token = NEXT.fetch_add(1, Ordering::Relaxed);
    if token <= 0 {
        return Err("video preview token exhausted".into());
    }
    {
        let mut slot = SOURCE
            .lock()
            .map_err(|_| "video preview source unavailable")?;
        if slot.is_some() {
            return Err("video preview busy".into());
        }
        *slot = Some(Source {
            token,
            video: ThumbnailSource::new_guarded(
                VideoSource::Range(source),
                Instant::now() + Duration::from_secs(5),
                owner,
            ),
        });
    }
    let result = extract_jni(token);
    if let Ok(mut slot) = SOURCE.lock() {
        if slot.as_ref().is_some_and(|s| s.token == token) {
            *slot = None;
        }
    }
    result
}
fn extract_jni(token: i64) -> Result<ImageDecodeResult, String> {
    let mut env = activity::jni_env()?;
    let result = env.with_local_frame(16, |env| -> jni::errors::Result<Vec<u8>> {
        // Native worker threads cannot FindClass app classes: use the Activity loader.
        let loader = env
            .call_method(
                activity::activity()
                    .map_err(|_| jni::errors::Error::NullPtr("activity"))?
                    .as_obj(),
                "getClassLoader",
                "()Ljava/lang/ClassLoader;",
                &[],
            )?
            .l()?;
        let name = env.new_string("com.chatter.client.VideoThumbnail")?;
        let class = env
            .call_method(
                loader,
                "loadClass",
                "(Ljava/lang/String;)Ljava/lang/Class;",
                &[JValue::Object(name.as_ref())],
            )?
            .l()?;
        let helper = env.new_object(JClass::from(class), "()V", &[])?;
        let bytes = env
            .call_method(helper, "extract", "(J)[B", &[JValue::Long(token)])?
            .l()?;
        if bytes.is_null() {
            return Err(jni::errors::Error::NullPtr("video preview unavailable"));
        }
        let bytes = JByteArray::from(bytes);
        if env.get_array_length(&bytes)? > 256 * 1024 {
            return Err(jni::errors::Error::NullPtr("thumbnail budget"));
        }
        env.convert_byte_array(bytes)
    });
    let _ = env.exception_clear();
    let bytes = result
        .map_err(|_| "video preview unsupported, malformed, busy or timed out".to_string())?;
    let (width, height) = chatter_core::media::ImageProcessor::get_dimensions_from_memory(&bytes)
        .map_err(|_| "invalid video thumbnail".to_string())?;
    Ok(ImageDecodeResult {
        image_bytes: bytes,
        width,
        height,
    })
}
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_VideoThumbnail_nativeSize(
    _env: JNIEnv,
    _this: JObject,
    token: jlong,
) -> jlong {
    SOURCE
        .lock()
        .ok()
        .and_then(|s| {
            s.as_ref()
                .filter(|s| s.token == token)
                .and_then(|s| s.video.len().map(|len| len as i64))
        })
        .unwrap_or(-1)
}
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_VideoThumbnail_nativeRead(
    mut env: JNIEnv,
    _this: JObject,
    token: jlong,
    position: jlong,
    buffer: JByteArray,
    offset: jint,
    count: jint,
) -> jint {
    if position < 0 || offset < 0 || count < 0 {
        return -1;
    }
    let Ok(capacity) = env.get_array_length(&buffer) else {
        let _ = env.exception_clear();
        return -1;
    };
    if offset > capacity || count > capacity - offset {
        return -1;
    }
    let Some(mut bytes) = SOURCE.lock().ok().and_then(|mut slot| {
        let s = slot.as_mut()?;
        if s.token != token {
            return None;
        }
        s.video.read(position, count as usize)
    }) else {
        return -1;
    };
    if bytes.is_empty() {
        return if count == 0 { 0 } else { -1 };
    }
    let signed = unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast::<i8>(), bytes.len()) };
    let result = env.set_byte_array_region(&buffer, offset, signed);
    let len = bytes.len() as jint;
    bytes.zeroize();
    if result.is_ok() {
        len
    } else {
        let _ = env.exception_clear();
        -1
    }
}
