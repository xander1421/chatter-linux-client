//! MediaProjection frames delivered by the Android platform layer.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use livekit::webrtc::native::yuv_helper;
use livekit::webrtc::video_frame::{I420Buffer, VideoFrame, VideoRotation};
use livekit::webrtc::video_source::native::NativeVideoSource;

use super::video_capture::LocalPreview;

pub trait PlatformScreen: Send + Sync {
    fn start(&self, token: u64) -> Result<(), String>;
    fn stop(&self, token: u64);
}

static PLATFORM: OnceLock<Box<dyn PlatformScreen>> = OnceLock::new();
static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);
static STATE: Mutex<Option<Capture>> = Mutex::new(None);

struct Capture {
    token: u64,
    ready: Option<tokio::sync::oneshot::Sender<Result<(u32, u32), String>>>,
    source: Option<NativeVideoSource>,
    preview: Option<LocalPreview>,
    width: u32,
    height: u32,
    last_preview: Option<Instant>,
}

pub fn register_platform_screen(platform: Box<dyn PlatformScreen>) {
    if PLATFORM.set(platform).is_err() {
        tracing::warn!("Android screen: platform already registered");
    }
}

pub fn begin(
    preview: Option<LocalPreview>,
) -> Result<
    (
        u64,
        tokio::sync::oneshot::Receiver<Result<(u32, u32), String>>,
    ),
    String,
> {
    let platform = PLATFORM
        .get()
        .ok_or("Android screen platform unavailable")?;
    let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut guard = STATE.lock().unwrap();
    if guard.is_some() {
        return Err("Screen share already starting or active".into());
    }
    *guard = Some(Capture {
        token,
        ready: Some(tx),
        source: None,
        preview,
        width: 0,
        height: 0,
        last_preview: None,
    });
    drop(guard);
    if let Err(error) = platform.start(token) {
        stopped(token);
        return Err(error);
    }
    Ok((token, rx))
}

pub fn started(token: u64, width: u32, height: u32) {
    let mut guard = STATE.lock().unwrap();
    let Some(capture) = guard.as_mut().filter(|capture| capture.token == token) else {
        return;
    };
    if width == 0
        || height == 0
        || width > 1280
        || height > 1280
        || width % 2 != 0
        || height % 2 != 0
    {
        if let Some(tx) = capture.ready.take() {
            let _ = tx.send(Err("Invalid Android screen dimensions".into()));
        }
        return;
    }
    capture.width = width;
    capture.height = height;
    if let Some(tx) = capture.ready.take() {
        let _ = tx.send(Ok((width, height)));
    }
}

pub fn install_source(token: u64, source: NativeVideoSource) -> bool {
    let mut guard = STATE.lock().unwrap();
    let Some(capture) = guard.as_mut().filter(|capture| capture.token == token) else {
        return false;
    };
    capture.source = Some(source);
    true
}

pub fn is_running() -> bool {
    STATE
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|capture| capture.source.is_some())
}

pub fn stopped(token: u64) {
    let mut guard = STATE.lock().unwrap();
    if guard.as_ref().is_some_and(|capture| capture.token == token) {
        if let Some(mut capture) = guard.take() {
            if let Some(tx) = capture.ready.take() {
                let _ = tx.send(Err("Screen share unavailable".into()));
            }
            if let Some(preview) = capture.preview.take() {
                preview.shared.remove_frame(&preview.local_id);
            }
        }
    }
}

pub fn stop() {
    let token = STATE.lock().unwrap().as_ref().map(|capture| capture.token);
    if let Some(token) = token {
        stopped(token);
        if let Some(platform) = PLATFORM.get() {
            platform.stop(token);
        }
    }
}

/// Tightly packed BGRA (libyuv ARGB memory order), copied from ImageReader RGBA.
pub fn push_frame(token: u64, bgra: &[u8], width: u32, height: u32, timestamp_us: i64) {
    let mut guard = STATE.lock().unwrap();
    let Some(capture) = guard.as_mut().filter(|capture| capture.token == token) else {
        return;
    };
    let Some(source) = capture.source.as_ref() else {
        return;
    };
    if width != capture.width
        || height != capture.height
        || bgra.len()
            != (width as usize)
                .saturating_mul(height as usize)
                .saturating_mul(4)
    {
        return;
    }
    let mut buffer = I420Buffer::new(width, height);
    let (y, u, v) = buffer.data_mut();
    yuv_helper::argb_to_i420(
        bgra,
        width * 4,
        y,
        width,
        u,
        width / 2,
        v,
        width / 2,
        width as i32,
        height as i32,
    );
    let frame = VideoFrame {
        rotation: VideoRotation::VideoRotation0,
        timestamp_us,
        frame_metadata: None,
        buffer,
    };
    source.capture_frame(&frame);
    if let Some(preview) = capture.preview.as_ref() {
        let now = Instant::now();
        if capture
            .last_preview
            .is_some_and(|last| now.duration_since(last) < Duration::from_millis(400))
        {
            return;
        }
        capture.last_preview = Some(now);
        let rgba = super::video_playback::convert_i420_into(
            None,
            &frame.buffer,
            width,
            height,
            width as usize * height as usize * 4,
        );
        preview
            .shared
            .set_frame(preview.local_id.clone(), rgba, width, height, true);
    }
}
