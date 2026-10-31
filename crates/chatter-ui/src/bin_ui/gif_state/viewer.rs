//! Viewer playback consumes complete background-decoded loops, never decode ticks.
use super::{MAX_VIEWER_FRAMES, VIEWER_ANIMATION_BYTES, VIEWER_FRAME_PX};
use crate::MainWindow;
use chatter_core::media::gif::{animator::advance_playback, decode_first_frame_only};
use slint::{ComponentHandle, Image, Timer, TimerMode, Weak};
use std::cell::RefCell;
use std::time::{Duration, Instant};

thread_local! {
    static STATE: RefCell<Option<super::inline::InlineGifState>> = const { RefCell::new(None) };
    static TIMER: RefCell<Option<Timer>> = const { RefCell::new(None) };
    // Identifies this opening, including close/reopen of the same attachment.
    static GENERATION: RefCell<u64> = const { RefCell::new(0) };
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
        frame.data,
        frame.width,
        frame.height,
        VIEWER_FRAME_PX,
        VIEWER_FRAME_PX,
    );
    if data.is_empty() {
        return None;
    }
    let pixels =
        slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&data, width, height);
    Some((pixels, frame.delay.as_millis() as u64))
}

/// Display the prepared first frame until the complete loop is ready.
pub fn start_gif_animation_and_inline(
    data: &[u8],
    window: &Weak<MainWindow>,
    attachment_id: &str,
    first_frame: Option<(slint::SharedPixelBuffer<slint::Rgba8Pixel>, u64)>,
) -> Option<Image> {
    let (pixels, _) = first_frame?;
    stop_gif_animation();
    let generation = GENERATION.with(|g| *g.borrow());
    let cache_generation = crate::bin_ui::decode_cache::thumbnail_generation();
    let first = Image::from_rgba8(pixels);
    let data = data.to_vec();
    let id = attachment_id.to_string();
    let weak = window.clone();
    let size = window.upgrade().map(|w| w.window().size());
    let width = size
        .map(|s| s.width.min(VIEWER_FRAME_PX))
        .unwrap_or(VIEWER_FRAME_PX);
    let height = size
        .map(|s| s.height.min(VIEWER_FRAME_PX))
        .unwrap_or(VIEWER_FRAME_PX);
    chatter_core::media::gif::workers::submit(
        format!("viewer:{id}:{generation}"),
        true,
        move || {
            let frames = match chatter_core::media::gif::progressive::decode_loop(
                &data,
                width,
                height,
                MAX_VIEWER_FRAMES,
                VIEWER_ANIMATION_BYTES,
            ) {
                Ok(frames) => frames,
                Err(e) => {
                    tracing::warn!("GIF viewer decode failed: {e}");
                    return;
                }
            };
            let mut buffers = Vec::new();
            let mut delays = Vec::new();
            let mut inline = chatter_core::media::gif::DecimatedGif::default();
            for frame in frames {
                let delay = frame.delay.as_millis() as u64;
                buffers.push(
                    slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                        &frame.data,
                        frame.width,
                        frame.height,
                    ),
                );
                delays.push(delay);
                let (data, width, height) = super::fit_frame(
                    frame.data,
                    frame.width,
                    frame.height,
                    super::INLINE_FRAME_PX,
                    super::INLINE_FRAME_PX,
                );
                let bytes = data.len();
                let pixels = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                    &data, width, height,
                );
                inline = chatter_core::media::gif::decimate_gif_frame(
                    inline,
                    pixels,
                    delay,
                    bytes,
                    MAX_VIEWER_FRAMES,
                    super::INLINE_ANIMATION_BYTES,
                );
            }
            let (inline_buffers, inline_delays): (Vec<_>, Vec<_>) = inline
                .frames
                .into_iter()
                .map(|sample| (sample.frame, sample.delay_ms))
                .unzip();
            crate::bin_ui::decode_cache::put_gif_guarded(
                &id,
                cache_generation,
                inline_buffers.clone(),
                inline_delays.clone(),
            );
            let _ = slint::invoke_from_event_loop(move || {
                if GENERATION.with(|g| *g.borrow() != generation) || cache_generation != crate::bin_ui::decode_cache::thumbnail_generation() {
                    return;
                }
                super::inline::register_inline_gif_from_buffers(&id, inline_buffers, inline_delays);
                let frames = buffers.into_iter().map(Image::from_rgba8).collect();
                STATE.with(|s| {
                    *s.borrow_mut() = Some(super::inline::InlineGifState {
                        frames,
                        delays,
                        current_frame: 0,
                        accumulated_ms: 0,
                    })
                });
                start_timer(weak);
            });
        },
    );
    Some(first)
}

fn start_timer(window: Weak<MainWindow>) {
    let timer = Timer::default();
    let mut last = Instant::now();
    timer.start(TimerMode::Repeated, Duration::from_millis(16), move || {
        let now = Instant::now();
        let elapsed = now.duration_since(last).as_millis() as u64;
        last += Duration::from_millis(elapsed);
        let image = STATE.with(|cell| {
            let mut state = cell.borrow_mut();
            let state = state.as_mut()?;
            advance_playback(
                &state.delays,
                &mut state.current_frame,
                &mut state.accumulated_ms,
                elapsed,
            )
            .then(|| state.current_image())
        });
        if let (Some(w), Some(image)) = (window.upgrade(), image) {
            w.global::<crate::ChatPane>().set_image_viewer_source(image);
        }
    });
    TIMER.with(|t| *t.borrow_mut() = Some(timer));
}

/// Playback starts automatically when a complete loop is installed.
/// Kept for attachment-view callbacks that also use the inline reuse path.
pub fn resume() {}

pub fn start_viewer_from_inline(attachment_id: &str, window: &Weak<MainWindow>) -> Option<Image> {
    let mut state = super::inline::get_inline_gif(attachment_id).or_else(|| {
        let cached = crate::bin_ui::decode_cache::gif(attachment_id)?;
        Some(super::inline::InlineGifState {
            frames: cached
                .frames
                .iter()
                .cloned()
                .map(Image::from_rgba8)
                .collect(),
            delays: cached.delays.to_vec(),
            current_frame: 0,
            accumulated_ms: 0,
        })
    })?;
    stop_gif_animation();
    state.current_frame = 0;
    state.accumulated_ms = 0;
    let first = state.current_image();
    STATE.with(|s| *s.borrow_mut() = Some(state));
    start_timer(window.clone());
    Some(first)
}

pub fn stop_gif_animation() {
    TIMER.with(|t| {
        t.borrow_mut().take();
    });
    STATE.with(|s| {
        s.borrow_mut().take();
    });
    GENERATION.with(|g| {
        let next = g.borrow().wrapping_add(1);
        *g.borrow_mut() = next;
    });
}
