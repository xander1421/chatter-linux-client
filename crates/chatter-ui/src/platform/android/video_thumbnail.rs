//! Thumbnail decoder source is independent of the active playback source.
use super::activity;
use crate::platform::video_source::{ThumbnailSource, VideoSource};
use std::cell::RefCell;
use chatter_core::media::VideoMetadata;
use chatter_core::services::attachment::{CbcRangeDecryptor, ImageDecodeResult, ThumbnailOwner};
use jni::{
    objects::{Global, JByteArray, JClass, JObject, JValue},
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
static HELPER: Mutex<Option<Arc<Global<JObject<'static>>>>> = Mutex::new(None);
thread_local! { static READ_SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) }; }

fn video_helper(env: &mut jni::Env<'_>) -> jni::errors::Result<Arc<Global<JObject<'static>>>> {
    let mut slot = HELPER.lock().map_err(|_| jni::errors::Error::NullPtr("video helper lock"))?;
    if slot.is_none() {
        let loader = env
            .call_method(
                activity::activity()
                    .map_err(|_| jni::errors::Error::NullPtr("activity"))?
                    .as_obj(),
                jni::jni_str!("getClassLoader"),
                jni::jni_sig!("()Ljava/lang/ClassLoader;"),
                &[],
            )?
            .l()?;
        let name = env.new_string("com.chatter.client.VideoThumbnail")?;
        let class = env
            .call_method(
                loader,
                jni::jni_str!("loadClass"),
                jni::jni_sig!("(Ljava/lang/String;)Ljava/lang/Class;"),
                &[JValue::Object(name.as_ref())],
            )?
            .l()?;
        let helper = env.new_object(env.cast_local::<JClass>(class)?, jni::jni_sig!("()V"), &[])?;
        *slot = Some(Arc::new(env.new_global_ref(helper)?));
    }
    Ok(Arc::clone(slot.as_ref().unwrap()))
}

pub fn init() {
    chatter_core::services::attachment::register_video_thumbnail_extractor(extract);
    chatter_core::media::register_video_metadata_extractor(extract_file);
}

fn extract_file(path: &std::path::Path) -> Result<VideoMetadata, String> {
    let path = path.to_str().ok_or("video path is not UTF-8")?;
    let packed = activity::with_jni_env(|env| {
        let result = env.with_local_frame(16, |env| -> jni::errors::Result<Vec<u8>> {
            let jpath = env.new_string(path)?;
            let helper = video_helper(env)?;
            let bytes = env
                .call_method(
                    helper.as_obj(),
                    jni::jni_str!("extractFile"),
                    jni::jni_sig!("(Ljava/lang/String;)[B"),
                    &[JValue::Object(jpath.as_ref())],
                )?
                .l()?;
            if bytes.is_null() {
                return Err(jni::errors::Error::NullPtr("video metadata unavailable"));
            }
            let bytes = env.cast_local::<JByteArray>(bytes)?;
            if bytes.len(env)? > 256 * 1024 + 24 {
                return Err(jni::errors::Error::NullPtr("video metadata budget"));
            }
            env.convert_byte_array(bytes)
        });
        let _ = env.exception_clear();
        result.map_err(|e| e.to_string())
    })?;
    if packed.len() < 24 {
        return Err("short video metadata".into());
    }
    let word = |offset: usize| u32::from_be_bytes(packed[offset..offset + 4].try_into().unwrap());
    let duration_ms = u64::from_be_bytes(packed[16..24].try_into().unwrap());
    let thumbnail = packed[24..].to_vec();
    if !thumbnail.is_empty() {
        let dimensions =
            chatter_core::media::ImageProcessor::get_dimensions_from_memory(&thumbnail)
                .map_err(|e| e.to_string())?;
        if dimensions != (word(0), word(4)) {
            return Err("video thumbnail dimensions mismatch".into());
        }
    }
    Ok(VideoMetadata {
        thumbnail,
        thumbnail_width: word(0),
        thumbnail_height: word(4),
        width: word(8),
        height: word(12),
        duration_ms,
    })
}
fn extract(
    source: Arc<CbcRangeDecryptor>,
    owner: ThumbnailOwner,
) -> Result<ImageDecodeResult, String> {
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
    activity::with_jni_env(|env| {
        let result = env.with_local_frame(16, |env| -> jni::errors::Result<Vec<u8>> {
            let helper = video_helper(env)?;
            let bytes = env
                .call_method(
                    helper.as_obj(),
                    jni::jni_str!("extract"),
                    jni::jni_sig!("(J)[B"),
                    &[JValue::Long(token)],
                )?
                .l()?;
            if bytes.is_null() {
                return Err(jni::errors::Error::NullPtr("video preview unavailable"));
            }
            let bytes = env.cast_local::<JByteArray>(bytes)?;
            if bytes.len(env)? > 256 * 1024 {
                return Err(jni::errors::Error::NullPtr("thumbnail budget"));
            }
            env.convert_byte_array(bytes)
        });
        let _ = env.exception_clear();
        let bytes = result
            .map_err(|_| "video preview unsupported, malformed, busy or timed out".to_string())?;
        let (width, height) =
            chatter_core::media::ImageProcessor::get_dimensions_from_memory(&bytes)
                .map_err(|_| "invalid video thumbnail".to_string())?;
        Ok(ImageDecodeResult {
            image_bytes: bytes,
            width,
            height,
        })
    })
}
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_VideoThumbnail_nativeSize(
    mut _env: jni::EnvUnowned<'_>,
    _this: JObject,
    token: jlong,
) -> jlong {
    _env.with_env(|_env| -> jni::errors::Result<_> {
        Ok(Some((|| {
            SOURCE
                .lock()
                .ok()
                .and_then(|s| {
                    s.as_ref()
                        .filter(|s| s.token == token)
                        .and_then(|s| s.video.len().map(|len| len as i64))
                })
                .unwrap_or(-1)
        })()))
    })
    .resolve::<jni::errors::LogErrorAndDefault>()
    .unwrap_or(-1)
}
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_VideoThumbnail_nativeRead(
    mut env: jni::EnvUnowned<'_>,
    _this: JObject,
    token: jlong,
    position: jlong,
    buffer: JByteArray,
    offset: jint,
    count: jint,
) -> jint {
    env.with_env(|env| -> jni::errors::Result<_> {
        Ok(Some((|| {
            if position < 0 || offset < 0 || count < 0 {
                return -1;
            }
            let Ok(capacity) = buffer.len(env) else {
                let _ = env.exception_clear();
                return -1;
            };
            if offset as usize > capacity || count as usize > capacity - offset as usize {
                return -1;
            }
            READ_SCRATCH.with(|scratch| {
                let mut bytes = scratch.borrow_mut();
                bytes.resize((count as usize).min(1024 * 1024), 0);
                let Some(len) = SOURCE.lock().ok().and_then(|mut slot| {
                    let s = slot.as_mut()?;
                    if s.token != token { return None; }
                    s.video.read_into(position, &mut bytes)
                }) else {
                    return -1;
                };
                if len == 0 {
                    return if count == 0 { 0 } else { -1 };
                }
                let signed = unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast::<i8>(), len) };
                let result = buffer.set_region(env, offset, signed);
                bytes[..len].zeroize();
                if result.is_ok() { len as jint } else {
                    let _ = env.exception_clear();
                    -1
                }
            })
        })()))
    })
    .resolve::<jni::errors::LogErrorAndDefault>()
    .unwrap_or(-1)
}
