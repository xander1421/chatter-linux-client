//! Android MediaCodec video decoder stub.
//!
//! TODO: Implement using Android's MediaCodec API for hardware-accelerated decoding.

use std::path::Path;

use crate::error::MediaError;
use super::{DecodedAudio, DecodedFrame, VideoDecoder, VideoInfo};

pub struct MediaCodecDecoder;

impl MediaCodecDecoder {
    pub fn new() -> Result<Self, MediaError> {
        Err(MediaError::InvalidFormat(
            "MediaCodec decoder not yet implemented".to_string(),
        ))
    }
}

impl VideoDecoder for MediaCodecDecoder {
    fn open(&mut self, _path: &Path) -> Result<VideoInfo, MediaError> {
        Err(MediaError::InvalidFormat("Not implemented".to_string()))
    }

    fn open_memory(&mut self, _data: Vec<u8>) -> Result<VideoInfo, MediaError> {
        Err(MediaError::InvalidFormat("Not implemented".to_string()))
    }

    fn decode_video_frame(&mut self) -> Result<Option<DecodedFrame>, MediaError> {
        Ok(None)
    }

    fn decode_audio(&mut self) -> Result<Option<DecodedAudio>, MediaError> {
        Ok(None)
    }

    fn seek(&mut self, _timestamp_ms: u64) -> Result<(), MediaError> {
        Ok(())
    }

    fn position_ms(&self) -> u64 { 0 }

    fn query_actual_position_ms(&self) -> Result<u64, MediaError> { Ok(0) }

    fn duration_ms(&self) -> u64 { 0 }

    fn has_audio(&self) -> bool { false }

    fn dimensions(&self) -> (u32, u32) { (0, 0) }

    fn fps(&self) -> f64 { 0.0 }

    fn is_seekable(&self) -> bool { false }

    fn play(&mut self) -> Result<(), MediaError> { Ok(()) }

    fn pause(&mut self) -> Result<(), MediaError> { Ok(()) }

    fn set_volume(&mut self, _volume: f64) -> Result<(), MediaError> { Ok(()) }

    fn close(&mut self) {}
}
