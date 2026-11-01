//! One decoded GIF frame.

use std::time::Duration;

/// A single frame from a GIF.
#[derive(Clone)]
pub struct GifFrame {
    /// RGBA pixel data.
    pub data: Vec<u8>,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// How long this frame should stay on screen.
    pub delay: Duration,
}

/// Browser-compatible GIF timing: unspecified and <=10 ms delays mean 100 ms.
pub fn normalize_delay_ms(delay: u64) -> u64 {
    if delay <= 10 {
        100
    } else {
        delay
    }
}

/// Advance a loop using elapsed monotonic time, including delayed timer callbacks.
/// Delays here are already normalized before temporal decimation.
pub fn advance_playback(
    delays: &[u64],
    frame: &mut usize,
    remainder: &mut u64,
    elapsed: u64,
) -> bool {
    if delays.is_empty() {
        return false;
    }
    *frame %= delays.len();
    let previous = *frame;
    let total = delays.iter().copied().map(|d| d.max(1)).sum::<u64>();
    *remainder = remainder.saturating_add(elapsed) % total;
    while *remainder >= delays[*frame].max(1) {
        *remainder -= delays[*frame].max(1);
        *frame = (*frame + 1) % delays.len();
    }
    *frame != previous
}

#[cfg(test)]
mod timing_tests {
    use super::*;
    #[test]
    fn short_delays_follow_browser_timing() {
        assert_eq!(
            [0, 1, 10, 11, 20, 250].map(normalize_delay_ms),
            [100, 100, 100, 11, 20, 250]
        );
    }
    #[test]
    fn elapsed_time_catches_up_and_loops_with_variable_delays() {
        let (mut frame, mut remainder) = (0, 0);
        assert!(!advance_playback(
            &[100, 200, 50],
            &mut frame,
            &mut remainder,
            99
        ));
        assert!(advance_playback(
            &[100, 200, 50],
            &mut frame,
            &mut remainder,
            206
        ));
        assert_eq!((frame, remainder), (2, 5));
        advance_playback(&[100, 200, 50], &mut frame, &mut remainder, 750);
        assert_eq!((frame, remainder), (0, 5));
    }
}
