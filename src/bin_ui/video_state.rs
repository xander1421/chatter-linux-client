//! Global video playback state.
//!
//! Tracks video playback state for the in-app video player.
//! Uses a shared frame buffer that the UI timer reads from.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use chatter_linux::media::InAppVideoPlayer as VideoPlayer;

/// Global video player instance.
static VIDEO_PLAYER: OnceLock<Arc<Mutex<Option<VideoPlayer>>>> = OnceLock::new();

/// Global state tracking video playback.
static VIDEO_STATE: OnceLock<Arc<Mutex<VideoState>>> = OnceLock::new();

/// Shared frame buffer for cross-thread frame passing.
static FRAME_BUFFER: OnceLock<Arc<Mutex<Option<FrameData>>>> = OnceLock::new();

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

/// Start video playback from memory (secure: no filesystem access).
///
/// Used for playing decrypted video attachments directly from memory.
pub fn start_video_from_memory(data: Vec<u8>, filename: &str) -> Result<(f64, String), String> {
    // Stop any existing playback
    stop_video();

    let frame_buffer = get_frame_buffer();

    // Start the video player from memory
    let player = VideoPlayer::start_from_memory(data, move |width, height, rgb_data| {
        if let Ok(mut buffer) = frame_buffer.lock() {
            *buffer = Some(FrameData {
                width,
                height,
                rgb_data,
            });
        }
    }).map_err(|e| e.to_string())?;

    // Wait for the player to finish loading (up to 15 seconds)
    // The background thread sets loading=false and duration after open_memory completes
    let max_wait = Duration::from_secs(15);
    let start = Instant::now();
    let poll_interval = Duration::from_millis(50);

    loop {
        let state = player.state().get_state();

        // Check if loading completed
        if !state.loading {
            tracing::debug!("Video player finished loading: duration={}s", state.duration);
            break;
        }

        // Check for error
        if let Some(ref err) = state.error {
            return Err(err.clone());
        }

        // Check timeout
        if start.elapsed() > max_wait {
            tracing::warn!("Timeout waiting for video player to load");
            break;
        }

        std::thread::sleep(poll_interval);
    }

    // Now get the state with actual duration
    let state = player.state().get_state();
    let duration = state.duration;
    let duration_str = chatter_linux::media::format_duration(duration);
    tracing::info!("Video loaded: {}s ({})", duration, duration_str);

    // Update global state
    if let Ok(mut video_state) = get_video_state().lock() {
        video_state.current_path = None; // No filesystem path for memory playback
        video_state.filename = filename.to_string();
        video_state.is_playing = true;
        video_state.duration = duration;
        video_state.position = 0.0;
        video_state.volume = 1.0;
    }

    // Store player
    if let Ok(mut player_guard) = get_video_player().lock() {
        *player_guard = Some(player);
    }

    Ok((duration, duration_str))
}

/// Stop video playback.
pub fn stop_video() {
    // Stop the player
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
    if let Ok(player_guard) = get_video_player().lock() {
        if let Some(ref player) = *player_guard {
            let state = player.state().get_state();
            return Some((state.position, state.duration, state.playing));
        }
    }
    None
}
