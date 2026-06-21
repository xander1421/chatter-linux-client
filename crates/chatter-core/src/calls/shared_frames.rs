//! Shared video frame storage for cross-thread access.
//!
//! Video frames are stored here by the call worker and can be read
//! by the UI thread for display.

use std::collections::HashMap;
use std::sync::Arc;
use parking_lot::RwLock;

/// Raw video frame data (Send + Sync safe).
///
/// Uses Arc<Vec<u8>> for rgba_data to avoid deep clones of frame buffers.
#[derive(Clone)]
pub struct VideoFrameData {
    /// RGBA8 pixel data (Arc-wrapped to avoid deep clones).
    pub rgba_data: Arc<Vec<u8>>,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Whether this is a screen share (vs camera).
    pub is_screen_share: bool,
    /// Frame timestamp (monotonic counter for change detection).
    pub frame_id: u64,
    /// Capture-device rotation — 0 / 90 / 180 / 270. Renderer applies in
    /// fragment shader so upload stays tightly packed RGBA.
    pub rotation_degrees: u32,
}

/// Shared storage for video frames from remote participants.
///
/// This is thread-safe and can be shared between the call worker
/// (which writes frames) and the UI thread (which reads them).
#[derive(Clone, Default)]
pub struct SharedVideoFrames {
    frames: Arc<RwLock<HashMap<String, VideoFrameData>>>,
    frame_counter: Arc<RwLock<u64>>,
    /// Wakes any task awaiting a fresh frame. The UI subscriber uses this
    /// to push render updates the moment a frame arrives instead of
    /// polling on a fixed-interval timer. `Notify` coalesces multiple
    /// `notify_one` calls into a single wake-up so a burst of late frames
    /// turns into one render pass.
    notify: Arc<tokio::sync::Notify>,
}

impl SharedVideoFrames {
    /// Create a new shared frame store.
    pub fn new() -> Self {
        Self {
            frames: Arc::new(RwLock::new(HashMap::new())),
            frame_counter: Arc::new(RwLock::new(0)),
            notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Handle to the frame-arrival notifier. Callers can `notify.notified().await`
    /// to be woken whenever `set_frame_rotated` lands a new frame.
    pub fn notifier(&self) -> Arc<tokio::sync::Notify> {
        self.notify.clone()
    }

    /// Subscribe to frame updates with a rate-limited push callback.
    ///
    /// Spawns a tokio task on `rt` that waits on the internal `Notify`
    /// and, no more often than `min_interval`, invokes `on_frames` with
    /// a snapshot of every current frame. The returned `Arc<AtomicBool>`
    /// is the stop flag — set it to `true` to terminate the subscriber.
    ///
    /// The callback runs on the tokio runtime, not the caller's UI
    /// thread; consumers needing to touch UI state should re-dispatch
    /// (e.g. `slint::invoke_from_event_loop`) inside the closure. Keeping
    /// the rate-limit + snapshot logic here lets the UI crate stay a thin
    /// presentation layer that only knows how to draw what core hands it.
    pub fn subscribe<F>(
        &self,
        rt: &tokio::runtime::Handle,
        min_interval: std::time::Duration,
        mut on_frames: F,
    ) -> Arc<std::sync::atomic::AtomicBool>
    where
        F: FnMut(Vec<(String, VideoFrameData)>) + Send + 'static,
    {
        let notifier = self.notify.clone();
        let frames = self.frames.clone();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_for_task = stop.clone();
        rt.spawn(async move {
            use std::sync::atomic::Ordering;
            use std::time::Instant;
            let mut last_emit = Instant::now()
                .checked_sub(min_interval)
                .unwrap_or_else(Instant::now);
            loop {
                notifier.notified().await;
                if stop_for_task.load(Ordering::Relaxed) {
                    break;
                }
                let since = last_emit.elapsed();
                if since < min_interval {
                    tokio::time::sleep(min_interval - since).await;
                }
                last_emit = Instant::now();

                let snapshot: Vec<(String, VideoFrameData)> = frames
                    .read()
                    .iter()
                    .map(|(id, frame)| (id.clone(), frame.clone()))
                    .collect();
                if snapshot.is_empty() {
                    continue;
                }
                on_frames(snapshot);
            }
        });
        stop
    }

    /// Store a frame for a participant.
    ///
    /// Accepts an `Arc<Vec<u8>>` so callers can share the same buffer between
    /// the shared store and their own local caches without a deep copy of
    /// the RGBA pixel data on every frame.
    pub fn set_frame(&self, participant_id: String, rgba_data: Arc<Vec<u8>>, width: u32, height: u32, is_screen_share: bool) {
        self.set_frame_rotated(participant_id, rgba_data, width, height, is_screen_share, 0);
    }

    /// Same as `set_frame` but carries capture rotation so the renderer
    /// can apply it in its fragment shader.
    pub fn set_frame_rotated(&self, participant_id: String, rgba_data: Arc<Vec<u8>>, width: u32, height: u32, is_screen_share: bool, rotation_degrees: u32) {
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
            rotation_degrees,
        });
        drop(frames);

        // Wake the UI subscriber so it renders this frame without waiting
        // for the next polling tick. `notify_one` coalesces — if the UI
        // hasn't drained yet we don't queue up wakeups.
        self.notify.notify_one();
    }

    /// Get a frame for a participant (clones the data).
    pub fn get_frame(&self, participant_id: &str) -> Option<VideoFrameData> {
        self.frames.read().get(participant_id).cloned()
    }

    /// Drop a participant's stored frame. Called when a track ends or
    /// when a screen-share session terminates.
    pub fn remove_frame(&self, participant_id: &str) {
        self.frames.write().remove(participant_id);
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
        shared.set_frame("user1".into(), Arc::new(vec![0u8; 100]), 10, 10, false);
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
