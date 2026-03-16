//! Image viewer / lightbox GIF animation.
//!
//! Manages animated GIF playback in the full-screen image viewer.

use std::cell::RefCell;
use std::time::Duration;

use slint::{Image, Timer, TimerMode, Weak};

use chatter_linux::media::gif::{
    decode_first_frame_only,
    decode_gif_auto, is_gpu_acceleration_available,
};
use crate::bin_ui::slint_conversions::gif_frame_to_slint_image;
use tokio::sync::mpsc;

use crate::MainWindow;
use super::RawFrame;
use super::MAX_VIEWER_FRAMES;

thread_local! {
    /// Current GIF animation state for image viewer (must be on UI thread).
    static VIEWER_GIF_STATE: RefCell<Option<ViewerGifState>> = const { RefCell::new(None) };
    /// Animation timer for image viewer (must be on UI thread).
    static VIEWER_TIMER: RefCell<Option<Timer>> = const { RefCell::new(None) };
    /// Receiver for streaming frames from background decoder.
    static VIEWER_FRAME_RX: RefCell<Option<mpsc::Receiver<RawFrame>>> = const { RefCell::new(None) };
    /// Attachment ID for inline registration when decode completes.
    static VIEWER_INLINE_ID: RefCell<Option<String>> = const { RefCell::new(None) };
}

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

    let first_frame_image = gif_frame_to_slint_image(&first_frame);
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
                    let inline_state = super::inline::InlineGifState {
                        frames: state.frames.clone(),
                        delays: state.delays.clone(),
                        current_frame: 0,
                        accumulated_ms: 0,
                    };

                    super::inline::insert_inline_gif(att_id.clone(), inline_state);
                    super::inline::start_inline_timer_if_needed();
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

/// Try to start viewer animation from existing inline frames (no decryption needed).
///
/// Returns `Some(first_frame)` if inline frames were found and copied to viewer,
/// or `None` if this attachment isn't registered as an inline GIF.
///
/// This is an optimization: if a GIF is already animating in a message bubble,
/// we reuse those decoded frames instead of re-decrypting and re-decoding.
pub fn start_viewer_from_inline(
    attachment_id: &str,
    window: &Weak<MainWindow>,
) -> Option<Image> {
    // Check if we have inline frames for this attachment
    let inline_state = super::inline::get_inline_gif(attachment_id);

    let state = inline_state?;

    if state.frames.is_empty() {
        return None;
    }

    let first_frame = state.frames.first().cloned().unwrap_or_default();
    let frame_count = state.frames.len();

    tracing::info!(
        "Reusing {} inline GIF frames for viewer: {} (no decryption needed)",
        frame_count,
        attachment_id
    );

    // Copy frames to viewer state
    let viewer_state = ViewerGifState {
        frames: state.frames,
        delays: state.delays,
        current_frame: 0,
        playing: true,
    };

    VIEWER_GIF_STATE.with(|cell| {
        *cell.borrow_mut() = Some(viewer_state);
    });

    // Start viewer animation timer
    start_viewer_timer(window.clone());

    Some(first_frame)
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
