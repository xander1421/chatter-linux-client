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
    pub pixels: slint::SharedPixelBuffer<slint::Rgb8Pixel>,
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

/// Serializes request acceptance, player installation, stop and frame publication.
/// Decoder shutdown happens outside this lock: stop joins its frame callback thread.
static LOAD_GENERATION: Mutex<u64> = Mutex::new(0);

#[derive(Clone)]
struct LoadRequest {
    generation: u64,
    still_current: Arc<dyn Fn() -> bool + Send + Sync>,
}

impl LoadRequest {
    fn matches(&self, generation: u64) -> bool {
        self.generation == generation && (self.still_current)()
    }

    fn is_current(&self) -> bool {
        LOAD_GENERATION
            .lock()
            .is_ok_and(|generation| self.matches(*generation))
    }

    fn publish_frame(&self, width: u32, height: u32, rgb_data: Vec<u8>) {
        if !self.is_current() { return; }
        // Prepare the presentation buffer on the decoder thread. The UI only
        // wraps the shared pixels; it never copies a full RGB frame on a tick.
        let pixels = slint::SharedPixelBuffer::clone_from_slice(&rgb_data, width, height);
        let Ok(generation) = LOAD_GENERATION.lock() else {
            return;
        };
        if !self.matches(*generation) {
            return;
        }
        if let Ok(mut buffer) = FRAME_BUFFER.lock() {
            *buffer = Some(FrameData {
                width,
                height,
                pixels,
            });
        }
    }
}

fn clear_playback() -> Option<VideoPlayer> {
    USING_GL_VIDEO.store(false, Ordering::SeqCst);
    let player = VIDEO_PLAYER
        .lock()
        .ok()
        .and_then(|mut player| player.take());
    if let Ok(mut frame) = FRAME_BUFFER.lock() {
        *frame = None;
    }
    if let Ok(mut state) = VIDEO_STATE.lock() {
        *state = VideoState::default();
    }
    player
}

fn stop_hardware_for(generation: u64) {
    if !hw_video::is_hw_available() {
        return;
    }
    // Hardware state belongs to the UI thread. A delayed stop cannot stop a
    // newer start: both operations compare the generation under the same gate.
    let _ = slint::invoke_from_event_loop(move || {
        let Ok(current) = LOAD_GENERATION.lock() else {
            return;
        };
        if *current == generation {
            hw_video::stop_hw_video();
        }
    });
}

fn begin_request(still_current: Arc<dyn Fn() -> bool + Send + Sync>) -> Option<LoadRequest> {
    let (generation, previous) = {
        let mut generation = LOAD_GENERATION.lock().ok()?;
        // A slow, superseded decrypt must not even stop the current player.
        if !still_current() {
            return None;
        }
        *generation = generation.wrapping_add(1);
        (*generation, clear_playback())
    };
    drop(previous);
    stop_hardware_for(generation);
    Some(LoadRequest {
        generation,
        still_current,
    })
}

fn stop_generation(expected: Option<u64>) {
    let (generation, previous) = {
        let Ok(mut generation) = LOAD_GENERATION.lock() else {
            return;
        };
        if expected.is_some_and(|expected| expected != *generation) {
            return;
        }
        *generation = generation.wrapping_add(1);
        (*generation, clear_playback())
    };
    drop(previous);
    stop_hardware_for(generation);
}

fn update_loaded_state(filename: String, duration: f64) {
    if let Ok(mut state) = VIDEO_STATE.lock() {
        *state = VideoState {
            filename,
            is_playing: true,
            duration,
            volume: 1.0,
            ..Default::default()
        };
    }
}

/// Existing callers retain the unguarded API; gallery callers supply a request guard.
pub fn start_video_from_memory_async<F>(data: Vec<u8>, filename: String, on_ready: F)
where
    F: FnOnce(Result<(f64, String), String>) + Send + 'static,
{
    start_video_from_memory_guarded_async(data, filename, || true, on_ready);
}

pub fn start_video_from_memory_guarded_async<G, F>(
    data: Vec<u8>,
    filename: String,
    still_current: G,
    on_ready: F,
) where
    G: Fn() -> bool + Send + Sync + 'static,
    F: FnOnce(Result<(f64, String), String>) + Send + 'static,
{
    let Some(request) = begin_request(Arc::new(still_current)) else {
        return;
    };
    if hw_video::is_hw_available() {
        let _ = slint::invoke_from_event_loop(move || {
            let Ok(generation) = LOAD_GENERATION.lock() else {
                return;
            };
            if !request.matches(*generation) {
                return;
            }
            hw_video::stop_hw_video();
            let duration = hw_video::is_hw_connected()
                .then(|| hw_video::start_hw_video(data.clone()))
                .flatten();
            if let Some(duration) = duration {
                // External selection can change while the backend starts.
                if !request.matches(*generation) {
                    hw_video::stop_hw_video();
                    return;
                }
                USING_GL_VIDEO.store(true, Ordering::SeqCst);
                update_loaded_state(filename, duration);
                drop(generation);
                if request.is_current() {
                    on_ready(Ok((
                        duration,
                        chatter_core::media::format_duration(duration),
                    )));
                }
            } else {
                drop(generation);
                start_video_software(data, filename, request, on_ready);
            }
        });
    } else {
        start_video_software(data, filename, request, on_ready);
    }
}

pub fn start_video_from_range_async<F>(
    dec: Arc<chatter_core::services::attachment::CbcRangeDecryptor>,
    filename: String,
    on_ready: F,
) where
    F: FnOnce(Result<(f64, String), String>) + Send + 'static,
{
    start_video_from_range_guarded_async(dec, filename, || true, on_ready);
}

pub fn start_video_from_range_guarded_async<G, F>(
    dec: Arc<chatter_core::services::attachment::CbcRangeDecryptor>,
    filename: String,
    still_current: G,
    on_ready: F,
) where
    G: Fn() -> bool + Send + Sync + 'static,
    F: FnOnce(Result<(f64, String), String>) + Send + 'static,
{
    let Some(request) = begin_request(Arc::new(still_current)) else {
        return;
    };
    if hw_video::is_hw_available() {
        let _ = slint::invoke_from_event_loop(move || {
            let Ok(generation) = LOAD_GENERATION.lock() else {
                return;
            };
            if !request.matches(*generation) {
                return;
            }
            hw_video::stop_hw_video();
            let duration = hw_video::is_hw_connected()
                .then(|| hw_video::start_hw_video_range(dec.clone()))
                .flatten();
            if let Some(duration) = duration {
                if !request.matches(*generation) {
                    hw_video::stop_hw_video();
                    return;
                }
                USING_GL_VIDEO.store(true, Ordering::SeqCst);
                update_loaded_state(filename, duration);
                drop(generation);
                if request.is_current() {
                    on_ready(Ok((
                        duration,
                        chatter_core::media::format_duration(duration),
                    )));
                }
            } else {
                drop(generation);
                start_software_range(dec, filename, request, on_ready);
            }
        });
    } else {
        start_software_range(dec, filename, request, on_ready);
    }
}

fn start_software_range<F>(
    dec: Arc<chatter_core::services::attachment::CbcRangeDecryptor>,
    filename: String,
    request: LoadRequest,
    on_ready: F,
) where
    F: FnOnce(Result<(f64, String), String>) + Send + 'static,
{
    start_video_software_source(SoftwareSource::Range(dec), filename, request, on_ready);
}

enum SoftwareSource {
    Memory(Vec<u8>),
    Range(Arc<chatter_core::services::attachment::CbcRangeDecryptor>),
}

fn start_video_software<F>(data: Vec<u8>, filename: String, request: LoadRequest, on_ready: F)
where
    F: FnOnce(Result<(f64, String), String>) + Send + 'static,
{
    start_video_software_source(SoftwareSource::Memory(data), filename, request, on_ready);
}

fn start_video_software_source<F>(
    data: SoftwareSource,
    filename: String,
    request: LoadRequest,
    on_ready: F,
) where
    F: FnOnce(Result<(f64, String), String>) + Send + 'static,
{
    std::thread::spawn(move || {
        let frame_request = request.clone();
        // Creation only spawns the decoder thread. Serialize its installation
        // with stop, and never join/drop a decoder while holding this gate.
        let player_state = {
            let Ok(generation) = LOAD_GENERATION.lock() else {
                return;
            };
            if !request.matches(*generation) {
                return;
            }
            let callback = move |width, height, rgb_data| {
                frame_request.publish_frame(width, height, rgb_data);
            };
            let player = match data {
                SoftwareSource::Memory(data) => VideoPlayer::start_from_memory(data, callback),
                SoftwareSource::Range(range) => VideoPlayer::start_from_range(range, callback),
            };
            let player = match player {
                Ok(player) => player,
                Err(error) => {
                    drop(generation);
                    if request.is_current() {
                        on_ready(Err(error.to_string()));
                    }
                    return;
                }
            };
            if !request.matches(*generation) {
                drop(generation);
                drop(player);
                return;
            }
            let state = player.state().clone();
            let Ok(mut holder) = VIDEO_PLAYER.lock() else {
                drop(generation);
                drop(player);
                return;
            };
            let previous = holder.replace(player);
            USING_GL_VIDEO.store(false, Ordering::SeqCst);
            drop(holder);
            drop(generation);
            drop(previous);
            state
        };
        let start = Instant::now();
        loop {
            if !request.is_current() {
                stop_generation(Some(request.generation));
                return;
            }
            let state = player_state.get_state();
            if let Some(error) = state.error {
                if request.is_current() {
                    on_ready(Err(error));
                }
                stop_generation(Some(request.generation));
                return;
            }
            if !state.loading {
                let Ok(generation) = LOAD_GENERATION.lock() else {
                    return;
                };
                if !request.matches(*generation) {
                    return;
                }
                update_loaded_state(filename, state.duration);
                drop(generation);
                if request.is_current() {
                    on_ready(Ok((
                        state.duration,
                        chatter_core::media::format_duration(state.duration),
                    )));
                }
                return;
            }
            if start.elapsed() >= Duration::from_secs(15) {
                if request.is_current() {
                    on_ready(Err("Video loading timed out".into()));
                }
                stop_generation(Some(request.generation));
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    });
}

/// Invalidate queued loaders before taking and stopping the installed decoder.
pub fn stop_video() {
    stop_generation(None);
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

#[cfg(test)]
mod loader_tests {
    use super::*;
    static TEST_GATE: Mutex<()> = Mutex::new(());

    #[test]
    fn stale_decrypt_does_not_begin_or_clear_newer_playback() {
        let _serial = TEST_GATE.lock().unwrap();
        *LOAD_GENERATION.lock().unwrap() = 40;
        VIDEO_STATE.lock().unwrap().filename = "newer video".into();
        let called = Arc::new(AtomicBool::new(false));
        let callback = called.clone();
        start_video_from_memory_guarded_async(
            vec![1],
            "stale video".into(),
            || false,
            move |_| {
                callback.store(true, Ordering::SeqCst);
            },
        );
        assert_eq!(*LOAD_GENERATION.lock().unwrap(), 40);
        assert_eq!(VIDEO_STATE.lock().unwrap().filename, "newer video");
        assert!(!called.load(Ordering::SeqCst));
        *VIDEO_STATE.lock().unwrap() = VideoState::default();
    }

    #[test]
    fn obsolete_decoder_frames_cannot_overwrite_the_current_frame() {
        let _serial = TEST_GATE.lock().unwrap();
        *LOAD_GENERATION.lock().unwrap() = 42;
        let current = LoadRequest {
            generation: 42,
            still_current: Arc::new(|| true),
        };
        let old = LoadRequest {
            generation: 41,
            still_current: Arc::new(|| true),
        };
        current.publish_frame(1, 1, vec![1, 2, 3]);
        old.publish_frame(1, 1, vec![9, 9, 9]);
        assert_eq!(take_frame().unwrap().pixels.as_bytes(), &[1, 2, 3]);
        let cancelled = LoadRequest {
            generation: 42,
            still_current: Arc::new(|| false),
        };
        cancelled.publish_frame(1, 1, vec![8, 8, 8]);
        assert!(take_frame().is_none());
    }

    #[test]
    fn old_loader_cleanup_cannot_clear_newer_playback() {
        let _serial = TEST_GATE.lock().unwrap();
        *LOAD_GENERATION.lock().unwrap() = 44;
        VIDEO_STATE.lock().unwrap().filename = "keep new player".into();
        let request = LoadRequest {
            generation: 44,
            still_current: Arc::new(|| true),
        };
        request.publish_frame(1, 1, vec![1, 2, 3]);
        stop_generation(Some(43));
        assert_eq!(*LOAD_GENERATION.lock().unwrap(), 44);
        assert_eq!(VIDEO_STATE.lock().unwrap().filename, "keep new player");
        assert_eq!(take_frame().unwrap().pixels.as_bytes(), &[1, 2, 3]);
        *VIDEO_STATE.lock().unwrap() = VideoState::default();
    }
}
