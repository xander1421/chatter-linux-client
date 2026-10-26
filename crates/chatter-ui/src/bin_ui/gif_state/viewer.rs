//! Image viewer / lightbox GIF animation.
//!
//! Manages animated GIF playback in the full-screen image viewer.

use slint::ComponentHandle;
use std::cell::RefCell;
use std::time::Duration;

use slint::{Image, Timer, TimerMode, Weak};

use chatter_core::media::gif::decode_first_frame_only;
use tokio::sync::mpsc;

use super::{VIEWER_ANIMATION_BYTES, MAX_VIEWER_FRAMES, VIEWER_FRAME_PX};
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
    bytes: usize,
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
    let (data, width, height) = super::fit_frame(
        frame.data, frame.width, frame.height, VIEWER_FRAME_PX, VIEWER_FRAME_PX,
    );
    if data.is_empty() { return None; }
    let pixels = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&data, width, height);
    Some((pixels, frame.delay.as_millis() as u64))
}

/// Start GIF animation in viewer AND register for inline animation.
///
/// Uses bounded background decoding with the `image` crate.
///
/// For all backends:
/// 1. Receive a first frame prepared on the decrypt worker
/// 2. Start animation IMMEDIATELY (even with just 1 frame)
/// 3. Background decoder selects frames across the complete loop
/// 4. Selected frames are sent to the animation channel
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
    let gif_data = data.to_vec();
    let size = window.upgrade().map(|w| w.window().size());
    let max_width = size.map(|s| s.width.max(1).min(VIEWER_FRAME_PX)).unwrap_or(VIEWER_FRAME_PX);
    let max_height = size.map(|s| s.height.max(1).min(VIEWER_FRAME_PX)).unwrap_or(VIEWER_FRAME_PX);
    let (data, width, height) = super::fit_frame(
        pixels.as_bytes().to_vec(), pixels.width(), pixels.height(), max_width, max_height,
    );
    if data.is_empty() { return None; }
    let first_bytes = data.len();
    let pixels = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&data, width, height);
    let first_frame_image = Image::from_rgba8(pixels);

    tracing::info!(
        "GIF first frame ready, delay {}ms - starting animation immediately",
        first_delay
    );

    // Store initial state with just first frame - animation starts NOW
    let initial_state = ViewerGifState {
        frames: vec![first_frame_image.clone()],
        delays: vec![first_delay],
        bytes: first_bytes,
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

    // Spawn background decoder with the original GIF bytes.
    std::thread::spawn(move || {
        let bg_start = std::time::Instant::now();

        let decode_result = decode_gif_with_image_crate(&gif_data, &tx, max_width, max_height, first_delay);

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

/// Decode the complete loop before sending frames. A later budget crossing can
/// thin earlier samples, so no animation frames are uploaded until selection ends.
fn decode_gif_with_image_crate(
    data: &[u8], tx: &mpsc::Sender<RawFrame>, max_width: u32, max_height: u32,
    first_delay: u64,
) -> Result<usize, String> {
    use image::AnimationDecoder;
    use image::codecs::gif::GifDecoder as ImageGifDecoder;
    use std::io::Cursor;

    let decoder = ImageGifDecoder::new(Cursor::new(data))
        .map_err(|e| format!("Image crate GIF decode failed: {}", e))?;
    let mut selected = chatter_core::media::gif::DecimatedGif::default();
    for (idx, frame_result) in decoder.into_frames().enumerate() {
        let frame = match frame_result {
            Ok(frame) => frame,
            Err(e) => {
                tracing::warn!("GIF frame {} error: {}", idx, e);
                continue;
            }
        };
        let (numer, denom) = frame.delay().numer_denom_ms();
        let delay_ms = if denom == 0 { 100 } else { numer as u64 / denom as u64 };
        let buffer = frame.into_buffer();
        let (width, height) = buffer.dimensions();
        let (pixels, width, height) = super::fit_frame(
            buffer.into_raw(), width, height, max_width, max_height,
        );
        if pixels.is_empty() { continue; }
        let bytes = pixels.len();
        selected = chatter_core::media::gif::decimate_gif_frame(
            selected, (pixels, width, height), delay_ms, bytes,
            MAX_VIEWER_FRAMES, VIEWER_ANIMATION_BYTES,
        );
    }

    let mut samples = selected.frames.into_iter();
    let Some(first) = samples.next() else { return Ok(0) };
    let extra_first_delay = first.delay_ms.saturating_sub(first_delay);
    if extra_first_delay > 0
        && tx.blocking_send((Vec::new(), 0, 0, extra_first_delay)).is_err()
    {
        return Ok(0);
    }
    let mut sent = 0;
    for sample in samples {
        let (pixels, width, height) = sample.frame;
        if tx.blocking_send((pixels, width, height, sample.delay_ms)).is_err() {
            break;
        }
        sent += 1;
    }
    Ok(sent)
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
                    if data.is_empty() {
                        VIEWER_GIF_STATE.with(|cell| {
                            if let Some(last) = cell.borrow_mut().as_mut().and_then(|state| state.delays.last_mut()) {
                                *last = last.saturating_add(delay_ms);
                            }
                        });
                        continue;
                    }
                    // Check frame limit before adding
                    let at_limit = VIEWER_GIF_STATE.with(|state_cell| {
                        state_cell
                            .borrow()
                            .as_ref()
                            .map(|s| s.frames.len() >= MAX_VIEWER_FRAMES || data.len() > VIEWER_ANIMATION_BYTES.saturating_sub(s.bytes))
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
                            state.bytes += data.len();
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
                    let mut selected = chatter_core::media::gif::DecimatedGif::default();
                    for (frame, delay_ms) in state.frames.iter().zip(&state.delays) {
                        let Some(pixels) = frame.to_rgba8() else { return };
                        let (data, width, height) = super::fit_frame(
                            pixels.as_bytes().to_vec(), pixels.width(), pixels.height(),
                            super::INLINE_FRAME_PX, super::INLINE_FRAME_PX,
                        );
                        if data.is_empty() { return; }
                        let bytes = data.len();
                        let image = crate::bin_ui::slint_conversions::rgba_to_slint_image(
                            &data, width, height,
                        );
                        selected = chatter_core::media::gif::decimate_gif_frame(
                            selected, image, *delay_ms, bytes,
                            MAX_VIEWER_FRAMES, super::INLINE_ANIMATION_BYTES,
                        );
                    }
                    let (frames, delays): (Vec<_>, Vec<_>) = selected.frames.into_iter()
                        .map(|sample| (sample.frame, sample.delay_ms)).unzip();
                    let inline_state = super::inline::InlineGifState {
                        frames,
                        delays,
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
        bytes: state.bytes(),
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
