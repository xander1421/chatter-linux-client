//! Global video playback state.
//!
//! Tracks video playback state for the in-app video player.
//! Supports two rendering modes:
//! - GL mode: Hardware-accelerated zero-copy rendering via EGL texture sharing
//! - Software mode: Frame buffer passing with RGB conversion (fallback)

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use chatter_linux::media::InAppVideoPlayer as VideoPlayer;

#[cfg(target_os = "linux")]
use super::gl_video;

/// Global video player instance (software mode).
static VIDEO_PLAYER: OnceLock<Arc<Mutex<Option<VideoPlayer>>>> = OnceLock::new();

/// Global state tracking video playback.
static VIDEO_STATE: OnceLock<Arc<Mutex<VideoState>>> = OnceLock::new();

/// Shared frame buffer for cross-thread frame passing (software mode).
static FRAME_BUFFER: OnceLock<Arc<Mutex<Option<FrameData>>>> = OnceLock::new();

/// Whether GL video is being used for current playback.
static USING_GL_VIDEO: AtomicBool = AtomicBool::new(false);

/// Raw frame data from the video decoder.
#[derive(Clone)]
pub struct FrameData {
    pub width: u32,
    pub height: u32,
    pub rgb_data: Vec<u8>,
}

/// Video playback state.
#[derive(Clone, Debug, Default)]
pub struct VideoState {
    /// Path to the currently loaded video, if any.
    pub current_path: Option<PathBuf>,
    /// Filename for display.
    pub filename: String,
    /// Whether video is currently playing (vs paused).
    pub is_playing: bool,
    /// Current position in seconds.
    pub position: f64,
    /// Total duration in seconds.
    pub duration: f64,
    /// Current volume (0.0 - 1.0).
    pub volume: f32,
}

/// Get or initialize the global video player holder.
pub fn get_video_player() -> Arc<Mutex<Option<VideoPlayer>>> {
    VIDEO_PLAYER
        .get_or_init(|| Arc::new(Mutex::new(None)))
        .clone()
}

/// Get or initialize the global video state.
pub fn get_video_state() -> Arc<Mutex<VideoState>> {
    VIDEO_STATE
        .get_or_init(|| Arc::new(Mutex::new(VideoState::default())))
        .clone()
}

/// Get or initialize the frame buffer.
fn get_frame_buffer() -> Arc<Mutex<Option<FrameData>>> {
    FRAME_BUFFER
        .get_or_init(|| Arc::new(Mutex::new(None)))
        .clone()
}

/// Take the latest frame from the buffer (called by UI timer on main thread).
pub fn take_frame() -> Option<FrameData> {
    if let Ok(mut buffer) = get_frame_buffer().lock() {
        buffer.take()
    } else {
        None
    }
}

/// Check if GL video is currently being used for playback.
#[cfg(target_os = "linux")]
pub fn is_using_gl_video() -> bool {
    USING_GL_VIDEO.load(Ordering::SeqCst)
}

/// Start video playback from memory (non-blocking).
///
/// Tries GL video first for hardware-accelerated rendering,
/// falls back to software rendering if GL is not available.
///
/// # Arguments
/// * `data` - Video data bytes
/// * `filename` - Display filename
/// * `on_ready` - Callback invoked when video is ready (duration, duration_str) or error
pub fn start_video_from_memory_async<F>(
    data: Vec<u8>,
    filename: String,
    on_ready: F,
) where
    F: FnOnce(Result<(f64, String), String>) + Send + 'static,
{
    // Stop any existing playback
    stop_video();

    // Try GL video first (hardware-accelerated)
    #[cfg(target_os = "linux")]
    {
        // Check if GL is connected (rendering notifier has set up the context)
        if gl_video::is_gl_connected() {
            tracing::info!("Starting GL video (hardware-accelerated)");

            // GL video must be started from UI thread (thread_local state)
            // We handle everything in the UI thread closure, including fallback
            let _ = slint::invoke_from_event_loop(move || {
                match gl_video::start_gl_video(data.clone()) {
                    Some(duration) => {
                        USING_GL_VIDEO.store(true, Ordering::SeqCst);

                        // Update global state
                        if let Ok(mut video_state) = get_video_state().lock() {
                            video_state.current_path = None;
                            video_state.filename = filename;
                            video_state.is_playing = true;
                            video_state.duration = duration;
                            video_state.position = 0.0;
                            video_state.volume = 1.0;
                        }

                        let duration_str = chatter_linux::media::format_duration(duration);
                        tracing::info!("GL video started: {}s ({})", duration, duration_str);
                        on_ready(Ok((duration, duration_str)));
                    }
                    None => {
                        tracing::warn!("GL video start failed, falling back to software");
                        USING_GL_VIDEO.store(false, Ordering::SeqCst);

                        // Fall back to software rendering
                        // Note: This runs software loading on UI thread, but it spawns its own worker
                        start_video_software(data, filename, on_ready);
                    }
                }
            });
            return;
        } else if gl_video::is_gl_available() {
            tracing::debug!("GL available but not connected yet, using software rendering");
        }
    }

    // Software rendering path (fallback)
    start_video_software(data, filename, on_ready);
}

/// Start video playback using software rendering.
fn start_video_software<F>(
    data: Vec<u8>,
    filename: String,
    on_ready: F,
) where
    F: FnOnce(Result<(f64, String), String>) + Send + 'static,
{
    USING_GL_VIDEO.store(false, Ordering::SeqCst);

    let frame_buffer = get_frame_buffer();
    let filename_clone = filename.clone();

    // Spawn loading in background thread to avoid blocking
    std::thread::Builder::new()
        .name("video-loader".into())
        .spawn(move || {
            // Start the video player from memory
            let player = match VideoPlayer::start_from_memory(data, move |width, height, rgb_data| {
                if let Ok(mut buffer) = frame_buffer.lock() {
                    *buffer = Some(FrameData {
                        width,
                        height,
                        rgb_data,
                    });
                }
            }) {
                Ok(p) => p,
                Err(e) => {
                    on_ready(Err(e.to_string()));
                    return;
                }
            };

            // Poll for loading completion (in background thread, doesn't block UI)
            let max_wait = Duration::from_secs(15);
            let start = Instant::now();
            let poll_interval = Duration::from_millis(50);

            loop {
                let state = player.state().get_state();

                if !state.loading {
                    tracing::debug!("Video player finished loading: duration={}s", state.duration);
                    break;
                }

                if let Some(ref err) = state.error {
                    on_ready(Err(err.clone()));
                    return;
                }

                if start.elapsed() > max_wait {
                    tracing::warn!("Timeout waiting for video player to load");
                    break;
                }

                std::thread::sleep(poll_interval);
            }

            // Get final state
            let state = player.state().get_state();
            let duration = state.duration;
            let duration_str = chatter_linux::media::format_duration(duration);
            tracing::info!("Video loaded (software): {}s ({})", duration, duration_str);

            // Update global state
            if let Ok(mut video_state) = get_video_state().lock() {
                video_state.current_path = None;
                video_state.filename = filename_clone;
                video_state.is_playing = true;
                video_state.duration = duration;
                video_state.position = 0.0;
                video_state.volume = 1.0;
            }

            // Store player
            if let Ok(mut player_guard) = get_video_player().lock() {
                *player_guard = Some(player);
            }

            on_ready(Ok((duration, duration_str)));
        })
        .expect("Failed to spawn video loader thread");
}

/// Start video playback from memory (blocking - prefer async version).
///
/// Used for playing decrypted video attachments directly from memory.
/// NOTE: This blocks the calling thread. Use `start_video_from_memory_async` instead.
pub fn start_video_from_memory(data: Vec<u8>, filename: &str) -> Result<(f64, String), String> {
    use std::sync::mpsc;

    let (tx, rx) = mpsc::channel();

    start_video_from_memory_async(data, filename.to_string(), move |result| {
        let _ = tx.send(result);
    });

    // Block waiting for result (still blocking, but at least isolated)
    rx.recv().map_err(|e| format!("Channel error: {}", e))?
}

/// Stop video playback.
pub fn stop_video() {
    // Stop GL video if active
    #[cfg(target_os = "linux")]
    if USING_GL_VIDEO.load(Ordering::SeqCst) {
        gl_video::stop_gl_video();
        USING_GL_VIDEO.store(false, Ordering::SeqCst);
    }

    // Stop the software player
    if let Ok(mut player_guard) = get_video_player().lock() {
        if let Some(ref mut player) = *player_guard {
            player.stop();
        }
        *player_guard = None;
    }

    // Clear frame buffer
    if let Ok(mut buffer) = get_frame_buffer().lock() {
        *buffer = None;
    }

    // Clear state
    if let Ok(mut state) = get_video_state().lock() {
        state.current_path = None;
        state.filename = String::new();
        state.is_playing = false;
        state.position = 0.0;
        state.duration = 0.0;
    }
}

/// Toggle play/pause.
pub fn toggle_play_pause() -> bool {
    #[cfg(target_os = "linux")]
    if USING_GL_VIDEO.load(Ordering::SeqCst) {
        let is_playing = gl_video::is_gl_video_active();
        gl_video::set_gl_video_playing(!is_playing);
        let new_playing = !is_playing;
        if let Ok(mut state) = get_video_state().lock() {
            state.is_playing = new_playing;
        }
        return new_playing;
    }

    if let Ok(player_guard) = get_video_player().lock() {
        if let Some(ref player) = *player_guard {
            player.toggle_play_pause();
            let is_playing = player.state().is_playing();

            if let Ok(mut state) = get_video_state().lock() {
                state.is_playing = is_playing;
            }

            return is_playing;
        }
    }
    false
}

/// Seek to position (0.0 - 1.0).
pub fn seek(position: f32) {
    #[cfg(target_os = "linux")]
    if USING_GL_VIDEO.load(Ordering::SeqCst) {
        if let Some(duration) = gl_video::get_gl_video_duration() {
            let seek_pos = position as f64 * duration;
            gl_video::seek_gl_video(seek_pos);
        }
        return;
    }

    if let Ok(player_guard) = get_video_player().lock() {
        if let Some(ref player) = *player_guard {
            let state = player.state().get_state();
            let seek_pos = position as f64 * state.duration;
            player.seek(seek_pos);
        }
    }
}

/// Set volume (0.0 - 1.0).
pub fn set_volume(volume: f32) {
    #[cfg(target_os = "linux")]
    if USING_GL_VIDEO.load(Ordering::SeqCst) {
        gl_video::set_gl_video_volume(volume as f64);
    }

    if let Ok(player_guard) = get_video_player().lock() {
        if let Some(ref player) = *player_guard {
            player.set_volume(volume);
        }
    }

    if let Ok(mut state) = get_video_state().lock() {
        state.volume = volume;
    }
}

/// Get current playback state for UI updates.
pub fn get_playback_state() -> Option<(f64, f64, bool)> {
    #[cfg(target_os = "linux")]
    if USING_GL_VIDEO.load(Ordering::SeqCst) {
        let position = gl_video::get_gl_video_position().unwrap_or(0.0);
        let duration = gl_video::get_gl_video_duration().unwrap_or(0.0);
        let is_playing = gl_video::is_gl_video_active();
        return Some((position, duration, is_playing));
    }

    if let Ok(player_guard) = get_video_player().lock() {
        if let Some(ref player) = *player_guard {
            let state = player.state().get_state();
            return Some((state.position, state.duration, state.playing));
        }
    }
    None
}
