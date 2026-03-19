//! Shared video frame storage for cross-thread access.
//!
//! Video frames are stored here by the call worker and can be read
//! by the UI thread for display.

use std::collections::HashMap;
use std::sync::Arc;
use parking_lot::RwLock;

/// Raw video frame data (Send + Sync safe).
#[derive(Clone)]
pub struct VideoFrameData {
    /// RGBA8 pixel data.
    pub rgba_data: Vec<u8>,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Whether this is a screen share (vs camera).
    pub is_screen_share: bool,
    /// Frame timestamp (monotonic counter for change detection).
    pub frame_id: u64,
}

/// Shared storage for video frames from remote participants.
///
/// This is thread-safe and can be shared between the call worker
/// (which writes frames) and the UI thread (which reads them).
#[derive(Clone, Default)]
pub struct SharedVideoFrames {
    frames: Arc<RwLock<HashMap<String, VideoFrameData>>>,
    frame_counter: Arc<RwLock<u64>>,
}

impl SharedVideoFrames {
    /// Create a new shared frame store.
    pub fn new() -> Self {
        Self {
            frames: Arc::new(RwLock::new(HashMap::new())),
            frame_counter: Arc::new(RwLock::new(0)),
        }
    }

    /// Store a frame for a participant.
    pub fn set_frame(&self, participant_id: String, rgba_data: Vec<u8>, width: u32, height: u32, is_screen_share: bool) {
        let frame_id = {
            let mut counter = self.frame_counter.write();
            *counter += 1;
            *counter
        };

        let mut frames = self.frames.write();
        frames.insert(participant_id, VideoFrameData {
            rgba_data,
            width,
            height,
            is_screen_share,
            frame_id,
        });
    }

    /// Get a frame for a participant (clones the data).
    pub fn get_frame(&self, participant_id: &str) -> Option<VideoFrameData> {
        self.frames.read().get(participant_id).cloned()
    }

    /// Get all frames (clones the data).
    pub fn get_all_frames(&self) -> Vec<(String, VideoFrameData)> {
        self.frames
            .read()
            .iter()
            .map(|(id, frame)| (id.clone(), frame.clone()))
            .collect()
    }

    /// Get only camera frames (excludes screen shares).
    pub fn get_camera_frames(&self) -> Vec<(String, VideoFrameData)> {
        self.frames
            .read()
            .iter()
            .filter(|(_, frame)| !frame.is_screen_share)
            .map(|(id, frame)| (id.clone(), frame.clone()))
            .collect()
    }

    /// Get only screen share frames.
    pub fn get_screen_share_frames(&self) -> Vec<(String, VideoFrameData)> {
        self.frames
            .read()
            .iter()
            .filter(|(_, frame)| frame.is_screen_share)
            .map(|(id, frame)| (id.clone(), frame.clone()))
            .collect()
    }

    /// Remove a participant's frame.
    pub fn remove_frame(&self, participant_id: &str) {
        self.frames.write().remove(participant_id);
    }

    /// Clear all frames.
    pub fn clear(&self) {
        self.frames.write().clear();
    }

    /// Get the number of frames stored.
    pub fn len(&self) -> usize {
        self.frames.read().len()
    }

    /// Check if empty.
    pub fn is_empty(&self) -> bool {
        self.frames.read().is_empty()
    }

    /// Check if a participant has a frame.
    pub fn has_frame(&self, participant_id: &str) -> bool {
        self.frames.read().contains_key(participant_id)
    }

    /// Get frame IDs for change detection.
    pub fn get_frame_ids(&self) -> HashMap<String, u64> {
        self.frames
            .read()
            .iter()
            .map(|(id, frame)| (id.clone(), frame.frame_id))
            .collect()
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shared_frames() {
        let shared = SharedVideoFrames::new();

        // Initially empty
        assert!(shared.is_empty());

        // Add a frame
        shared.set_frame("user1".into(), vec![0u8; 100], 10, 10, false);
        assert_eq!(shared.len(), 1);
        assert!(shared.has_frame("user1"));

        // Get frame
        let frame = shared.get_frame("user1").unwrap();
        assert_eq!(frame.width, 10);
        assert_eq!(frame.height, 10);
        assert!(!frame.is_screen_share);

        // Remove frame
        shared.remove_frame("user1");
        assert!(shared.is_empty());
    }
}
