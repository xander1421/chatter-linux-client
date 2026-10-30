//! Platform-abstracted video decoder.
//!
//! GStreamer, with hardware acceleration, on Linux, Windows and macOS.
//!
//! Android playback and thumbnails use the platform player and thumbnail extractor
//! in chatter-ui; they do not use this desktop frame-decoding interface.

use std::path::Path;

use crate::error::MediaError;

#[cfg(all(
    feature = "desktop-gstreamer",
    any(target_os = "linux", target_os = "windows", target_os = "macos")
))]
mod gstreamer;

#[cfg(all(
    feature = "desktop-gstreamer",
    any(target_os = "linux", target_os = "windows", target_os = "macos")
))]
pub use self::gstreamer::GStreamerDecoder as PlatformDecoder;

/// Information about an opened video.
#[derive(Debug, Clone)]
pub struct VideoInfo {
    /// Video width in pixels.
    pub width: u32,
    /// Video height in pixels.
    pub height: u32,
    /// Duration in milliseconds.
    pub duration_ms: u64,
    /// Frame rate (frames per second).
    pub fps: f64,
    /// Codec name (e.g., "h264", "vp9").
    pub codec: String,
    /// Whether the video has audio.
    pub has_audio: bool,
    /// Whether the video is seekable (false for live streams).
    pub is_seekable: bool,
}

/// A decoded video frame.
#[derive(Debug, Clone)]
pub struct DecodedFrame {
    /// RGB24 pixel data (width * height * 3 bytes).
    pub data: Vec<u8>,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Presentation timestamp in milliseconds.
    pub timestamp_ms: u64,
}

/// Decoded audio samples.
#[derive(Debug, Clone)]
pub struct DecodedAudio {
    /// Interleaved f32 samples.
    pub samples: Vec<f32>,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Number of channels.
    pub channels: u32,
    /// Presentation timestamp in milliseconds.
    pub timestamp_ms: u64,
}

/// Platform-agnostic video decoder trait.
pub trait VideoDecoder: Send {
    /// Open a video file for decoding.
    fn open(&mut self, path: &Path) -> Result<VideoInfo, MediaError>;

    /// Open a video from memory buffer.
    fn open_memory(&mut self, data: Vec<u8>) -> Result<VideoInfo, MediaError>;

    /// Decode the next video frame.
    ///
    /// Returns `Ok(None)` at end of stream.
    fn decode_video_frame(&mut self) -> Result<Option<DecodedFrame>, MediaError>;

    /// Decode the next audio samples.
    ///
    /// Returns `Ok(None)` if no audio or at end of stream.
    fn decode_audio(&mut self) -> Result<Option<DecodedAudio>, MediaError>;

    /// Seek to a position in milliseconds.
    fn seek(&mut self, timestamp_ms: u64) -> Result<(), MediaError>;

    /// Get the current playback position in milliseconds.
    fn position_ms(&self) -> u64;

    /// Query the actual position from the decoder pipeline.
    ///
    /// Unlike `position_ms()` which may return a cached value, this queries
    /// the actual current position from the underlying decoder. Use this to
    /// verify seek operations succeeded.
    fn query_actual_position_ms(&self) -> Result<u64, MediaError>;

    /// Get video duration in milliseconds.
    fn duration_ms(&self) -> u64;

    /// Check if the video has audio.
    fn has_audio(&self) -> bool;

    /// Get video dimensions.
    fn dimensions(&self) -> (u32, u32);

    /// Get frame rate.
    fn fps(&self) -> f64;

    /// Check if the video is seekable.
    /// Returns false for live streams or formats without seek support.
    fn is_seekable(&self) -> bool;

    /// Set pipeline to PLAYING state.
    /// This starts the GStreamer clock and enables frame timing.
    fn play(&mut self) -> Result<(), MediaError>;

    /// Set pipeline to PAUSED state.
    /// This stops the GStreamer clock, freezing A/V timing.
    /// Essential for proper pause/resume without desync.
    fn pause(&mut self) -> Result<(), MediaError>;

    /// Set audio volume (0.0 - 1.0).
    /// Volume is applied by GStreamer's audio pipeline.
    fn set_volume(&mut self, volume: f64) -> Result<(), MediaError>;

    /// Close the decoder and release resources.
    fn close(&mut self);
}

/// Create a new platform-specific decoder.
pub fn create_decoder() -> Result<Box<dyn VideoDecoder>, MediaError> {
    #[cfg(all(
        feature = "desktop-gstreamer",
        any(target_os = "linux", target_os = "windows", target_os = "macos")
    ))]
    {
        Ok(Box::new(PlatformDecoder::new()?))
    }

    #[cfg(not(all(
        feature = "desktop-gstreamer",
        any(target_os = "linux", target_os = "windows", target_os = "macos")
    )))]
    {
        Err(MediaError::InvalidFormat(
            "Video decoding not implemented for this platform".to_string(),
        ))
    }
}

/// Extract a single frame (thumbnail) from a video file.
pub fn extract_thumbnail(path: &Path, time_ms: u64) -> Result<DecodedFrame, MediaError> {
    let mut decoder = create_decoder()?;
    let _info = decoder.open(path)?;

    if time_ms > 0 {
        decoder.seek(time_ms)?;
    }

    decoder
        .decode_video_frame()?
        .ok_or_else(|| MediaError::InvalidFormat("No frames in video".to_string()))
}

/// Extract a single frame (thumbnail) from video in memory.
pub fn extract_thumbnail_from_memory(
    data: Vec<u8>,
    time_ms: u64,
) -> Result<DecodedFrame, MediaError> {
    let mut decoder = create_decoder()?;
    let _info = decoder.open_memory(data)?;

    if time_ms > 0 {
        decoder.seek(time_ms)?;
    }

    decoder
        .decode_video_frame()?
        .ok_or_else(|| MediaError::InvalidFormat("No frames in video".to_string()))
}

/// Get video info without decoding frames.
pub fn get_video_info(path: &Path) -> Result<VideoInfo, MediaError> {
    let mut decoder = create_decoder()?;
    decoder.open(path)
}

/// Get video info from memory without decoding frames.
pub fn get_video_info_from_memory(data: Vec<u8>) -> Result<VideoInfo, MediaError> {
    let mut decoder = create_decoder()?;
    decoder.open_memory(data)
}
