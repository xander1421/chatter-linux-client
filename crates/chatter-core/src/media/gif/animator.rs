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
