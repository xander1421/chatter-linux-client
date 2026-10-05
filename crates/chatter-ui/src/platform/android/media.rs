//! In-process native browser/video bridges. Video sources never cross an Intent.
use super::activity;
use crate::platform::video_source::VideoSource;
use jni::{
    JNIEnv,
    objects::{JByteArray, JObject, JValue},
    sys::{jint, jlong},
};
use std::sync::{
    LazyLock, Mutex,
    atomic::{AtomicI64, Ordering},
};
use zeroize::Zeroize;

// Exactly one source: replacing it revokes stale decoder handles immediately.
static SOURCE: LazyLock<Mutex<Option<(i64, VideoSource)>>> = LazyLock::new(|| Mutex::new(None));
static NEXT: AtomicI64 = AtomicI64::new(1);

pub fn open_web_link(url: &str) -> Result<(), ()> {
    let mut env = activity::jni_env().map_err(|_| ())?;
    let activity = activity::activity().map_err(|_| ())?;
    let text = match env.new_string(url) {
        Ok(text) => env.auto_local(text),
        Err(_) => {
            let _ = env.exception_clear();
            return Err(());
        }
    };
    let result = env.call_method(
        activity.as_obj(),
        "openWebLink",
        "(Ljava/lang/String;)V",
        &[JValue::Object(text.as_ref())],
    );
    let failed = result.is_err() || env.exception_check().unwrap_or(true);
    let _ = env.exception_clear();
    if failed { Err(()) } else { Ok(()) }
}

pub fn play_video(source: VideoSource, title: &str) -> Result<(), ()> {
    let mut env = activity::jni_env().map_err(|_| ())?;
    let activity = activity::activity().map_err(|_| ())?;
    let title = match env.new_string(title) {
        Ok(title) => env.auto_local(title),
        Err(_) => {
            let _ = env.exception_clear();
            return Err(());
        }
    };
    let token = NEXT.fetch_add(1, Ordering::Relaxed);
    if token <= 0 {
        return Err(());
    }
    *SOURCE.lock().map_err(|_| ())? = Some((token, source));
    let result = env.call_method(
        activity.as_obj(),
        "playVideo",
        "(JLjava/lang/String;)V",
        &[JValue::Long(token), JValue::Object(title.as_ref())],
    );
    let failed = result.is_err() || env.exception_check().unwrap_or(true);
    let _ = env.exception_clear();
    if failed {
        release(token);
        Err(())
    } else {
        Ok(())
    }
}
fn release(token: i64) {
    if let Ok(mut source) = SOURCE.lock() {
        if source.as_ref().is_some_and(|(id, _)| *id == token) {
            *source = None;
        }
    }
}
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_VideoPlaybackActivity_nativeVideoSize(
    _env: JNIEnv,
    _this: JObject,
    token: jlong,
) -> jlong {
    SOURCE
        .lock()
        .ok()
        .and_then(|s| {
            s.as_ref()
                .filter(|(id, _)| *id == token)
                .map(|(_, s)| s.len() as i64)
        })
        .unwrap_or(-1)
}
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_VideoPlaybackActivity_nativeReleaseVideo(
    _env: JNIEnv,
    _this: JObject,
    token: jlong,
) {
    release(token);
}
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_VideoPlaybackActivity_nativeReadVideo(
    mut env: JNIEnv,
    _this: JObject,
    token: jlong,
    position: jlong,
    buffer: JByteArray,
    offset: jint,
    count: jint,
) -> jint {
    if offset < 0 || count < 0 || position < 0 {
        return -1;
    }
    let Ok(capacity) = env.get_array_length(&buffer) else {
        let _ = env.exception_clear();
        return -1;
    };
    if offset > capacity || count > capacity - offset {
        return -1;
    }
    if count == 0 {
        return 0;
    }
    let Some(mut bytes) = SOURCE.lock().ok().and_then(|s| {
        s.as_ref()
            .filter(|(id, _)| *id == token)
            .and_then(|(_, s)| s.read(position, count as usize))
    }) else {
        return -1;
    };
    if bytes.is_empty() {
        return -1;
    }
    // JNI takes signed bytes; preserve the exact bit pattern without another copy.
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

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_MainActivity_nativeReleasePendingVideo(
    _env: JNIEnv,
    _this: JObject,
    token: jlong,
) {
    release(token);
}

#[cfg(test)]
mod tests {
    use super::*;
    use chatter_core::prelude::SecureBuffer;
    #[test]
    fn closing_replaced_native_player_cannot_release_new_source() {
        *SOURCE.lock().unwrap() = Some((1, VideoSource::Memory(SecureBuffer::new(vec![1]))));
        *SOURCE.lock().unwrap() = Some((2, VideoSource::Memory(SecureBuffer::new(vec![2]))));
        release(1);
        assert_eq!(SOURCE.lock().unwrap().as_ref().unwrap().0, 2);
        release(2);
        assert!(SOURCE.lock().unwrap().is_none());
        release(2); // MediaDataSource.close + Activity.onDestroy are idempotent.
    }
}

pub fn show_video_error() {
    if let (Ok(mut env), Ok(activity)) = (activity::jni_env(), activity::activity()) {
        let _ = env.call_method(activity.as_obj(), "showVideoError", "()V", &[]);
        let _ = env.exception_clear();
    }
}
