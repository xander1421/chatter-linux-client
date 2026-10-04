//! Global video playback state.
//!
//! Tracks video playback state for the in-app video player.
//! Supports two rendering modes:
//! - Hardware mode: Zero-copy GPU rendering (GL on Linux, D3D11 on Windows)
//! - Software mode: Frame buffer passing with RGB conversion (fallback)

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chatter_core::media::InAppVideoPlayer as VideoPlayer;

use super::video as hw_video;

/// Global video player instance (software mode).
static VIDEO_PLAYER: Mutex<Option<VideoPlayer>> = Mutex::new(None);

/// Global state tracking video playback.
static VIDEO_STATE: Mutex<VideoState> = Mutex::new(VideoState {
    current_path: None,
    filename: String::new(),
    is_playing: false,
    position: 0.0,
    duration: 0.0,
    volume: 0.0,
});

/// Shared frame buffer for cross-thread frame passing (software mode).
static FRAME_BUFFER: Mutex<Option<FrameData>> = Mutex::new(None);

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

/// Get the global video player holder.
pub fn get_video_player() -> &'static Mutex<Option<VideoPlayer>> {
    &VIDEO_PLAYER
}

/// Get the global video state.
pub fn get_video_state() -> &'static Mutex<VideoState> {
    &VIDEO_STATE
}

/// Get the frame buffer.
fn get_frame_buffer() -> &'static Mutex<Option<FrameData>> {
    &FRAME_BUFFER
}

/// Take the latest frame from the buffer (called by UI timer on main thread).
pub fn take_frame() -> Option<FrameData> {
    if let Ok(mut buffer) = get_frame_buffer().lock() {
        buffer.take()
    } else {
        None
    }
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

    // Try hardware-accelerated video first (GL on Linux, D3D11 on Windows)
    // IMPORTANT: Check is_hw_connected() on UI thread where GL context lives
    if hw_video::is_hw_available() {
        tracing::info!("HW video available, checking connection on UI thread");

        // HW video must be started from UI thread (thread_local state)
        // Check connection AND start video on UI thread
        let _ = slint::invoke_from_event_loop(move || {
            // Check connection on UI thread where GL context is initialized
            if !hw_video::is_hw_connected() {
                tracing::debug!("HW video not connected on UI thread, falling back to software");
                USING_GL_VIDEO.store(false, Ordering::SeqCst);
                start_video_software(data, filename, on_ready);
                return;
            }

            match hw_video::start_hw_video(data.clone()) {
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

                    let duration_str = chatter_core::media::format_duration(duration);
                    tracing::info!("HW video started: {}s ({})", duration, duration_str);
                    on_ready(Ok((duration, duration_str)));
                }
                None => {
                    tracing::warn!("HW video start failed, falling back to software");
                    USING_GL_VIDEO.store(false, Ordering::SeqCst);

                    // Fall back to software rendering
                    start_video_software(data, filename, on_ready);
                }
            }
        });
        return;
    }

    // Software rendering path (no HW available)
    start_video_software(data, filename, on_ready);
}

/// Start video playback from an authenticated range decryptor (streaming).
///
/// The GL pipeline pulls decrypted ranges on demand, so playback starts without
/// decrypting the whole file into memory. If HW video isn't available/connected
/// (or the range backend isn't supported), falls back to a full decrypt + the
/// software renderer.
pub fn start_video_from_range_async<F>(
    dec: Arc<chatter_core::services::attachment::CbcRangeDecryptor>,
    filename: String,
    on_ready: F,
) where
    F: FnOnce(Result<(f64, String), String>) + Send + 'static,
{
    stop_video();

    if hw_video::is_hw_available() {
        tracing::info!("HW video available (range source), checking connection on UI thread");

        let _ = slint::invoke_from_event_loop(move || {
            if !hw_video::is_hw_connected() {
                tracing::debug!("HW video not connected, falling back to software (full decrypt)");
                USING_GL_VIDEO.store(false, Ordering::SeqCst);
                let data = dec.read_range(0, dec.plaintext_size());
                start_video_software(data, filename, on_ready);
                return;
            }

            match hw_video::start_hw_video_range(dec.clone()) {
                Some(duration) => {
                    USING_GL_VIDEO.store(true, Ordering::SeqCst);

                    if let Ok(mut video_state) = get_video_state().lock() {
                        video_state.current_path = None;
                        video_state.filename = filename;
                        video_state.is_playing = true;
                        video_state.duration = duration;
                        video_state.position = 0.0;
                        video_state.volume = 1.0;
                    }

                    let duration_str = chatter_core::media::format_duration(duration);
                    tracing::info!("HW video started (streaming): {}s ({})", duration, duration_str);
                    on_ready(Ok((duration, duration_str)));
                }
                None => {
                    tracing::warn!("HW range video start failed, falling back to software");
                    USING_GL_VIDEO.store(false, Ordering::SeqCst);
                    let data = dec.read_range(0, dec.plaintext_size());
                    start_video_software(data, filename, on_ready);
                }
            }
        });
        return;
    }

    // No HW: software renderer needs the full plaintext.
    let data = dec.read_range(0, dec.plaintext_size());
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

            // CRITICAL: Store player IMMEDIATELY after creation to prevent race condition.
            // If stop_video() is called while we're in the loading poll loop below,
            // it needs to find this player to stop it. Otherwise multiple pipelines
            // can run simultaneously, causing multiple PipeWire audio streams.
            let player_state = player.state().clone();
            if let Ok(mut player_guard) = get_video_player().lock() {
                *player_guard = Some(player);
            }

            // Poll for loading completion (in background thread, doesn't block UI)
            let max_wait = Duration::from_secs(15);
            let start = Instant::now();
            let poll_interval = Duration::from_millis(50);

            loop {
                let state = player_state.get_state();

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
            let state = player_state.get_state();
            let duration = state.duration;
            let duration_str = chatter_core::media::format_duration(duration);
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

            on_ready(Ok((duration, duration_str)));
        })
        .expect("Failed to spawn video loader thread");
}
/// Stop video playback.
///
/// IMPORTANT: This function handles both UI and non-UI thread calls.
/// For GL video, it stops directly if on UI thread, otherwise schedules on UI thread.
pub fn stop_video() {
    tracing::info!("stop_video called, USING_GL_VIDEO={}", USING_GL_VIDEO.load(Ordering::SeqCst));

    // Stop hardware video if active
    // CRITICAL: GL video state is thread_local - must stop on UI thread
    if USING_GL_VIDEO.load(Ordering::SeqCst) {
        tracing::info!("Stopping GL video...");

        // First mark as not using GL to prevent race conditions
        USING_GL_VIDEO.store(false, Ordering::SeqCst);

        // Check if we're already on the UI thread by trying invoke_from_event_loop
        // If we're on the UI thread, calling invoke_from_event_loop and blocking
        // would deadlock, so we call stop_hw_video directly instead.
        //
        // We detect this by checking if we can run something immediately via
        // slint's upgrade_in_event_loop pattern - but simpler is to just try
        // running directly and use a flag to track success.
        let (tx, rx) = std::sync::mpsc::channel();
        let tx_clone = tx.clone();

        // Try to schedule on event loop
        match slint::invoke_from_event_loop(move || {
            tracing::info!("GL video stop executing on UI thread (scheduled)");
            hw_video::stop_hw_video();
            let _ = tx_clone.send(());
        }) {
            Ok(()) => {
                // Successfully scheduled - wait with very short timeout
                // If we're on UI thread, invoke_from_event_loop returns Ok but
                // the closure won't run until this function returns (deadlock).
                // So use a very short timeout to detect this case.
                match rx.recv_timeout(std::time::Duration::from_millis(50)) {
                    Ok(()) => tracing::info!("GL video stop completed (async)"),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        // Likely on UI thread - call directly
                        tracing::info!("GL video stop: likely on UI thread, calling directly");
                        hw_video::stop_hw_video();
                        tracing::info!("GL video stop completed (direct)");
                    }
                    Err(e) => tracing::warn!("GL video stop error: {:?}", e),
                }
            }
            Err(_) => {
                // Event loop not running - call directly (we're likely shutting down)
                tracing::info!("GL video stop: event loop not available, calling directly");
                hw_video::stop_hw_video();
            }
        }
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
    if USING_GL_VIDEO.load(Ordering::SeqCst) {
        let is_playing = hw_video::is_hw_video_active();
        hw_video::set_hw_video_playing(!is_playing);
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
    if USING_GL_VIDEO.load(Ordering::SeqCst) {
        if let Some(duration) = hw_video::get_hw_video_duration() {
            let seek_pos = position as f64 * duration;
            hw_video::seek_hw_video(seek_pos);
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
    if USING_GL_VIDEO.load(Ordering::SeqCst) {
        hw_video::set_hw_video_volume(volume as f64);
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
    if USING_GL_VIDEO.load(Ordering::SeqCst) {
        // Try to apply pending volume if audio element is now ready
        hw_video::apply_pending_hw_volume();

        let position = hw_video::get_hw_video_position().unwrap_or(0.0);
        let duration = hw_video::get_hw_video_duration().unwrap_or(0.0);
        let is_playing = hw_video::is_hw_video_active();
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
