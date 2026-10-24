//! Image viewer / lightbox GIF animation.
//!
//! Manages animated GIF playback in the full-screen image viewer.

use slint::ComponentHandle;
use std::cell::RefCell;
use std::time::Duration;

use slint::{Image, Timer, TimerMode, Weak};

use chatter_core::media::gif::{decode_first_frame_only, MAX_UI_GIF_BYTES};
use tokio::sync::mpsc;

use super::MAX_VIEWER_FRAMES;
use super::RawFrame;
use crate::MainWindow;

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
        self.frames
            .get(self.current_frame)
            .cloned()
            .unwrap_or_default()
    }

    fn current_delay_ms(&self) -> u64 {
        self.delays.get(self.current_frame).copied().unwrap_or(100)
    }

    fn advance_frame(&mut self) {
        if !self.frames.is_empty() {
            self.current_frame = (self.current_frame + 1) % self.frames.len();
        }
    }
}

/// Decode the first frame and copy its pixels on a background thread.
pub fn prepare_first_gif_frame(
    data: &[u8],
) -> Option<(slint::SharedPixelBuffer<slint::Rgba8Pixel>, u64)> {
    let frame = match decode_first_frame_only(data) {
        Ok(Some(frame)) => frame,
        Ok(None) => return None,
        Err(e) => {
            tracing::warn!("Failed to decode GIF first frame: {:?}", e);
            return None;
        }
    };
    let pixels = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
        &frame.data, frame.width, frame.height,
    );
    Some((pixels, frame.delay.as_millis() as u64))
}

/// Start GIF animation in viewer AND register for inline animation.
///
/// Uses progressive decoding with a bounded GStreamer CPU path on desktop
/// and the `image` crate when GStreamer is unavailable.
///
/// For all backends:
/// 1. Receive a first frame prepared on the decrypt worker
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
    first_frame: Option<(slint::SharedPixelBuffer<slint::Rgba8Pixel>, u64)>,
) -> Option<Image> {
    let (pixels, first_delay) = first_frame?;
    let first_frame_image = Image::from_rgba8(pixels);

    tracing::info!(
        "GIF first frame ready, delay {}ms - starting animation immediately",
        first_delay
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
    let data_vec = data.to_vec();
    std::thread::spawn(move || {
        let bg_start = std::time::Instant::now();

        // Try GStreamer CPU decode, then the image crate.
        let decode_result = decode_gif_with_best_backend(&data_vec, &tx);

        match decode_result {
            Ok(frame_count) => {
                tracing::info!(
                    "GIF streaming decode complete: {} frames sent in {:?} (backend: {})",
                    frame_count,
                    bg_start.elapsed(),
                    "CPU"
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
/// Tries GStreamer CPU decode, then the pure Rust `image` crate.
fn decode_gif_with_best_backend(data: &[u8], tx: &mpsc::Sender<RawFrame>) -> Result<usize, String> {
    // GStreamer honors stride and stops at the frame and byte budget.
    // The bounded result is then sent to the animation channel.
    #[cfg(feature = "desktop-gstreamer")]
    {
        match chatter_core::media::gif::decode_frames_cpu_bounded(data, MAX_VIEWER_FRAMES, MAX_UI_GIF_BYTES) {
            Ok(frames) if frames.len() > 1 => {
                let mut sent = 0;
                // Skip frame 0 — already prepared on the decrypt worker.
                for frame in frames.into_iter().skip(1) {
                    if tx
                        .blocking_send((
                            frame.data,
                            frame.width,
                            frame.height,
                            frame.delay.as_millis() as u64,
                        ))
                        .is_err()
                    {
                        return Ok(sent);
                    }
                    sent += 1;
                }
                return Ok(sent);
            }
            Ok(_) => {} // 0/1 frame — fall through to image crate
            Err(e) => {
                tracing::debug!("CPU GStreamer decode failed ({}), image crate fallback", e);
            }
        }
    }

    decode_gif_with_image_crate(data, tx)
}

/// Fallback decoder using the `image` crate (pure Rust, no GPU).
fn decode_gif_with_image_crate(data: &[u8], tx: &mpsc::Sender<RawFrame>) -> Result<usize, String> {
    use image::AnimationDecoder;
    use image::codecs::gif::GifDecoder as ImageGifDecoder;
    use std::io::Cursor;

    let cursor = Cursor::new(data);
    let decoder = ImageGifDecoder::new(cursor)
        .map_err(|e| format!("Image crate GIF decode failed: {}", e))?;

    let frames_iter = decoder.into_frames();
    let mut frame_count = 0;
    let mut decoded_bytes = 0usize;

    for (idx, frame_result) in frames_iter.enumerate() {
        // Skip first frame (already prepared on the decrypt worker)
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
        let delay_ms = if denom == 0 {
            100
        } else {
            (numer as u64) / (denom as u64)
        };

        let buffer = frame.into_buffer();
        let (width, height) = buffer.dimensions();
        let frame_data = buffer.into_raw();

        if frame_count + 1 >= MAX_VIEWER_FRAMES
            || frame_data.len() > MAX_UI_GIF_BYTES.saturating_sub(decoded_bytes)
        {
            break;
        }
        decoded_bytes += frame_data.len();

        // Send frame immediately
        if tx
            .blocking_send((frame_data, width, height, delay_ms))
            .is_err()
        {
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
            state_cell
                .borrow()
                .as_ref()
                .map(|s| s.current_delay_ms())
                .unwrap_or(100)
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

    let (next_image, next_delay, should_continue, frame_idx, total_frames) =
        VIEWER_GIF_STATE.with(|state_cell| {
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
            w.global::<crate::ChatPane>().set_image_viewer_source(image);
            tracing::trace!(
                "GIF frame {}/{} displayed, next delay {}ms",
                frame_idx,
                total_frames,
                next_delay
            );
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
                        state_cell
                            .borrow()
                            .as_ref()
                            .map(|s| s.frames.len() >= MAX_VIEWER_FRAMES)
                            .unwrap_or(true)
                    });

                    if at_limit {
                        tracing::debug!(
                            "GIF frame limit reached ({}), stopping decode",
                            MAX_VIEWER_FRAMES
                        );
                        // Don't put receiver back - stop receiving
                        break;
                    }

                    // Convert to Slint Image and add to state
                    let image =
                        crate::bin_ui::slint_conversions::rgba_to_slint_image(&data, width, height);

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
pub fn start_viewer_from_inline(attachment_id: &str, window: &Weak<MainWindow>) -> Option<Image> {
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
