//! GIF animation state.
//!
//! Manages animated GIF playback state for the UI thread.
//! Supports both the image viewer lightbox and inline message thumbnails.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Duration;

use slint::{Image, Model, Timer, TimerMode, Weak};

use chatter_linux::media::gif::{frame_to_slint_image, decode_first_frame_only};
use tokio::sync::mpsc;

use crate::MainWindow;

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
    /// Global animation timer for inline GIFs.
    static INLINE_TIMER: RefCell<Option<Timer>> = const { RefCell::new(None) };
    /// Window weak reference for inline GIF updates.
    static INLINE_WINDOW: RefCell<Option<Weak<MainWindow>>> = const { RefCell::new(None) };
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
/// Uses **streaming progressive** decoding:
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
        "GIF first frame decoded in {:?}, delay {}ms - starting animation immediately",
        start.elapsed(),
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
        use image::codecs::gif::GifDecoder;
        use image::AnimationDecoder;
        use std::io::Cursor;

        let cursor = Cursor::new(&data_vec);
        let decoder = match GifDecoder::new(cursor) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("Background GIF decode failed: {}", e);
                return;
            }
        };

        let frames_iter = decoder.into_frames();
        let mut frame_count = 0;
        let bg_start = std::time::Instant::now();

        for (idx, frame_result) in frames_iter.enumerate() {
            // Skip first frame (already decoded)
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
            let data = buffer.into_raw();

            // Send frame immediately - don't wait for all frames
            if tx.blocking_send((data, width, height, delay_ms)).is_err() {
                tracing::debug!("GIF receiver dropped at frame {}", idx);
                break;
            }

            frame_count += 1;
        }

        tracing::info!(
            "GIF streaming decode complete: {} frames sent in {:?}",
            frame_count,
            bg_start.elapsed()
        );
    });

    // Start animation timer immediately - it will poll for new frames
    start_viewer_timer(window.clone());

    // Return first frame immediately
    Some(first_frame_image)
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
            state.advance_frame();
            let image = state.current_image();
            let delay = state.current_delay_ms();
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

/// Register an inline GIF from pre-decoded raw frame data.
///
/// This is called on the UI thread after background decoding completes.
/// Takes raw RGBA frame data that will be converted to Slint Images.
///
/// Returns the first frame image if successful.
pub fn register_inline_gif_from_raw(
    attachment_id: &str,
    raw_frames: Vec<(Vec<u8>, u32, u32, u64)>,
) -> Option<Image> {
    if raw_frames.len() <= 1 {
        return None;
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

    let first_frame = frames.first().cloned().unwrap_or_default();
    let frame_count = frames.len();

    let state = InlineGifState {
        frames,
        delays,
        current_frame: 0,
        accumulated_ms: 0,
    };

    INLINE_GIFS.with(|gifs| {
        gifs.borrow_mut().insert(attachment_id.to_string(), state);
    });

    tracing::debug!(
        "Registered inline GIF animation from raw: {} ({} frames)",
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
            return; // Already running
        }

        let timer = Timer::default();
        // Run at ~30fps for smooth animation
        timer.start(TimerMode::Repeated, Duration::from_millis(33), move || {
            tick_inline_gifs(33);
        });

        *timer_cell.borrow_mut() = Some(timer);
        tracing::debug!("Started inline GIF animation timer");
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

    // Update UI
    INLINE_WINDOW.with(|window_cell| {
        let window_ref = window_cell.borrow();
        if let Some(ref window_weak) = *window_ref {
            if let Some(w) = window_weak.upgrade() {
                update_message_attachment_thumbnails(&w, &updates);
            }
        }
    });
}

/// Update attachment thumbnails in the message model.
fn update_message_attachment_thumbnails(window: &MainWindow, updates: &[(String, Image)]) {
    use slint::VecModel;

    let messages = window.get_messages();

    for msg_idx in 0..messages.row_count() {
        if let Some(msg) = messages.row_data(msg_idx) {
            let attachments = &msg.attachments;
            let mut any_changed = false;

            for att_idx in 0..attachments.row_count() {
                if let Some(att) = attachments.row_data(att_idx) {
                    // Check if this attachment needs an update
                    for (update_id, new_image) in updates {
                        if att.id.as_str() == update_id {
                            // Update the thumbnail
                            let mut updated_att = att.clone();
                            updated_att.thumbnail = new_image.clone();
                            updated_att.has_thumbnail = true;

                            // Update in the attachments model
                            if let Some(att_model) = attachments
                                .as_any()
                                .downcast_ref::<VecModel<crate::MessageAttachmentData>>()
                            {
                                att_model.set_row_data(att_idx, updated_att);
                                any_changed = true;
                            }
                            break;
                        }
                    }
                }
            }

            // If attachments changed, we might need to update the message too
            // (Slint should handle this automatically via model bindings)
            if any_changed {
                // The model update above should trigger UI refresh
            }
        }
    }
}

/// Clear all inline GIF animations.
pub fn clear_inline_gifs() {
    INLINE_GIFS.with(|gifs| {
        gifs.borrow_mut().clear();
    });
    stop_inline_timer();
}
