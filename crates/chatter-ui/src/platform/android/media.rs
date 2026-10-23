//! In-process native browser/video bridges. Video sources never cross an Intent.
use super::activity;
use crate::platform::video_source::VideoSource;
use jni::{
    objects::{JByteArray, JObject, JValue},
    sys::{jint, jlong},
};
use std::sync::{
    LazyLock, Mutex,
    atomic::{AtomicI64, AtomicU64, Ordering},
};
use zeroize::Zeroize;

// Exactly one source: replacing it revokes stale decoder handles immediately.
static SOURCE: LazyLock<Mutex<Option<(i64, VideoSource)>>> = LazyLock::new(|| Mutex::new(None));
static NEXT: AtomicI64 = AtomicI64::new(1);
static SOURCE_REQUEST: AtomicU64 = AtomicU64::new(0);

pub fn open_web_link(url: &str) -> Result<(), ()> {
    activity::with_jni_env(|env| {
        Ok((|| -> Result<(), ()> {
            let activity = activity::activity().map_err(|_| ())?;
            let text = match env.new_string(url) {
                Ok(text) => text,
                Err(_) => {
                    let _ = env.exception_clear();
                    return Err(());
                }
            };
            let result = env.call_method(
                activity.as_obj(),
                jni::jni_str!("openWebLink"),
                jni::jni_sig!("(Ljava/lang/String;)V"),
                &[JValue::Object(text.as_ref())],
            );
            let failed = result.is_err() || env.exception_check();
            let _ = env.exception_clear();
            if failed { Err(()) } else { Ok(()) }
        })())
    })
    .map_err(|_| ())?
}

pub fn play_video(source: VideoSource, title: &str) -> Result<(), ()> {
    play_video_guarded(
        source,
        title,
        crate::bin_ui::features::chat_actions::media_viewer::current_request(),
    )
}

pub fn invalidate_pending_video() {
    if let Ok(mut source) = SOURCE.lock() {
        *source = None;
    }
}

fn source_request(token: i64) -> Option<u64> {
    let source = SOURCE.lock().ok()?;
    if !source.as_ref().is_some_and(|(id, _)| *id == token) {
        return None;
    }
    let request = SOURCE_REQUEST.load(Ordering::SeqCst);
    crate::bin_ui::features::chat_actions::media_viewer::is_current(request).then_some(request)
}

pub fn play_video_guarded(source: VideoSource, title: &str, request: u64) -> Result<(), ()> {
    activity::with_jni_env(|env| {
        Ok((|| -> Result<(), ()> {
            let activity = activity::activity().map_err(|_| ())?;
            let title = match env.new_string(title) {
                Ok(title) => title,
                Err(_) => {
                    let _ = env.exception_clear();
                    return Err(());
                }
            };
            let token = NEXT.fetch_add(1, Ordering::Relaxed);
            if token <= 0 {
                return Err(());
            }
            let mut slot = SOURCE.lock().map_err(|_| ())?;
            if !crate::bin_ui::features::chat_actions::media_viewer::is_current(request) {
                return Ok(());
            }
            SOURCE_REQUEST.store(request, Ordering::SeqCst);
            *slot = Some((token, source));
            drop(slot);
            let result = env.call_method(
                activity.as_obj(),
                jni::jni_str!("playVideo"),
                jni::jni_sig!("(JLjava/lang/String;Z)V"),
                &[
                    JValue::Long(token),
                    JValue::Object(title.as_ref()),
                    JValue::Bool((!activity::qa_allows_capture()).into()),
                ],
            );
            let failed = result.is_err() || env.exception_check();
            let _ = env.exception_clear();
            if failed {
                release(token);
                Err(())
            } else {
                Ok(())
            }
        })())
    })
    .map_err(|_| ())?
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
                        .filter(|(id, _)| *id == token)
                        .map(|(_, s)| s.len() as i64)
                })
                .unwrap_or(-1)
        })()))
    })
    .resolve::<jni::errors::LogErrorAndDefault>()
    .unwrap_or(-1)
}
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_VideoPlaybackActivity_nativeReleaseVideo(
    mut _env: jni::EnvUnowned<'_>,
    _this: JObject,
    token: jlong,
) {
    _env.with_env(|_env| -> jni::errors::Result<_> {
        Ok((|| {
            release(token);
        })())
    })
    .resolve::<jni::errors::LogErrorAndDefault>()
}
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_VideoPlaybackActivity_nativeReadVideo(
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
            if offset < 0 || count < 0 || position < 0 {
                return -1;
            }
            let Ok(capacity) = buffer.len(env) else {
                let _ = env.exception_clear();
                return -1;
            };
            if offset as usize > capacity || count as usize > capacity - offset as usize {
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
            let signed =
                unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast::<i8>(), bytes.len()) };
            let result = buffer.set_region(env, offset, signed);
            let len = bytes.len() as jint;
            bytes.zeroize();
            if result.is_ok() {
                len
            } else {
                let _ = env.exception_clear();
                -1
            }
        })()))
    })
    .resolve::<jni::errors::LogErrorAndDefault>()
    .unwrap_or(-1)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_MainActivity_nativeReleasePendingVideo(
    mut _env: jni::EnvUnowned<'_>,
    _this: JObject,
    token: jlong,
) {
    _env.with_env(|_env| -> jni::errors::Result<_> {
        Ok((|| {
            release(token);
        })())
    })
    .resolve::<jni::errors::LogErrorAndDefault>()
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
    let _ = activity::with_jni_env(|env| {
        if let Ok(activity) = activity::activity() {
            let _ = env.call_method(
                activity.as_obj(),
                jni::jni_str!("showVideoError"),
                jni::jni_sig!("()V"),
                &[],
            );
            env.exception_clear();
        }
        Ok(())
    });
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_VideoPlaybackActivity_nativeViewerAction(
    mut _env: jni::EnvUnowned<'_>,
    _this: JObject,
    token: jlong,
    action: jint,
) {
    _env.with_env(|_env| -> jni::errors::Result<_> {
        Ok((|| {
            if let Some(request) = source_request(token) {
                crate::bin_ui::features::chat_actions::media_viewer::native_action(action, request);
            }
        })())
    })
    .resolve::<jni::errors::LogErrorAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_VideoPlaybackActivity_nativeViewerCanNavigate(
    mut _env: jni::EnvUnowned<'_>,
    _this: JObject,
    token: jlong,
    delta: jint,
) -> jni::sys::jboolean {
    _env.with_env(|_env| -> jni::errors::Result<_> {
        Ok((|| {
            source_request(token).is_some()
                && crate::bin_ui::features::chat_actions::media_viewer::native_can_navigate(delta)
        })())
    })
    .resolve::<jni::errors::LogErrorAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_VideoPlaybackActivity_nativeViewerPosition(
    mut env: jni::EnvUnowned<'_>,
    _this: JObject,
    token: jlong,
) -> jlong {
    env.with_env(|_| -> jni::errors::Result<_> {
        Ok(source_request(token)
            .map(|_| crate::bin_ui::features::chat_actions::media_viewer::native_position() as jlong)
            .unwrap_or(0))
    })
    .resolve::<jni::errors::LogErrorAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_MainActivity_nativeVideoTokenIsCurrent(
    mut _env: jni::EnvUnowned<'_>,
    _this: JObject,
    token: jlong,
) -> jni::sys::jboolean {
    _env.with_env(|_env| -> jni::errors::Result<_> { Ok((|| source_request(token).is_some())()) })
        .resolve::<jni::errors::LogErrorAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_VideoPlaybackActivity_nativeVideoTokenIsCurrent(
    mut _env: jni::EnvUnowned<'_>,
    _this: JObject,
    token: jlong,
) -> jni::sys::jboolean {
    _env.with_env(|_env| -> jni::errors::Result<_> { Ok((|| source_request(token).is_some())()) })
        .resolve::<jni::errors::LogErrorAndDefault>()
}
