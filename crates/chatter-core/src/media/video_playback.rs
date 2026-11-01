//! In-app video player using platform-native decoders.
//!
//! Provides video playback with audio sync for viewing video attachments
//! within the application. Renders frames to Slint Image widget.
//!
//! Uses GStreamer on desktop; Android playback uses the UI's platform player.
//! Audio is handled by GStreamer's autoaudiosink (outputs to PulseAudio/PipeWire).

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::error::MediaError;
use crate::media::decoder::{self, VideoDecoder, VideoInfo};

/// Video player state for UI binding.
#[derive(Clone, Debug)]
pub struct PlayerState {
    /// Whether video is currently playing.
    pub playing: bool,
    /// Current playback position in seconds.
    pub position: f64,
    /// Total duration in seconds.
    pub duration: f64,
    /// Video dimensions (width, height).
    pub dimensions: (u32, u32),
    /// Current volume (0.0 - 1.0).
    pub volume: f32,
    /// Whether player is loading.
    pub loading: bool,
    /// Error message if any.
    pub error: Option<String>,
}

impl Default for PlayerState {
    fn default() -> Self {
        Self {
            playing: false,
            position: 0.0,
            duration: 0.0,
            dimensions: (0, 0),
            volume: 1.0,
            loading: false,
            error: None,
        }
    }
}

/// Shared player state for cross-thread communication.
#[derive(Clone)]
pub struct SharedPlayerState {
    inner: Arc<Mutex<PlayerStateInner>>,
}

struct PlayerStateInner {
    playing: bool,
    position: f64,
    duration: f64,
    dimensions: (u32, u32),
    volume: f32,
    loading: bool,
    error: Option<String>,
    should_stop: bool,
    seek_requested: Option<f64>,
}

impl SharedPlayerState {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(PlayerStateInner {
                playing: false,
                position: 0.0,
                duration: 0.0,
                dimensions: (0, 0),
                volume: 1.0,
                loading: true,
                error: None,
                should_stop: false,
                seek_requested: None,
            })),
        }
    }

    pub fn get_state(&self) -> PlayerState {
        let inner = self.inner.lock().unwrap();
        PlayerState {
            playing: inner.playing,
            position: inner.position,
            duration: inner.duration,
            dimensions: inner.dimensions,
            volume: inner.volume,
            loading: inner.loading,
            error: inner.error.clone(),
        }
    }

    pub fn set_playing(&self, playing: bool) {
        self.inner.lock().unwrap().playing = playing;
    }

    pub fn is_playing(&self) -> bool {
        self.inner.lock().unwrap().playing
    }

    pub fn set_position(&self, position: f64) {
        self.inner.lock().unwrap().position = position;
    }

    pub fn set_duration(&self, duration: f64) {
        self.inner.lock().unwrap().duration = duration;
    }

    pub fn set_dimensions(&self, width: u32, height: u32) {
        self.inner.lock().unwrap().dimensions = (width, height);
    }

    pub fn set_volume(&self, volume: f32) {
        self.inner.lock().unwrap().volume = volume.clamp(0.0, 1.0);
    }

    pub fn get_volume(&self) -> f32 {
        self.inner.lock().unwrap().volume
    }

    pub fn set_loading(&self, loading: bool) {
        self.inner.lock().unwrap().loading = loading;
    }

    pub fn set_error(&self, error: Option<String>) {
        self.inner.lock().unwrap().error = error;
    }

    pub fn should_stop(&self) -> bool {
        self.inner.lock().unwrap().should_stop
    }

    pub fn request_stop(&self) {
        self.inner.lock().unwrap().should_stop = true;
    }

    pub fn request_seek(&self, position: f64) {
        self.inner.lock().unwrap().seek_requested = Some(position);
    }

    pub fn take_seek_request(&self) -> Option<f64> {
        self.inner.lock().unwrap().seek_requested.take()
    }

    pub fn has_pending_seek(&self) -> bool {
        self.inner.lock().unwrap().seek_requested.is_some()
    }
}

impl Default for SharedPlayerState {
    fn default() -> Self {
        Self::new()
    }
}

/// Video player that renders frames to a callback.
pub struct VideoPlayer {
    state: SharedPlayerState,
    playback_thread: Option<std::thread::JoinHandle<()>>,
}

impl VideoPlayer {
    /// Start playing a video file.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the video file
    /// * `frame_callback` - Called with each decoded frame as RGB pixel buffer
    ///
    /// The frame callback receives (width, height, rgb_data) where rgb_data is
    /// packed RGB24 bytes that can be converted to a Slint Image.
    pub fn start<F>(path: &Path, frame_callback: F) -> Result<Self, MediaError>
    where
        F: FnMut(u32, u32, Vec<u8>) + Send + 'static,
    {
        let path = path.to_path_buf();
        let state = SharedPlayerState::new();
        let state_clone = state.clone();

        let playback_thread = std::thread::Builder::new()
            .name("video-playback".into())
            .spawn(move || {
                if let Err(e) = run_playback(&path, state_clone.clone(), frame_callback) {
                    tracing::error!("Video playback error: {}", e);
                    state_clone.set_error(Some(e.to_string()));
                }
                state_clone.set_playing(false);
                state_clone.set_loading(false);
            })
            .map_err(|e| {
                MediaError::PlaybackError(format!("Failed to start playback thread: {}", e))
            })?;

        Ok(Self {
            state,
            playback_thread: Some(playback_thread),
        })
    }

    /// Start playing a video from memory buffer.
    ///
    /// # Arguments
    ///
    /// * `data` - Video data bytes (MP4, WebM, etc.)
    /// * `frame_callback` - Called with each decoded frame as RGB pixel buffer
    pub fn start_from_memory<F>(data: Vec<u8>, frame_callback: F) -> Result<Self, MediaError>
    where
        F: FnMut(u32, u32, Vec<u8>) + Send + 'static,
    {
        Self::start_from_source(
            decoder::VideoSource::Memory(std::sync::Arc::new(data)),
            frame_callback,
        )
    }

    /// Stream an authenticated attachment through the software fallback.
    pub fn start_from_range<F>(
        range: std::sync::Arc<crate::services::attachment::CbcRangeDecryptor>,
        frame_callback: F,
    ) -> Result<Self, MediaError>
    where
        F: FnMut(u32, u32, Vec<u8>) + Send + 'static,
    {
        Self::start_from_source(decoder::VideoSource::Range(range), frame_callback)
    }

    fn start_from_source<F>(
        data: decoder::VideoSource,
        frame_callback: F,
    ) -> Result<Self, MediaError>
    where
        F: FnMut(u32, u32, Vec<u8>) + Send + 'static,
    {
        if data.len() == 0 {
            return Err(MediaError::InvalidFormat("empty video data".to_string()));
        }

        let state = SharedPlayerState::new();
        let state_clone = state.clone();

        let playback_thread = std::thread::Builder::new()
            .name("video-playback".into())
            .spawn(move || {
                if let Err(e) = run_playback_from_memory(data, state_clone.clone(), frame_callback)
                {
                    tracing::error!("Video playback error: {}", e);
                    state_clone.set_error(Some(e.to_string()));
                }
                state_clone.set_playing(false);
                state_clone.set_loading(false);
            })
            .map_err(|e| {
                MediaError::PlaybackError(format!("Failed to start playback thread: {}", e))
            })?;

        Ok(Self {
            state,
            playback_thread: Some(playback_thread),
        })
    }

    /// Get the current player state.
    pub fn state(&self) -> &SharedPlayerState {
        &self.state
    }

    /// Toggle play/pause.
    pub fn toggle_play_pause(&self) {
        let playing = self.state.is_playing();
        self.state.set_playing(!playing);
    }

    /// Play the video.
    pub fn play(&self) {
        self.state.set_playing(true);
    }

    /// Pause the video.
    pub fn pause(&self) {
        self.state.set_playing(false);
    }

    /// Stop playback and release resources.
    pub fn stop(&mut self) {
        self.state.request_stop();
        self.state.set_playing(false);
        if let Some(thread) = self.playback_thread.take() {
            let _ = thread.join();
        }
    }

    /// Seek to a position in seconds.
    pub fn seek(&self, position: f64) {
        self.state.request_seek(position);
    }

    /// Set volume (0.0 - 1.0).
    pub fn set_volume(&self, volume: f32) {
        self.state.set_volume(volume);
    }
}

impl Drop for VideoPlayer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Run the video playback loop.
fn run_playback<F>(
    path: &Path,
    state: SharedPlayerState,
    frame_callback: F,
) -> Result<(), MediaError>
where
    F: FnMut(u32, u32, Vec<u8>) + Send + 'static,
{
    let mut dec = decoder::create_decoder()?;
    let info = dec.open(path)?;

    run_playback_impl(&mut *dec, info, state, frame_callback)
}

/// Run the video playback loop from memory.
fn run_playback_from_memory<F>(
    data: decoder::VideoSource,
    state: SharedPlayerState,
    frame_callback: F,
) -> Result<(), MediaError>
where
    F: FnMut(u32, u32, Vec<u8>) + Send + 'static,
{
    let mut dec = decoder::create_decoder()?;
    let info = match data {
        decoder::VideoSource::Memory(data) => dec
            .open_memory(std::sync::Arc::try_unwrap(data).unwrap_or_else(|data| (*data).clone()))?,
        decoder::VideoSource::Range(range) => dec.open_range(range)?,
    };

    run_playback_impl(&mut *dec, info, state, frame_callback)
}

/// Internal playback implementation.
fn run_playback_impl<F>(
    dec: &mut dyn VideoDecoder,
    info: VideoInfo,
    state: SharedPlayerState,
    mut frame_callback: F,
) -> Result<(), MediaError>
where
    F: FnMut(u32, u32, Vec<u8>) + Send + 'static,
{
    let width = info.width;
    let height = info.height;
    let duration_secs = info.duration_ms as f64 / 1000.0;
    let fps = info.fps;

    tracing::debug!(
        "Video: {}x{}, {:.2} fps, duration {:.2}s, has_audio: {}",
        width,
        height,
        fps,
        duration_secs,
        info.has_audio
    );

    state.set_duration(duration_secs);
    state.set_dimensions(width, height);

    // Audio is handled by GStreamer's autoaudiosink - outputs directly to system audio
    // (PulseAudio/PipeWire). No manual audio handling needed.

    // Set initial volume
    let initial_volume = state.get_volume();
    if let Err(e) = dec.set_volume(initial_volume as f64) {
        tracing::warn!("Failed to set initial volume: {}", e);
    }

    // Mark as ready
    state.set_loading(false);
    state.set_playing(true);

    // Explicitly seek to 0 at start - some containers may have incorrect
    // initial position after preroll due to index/metadata issues (e.g., MP4
    // with moov atom at end, or containers with inaccurate duration info).
    if let Err(e) = dec.seek(0) {
        tracing::warn!("Initial seek to 0 failed: {}", e);
    }

    // Verify actual position - seek may fail silently for some formats
    let actual_pos = match dec.query_actual_position_ms() {
        Ok(pos) => pos,
        Err(_) => dec.position_ms(),
    };
    if actual_pos > 500 {
        tracing::warn!(
            "Decoder position after seek is {}ms, expected ~0 - video may have seek issues",
            actual_pos
        );
    }
    state.set_position(actual_pos as f64 / 1000.0);

    // Start pipeline in PLAYING state
    // GStreamer's clock handles all A/V timing with sync(true) on sinks
    if let Err(e) = dec.play() {
        tracing::warn!("Failed to start playback: {}", e);
    }

    // Track if pipeline is currently playing (to avoid redundant state changes)
    let mut pipeline_playing = true;
    // Track last volume to avoid redundant set_volume calls
    let mut last_volume = initial_volume;

    // Main playback loop
    // With sync(true) on appsinks, GStreamer handles all timing via its clock.
    // We just pull frames when ready and render them.
    loop {
        // Check stop request
        if state.should_stop() {
            break;
        }

        // Handle seek requests BEFORE pause check so seeking works when paused
        if let Some(seek_pos) = state.take_seek_request() {
            let seek_ms = (seek_pos * 1000.0) as u64;
            tracing::debug!("Seeking to {} seconds ({}ms)", seek_pos, seek_ms);

            if let Err(e) = dec.seek(seek_ms) {
                tracing::warn!("Seek failed: {}", e);
            } else {
                state.set_position(seek_pos);
                tracing::debug!("Seek completed to {} seconds", seek_pos);
            }
        }

        // Handle pause via pipeline state - this stops GStreamer's clock
        // which is essential for proper A/V sync on resume
        if !state.is_playing() {
            // Pause the GStreamer pipeline (stops clock)
            if pipeline_playing {
                if let Err(e) = dec.pause() {
                    tracing::warn!("Failed to pause pipeline: {}", e);
                }
                pipeline_playing = false;
            }

            // While paused, check for seek requests
            while !state.is_playing() && !state.should_stop() {
                if let Some(seek_pos) = state.take_seek_request() {
                    let seek_ms = (seek_pos * 1000.0) as u64;
                    tracing::debug!(
                        "Seeking while paused to {} seconds ({}ms)",
                        seek_pos,
                        seek_ms
                    );

                    if let Err(e) = dec.seek(seek_ms) {
                        tracing::warn!("Seek failed: {}", e);
                    } else {
                        state.set_position(seek_pos);

                        // Decode one frame to show the new position immediately
                        // Pipeline is paused, so this just pulls the seeked frame
                        if let Ok(Some(frame)) = dec.decode_video_frame() {
                            state.set_position(frame.timestamp_ms as f64 / 1000.0);
                            frame_callback(frame.width, frame.height, frame.data);
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(50));
            }

            // Resume: set pipeline back to PLAYING
            if state.is_playing() && !state.should_stop() {
                if let Err(e) = dec.play() {
                    tracing::warn!("Failed to resume playback: {}", e);
                }
                pipeline_playing = true;
            }
        }

        if state.should_stop() {
            break;
        }

        // Update volume only when changed (GStreamer handles audio output via autoaudiosink)
        let current_volume = state.get_volume();
        if (current_volume - last_volume).abs() > 0.001 {
            if let Err(e) = dec.set_volume(current_volume as f64) {
                tracing::warn!("Failed to set volume: {}", e);
            }
            last_volume = current_volume;
        }

        // Pull next video frame - with sync(true), this blocks until frame should display
        // GStreamer's clock handles all timing, no manual PTS comparison needed
        match dec.decode_video_frame() {
            Ok(Some(frame)) => {
                // Update position and render frame
                let position = frame.timestamp_ms as f64 / 1000.0;
                state.set_position(position);
                frame_callback(frame.width, frame.height, frame.data);
            }
            Ok(None) => {
                // End of stream
                tracing::debug!("Video stream ended (EOS)");
                break;
            }
            Err(e) => {
                // Check if this is a "no sample yet" case (pipeline warming up)
                if e.to_string().contains("no_sample") {
                    // Pipeline not ready yet, retry immediately - the try_pull_sample
                    // already waited its timeout (50ms), no need to sleep more
                    continue;
                }
                tracing::warn!("Frame decode error: {}", e);
                // Android's unsupported layouts/codec errors are terminal;
                // retrying them would spin forever instead of surfacing the
                // decoder's explicit limitation through PlayerState::error.
                #[cfg(target_os = "android")]
                {
                    dec.close();
                    return Err(e);
                }
                #[cfg(not(target_os = "android"))]
                continue;
            }
        }
    }

    dec.close();
    tracing::debug!("Video playback finished");
    Ok(())
}

/// Format duration as MM:SS string.
pub fn format_duration(seconds: f64) -> String {
    let total_secs = seconds as u64;
    let mins = total_secs / 60;
    let secs = total_secs % 60;
    format!("{}:{:02}", mins, secs)
}
