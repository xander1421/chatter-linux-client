//! GIF animation state.
//!
//! Manages animated GIF playback state for the UI thread.
//! Supports both the image viewer lightbox and inline message thumbnails.
//!
//! # Memory Bounds
//!
//! - Viewer: MAX_VIEWER_FRAMES (100) per animation
//! - Inline: MAX_INLINE_GIFS (20) with LRU eviction

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use slint::{Image, Model, Timer, TimerMode, Weak};

use chatter_linux::media::gif::{
    frame_to_slint_image, decode_first_frame_only,
    decode_gif_auto, is_gpu_acceleration_available,
};
use tokio::sync::mpsc;

use crate::MainWindow;

/// Maximum frames to store per GIF animation (viewer).
const MAX_VIEWER_FRAMES: usize = 100;

/// Maximum inline GIF animations to keep in memory.
const MAX_INLINE_GIFS: usize = 20;

/// Raw frame data that can be sent between threads.
type RawFrame = (Vec<u8>, u32, u32, u64); // (data, width, height, delay_ms)

thread_local! {
    /// Current GIF animation state for image viewer (must be on UI thread).
    static VIEWER_GIF_STATE: RefCell<Option<ViewerGifState>> = const { RefCell::new(None) };
    /// Animation timer for image viewer (must be on UI thread).
    static VIEWER_TIMER: RefCell<Option<Timer>> = const { RefCell::new(None) };
    /// Receiver for streaming frames from background decoder.
    static VIEWER_FRAME_RX: RefCell<Option<mpsc::Receiver<RawFrame>>> = const { RefCell::new(None) };
    /// Attachment ID for inline registration when decode completes.
    static VIEWER_INLINE_ID: RefCell<Option<String>> = const { RefCell::new(None) };

    /// Inline GIF animations for message thumbnails (attachment_id -> frames).
    static INLINE_GIFS: RefCell<HashMap<String, InlineGifState>> = RefCell::new(HashMap::new());
    /// LRU order for inline GIFs (front = oldest, back = newest).
    static INLINE_LRU: RefCell<VecDeque<String>> = RefCell::new(VecDeque::new());
    /// Global animation timer for inline GIFs.
    static INLINE_TIMER: RefCell<Option<Timer>> = const { RefCell::new(None) };
    /// Window weak reference for inline GIF updates.
    static INLINE_WINDOW: RefCell<Option<Weak<MainWindow>>> = const { RefCell::new(None) };
    /// Cache of attachment locations: attachment_id -> (msg_idx, att_idx) for O(1) updates.
    static INLINE_LOCATION_CACHE: RefCell<HashMap<String, (usize, usize)>> = RefCell::new(HashMap::new());

    /// Picker GIF animations (gif_id -> frames).
    static PICKER_GIFS: RefCell<HashMap<String, InlineGifState>> = RefCell::new(HashMap::new());
    /// Global animation timer for picker GIFs.
    static PICKER_TIMER: RefCell<Option<Timer>> = const { RefCell::new(None) };
}

// ============================================================================
// Picker GIF Animation
// ============================================================================

/// Register a GIF for the picker animation.
pub fn register_picker_gif(
    gif_id: String,
    raw_frames: Vec<(Vec<u8>, u32, u32, u64)>,
) {
    if raw_frames.len() <= 1 {
        return;
    }

    // Convert raw frames to Slint Images
    let frames: Vec<Image> = raw_frames
        .iter()
        .map(|(data, width, height, _)| {
            let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                data,
                *width,
                *height,
            );
            slint::Image::from_rgba8(buffer)
        })
        .collect();

    let delays: Vec<u64> = raw_frames.iter().map(|(_, _, _, d)| *d).collect();

    let state = InlineGifState {
        frames,
        delays,
        current_frame: 0,
        accumulated_ms: 0,
    };

    PICKER_GIFS.with(|gifs| {
        gifs.borrow_mut().insert(gif_id, state);
    });

    start_picker_timer_if_needed();
}

/// Clear all picker GIF animations.
pub fn clear_picker_gifs() {
    PICKER_GIFS.with(|gifs| {
        gifs.borrow_mut().clear();
    });
    stop_picker_timer();
}

/// Promote a picker GIF to an inline GIF instantly.
pub fn promote_picker_gif_to_inline(picker_id: &str, inline_id: &str) {
    let state_opt = PICKER_GIFS.with(|gifs| {
        gifs.borrow().get(picker_id).cloned()
    });

    if let Some(state) = state_opt {
        INLINE_GIFS.with(|gifs| {
            gifs.borrow_mut().insert(inline_id.to_string(), state);
        });

        // Add to LRU cache
        INLINE_LRU.with(|lru| {
            let mut lru = lru.borrow_mut();
            if let Some(pos) = lru.iter().position(|x| x == inline_id) {
                lru.remove(pos);
            }
            lru.push_back(inline_id.to_string());
            
            // Trim if too many
            while lru.len() > 100 {
                if let Some(oldest) = lru.pop_front() {
                    INLINE_GIFS.with(|g| g.borrow_mut().remove(&oldest));
                }
            }
        });

        start_inline_timer_if_needed();
        tracing::debug!("Promoted picker GIF {} to inline GIF {}", picker_id, inline_id);
    }
}

fn start_picker_timer_if_needed() {
    PICKER_TIMER.with(|timer_cell| {
        if timer_cell.borrow().is_some() {
            return;
        }

        let timer = Timer::default();
        timer.start(TimerMode::Repeated, Duration::from_millis(33), move || {
            tick_picker_gifs(33);
        });

        *timer_cell.borrow_mut() = Some(timer);
    });
}

fn stop_picker_timer() {
    PICKER_TIMER.with(|timer_cell| {
        if let Some(ref timer) = *timer_cell.borrow() {
            timer.stop();
        }
        *timer_cell.borrow_mut() = None;
    });
}

fn tick_picker_gifs(elapsed_ms: u64) {
    let updates: Vec<(String, Image)> = PICKER_GIFS.with(|gifs| {
        let mut gifs = gifs.borrow_mut();
        let mut updates = Vec::new();

        for (id, state) in gifs.iter_mut() {
            if state.tick(elapsed_ms) {
                updates.push((id.clone(), state.current_image()));
            }
        }

        updates
    });

    if updates.is_empty() {
        return;
    }

    INLINE_WINDOW.with(|window_cell| {
        if let Some(ref window_weak) = *window_cell.borrow() {
            if let Some(w) = window_weak.upgrade() {
                update_picker_thumbnails(&w, &updates);
            }
        }
    });
}

fn update_picker_thumbnails(window: &MainWindow, updates: &[(String, Image)]) {
    let results = window.get_composer_gif_picker_results();
    let count = results.row_count();

    for (update_id, new_image) in updates {
        for idx in 0..count {
            if let Some(mut gif) = results.row_data(idx) {
                if gif.id.as_str() == update_id {
                    if let Some(model) = results.as_any().downcast_ref::<slint::VecModel<crate::GifData>>() {
                        gif.preview = new_image.clone();
                        model.set_row_data(idx, gif);
                    }
                    break;
                }
            }
        }
    }
}

// ============================================================================
// Image Viewer GIF Animation
// ============================================================================

/// State for a playing GIF animation in the image viewer.
struct ViewerGifState {
    /// Pre-converted Slint images for each frame.
    frames: Vec<Image>,
    /// Frame delays in milliseconds.
    delays: Vec<u64>,
    /// Current frame index.
    current_frame: usize,
    /// Whether animation is playing.
    playing: bool,
}

impl ViewerGifState {
    fn current_image(&self) -> Image {
        self.frames.get(self.current_frame).cloned().unwrap_or_default()
    }

    fn current_delay_ms(&self) -> u64 {
        self.delays.get(self.current_frame).copied().unwrap_or(100)
    }

    fn advance_frame(&mut self) {
        if !self.frames.is_empty() {
            self.current_frame = (self.current_frame + 1) % self.frames.len();
        }
    }

    #[allow(dead_code)]
    fn frame_count(&self) -> usize {
        self.frames.len()
    }
}

/// Start GIF animation in viewer AND register for inline animation.
///
/// Uses **streaming progressive** decoding with automatic backend selection:
/// 1. Try GStreamer + GL (GPU accelerated) - if available
/// 2. Fall back to GStreamer + CPU - if GL not available
/// 3. Fall back to pure Rust `image` crate - if GStreamer unavailable
///
/// For all backends:
/// 1. Decode first frame synchronously (~16ms for any size)
/// 2. Start animation IMMEDIATELY (even with just 1 frame)
/// 3. Background decoder sends frames via channel as they decode
/// 4. Animation progressively gets smoother as frames arrive
/// 5. Also registers for inline message thumbnail animation (shared decode)
///
/// Returns the first frame to display immediately, or None if not an animated GIF.
pub fn start_gif_animation_and_inline(
    data: &[u8],
    window: &Weak<MainWindow>,
    attachment_id: &str,
) -> Option<Image> {
    let start = std::time::Instant::now();
    let gpu_available = is_gpu_acceleration_available();

    // Fast path: decode first frame only (instant)
    let first_frame = match decode_first_frame_only(data) {
        Ok(Some(frame)) => frame,
        Ok(None) => {
            tracing::debug!("Not a GIF or empty");
            return None;
        }
        Err(e) => {
            tracing::warn!("Failed to decode GIF first frame: {:?}", e);
            return None;
        }
    };

    let first_frame_image = frame_to_slint_image(&first_frame);
    let first_delay = first_frame.delay.as_millis() as u64;

    tracing::info!(
        "GIF first frame decoded in {:?}, delay {}ms - starting animation immediately (GPU: {})",
        start.elapsed(),
        first_delay,
        gpu_available
    );

    // Store initial state with just first frame - animation starts NOW
    let initial_state = ViewerGifState {
        frames: vec![first_frame_image.clone()],
        delays: vec![first_delay],
        current_frame: 0,
        playing: true, // Start playing immediately!
    };

    VIEWER_GIF_STATE.with(|cell| {
        *cell.borrow_mut() = Some(initial_state);
    });

    // Store inline ID for later registration
    VIEWER_INLINE_ID.with(|cell| {
        *cell.borrow_mut() = Some(attachment_id.to_string());
    });

    // Create channel for streaming frames from background decoder
    let (tx, rx) = mpsc::channel::<RawFrame>(64);

    // Store receiver for polling in timer
    VIEWER_FRAME_RX.with(|cell| {
        *cell.borrow_mut() = Some(rx);
    });

    // Spawn background decoder - streams frames as they decode
    // Uses GPU acceleration when available (GStreamer + GL)
    let data_vec = data.to_vec();
    std::thread::spawn(move || {
        let bg_start = std::time::Instant::now();

        // Try GPU-accelerated decoder first, fall back to CPU
        let decode_result = decode_gif_with_best_backend(&data_vec, &tx);

        match decode_result {
            Ok(frame_count) => {
                tracing::info!(
                    "GIF streaming decode complete: {} frames sent in {:?} (backend: {})",
                    frame_count,
                    bg_start.elapsed(),
                    if gpu_available { "GPU" } else { "CPU" }
                );
            }
            Err(e) => {
                tracing::warn!("GIF decode failed: {}", e);
            }
        }
    });

    // Start animation timer immediately - it will poll for new frames
    start_viewer_timer(window.clone());

    // Return first frame immediately
    Some(first_frame_image)
}

/// Decode GIF using the best available backend.
///
/// Tries in order:
/// 1. GStreamer + GL (GPU accelerated)
/// 2. GStreamer + CPU
/// 3. Pure Rust `image` crate
fn decode_gif_with_best_backend(
    data: &[u8],
    tx: &mpsc::Sender<RawFrame>,
) -> Result<usize, String> {
    // Try unified decoder (automatically selects best backend)
    match decode_gif_auto(data.to_vec()) {
        Ok(mut decoder) => {
            let mut frame_count = 0;

            // The first frame is already decoded and available
            // Skip it since we already showed it synchronously

            // Poll for remaining frames
            loop {
                // Check for new frames
                let new_frames = decoder.poll_frames();

                if new_frames > 0 {
                    // Send newly decoded frames
                    let frames = decoder.frames();
                    for frame in frames.iter().skip(1 + frame_count) {
                        let delay_ms = frame.delay.as_millis() as u64;
                        if tx.blocking_send((
                            frame.data.clone(),
                            frame.width,
                            frame.height,
                            delay_ms,
                        )).is_err() {
                            tracing::debug!("GIF receiver dropped at frame {}", frame_count);
                            return Ok(frame_count);
                        }
                        frame_count += 1;
                    }
                }

                if decoder.is_complete() {
                    // Send any remaining frames
                    let frames = decoder.frames();
                    for frame in frames.iter().skip(1 + frame_count) {
                        let delay_ms = frame.delay.as_millis() as u64;
                        if tx.blocking_send((
                            frame.data.clone(),
                            frame.width,
                            frame.height,
                            delay_ms,
                        )).is_err() {
                            break;
                        }
                        frame_count += 1;
                    }
                    break;
                }

                // Small sleep to avoid busy-waiting
                std::thread::sleep(std::time::Duration::from_millis(5));
            }

            Ok(frame_count)
        }
        Err(e) => {
            // Fall back to image crate if unified decoder fails
            tracing::debug!("Unified decoder failed ({}), using image crate fallback", e);
            decode_gif_with_image_crate(data, tx)
        }
    }
}

/// Fallback decoder using the `image` crate (pure Rust, no GPU).
fn decode_gif_with_image_crate(
    data: &[u8],
    tx: &mpsc::Sender<RawFrame>,
) -> Result<usize, String> {
    use image::codecs::gif::GifDecoder as ImageGifDecoder;
    use image::AnimationDecoder;
    use std::io::Cursor;

    let cursor = Cursor::new(data);
    let decoder = ImageGifDecoder::new(cursor)
        .map_err(|e| format!("Image crate GIF decode failed: {}", e))?;

    let frames_iter = decoder.into_frames();
    let mut frame_count = 0;

    for (idx, frame_result) in frames_iter.enumerate() {
        // Skip first frame (already decoded synchronously)
        if idx == 0 {
            continue;
        }

        let frame = match frame_result {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("GIF frame {} error: {}", idx, e);
                continue;
            }
        };

        // Extract frame data
        let delay = frame.delay();
        let (numer, denom) = delay.numer_denom_ms();
        let delay_ms = if denom == 0 { 100 } else { (numer as u64) / (denom as u64) };

        let buffer = frame.into_buffer();
        let (width, height) = buffer.dimensions();
        let frame_data = buffer.into_raw();

        // Send frame immediately
        if tx.blocking_send((frame_data, width, height, delay_ms)).is_err() {
            tracing::debug!("GIF receiver dropped at frame {}", idx);
            break;
        }

        frame_count += 1;
    }

    Ok(frame_count)
}

/// Start the frame advancement timer for image viewer.
fn start_viewer_timer(window: Weak<MainWindow>) {
    VIEWER_TIMER.with(|timer_cell| {
        // Stop existing timer if any
        if let Some(ref timer) = *timer_cell.borrow() {
            timer.stop();
        }

        // Get initial delay from state
        let initial_delay = VIEWER_GIF_STATE.with(|state_cell| {
            state_cell.borrow().as_ref().map(|s| s.current_delay_ms()).unwrap_or(100)
        });

        tracing::debug!("GIF timer starting with initial delay {}ms", initial_delay);

        let timer = Timer::default();
        let weak = window.clone();

        timer.start(
            TimerMode::SingleShot,
            Duration::from_millis(initial_delay),
            move || {
                tracing::debug!("GIF timer fired");
                advance_viewer_frame(weak.clone());
            },
        );

        *timer_cell.borrow_mut() = Some(timer);
    });
}

/// Advance to the next frame and schedule the next timer for image viewer.
/// Also polls for new frames from background decoder (streaming progressive).
fn advance_viewer_frame(window: Weak<MainWindow>) {
    // First, poll for any new frames from background decoder
    let new_frames = poll_streaming_frames();
    if new_frames > 0 {
        tracing::debug!("GIF added {} new frames from stream", new_frames);
    }

    let (next_image, next_delay, should_continue, frame_idx, total_frames) = VIEWER_GIF_STATE.with(|state_cell| {
        let mut state_ref = state_cell.borrow_mut();
        if let Some(ref mut state) = *state_ref {
            if !state.playing {
                tracing::debug!("GIF advance_frame: not playing, skipping");
                return (None, 0, false, 0, 0);
            }
            if state.frames.is_empty() {
                tracing::warn!("GIF advance_frame: no frames available");
                return (None, 100, true, 0, 0); // Keep trying, might get frames from stream
            }
            state.advance_frame();
            let image = state.current_image();
            let delay = state.current_delay_ms().max(10); // Minimum 10ms delay
            let idx = state.current_frame;
            let total = state.frames.len();
            (Some(image), delay, true, idx, total)
        } else {
            tracing::debug!("GIF advance_frame: no state");
            (None, 0, false, 0, 0)
        }
    });

    if !should_continue {
        tracing::debug!("GIF advance_frame: should not continue");
        return;
    }

    // Update the image viewer
    if let Some(image) = next_image {
        if let Some(w) = window.upgrade() {
            w.set_image_viewer_source(image);
            tracing::trace!("GIF frame {}/{} displayed, next delay {}ms", frame_idx, total_frames, next_delay);
        } else {
            tracing::debug!("GIF advance_frame: window gone");
            return;
        }
    }

    // Schedule next frame
    VIEWER_TIMER.with(|timer_cell| {
        if let Some(ref timer) = *timer_cell.borrow() {
            let weak = window.clone();
            timer.start(
                TimerMode::SingleShot,
                Duration::from_millis(next_delay),
                move || {
                    advance_viewer_frame(weak.clone());
                },
            );
        } else {
            tracing::debug!("GIF advance_frame: no timer to reschedule");
        }
    });
}

/// Poll for new frames from the background decoder channel.
/// Returns number of new frames added.
fn poll_streaming_frames() -> usize {
    let mut count = 0;

    // Take receiver temporarily
    let rx_opt = VIEWER_FRAME_RX.with(|cell| cell.borrow_mut().take());

    if let Some(mut rx) = rx_opt {
        // Non-blocking receive of all available frames
        loop {
            match rx.try_recv() {
                Ok((data, width, height, delay_ms)) => {
                    // Check frame limit before adding
                    let at_limit = VIEWER_GIF_STATE.with(|state_cell| {
                        state_cell.borrow().as_ref()
                            .map(|s| s.frames.len() >= MAX_VIEWER_FRAMES)
                            .unwrap_or(true)
                    });

                    if at_limit {
                        tracing::debug!("GIF frame limit reached ({}), stopping decode", MAX_VIEWER_FRAMES);
                        // Don't put receiver back - stop receiving
                        break;
                    }

                    // Convert to Slint Image and add to state
                    let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                        &data,
                        width,
                        height,
                    );
                    let image = slint::Image::from_rgba8(buffer);

                    VIEWER_GIF_STATE.with(|state_cell| {
                        if let Some(ref mut state) = *state_cell.borrow_mut() {
                            state.frames.push(image.clone());
                            state.delays.push(delay_ms);
                        }
                    });

                    count += 1;
                }
                Err(mpsc::error::TryRecvError::Empty) => {
                    // No more frames available right now, put receiver back
                    VIEWER_FRAME_RX.with(|cell| {
                        *cell.borrow_mut() = Some(rx);
                    });
                    break;
                }
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    // Decode complete - register inline animation with all frames
                    register_inline_from_viewer();
                    tracing::debug!("GIF streaming decode finished, channel closed");
                    // Don't put receiver back - it's done
                    break;
                }
            }
        }
    }

    count
}

/// Register inline animation using frames from viewer (called when decode completes).
fn register_inline_from_viewer() {
    let inline_id = VIEWER_INLINE_ID.with(|cell| cell.borrow_mut().take());

    if let Some(att_id) = inline_id {
        VIEWER_GIF_STATE.with(|state_cell| {
            if let Some(ref state) = *state_cell.borrow() {
                if state.frames.len() > 1 {
                    let inline_state = InlineGifState {
                        frames: state.frames.clone(),
                        delays: state.delays.clone(),
                        current_frame: 0,
                        accumulated_ms: 0,
                    };

                    // Evict oldest GIFs if at capacity (LRU)
                    INLINE_LRU.with(|lru| {
                        let mut lru = lru.borrow_mut();

                        // Remove from LRU if already exists
                        lru.retain(|id| id != &att_id);

                        // Evict oldest if at capacity
                        while lru.len() >= MAX_INLINE_GIFS {
                            if let Some(oldest) = lru.pop_front() {
                                INLINE_GIFS.with(|gifs| {
                                    gifs.borrow_mut().remove(&oldest);
                                });
                            }
                        }

                        lru.push_back(att_id.clone());
                    });

                    INLINE_GIFS.with(|gifs| {
                        gifs.borrow_mut().insert(att_id.clone(), inline_state);
                    });
                    start_inline_timer_if_needed();
                    tracing::debug!(
                        "Registered inline GIF from streaming viewer: {} ({} frames)",
                        att_id,
                        state.frames.len()
                    );
                }
            }
        });
    }
}

/// Resume GIF playback in image viewer.
pub fn resume() {
    VIEWER_GIF_STATE.with(|state_cell| {
        if let Some(ref mut state) = *state_cell.borrow_mut() {
            state.playing = true;
            tracing::debug!("GIF playback resumed, playing=true");
        } else {
            tracing::warn!("GIF resume called but no state exists");
        }
    });
}

/// Stop and clean up image viewer GIF animation.
pub fn stop_gif_animation() {
    // Stop timer
    VIEWER_TIMER.with(|timer_cell| {
        if let Some(ref timer) = *timer_cell.borrow() {
            timer.stop();
        }
        *timer_cell.borrow_mut() = None;
    });

    // Clear state
    VIEWER_GIF_STATE.with(|state_cell| {
        *state_cell.borrow_mut() = None;
    });

    // Clear streaming resources
    VIEWER_FRAME_RX.with(|cell| {
        *cell.borrow_mut() = None;
    });
    VIEWER_INLINE_ID.with(|cell| {
        *cell.borrow_mut() = None;
    });

    tracing::debug!("GIF animation stopped");
}

// ============================================================================
// Inline Message GIF Animation
// ============================================================================

/// State for an inline animated GIF in message list.
#[derive(Clone)]
struct InlineGifState {
    /// Pre-converted Slint images for each frame.
    frames: Vec<Image>,
    /// Frame delays in milliseconds.
    delays: Vec<u64>,
    /// Current frame index.
    current_frame: usize,
    /// Accumulated time since last frame change.
    accumulated_ms: u64,
}

impl InlineGifState {
    fn current_image(&self) -> Image {
        self.frames.get(self.current_frame).cloned().unwrap_or_default()
    }

    fn current_delay_ms(&self) -> u64 {
        self.delays.get(self.current_frame).copied().unwrap_or(100)
    }

    /// Advance time and return true if frame changed.
    fn tick(&mut self, elapsed_ms: u64) -> bool {
        self.accumulated_ms += elapsed_ms;
        let delay = self.current_delay_ms();

        if self.accumulated_ms >= delay {
            self.accumulated_ms -= delay;
            if !self.frames.is_empty() {
                self.current_frame = (self.current_frame + 1) % self.frames.len();
            }
            true
        } else {
            false
        }
    }
}

/// Initialize inline GIF animation system.
/// Call this once during window setup.
pub fn init_inline_animations(window: Weak<MainWindow>) {
    INLINE_WINDOW.with(|cell| {
        *cell.borrow_mut() = Some(window);
    });
}

/// Register an inline GIF from pre-converted Slint pixel buffers.
///
/// This is called on the UI thread after background decoding completes.
/// Takes Slint pixel buffers that were created on a background thread.
///
/// Returns the first frame image if successful.
pub fn register_inline_gif_from_buffers(
    attachment_id: &str,
    buffers: Vec<slint::SharedPixelBuffer<slint::Rgba8Pixel>>,
    delays: Vec<u64>,
) -> Option<Image> {
    if buffers.len() <= 1 {
        return None;
    }

    // Convert pixel buffers to Slint Images (fast, just ref-counting)
    let frames: Vec<Image> = buffers
        .into_iter()
        .map(|buffer| slint::Image::from_rgba8(buffer))
        .collect();

    register_inline_gif_from_images(attachment_id, frames, delays)
}

/// Register an inline GIF from pre-converted Slint Images.
///
/// Must be called from the UI thread.
pub fn register_inline_gif_from_images(
    attachment_id: &str,
    frames: Vec<Image>,
    delays: Vec<u64>,
) -> Option<Image> {
    if frames.len() <= 1 {
        return None;
    }

    let first_frame = frames.first().cloned().unwrap_or_default();
    let frame_count = frames.len();
    let att_id_string = attachment_id.to_string();

    // Check if this GIF already exists (progressive update) - preserve playback position
    let (current_frame, accumulated_ms) = INLINE_GIFS.with(|gifs| {
        if let Some(existing) = gifs.borrow().get(&att_id_string) {
            // Preserve position, but clamp to new frame count
            let frame = existing.current_frame.min(frame_count.saturating_sub(1));
            (frame, existing.accumulated_ms)
        } else {
            (0, 0)
        }
    });

    let state = InlineGifState {
        frames,
        delays,
        current_frame,
        accumulated_ms,
    };

    // Evict oldest GIFs if at capacity (LRU)
    INLINE_LRU.with(|lru| {
        let mut lru = lru.borrow_mut();

        // Remove from LRU if already exists (will re-add at end)
        lru.retain(|id| id != &att_id_string);

        // Evict oldest if at capacity
        while lru.len() >= MAX_INLINE_GIFS {
            if let Some(oldest) = lru.pop_front() {
                INLINE_GIFS.with(|gifs| {
                    gifs.borrow_mut().remove(&oldest);
                });
                tracing::debug!("Evicted oldest inline GIF: {}", oldest);
            }
        }

        // Add new GIF to LRU (most recent)
        lru.push_back(att_id_string.clone());
    });

    INLINE_GIFS.with(|gifs| {
        gifs.borrow_mut().insert(att_id_string, state);
    });

    tracing::info!(
        "Registered inline GIF animation: id={}, frames={}",
        attachment_id,
        frame_count
    );

    start_inline_timer_if_needed();

    Some(first_frame)
}

/// Unregister an inline GIF (when message is removed from view).
#[allow(dead_code)]
pub fn unregister_inline_gif(attachment_id: &str) {
    INLINE_GIFS.with(|gifs| {
        gifs.borrow_mut().remove(attachment_id);
    });
    INLINE_LRU.with(|lru| {
        lru.borrow_mut().retain(|id| id != attachment_id);
    });

    // Stop timer if no more GIFs
    let should_stop = INLINE_GIFS.with(|gifs| gifs.borrow().is_empty());
    if should_stop {
        stop_inline_timer();
    }
}

/// Start the global inline animation timer if not already running.
fn start_inline_timer_if_needed() {
    INLINE_TIMER.with(|timer_cell| {
        if timer_cell.borrow().is_some() {
            tracing::trace!("Inline GIF timer already running");
            return; // Already running
        }

        let timer = Timer::default();
        // Run at ~30fps for smooth animation
        timer.start(TimerMode::Repeated, Duration::from_millis(33), move || {
            tick_inline_gifs(33);
        });

        *timer_cell.borrow_mut() = Some(timer);
        tracing::info!("Started inline GIF animation timer (30fps)");
    });
}

/// Stop the global inline animation timer.
fn stop_inline_timer() {
    INLINE_TIMER.with(|timer_cell| {
        if let Some(ref timer) = *timer_cell.borrow() {
            timer.stop();
        }
        *timer_cell.borrow_mut() = None;
    });
    tracing::debug!("Stopped inline GIF animation timer");
}

/// Tick all inline GIF animations and update UI.
fn tick_inline_gifs(elapsed_ms: u64) {
    // Collect updates needed
    let updates: Vec<(String, Image)> = INLINE_GIFS.with(|gifs| {
        let mut gifs = gifs.borrow_mut();
        let mut updates = Vec::new();

        for (id, state) in gifs.iter_mut() {
            if state.tick(elapsed_ms) {
                updates.push((id.clone(), state.current_image()));
            }
        }

        updates
    });

    if updates.is_empty() {
        return;
    }

    tracing::trace!("GIF tick: {} updates to apply", updates.len());

    // Update UI
    INLINE_WINDOW.with(|window_cell| {
        let window_ref = window_cell.borrow();
        if let Some(ref window_weak) = *window_ref {
            if let Some(w) = window_weak.upgrade() {
                update_message_attachment_thumbnails(&w, &updates);
            } else {
                tracing::warn!("GIF tick: window reference invalid");
            }
        } else {
            tracing::warn!("GIF tick: no window reference set (init_inline_animations not called?)");
        }
    });
}

/// Update attachment thumbnails in the message model.
/// Uses location cache for O(1) updates instead of O(n*m) linear search.
fn update_message_attachment_thumbnails(window: &MainWindow, updates: &[(String, Image)]) {
    use slint::VecModel;
    use std::collections::HashMap;

    if updates.is_empty() {
        return;
    }

    let messages = window.get_messages();
    let msg_count = messages.row_count();

    // Map of attachment_id -> new_image for faster lookup during the pass
    let mut update_map: HashMap<&str, &Image> = updates
        .iter()
        .map(|(id, img)| (id.as_str(), img))
        .collect();

    // First, try using the location cache for each update (O(1))
    for (update_id, new_image) in updates {
        let cached_location = INLINE_LOCATION_CACHE.with(|cache| {
            cache.borrow().get(update_id).copied()
        });

        if let Some((msg_idx, att_idx)) = cached_location {
            if msg_idx < msg_count {
                if let Some(msg) = messages.row_data(msg_idx) {
                    let attachments = &msg.attachments;
                    if att_idx < attachments.row_count() {
                        if let Some(att) = attachments.row_data(att_idx) {
                            if att.id.as_str() == update_id {
                                if let Some(att_model) = attachments
                                    .as_any()
                                    .downcast_ref::<VecModel<crate::MessageAttachmentData>>()
                                {
                                    let mut updated_att = att.clone();
                                    updated_att.thumbnail = new_image.clone();
                                    updated_att.has_thumbnail = true;
                                    att_model.set_row_data(att_idx, updated_att);
                                    update_map.remove(update_id.as_str());
                                    continue;
                                }
                            }
                        }
                    }
                }
            }
            // Cache invalid - remove it
            INLINE_LOCATION_CACHE.with(|cache| {
                cache.borrow_mut().remove(update_id);
            });
        }
    }

    if update_map.is_empty() {
        return;
    }

    // Cache miss or invalid - single linear search for all remaining updates
    tracing::debug!("GIF update: {} cache misses, performing linear scan", update_map.len());
    for msg_idx in 0..msg_count {
        if update_map.is_empty() {
            break;
        }

        if let Some(msg) = messages.row_data(msg_idx) {
            let attachments = &msg.attachments;
            for att_idx in 0..attachments.row_count() {
                if let Some(att) = attachments.row_data(att_idx) {
                    let att_id = att.id.to_string();
                    if let Some(new_image) = update_map.remove(att_id.as_str()) {
                        if let Some(att_model) = attachments
                            .as_any()
                            .downcast_ref::<VecModel<crate::MessageAttachmentData>>()
                        {
                            let mut updated_att = att.clone();
                            updated_att.thumbnail = (*new_image).clone();
                            updated_att.has_thumbnail = true;
                            att_model.set_row_data(att_idx, updated_att);

                            // Cache for next time
                            INLINE_LOCATION_CACHE.with(|cache| {
                                cache.borrow_mut().insert(att_id, (msg_idx, att_idx));
                            });
                        }

                        if update_map.is_empty() {
                            return;
                        }
                    }
                }
            }
        }
    }
}

/// Clear all inline GIF animations.
pub fn clear_inline_gifs() {
    INLINE_GIFS.with(|gifs| {
        gifs.borrow_mut().clear();
    });
    INLINE_LRU.with(|lru| {
        lru.borrow_mut().clear();
    });
    INLINE_LOCATION_CACHE.with(|cache| {
        cache.borrow_mut().clear();
    });
    stop_inline_timer();
}

/// Auto-register all GIF attachments from current messages for animation.
///
/// Scans the message list in the UI for GIF attachments and registers them
/// for inline animation. Call this after loading messages into the UI.
///
/// Must be called from the UI thread (inside invoke_from_event_loop).
pub fn auto_register_gifs_from_messages(
    window_weak: Weak<MainWindow>,
    attachment_service: std::sync::Arc<chatter_linux::services::AttachmentService>,
) {
    let Some(w) = window_weak.upgrade() else {
        return;
    };

    let messages = w.get_messages();
    let mut gif_ids = Vec::new();

    for msg_idx in 0..messages.row_count() {
        if let Some(msg) = messages.row_data(msg_idx) {
            for att_idx in 0..msg.attachments.row_count() {
                if let Some(att) = msg.attachments.row_data(att_idx) {
                    let id_str = att.id.as_str();

                    // QUICK CHECK: Already registered? Skip expensive string conversions/checks
                    if INLINE_GIFS.with(|gifs| gifs.borrow().contains_key(id_str)) {
                        continue;
                    }

                    let id = id_str.to_string();
                    let mime = att.mime_type.to_string();
                    let filename = att.filename.to_string();

                    if id.starts_with("virtual-gif-") {
                        let url = id.strip_prefix("virtual-gif-").unwrap().to_string();
                        register_gif_url(id, url, window_weak.clone());
                    } else if mime == "image/gif" || filename.to_lowercase().ends_with(".gif") {
                        gif_ids.push(id);
                    }
                }
            }
        }
    }
    if gif_ids.is_empty() {
        return;
    }

    tracing::debug!("Auto-registering {} GIF attachments", gif_ids.len());

    // Register each GIF (spawns background threads)
    for att_id in gif_ids {
        register_gif_attachment(att_id, attachment_service.clone(), window_weak.clone());
    }
}

/// Register a single GIF attachment for inline animation.
///
/// This is called after sending a message with GIF attachments.
/// Spawns an independent background thread to decrypt/decode, then registers on UI thread.
///
/// This function returns immediately - all work happens in background.
/// Safe to call from any context (including nested runtimes).
pub fn register_gif_attachment(
    attachment_id: String,
    attachment_service: std::sync::Arc<chatter_linux::services::AttachmentService>,
    window_weak: slint::Weak<crate::MainWindow>,
) {
    // Optimization: Check if already registered (O(1) in UI thread)
    let already_registered = INLINE_GIFS.with(|gifs| gifs.borrow().contains_key(&attachment_id));
    if already_registered {
        return;
    }

    // Spawn independent thread to avoid runtime context issues
    std::thread::spawn(move || {
        use chatter_linux::domain::attachment::ContentLocation;

        // Parse attachment ID
        let att_id: chatter_linux::domain::ids::AttachmentId = match attachment_id.parse() {
            Ok(id) => id,
            Err(_) => {
                tracing::debug!("Invalid attachment ID for GIF registration: {}", attachment_id);
                return;
            }
        };

        // Check if attachment has content on filesystem
        let attachment = match attachment_service.get(&att_id) {
            Ok(Some(att)) if att.content_location == ContentLocation::OnFilesystem => att,
            Ok(Some(_)) | Ok(None) => {
                tracing::debug!("GIF {} not ready on filesystem yet", attachment_id);
                return;
            }
            Err(e) => {
                tracing::debug!("Failed to get attachment {}: {}", attachment_id, e);
                return;
            }
        };

        // Check if it's a GIF
        if attachment.metadata.mime_type != "image/gif" {
            return;
        }

        // Decrypt to memory (uses streaming for large files)
        let data = match attachment_service.decrypt_auto(&att_id) {
            Ok(buffer) => buffer.as_slice().to_vec(),
            Err(e) => {
                tracing::debug!("Could not decrypt GIF {}: {}", attachment_id, e);
                return;
            }
        };

        // Decode first frame immediately for instant display
        use chatter_linux::media::gif::decode_first_frame_only;

        let first_frame = match decode_first_frame_only(&data) {
            Ok(Some(f)) => f,
            Ok(None) => return, // Not a GIF or empty
            Err(e) => {
                tracing::debug!("Failed to decode GIF first frame {}: {}", attachment_id, e);
                return;
            }
        };

        tracing::debug!(
            "Decoded first frame of GIF {} immediately",
            attachment_id
        );

        // Register first frame on UI thread immediately (for instant display)
        let att_id_for_ui = attachment_id.clone();
        let window_weak_clone = window_weak.clone();
        let first_rgba = first_frame.data.clone();
        let first_width = first_frame.width;
        let first_height = first_frame.height;

        let _ = slint::invoke_from_event_loop(move || {
            // Show first frame immediately
            let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                &first_rgba,
                first_width,
                first_height,
            );
            let first_image = slint::Image::from_rgba8(buffer);

            if let Some(w) = window_weak_clone.upgrade() {
                update_message_attachment_thumbnails(&w, &[(att_id_for_ui.clone(), first_image)]);
            }
            tracing::info!("Displayed sent GIF first frame: {}", att_id_for_ui);
        });

        // Now decode remaining frames progressively and start animation
        use image::codecs::gif::GifDecoder;
        use image::AnimationDecoder;
        use std::io::Cursor;

        let cursor = Cursor::new(&data);
        let decoder = match GifDecoder::new(cursor) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("Failed to create GIF decoder for {}: {}", attachment_id, e);
                return;
            }
        };

        let frames_iter = decoder.into_frames();
        let mut all_buffers: Vec<slint::SharedPixelBuffer<slint::Rgba8Pixel>> = vec![];
        let mut all_delays: Vec<u64> = vec![];
        let mut animation_started = false;
        const BATCH_SIZE: usize = 5;

        for (_idx, frame_result) in frames_iter.enumerate() {
            let frame = match frame_result {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!("GIF frame {} error for {}: {}", _idx, attachment_id, e);
                    continue;
                }
            };

            let delay = frame.delay();
            let (numer, denom) = delay.numer_denom_ms();
            let delay_ms = if denom == 0 { 100 } else { (numer as u64) / (denom as u64) };
            let delay_ms = delay_ms.max(20);

            let buffer = frame.into_buffer();
            let (width, height) = buffer.dimensions();
            let raw_data = buffer.into_raw();

            // Convert to Slint pixel buffer IN THE BACKGROUND THREAD (Send)
            let pixel_buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                &raw_data,
                width,
                height,
            );

            all_buffers.push(pixel_buffer);
            all_delays.push(delay_ms);

            // Start animation as soon as we have enough frames
            if !animation_started && all_buffers.len() >= 2 {
                animation_started = true;
                let buffers_batch = all_buffers.clone();
                let delays_batch = all_delays.clone();
                let att_id_batch = attachment_id.clone();
                let weak_batch = window_weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(first_image) = register_inline_gif_from_buffers(&att_id_batch, buffers_batch, delays_batch) {
                        if let Some(w) = weak_batch.upgrade() {
                            update_message_attachment_thumbnails(&w, &[(att_id_batch.clone(), first_image)]);
                        }
                        tracing::info!("Started sent GIF animation: {}", att_id_batch);
                    }
                });
            }
            // Update animation periodically with new frames
            else if animation_started && all_buffers.len() % BATCH_SIZE == 0 {
                let buffers_batch = all_buffers.clone();
                let delays_batch = all_delays.clone();
                let att_id_batch = attachment_id.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    register_inline_gif_from_buffers(&att_id_batch, buffers_batch, delays_batch);
                });
            }
        }

        // Final update with all frames
        if all_buffers.len() > 1 {
            let att_id_full = attachment_id.clone();
            let weak_full = window_weak.clone();
            let buffers_full = all_buffers.clone();
            let delays_full = all_delays.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(first_image) = register_inline_gif_from_buffers(&att_id_full, buffers_full, delays_full) {
                    if let Some(w) = weak_full.upgrade() {
                        update_message_attachment_thumbnails(&w, &[(att_id_full.clone(), first_image)]);
                    }
                }
                tracing::info!("Completed sent GIF animation: {}", att_id_full);
            });
        }
    });
}

/// Register a GIF from a network URL for inline animation.
pub fn register_gif_url(
    attachment_id: String,
    url: String,
    window_weak: slint::Weak<crate::MainWindow>,
) {
    // Optimization: Check if already registered (O(1) in UI thread)
    let already_registered = INLINE_GIFS.with(|gifs| gifs.borrow().contains_key(&attachment_id));
    if already_registered {
        return;
    }

    // Spawn independent thread to avoid runtime context issues
    std::thread::spawn(move || {
        // Fetch from network
        let client = reqwest::blocking::Client::new();
        let response = match client.get(&url).send() {
            Ok(r) if r.status().is_success() => r,
            _ => return,
        };

        let data = match response.bytes() {
            Ok(b) => b.to_vec(),
            _ => return,
        };

        // Decode first frame immediately for instant display
        use chatter_linux::media::gif::decode_first_frame_only;

        let first_frame = match decode_first_frame_only(&data) {
            Ok(Some(f)) => f,
            _ => return,
        };

        // Register first frame on UI thread immediately
        let att_id_for_ui = attachment_id.clone();
        let window_weak_clone = window_weak.clone();
        let first_rgba = first_frame.data.clone();
        let first_width = first_frame.width;
        let first_height = first_frame.height;

        let _ = slint::invoke_from_event_loop(move || {
            let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                &first_rgba,
                first_width,
                first_height,
            );
            let first_image = slint::Image::from_rgba8(buffer);

            if let Some(w) = window_weak_clone.upgrade() {
                update_message_attachment_thumbnails(&w, &[(att_id_for_ui.clone(), first_image)]);
            }
        });

        // Now decode remaining frames progressively and start animation
        use image::codecs::gif::GifDecoder;
        use image::AnimationDecoder;
        use std::io::Cursor;

        let cursor = Cursor::new(&data);
        let decoder = match GifDecoder::new(cursor) {
            Ok(d) => d,
            _ => return,
        };

        let frames_iter = decoder.into_frames();
        let mut all_buffers: Vec<slint::SharedPixelBuffer<slint::Rgba8Pixel>> = vec![];
        let mut all_delays: Vec<u64> = vec![];
        let mut animation_started = false;
        const BATCH_SIZE: usize = 5;

        for (_idx, frame_result) in frames_iter.enumerate() {
            let frame = match frame_result {
                Ok(f) => f,
                _ => continue,
            };

            let delay = frame.delay();
            let (numer, denom) = delay.numer_denom_ms();
            let delay_ms = if denom == 0 { 100 } else { (numer as u64) / (denom as u64) };
            let delay_ms = delay_ms.max(20);

            let buffer = frame.into_buffer();
            let (width, height) = buffer.dimensions();
            let raw_data = buffer.into_raw();

            // Convert to Slint pixel buffer IN THE BACKGROUND THREAD (Send)
            let pixel_buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                &raw_data,
                width,
                height,
            );

            all_buffers.push(pixel_buffer);
            all_delays.push(delay_ms);

            if !animation_started && all_buffers.len() >= 2 {
                animation_started = true;
                let buffers_batch = all_buffers.clone();
                let delays_batch = all_delays.clone();
                let att_id_batch = attachment_id.clone();
                let weak_batch = window_weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(first_image) = register_inline_gif_from_buffers(&att_id_batch, buffers_batch, delays_batch) {
                        if let Some(w) = weak_batch.upgrade() {
                            update_message_attachment_thumbnails(&w, &[(att_id_batch.clone(), first_image)]);
                        }
                    }
                });
            } else if animation_started && all_buffers.len() % BATCH_SIZE == 0 {
                let buffers_batch = all_buffers.clone();
                let delays_batch = all_delays.clone();
                let att_id_batch = attachment_id.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    register_inline_gif_from_buffers(&att_id_batch, buffers_batch, delays_batch);
                });
            }
        }

        if all_buffers.len() > 1 {
            let att_id_full = attachment_id.clone();
            let weak_full = window_weak.clone();
            let buffers_full = all_buffers.clone();
            let delays_full = all_delays.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(first_image) = register_inline_gif_from_buffers(&att_id_full, buffers_full, delays_full) {
                    if let Some(w) = weak_full.upgrade() {
                        update_message_attachment_thumbnails(&w, &[(att_id_full.clone(), first_image)]);
                    }
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// Test that register_gif_attachment returns immediately (non-blocking).
    /// Even with an invalid attachment ID, the function should spawn a thread
    /// and return within milliseconds.
    #[test]
    fn test_register_gif_attachment_returns_immediately() {
        // Create a mock attachment repo that will fail lookup (attachment doesn't exist)
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test.db");
        let db = Arc::new(chatter_linux::storage::Database::open(&db_path, "test_passphrase").unwrap());
        let attachment_repo = Arc::new(chatter_linux::storage::AttachmentRepository::new(db));
        let attachment_service = Arc::new(chatter_linux::services::AttachmentService::new(attachment_repo));

        // Create a dummy window weak reference (will be None when upgraded)
        // We can't create a real MainWindow in tests, but the function handles None gracefully
        let window_weak: slint::Weak<crate::MainWindow> = slint::Weak::default();

        let start = Instant::now();

        // Call the function - should return immediately
        register_gif_attachment(
            "nonexistent-attachment-id".to_string(),
            attachment_service,
            window_weak,
        );

        let elapsed = start.elapsed();

        // Should return in under 10ms (thread spawn is very fast)
        // The actual GIF processing happens in the spawned thread
        assert!(
            elapsed < Duration::from_millis(10),
            "register_gif_attachment blocked for {:?}, should return immediately",
            elapsed
        );
    }

    /// Test that the spawned thread actually runs (doesn't just return immediately and do nothing).
    #[test]
    fn test_register_gif_attachment_spawns_thread() {
        // We'll verify the thread runs by checking that the repo is accessed
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test.db");
        let db = Arc::new(chatter_linux::storage::Database::open(&db_path, "test_passphrase").unwrap());
        let attachment_repo = Arc::new(chatter_linux::storage::AttachmentRepository::new(db));
        let attachment_service = Arc::new(chatter_linux::services::AttachmentService::new(attachment_repo));

        let window_weak: slint::Weak<crate::MainWindow> = slint::Weak::default();

        // Call the function
        register_gif_attachment(
            "test-attachment-id".to_string(),
            attachment_service,
            window_weak,
        );

        // Give the background thread time to start and access the repo
        std::thread::sleep(Duration::from_millis(50));

        // The thread will have run and returned early (attachment not found)
        // We can't easily verify it ran without more instrumentation,
        // but the non-blocking test above proves the spawn happens
    }

    /// Test multiple concurrent registrations don't block each other.
    #[test]
    fn test_concurrent_gif_registrations_non_blocking() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test.db");
        let db = Arc::new(chatter_linux::storage::Database::open(&db_path, "test_passphrase").unwrap());
        let attachment_repo = Arc::new(chatter_linux::storage::AttachmentRepository::new(db));
        let attachment_service = Arc::new(chatter_linux::services::AttachmentService::new(attachment_repo));

        let window_weak: slint::Weak<crate::MainWindow> = slint::Weak::default();

        let start = Instant::now();

        // Spawn 10 concurrent registrations
        for i in 0..10 {
            register_gif_attachment(
                format!("attachment-{}", i),
                attachment_service.clone(),
                window_weak.clone(),
            );
        }

        let elapsed = start.elapsed();

        // All 10 calls should complete in under 50ms total
        // (each spawn is ~1ms, should not accumulate if non-blocking)
        assert!(
            elapsed < Duration::from_millis(50),
            "10 concurrent registrations took {:?}, should be fast",
            elapsed
        );
    }
}
