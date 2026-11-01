//! MediaProjection frames delivered by the Android platform layer.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
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
static METERED: AtomicBool = AtomicBool::new(true);
static TARGET_FPS: AtomicU32 = AtomicU32::new(60);

pub fn is_metered() -> bool {
    METERED.load(Ordering::Relaxed)
}
pub fn set_target_fps(fps: u32) {
    TARGET_FPS.store(fps.clamp(1, 60), Ordering::Relaxed);
}

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
    probe: Option<(I420Buffer, i64)>,
    next_frame_us: Option<i64>,
    rgba_scratch: Vec<u8>,
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
        probe: None,
        next_frame_us: None,
        rgba_scratch: Vec::new(),
    });
    drop(guard);
    if let Err(error) = platform.start(token) {
        stopped(token);
        return Err(error);
    }
    Ok((token, rx))
}

pub fn started(token: u64, width: u32, height: u32, metered: bool) {
    let mut guard = STATE.lock().unwrap();
    let Some(capture) = guard.as_mut().filter(|capture| capture.token == token) else {
        return;
    };
    if width == 0
        || height == 0
        || width > 1920
        || height > 1920
        || width % 2 != 0
        || height % 2 != 0
    {
        if let Some(tx) = capture.ready.take() {
            let _ = tx.send(Err("Invalid Android screen dimensions".into()));
        }
        return;
    }
    METERED.store(metered, Ordering::Relaxed);
    set_target_fps(60);
    capture.width = width;
    capture.height = height;
    // Readiness requires a captured frame, not just display allocation.
}

pub fn install_source(token: u64, source: NativeVideoSource) -> bool {
    let mut guard = STATE.lock().unwrap();
    let Some(capture) = guard.as_mut().filter(|capture| capture.token == token) else {
        return false;
    };
    capture.source = Some(source.clone());
    if let Some((buffer, timestamp_us)) = capture.probe.take() {
        source.capture_frame(&VideoFrame {
            rotation: VideoRotation::VideoRotation0,
            timestamp_us,
            frame_metadata: None,
            buffer,
        });
    }
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

/// Borrow the ImageReader RGBA plane; convert with libyuv's SIMD ABGR path.
/// Scratch is needed only for unusual pixel strides or missing final-row padding.
#[allow(clippy::too_many_arguments)]
pub fn push_frame(
    token: u64,
    rgba: &[u8],
    row_stride: u32,
    pixel_stride: u32,
    width: u32,
    height: u32,
    timestamp_us: i64,
    metered: bool,
) {
    let mut guard = STATE.lock().unwrap();
    let Some(capture) = guard.as_mut().filter(|capture| capture.token == token) else {
        return;
    };
    METERED.store(metered, Ordering::Relaxed);
    if width == 0
        || height == 0
        || width != capture.width
        || height != capture.height
        || pixel_stride < 4
    {
        return;
    }
    let Some(row_bytes) = (width as usize - 1)
        .checked_mul(pixel_stride as usize)
        .and_then(|bytes| bytes.checked_add(4))
    else {
        return;
    };
    let Some(required) = (height as usize - 1)
        .checked_mul(row_stride as usize)
        .and_then(|bytes| bytes.checked_add(row_bytes))
    else {
        return;
    };
    if (row_stride as usize) < row_bytes || rgba.len() < required {
        return;
    }
    let period_us = 1_000_000 / TARGET_FPS.load(Ordering::Relaxed) as i64;
    if capture
        .next_frame_us
        .is_some_and(|next| timestamp_us < next)
    {
        return;
    }
    // Preserve phase with half-period credit so minor callback jitter does not
    // turn a nominal 60Hz display into a 30fps feed. Never accumulate a backlog.
    capture.next_frame_us = Some(
        capture
            .next_frame_us
            .map_or(timestamp_us + period_us / 2, |next| {
                (next + period_us).max(timestamp_us + period_us / 2)
            }),
    );
    let (rgba, stride) = if pixel_stride == 4
        && (row_stride as usize)
            .checked_mul(height as usize)
            .is_some_and(|padded_len| rgba.len() >= padded_len)
    {
        (rgba, row_stride)
    } else {
        capture
            .rgba_scratch
            .resize(width as usize * height as usize * 4, 0);
        for row in 0..height as usize {
            let src = &rgba[row * row_stride as usize..][..row_bytes];
            let dst = &mut capture.rgba_scratch[row * width as usize * 4..][..width as usize * 4];
            if pixel_stride == 4 {
                dst.copy_from_slice(src);
            } else {
                for (col, pixel) in dst.chunks_exact_mut(4).enumerate() {
                    pixel.copy_from_slice(&src[col * pixel_stride as usize..][..4]);
                }
            }
        }
        (capture.rgba_scratch.as_slice(), width * 4)
    };
    let mut buffer = I420Buffer::new(width, height);
    let (y, u, v) = buffer.data_mut();
    // libyuv ABGR is RGBA in little-endian memory: no channel-swap pass.
    yuv_helper::abgr_to_i420(
        rgba,
        stride,
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
    let Some(source) = capture.source.as_ref() else {
        capture.probe = Some((frame.buffer, timestamp_us));
        if let Some(tx) = capture.ready.take() {
            let _ = tx.send(Ok((width, height)));
        }
        return;
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
