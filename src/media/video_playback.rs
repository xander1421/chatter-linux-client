//! In-app video player using platform-native decoders.
//!
//! Provides video playback with audio sync for viewing video attachments
//! within the application. Renders frames to Slint Image widget.
//!
//! Uses GStreamer on Linux, VideoToolbox on macOS/iOS, MediaCodec on Android.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::HeapRb;

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
            .map_err(|e| MediaError::PlaybackError(format!("Failed to start playback thread: {}", e)))?;

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
        if data.is_empty() {
            return Err(MediaError::InvalidFormat("empty video data".to_string()));
        }

        let state = SharedPlayerState::new();
        let state_clone = state.clone();

        let playback_thread = std::thread::Builder::new()
            .name("video-playback".into())
            .spawn(move || {
                if let Err(e) = run_playback_from_memory(data, state_clone.clone(), frame_callback) {
                    tracing::error!("Video playback error: {}", e);
                    state_clone.set_error(Some(e.to_string()));
                }
                state_clone.set_playing(false);
                state_clone.set_loading(false);
            })
            .map_err(|e| MediaError::PlaybackError(format!("Failed to start playback thread: {}", e)))?;

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
    data: Vec<u8>,
    state: SharedPlayerState,
    frame_callback: F,
) -> Result<(), MediaError>
where
    F: FnMut(u32, u32, Vec<u8>) + Send + 'static,
{
    let mut dec = decoder::create_decoder()?;
    let info = dec.open_memory(data)?;

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
        width, height, fps, duration_secs, info.has_audio
    );

    state.set_duration(duration_secs);
    state.set_dimensions(width, height);

    // Setup audio output if video has audio
    let audio_output = if info.has_audio {
        match setup_audio_output(state.clone()) {
            Ok(output) => Some(output),
            Err(e) => {
                tracing::warn!("Audio setup failed, playing without audio: {}", e);
                None
            }
        }
    } else {
        None
    };

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

    // Playback timing: track when playback started and at what video position.
    // This allows accurate PTS-based timing that survives seeks.
    let mut playback_start_time = Instant::now();
    let mut playback_start_pts_ms: u64 = actual_pos; // Video position when playback started

    // Main playback loop
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
                // Reset timing after seek - playback restarts from this position
                playback_start_time = Instant::now();
                playback_start_pts_ms = seek_ms;
                tracing::debug!("Seek completed to {} seconds", seek_pos);
            }
        }

        // Handle pause - placed AFTER seek so users can seek while paused
        while !state.is_playing() && !state.should_stop() {
            // Check for seek requests while paused
            if let Some(seek_pos) = state.take_seek_request() {
                let seek_ms = (seek_pos * 1000.0) as u64;
                tracing::debug!("Seeking while paused to {} seconds ({}ms)", seek_pos, seek_ms);

                if let Err(e) = dec.seek(seek_ms) {
                    tracing::warn!("Seek failed: {}", e);
                } else {
                    state.set_position(seek_pos);
                    playback_start_pts_ms = seek_ms;

                    // Decode one frame to show the new position immediately
                    // This gives visual feedback when scrubbing while paused
                    if let Ok(Some(frame)) = dec.decode_video_frame() {
                        state.set_position(frame.timestamp_ms as f64 / 1000.0);
                        frame_callback(frame.width, frame.height, frame.data);
                    }
                }
            }
            // Reset timing reference when unpausing
            playback_start_time = Instant::now();
            std::thread::sleep(Duration::from_millis(50));
        }

        if state.should_stop() {
            break;
        }

        // Decode and process audio if available
        if let Some(ref audio) = audio_output {
            // Try to decode some audio samples
            if let Ok(Some(audio_data)) = dec.decode_audio() {
                let volume = state.get_volume();
                audio.send_samples(&audio_data.samples, volume);
            }
        }

        // Decode next video frame
        match dec.decode_video_frame() {
            Ok(Some(frame)) => {
                // Update position
                let position = frame.timestamp_ms as f64 / 1000.0;
                state.set_position(position);

                // Send frame to callback
                frame_callback(frame.width, frame.height, frame.data);

                // Frame timing using PTS (presentation timestamp) for accurate sync.
                // Calculate when this frame should be displayed relative to playback start.
                //
                // frame_relative_pts = how far into playback this frame should appear
                // elapsed = how much time has actually passed since playback started
                //
                // If frame_relative_pts > elapsed, we're ahead and should wait.
                // If elapsed > frame_relative_pts, we're behind and should play immediately.
                let frame_relative_pts_ms = frame.timestamp_ms.saturating_sub(playback_start_pts_ms);
                let frame_relative_pts = Duration::from_millis(frame_relative_pts_ms);
                let elapsed = playback_start_time.elapsed();

                if frame_relative_pts > elapsed {
                    // Frame should be shown in the future - wait
                    let wait_time = frame_relative_pts - elapsed;
                    // Cap maximum wait to prevent issues with bad timestamps
                    if wait_time < Duration::from_secs(2) {
                        std::thread::sleep(wait_time);
                    }
                }
                // If elapsed > frame_relative_pts, we're behind - play immediately to catch up
            }
            Ok(None) => {
                // End of stream
                tracing::debug!("Video stream ended (EOS)");
                break;
            }
            Err(e) => {
                // Check if this is a "no sample yet" case (pipeline warming up)
                if e.to_string().contains("no_sample") {
                    // Pipeline not ready yet, wait a bit and retry
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                tracing::warn!("Frame decode error: {}", e);
                // Try to continue for other errors
                continue;
            }
        }
    }

    dec.close();
    tracing::debug!("Video playback finished");
    Ok(())
}

/// Audio output handler.
struct AudioOutput {
    producer: Arc<Mutex<ringbuf::HeapProd<f32>>>,
    _stream: cpal::Stream,
}

impl AudioOutput {
    fn send_samples(&self, samples: &[f32], volume: f32) {
        if let Ok(mut prod) = self.producer.lock() {
            for &sample in samples {
                // Apply volume and push to ring buffer
                // If buffer is full, drop samples (better than blocking)
                if prod.vacant_len() > 0 {
                    let _ = prod.try_push(sample * volume);
                }
            }
        }
    }
}

/// Setup audio playback.
fn setup_audio_output(_state: SharedPlayerState) -> Result<AudioOutput, MediaError> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| MediaError::PlaybackError("No audio output device".to_string()))?;

    let config = device
        .default_output_config()
        .map_err(|e| MediaError::PlaybackError(format!("Audio config error: {}", e)))?;

    let sample_rate = config.sample_rate().0;
    let channels = config.channels() as usize;

    // Audio buffer (1 second worth)
    let buffer = HeapRb::<f32>::new(sample_rate as usize * channels * 2);
    let (producer, mut consumer) = buffer.split();
    let producer = Arc::new(Mutex::new(producer));

    // Build output stream
    let stream = device
        .build_output_stream(
            &config.into(),
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                for sample in data.iter_mut() {
                    *sample = consumer.try_pop().unwrap_or(0.0);
                }
            },
            |err| tracing::error!("Audio stream error: {}", err),
            None,
        )
        .map_err(|e| MediaError::PlaybackError(format!("Audio stream error: {}", e)))?;

    stream
        .play()
        .map_err(|e| MediaError::PlaybackError(format!("Audio play error: {}", e)))?;

    Ok(AudioOutput {
        producer,
        _stream: stream,
    })
}

/// Convert RGB data to Slint SharedPixelBuffer.
///
/// This is a helper for the UI layer to convert the raw RGB bytes
/// from the frame callback into a Slint Image.
pub fn rgb_to_slint_image(width: u32, height: u32, rgb_data: &[u8]) -> slint::Image {
    let mut pixel_buffer = slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(width, height);
    let bytes = pixel_buffer.make_mut_bytes();
    bytes.copy_from_slice(rgb_data);
    slint::Image::from_rgb8(pixel_buffer)
}

/// Format duration as MM:SS string.
pub fn format_duration(seconds: f64) -> String {
    let total_secs = seconds as u64;
    let mins = total_secs / 60;
    let secs = total_secs % 60;
    format!("{}:{:02}", mins, secs)
}
