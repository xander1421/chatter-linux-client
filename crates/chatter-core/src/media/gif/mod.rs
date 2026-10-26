//! GIF frame decode.

pub mod animator;
#[cfg(feature = "desktop-gstreamer")]
pub mod gstreamer_decoder;
pub mod progressive;

pub use animator::GifFrame;
pub use progressive::decode_first_frame_only;

#[cfg(feature = "desktop-gstreamer")]
pub use gstreamer_decoder::{decode_all_frames_cpu, decode_frames_cpu_bounded, is_gl_available, is_gstreamer_gif_available};

/// Whether a GPU GIF decode path is available on this build.
pub fn is_gpu_acceleration_available() -> bool {
    #[cfg(feature = "desktop-gstreamer")]
    {
        is_gstreamer_gif_available() && is_gl_available()
    }
    #[cfg(not(feature = "desktop-gstreamer"))]
    {
        false
    }
}

/// Memory budget for one decoded UI GIF animation.
pub const MAX_UI_GIF_BYTES: usize = 48 * 1024 * 1024;

/// Whether another RGBA frame fits the animation's frame and byte budget.
pub fn frame_fits_budget(
    frame_count: usize,
    used_bytes: usize,
    frame_bytes: usize,
    max_frames: usize,
    max_bytes: usize,
) -> bool {
    frame_count < max_frames && frame_bytes <= max_bytes.saturating_sub(used_bytes)
}

/// One retained frame and the time until the next retained frame.
pub struct DecimatedGifFrame<T> {
    pub frame: T,
    pub delay_ms: u64,
    pub bytes: usize,
    pub index: usize,
}

/// Bounded, temporally spread samples of a GIF loop.
pub struct DecimatedGif<T> {
    pub frames: Vec<DecimatedGifFrame<T>>,
    pub stride: usize,
    pub seen: usize,
    pub bytes: usize,
}

impl<T> Default for DecimatedGif<T> {
    fn default() -> Self {
        Self { frames: Vec::new(), stride: 1, seen: 0, bytes: 0 }
    }
}

/// Select a frame without changing the loop's total duration. When full, thin
/// existing samples and double the stride before considering later frames.
pub fn decimate_gif_frame<T>(
    mut gif: DecimatedGif<T>, frame: T, delay_ms: u64, bytes: usize,
    max_frames: usize, max_bytes: usize,
) -> DecimatedGif<T> {
    let index = gif.seen;
    gif.seen += 1;
    if gif.frames.is_empty() {
        if max_frames > 0 && bytes <= max_bytes {
            gif.bytes = bytes;
            gif.frames.push(DecimatedGifFrame { frame, delay_ms, bytes, index });
        }
        return gif;
    }

    if index % gif.stride != 0 {
        gif.frames.last_mut().unwrap().delay_ms += delay_ms;
        return gif;
    }

    while !frame_fits_budget(gif.frames.len(), gif.bytes, bytes, max_frames, max_bytes)
        && gif.frames.len() > 1
    {
        gif.stride = gif.stride.saturating_mul(2);
        let mut retained: Vec<DecimatedGifFrame<T>> = Vec::with_capacity((gif.frames.len() + 1) / 2);
        for sample in gif.frames.drain(..) {
            if sample.index % gif.stride == 0 {
                retained.push(sample);
            } else {
                retained.last_mut().unwrap().delay_ms += sample.delay_ms;
                gif.bytes -= sample.bytes;
            }
        }
        gif.frames = retained;
    }

    if index % gif.stride == 0
        && frame_fits_budget(gif.frames.len(), gif.bytes, bytes, max_frames, max_bytes)
    {
        gif.bytes += bytes;
        gif.frames.push(DecimatedGifFrame { frame, delay_ms, bytes, index });
    } else {
        gif.frames.last_mut().unwrap().delay_ms += delay_ms;
    }
    gif
}

#[cfg(test)]
mod budget_tests {
    use super::{decimate_gif_frame, frame_fits_budget, DecimatedGif};

    #[test]
    fn stops_before_crossing_either_budget() {
        assert!(frame_fits_budget(1, 20, 30, 2, 50));
        assert!(!frame_fits_budget(2, 20, 1, 2, 50));
        assert!(!frame_fits_budget(1, 20, 31, 2, 50));
        assert!(!frame_fits_budget(0, usize::MAX, 1, 2, 50));
    }

    #[test]
    fn decimation_spans_loop_and_preserves_duration() {
        let mut gif = DecimatedGif::default();
        for index in 0..120 {
            gif = decimate_gif_frame(gif, index, 50, 1, 10, 10);
        }
        assert!(gif.frames.len() <= 10);
        assert_eq!(gif.frames[0].frame, 0);
        assert_eq!(gif.frames.iter().map(|f| f.delay_ms).sum::<u64>(), 6000);
        assert!(gif.frames.last().unwrap().index >= 100);
    }
}
