//! Video playback for remote participant streams.
//!
//! Receives video frames from remote participants via LiveKit and converts
//! them to Slint-compatible images for display.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures::StreamExt;
use livekit::track::RemoteVideoTrack;
use livekit::webrtc::video_stream::native::NativeVideoStream;
use parking_lot::RwLock;

use super::shared_frames::{SharedVideoFrames, VideoFrameData};

/// Information about a video stream.
#[derive(Debug, Clone)]
pub struct VideoStreamInfo {
    pub participant_id: String,
    pub track_sid: String,
    pub width: u32,
    pub height: u32,
    pub is_screen_share: bool,
}

/// Manages video playback for remote participants.
pub struct VideoPlayback {
    /// Active video streams keyed by participant_id.
    streams: Arc<RwLock<HashMap<String, ActiveVideoStream>>>,
    /// Latest frames keyed by participant_id.
    frames: Arc<RwLock<HashMap<String, LatestFrame>>>,
    /// Shared frame store for cross-thread access (UI reads this).
    shared_frames: Option<SharedVideoFrames>,
    /// Whether playback is active.
    is_active: Arc<AtomicBool>,
}

struct ActiveVideoStream {
    track_sid: String,
    stop_signal: Arc<AtomicBool>,
    is_screen_share: bool,
}

struct LatestFrame {
    /// RGBA8 pixel data (Arc-wrapped for cheap sharing).
    rgba_data: Arc<Vec<u8>>,
    /// Frame width.
    width: u32,
    /// Frame height.
    height: u32,
    /// Whether this is a screen share frame.
    is_screen_share: bool,
    /// Orientation the UI should apply before drawing, in degrees
    /// (0 / 90 / 180 / 270). Set from the track's rotation metadata.
    rotation_degrees: u32,
}

impl VideoPlayback {
    /// Create a new video playback manager.
    pub fn new() -> Self {
        tracing::info!("Video playback manager created");
        Self {
            streams: Arc::new(RwLock::new(HashMap::new())),
            frames: Arc::new(RwLock::new(HashMap::new())),
            shared_frames: None,
            is_active: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Create a new video playback manager with shared frame store.
    pub fn with_shared_frames(shared_frames: SharedVideoFrames) -> Self {
        tracing::info!("Video playback manager created with shared frames");
        Self {
            streams: Arc::new(RwLock::new(HashMap::new())),
            frames: Arc::new(RwLock::new(HashMap::new())),
            shared_frames: Some(shared_frames),
            is_active: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Add a remote video track for playback.
    pub fn add_track(&self, participant_id: String, track: RemoteVideoTrack, is_screen_share: bool) {
        let track_sid = track.sid().to_string();
        tracing::info!(
            "Adding video track from participant: {} (sid: {}, screen_share: {})",
            participant_id,
            track_sid,
            is_screen_share
        );

        let stop_signal = Arc::new(AtomicBool::new(false));
        let stop_signal_clone = stop_signal.clone();
        let frames = self.frames.clone();
        let shared_frames = self.shared_frames.clone();
        let participant_id_clone = participant_id.clone();

        // Create native video stream from the track
        let rtc_track = track.rtc_track();
        let mut video_stream = NativeVideoStream::new(rtc_track);

        // Spawn task to receive and convert video frames
        tokio::spawn(async move {
            tracing::debug!("Video stream receiver started for {}", participant_id_clone);

            while !stop_signal_clone.load(Ordering::Relaxed) {
                match video_stream.next().await {
                    Some(frame) => {
                        // Get frame dimensions from the VideoBuffer trait
                        let width = frame.buffer.width();
                        let height = frame.buffer.height();

                        // Convert to I420 buffer for RGBA conversion
                        let i420_buffer = frame.buffer.to_i420();

                        // Convert I420 to RGBA
                        let rgba_data = i420_to_rgba(&i420_buffer, width, height);

                        // Wrap once so both the local frames map and the
                        // cross-thread shared store share the same buffer via
                        // Arc cloning instead of a per-frame deep copy of
                        // potentially megabytes of pixel data.
                        let rgba_arc = Arc::new(rgba_data);

                        // Store in local frames map
                        {
                            let mut frames_guard = frames.write();
                            frames_guard.insert(
                                participant_id_clone.clone(),
                                LatestFrame {
                                    rgba_data: rgba_arc.clone(),
                                    width,
                                    height,
                                    is_screen_share,
                                    rotation_degrees: 0,
                                },
                            );
                        }

                        // Also store in shared frames for UI access
                        if let Some(ref shared) = shared_frames {
                            shared.set_frame(
                                participant_id_clone.clone(),
                                rgba_arc,
                                width,
                                height,
                                is_screen_share,
                            );
                        }
                    }
                    None => {
                        tracing::debug!("Video stream ended for {}", participant_id_clone);
                        break;
                    }
                }
            }

            tracing::debug!("Video stream receiver stopped for {}", participant_id_clone);
        });

        // Store the active stream
        let mut streams = self.streams.write();
        streams.insert(
            participant_id,
            ActiveVideoStream {
                track_sid,
                stop_signal,
                is_screen_share,
            },
        );
    }

    /// Remove a participant's video track.
    pub fn remove_track(&self, participant_id: &str) {
        tracing::info!("Removing video track from participant: {}", participant_id);

        // Stop the stream receiver
        let mut streams = self.streams.write();
        if let Some(stream) = streams.remove(participant_id) {
            stream.stop_signal.store(true, Ordering::Relaxed);
        }

        // Remove stored frame
        let mut frames = self.frames.write();
        frames.remove(participant_id);

        // Remove from shared frames
        if let Some(ref shared) = self.shared_frames {
            shared.remove_frame(participant_id);
        }
    }

    /// Get the latest frame for a participant as raw video data.
    ///
    /// Returns None if no frame is available for the participant.
    pub fn get_frame(&self, participant_id: &str) -> Option<VideoFrameData> {
        let frames = self.frames.read();
        frames.get(participant_id).map(|frame| VideoFrameData {
            rgba_data: frame.rgba_data.clone(),
            width: frame.width,
            height: frame.height,
            is_screen_share: frame.is_screen_share,
            frame_id: 0,
            rotation_degrees: frame.rotation_degrees,
        })
    }

    /// Get all remote video frames as raw data.
    pub fn get_all_frames(&self) -> Vec<VideoFrameData> {
        let frames = self.frames.read();
        frames
            .values()
            .map(|frame| VideoFrameData {
                rgba_data: frame.rgba_data.clone(),
                width: frame.width,
                height: frame.height,
                is_screen_share: frame.is_screen_share,
                frame_id: 0,
                rotation_degrees: frame.rotation_degrees,
            })
            .collect()
    }

    /// Get only camera video frames (excludes screen shares).
    pub fn get_camera_frames(&self) -> Vec<(String, VideoFrameData)> {
        let frames = self.frames.read();
        frames
            .iter()
            .filter(|(_, frame)| !frame.is_screen_share)
            .map(|(participant_id, frame)| {
                (participant_id.clone(), VideoFrameData {
                    rgba_data: frame.rgba_data.clone(),
                    width: frame.width,
                    height: frame.height,
                    is_screen_share: frame.is_screen_share,
                    frame_id: 0,
                    rotation_degrees: frame.rotation_degrees,
                })
            })
            .collect()
    }

    /// Get only screen share frames.
    pub fn get_screen_share_frames(&self) -> Vec<(String, VideoFrameData)> {
        let frames = self.frames.read();
        frames
            .iter()
            .filter(|(_, frame)| frame.is_screen_share)
            .map(|(participant_id, frame)| {
                (participant_id.clone(), VideoFrameData {
                    rgba_data: frame.rgba_data.clone(),
                    width: frame.width,
                    height: frame.height,
                    is_screen_share: frame.is_screen_share,
                    frame_id: 0,
                    rotation_degrees: frame.rotation_degrees,
                })
            })
            .collect()
    }

    /// Check if a participant is sharing their screen.
    pub fn is_screen_sharing(&self, participant_id: &str) -> bool {
        self.frames
            .read()
            .get(participant_id)
            .map(|f| f.is_screen_share)
            .unwrap_or(false)
    }

    /// Get video stream info for all active streams.
    pub fn get_stream_info(&self) -> Vec<VideoStreamInfo> {
        let streams = self.streams.read();
        let frames = self.frames.read();

        streams
            .iter()
            .map(|(participant_id, stream)| {
                let (width, height) = frames
                    .get(participant_id)
                    .map(|f| (f.width, f.height))
                    .unwrap_or((0, 0));

                VideoStreamInfo {
                    participant_id: participant_id.clone(),
                    track_sid: stream.track_sid.clone(),
                    width,
                    height,
                    is_screen_share: stream.is_screen_share,
                }
            })
            .collect()
    }

    /// Check if we have a frame available for a participant.
    pub fn has_frame(&self, participant_id: &str) -> bool {
        self.frames.read().contains_key(participant_id)
    }

    /// Get the number of active video streams.
    pub fn stream_count(&self) -> usize {
        self.streams.read().len()
    }

    /// Stop all video playback.
    pub fn stop(&self) {
        self.is_active.store(false, Ordering::Relaxed);

        // Stop all active streams
        let mut streams = self.streams.write();
        for (_, stream) in streams.drain() {
            stream.stop_signal.store(true, Ordering::Relaxed);
        }

        // Clear all frames
        let mut frames = self.frames.write();
        frames.clear();

        // Clear shared frames
        if let Some(ref shared) = self.shared_frames {
            shared.clear();
        }

        tracing::info!("Video playback stopped");
    }
}

impl Drop for VideoPlayback {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Default for VideoPlayback {
    fn default() -> Self {
        Self::new()
    }
}

/// Convert I420 (YUV 4:2:0) buffer to RGBA8.
fn i420_to_rgba(buffer: &livekit::webrtc::video_frame::I420Buffer, width: u32, height: u32) -> Vec<u8> {
    let width = width as usize;
    let height = height as usize;
    let (y_data, u_data, v_data) = buffer.data();

    let mut rgba = vec![0u8; width * height * 4];

    // Y plane stride
    let y_stride = buffer.strides().0 as usize;
    let u_stride = buffer.strides().1 as usize;
    let v_stride = buffer.strides().2 as usize;

    for row in 0..height {
        for col in 0..width {
            let y_idx = row * y_stride + col;
            let uv_row = row / 2;
            let uv_col = col / 2;
            let u_idx = uv_row * u_stride + uv_col;
            let v_idx = uv_row * v_stride + uv_col;

            let y = y_data.get(y_idx).copied().unwrap_or(0) as f32;
            let u = u_data.get(u_idx).copied().unwrap_or(128) as f32;
            let v = v_data.get(v_idx).copied().unwrap_or(128) as f32;

            // YUV to RGB conversion (BT.601)
            let r = (y + 1.402 * (v - 128.0)).clamp(0.0, 255.0) as u8;
            let g = (y - 0.344136 * (u - 128.0) - 0.714136 * (v - 128.0)).clamp(0.0, 255.0) as u8;
            let b = (y + 1.772 * (u - 128.0)).clamp(0.0, 255.0) as u8;

            let rgba_idx = (row * width + col) * 4;
            rgba[rgba_idx] = r;
            rgba[rgba_idx + 1] = g;
            rgba[rgba_idx + 2] = b;
            rgba[rgba_idx + 3] = 255; // Alpha
        }
    }

    rgba
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_video_playback_new() {
        let playback = VideoPlayback::new();
        assert_eq!(playback.stream_count(), 0);
    }
}
