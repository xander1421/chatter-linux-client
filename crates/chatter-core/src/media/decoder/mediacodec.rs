//! Android video decoding is NOT implemented.
//!
//! `MediaCodecDecoder::new()` is the only constructor and it always fails, so no
//! value of this type can exist and none of the trait methods below can run. They
//! used to claim otherwise: `seek`, `play`, `pause` and `set_volume` returned
//! `Ok(())` while doing nothing, and the getters returned plausible zeroes
//! (FEAT-11, OPEN-ITEMS §91). A future implementation must replace them, not fill
//! them in around the edges — hence `unreachable!` rather than more `Ok(())`.
//!
//! The module doc above the platform selector claimed GStreamer "works on Desktop
//! and Mobile (Android/iOS)". It does not: `mod.rs` selects GStreamer only for
//! linux/windows/macos and this stub for android.

use std::path::Path;

use crate::error::MediaError;
use super::{DecodedAudio, DecodedFrame, VideoDecoder, VideoInfo};

const NOT_IMPLEMENTED: &str = "Android video playback is not implemented: no MediaCodec decoder";

pub struct MediaCodecDecoder;

impl MediaCodecDecoder {
    pub fn new() -> Result<Self, MediaError> {
        Err(MediaError::InvalidFormat(NOT_IMPLEMENTED.to_string()))
    }
}

/// Every method is unreachable: `new()` never yields a value to call them on.
/// They exist because `PlatformDecoder` must implement the trait.
impl VideoDecoder for MediaCodecDecoder {
    fn open(&mut self, _path: &Path) -> Result<VideoInfo, MediaError> {
        unreachable!("{NOT_IMPLEMENTED}")
    }

    fn open_memory(&mut self, _data: Vec<u8>) -> Result<VideoInfo, MediaError> {
        unreachable!("{NOT_IMPLEMENTED}")
    }

    fn decode_video_frame(&mut self) -> Result<Option<DecodedFrame>, MediaError> {
        unreachable!("{NOT_IMPLEMENTED}")
    }

    fn decode_audio(&mut self) -> Result<Option<DecodedAudio>, MediaError> {
        unreachable!("{NOT_IMPLEMENTED}")
    }

    fn seek(&mut self, _timestamp_ms: u64) -> Result<(), MediaError> {
        unreachable!("{NOT_IMPLEMENTED}")
    }

    fn position_ms(&self) -> u64 {
        unreachable!("{NOT_IMPLEMENTED}")
    }

    fn query_actual_position_ms(&self) -> Result<u64, MediaError> {
        unreachable!("{NOT_IMPLEMENTED}")
    }

    fn duration_ms(&self) -> u64 {
        unreachable!("{NOT_IMPLEMENTED}")
    }

    fn has_audio(&self) -> bool {
        unreachable!("{NOT_IMPLEMENTED}")
    }

    fn dimensions(&self) -> (u32, u32) {
        unreachable!("{NOT_IMPLEMENTED}")
    }

    fn fps(&self) -> f64 {
        unreachable!("{NOT_IMPLEMENTED}")
    }

    fn is_seekable(&self) -> bool {
        unreachable!("{NOT_IMPLEMENTED}")
    }

    fn play(&mut self) -> Result<(), MediaError> {
        unreachable!("{NOT_IMPLEMENTED}")
    }

    fn pause(&mut self) -> Result<(), MediaError> {
        unreachable!("{NOT_IMPLEMENTED}")
    }

    fn set_volume(&mut self, _volume: f64) -> Result<(), MediaError> {
        unreachable!("{NOT_IMPLEMENTED}")
    }

    fn close(&mut self) {
        unreachable!("{NOT_IMPLEMENTED}")
    }
}
