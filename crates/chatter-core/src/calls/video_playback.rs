//! Video playback for remote participant streams.
//!
//! Receives video frames from remote participants via LiveKit and converts
//! them to Slint-compatible images for display.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::StreamExt;
use livekit::track::RemoteVideoTrack;
use livekit::webrtc::video_frame::VideoBufferType;
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
    /// Participants whose video the local viewer has paused. Frames still
    /// decode (keeps the LiveKit stream healthy and resume snappy) but are
    /// not stored or pushed to `shared_frames`.
    paused_peers: Arc<RwLock<HashSet<String>>>,
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
            paused_peers: Arc::new(RwLock::new(HashSet::new())),
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
            paused_peers: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    /// Add a remote video track for playback.
    pub fn add_track(
        &self,
        participant_id: String,
        track: RemoteVideoTrack,
        is_screen_share: bool,
    ) {
        let track_sid = track.sid().to_string();
        // Camera and screen share are separate tracks from the SAME peer. Store
        // them under distinct frame keys so they don't overwrite each other (one
        // tile would otherwise flash between the two feeds). The screen key sorts
        // right after the camera key, which keeps the id-sorted frame list aligned
        // with its id-sorted participant tiles.
        let frame_key = screen_share_frame_id(&participant_id, is_screen_share);
        tracing::info!(
            "Adding video track from participant: {} (sid: {}, screen_share: {}, frame_key: {})",
            participant_id,
            track_sid,
            is_screen_share,
            frame_key,
        );

        let stop_signal = Arc::new(AtomicBool::new(false));
        let stop_signal_clone = stop_signal.clone();
        let shared_frames = self.shared_frames.clone();
        let paused_peers = self.paused_peers.clone();
        let participant_id_clone = frame_key.clone();

        // Create native video stream from the track
        let rtc_track = track.rtc_track();
        let mut video_stream = NativeVideoStream::new(rtc_track);

        // Run the receive+convert loop on a DEDICATED OS thread with its own
        // current-thread runtime, NOT the shared tokio pool. The per-frame
        // libyuv convert is CPU-bound (~2 ms); on the shared runtime it
        // competed with audio relay / inbox polling / signaling, inflating
        // wall-time and adding render jitter (the part that "feels laggy").
        // An isolated thread keeps the decode->convert cadence steady.
        // `video_stream` is `Send` (it had to be for the previous
        // `tokio::spawn`), so moving it across the thread boundary is sound.
        let recv_thread_name = format!("video-recv-{participant_id_clone}");
        let _ = std::thread::Builder::new()
            .name(recv_thread_name)
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        tracing::error!("Failed to build video-recv runtime: {}", e);
                        return;
                    }
                };
                rt.block_on(async move {
            tracing::debug!("Video stream receiver started for {}", participant_id_clone);

            // FPS instrumentation: count received frames and the cost of each
            // pipeline stage so we can isolate where the wire-30fps stream
            // turns into a 10-15fps UI render. Logged once per second.
            let mut frames_in_window: u32 = 0;
            let mut to_i420_total = std::time::Duration::ZERO;
            let mut convert_total = std::time::Duration::ZERO;
            let mut store_total = std::time::Duration::ZERO;
            let mut window_start = std::time::Instant::now();

            // Recycle pool: a small set of RGBA buffers we hand downstream as
            // `Arc<Vec<u8>>`. A buffer is only reusable once the UI/shared
            // store has dropped its clone (`Arc::strong_count == 1`). Each
            // frame we try to reuse one; whatever we can't reuse we re-alloc.
            // Cap is tiny because at most ~1 frame is in flight to the UI.
            const SPARE_POOL_CAP: usize = 3;
            let mut spare_pool: Vec<Arc<Vec<u8>>> = Vec::with_capacity(SPARE_POOL_CAP);
            // The Arc handed downstream last iteration. Once we've stored a new
            // frame, the previous one is no longer referenced by *our* maps, so
            // it becomes a recycle candidate (it may still be held by the UI,
            // in which case `Arc::get_mut` will simply fail next round).
            let mut prev_arc: Option<Arc<Vec<u8>>> = None;

            while !stop_signal_clone.load(Ordering::Relaxed) {
                match video_stream.next().await {
                    Some(frame) => {
                        // Locally paused — drop the frame after decode so the
                        // LiveKit ring buffer drains and resume is instant.
                        if paused_peers.read().contains(&participant_id_clone) {
                            continue;
                        }

                        // Get frame dimensions from the VideoBuffer trait
                        let width = frame.buffer.width();
                        let height = frame.buffer.height();

                        let rgba_len = (width as usize) * (height as usize) * 4;

                        // Pull a reusable RGBA buffer out of the recycle pool:
                        // it must be uniquely owned (UI/shared store has
                        // released its clone, so `strong_count == 1`) and sized
                        // for this frame. Anything still in flight to the UI is
                        // skipped and we fall through to a fresh allocation.
                        let mut recycled: Option<Arc<Vec<u8>>> = None;
                        {
                            let mut i = 0;
                            while i < spare_pool.len() {
                                let reusable = Arc::get_mut(&mut spare_pool[i])
                                    .map(|v| v.len() == rgba_len)
                                    .unwrap_or(false);
                                if reusable {
                                    recycled = Some(spare_pool.swap_remove(i));
                                    break;
                                }
                                i += 1;
                            }
                        }

                        // Convert directly from the source pixel format, avoiding
                        // the extra `to_i420()` copy when the frame is already
                        // I420 or NV12. Only an unrecognised format falls back to
                        // `to_i420()`. `t_i420` captures any such fallback copy so
                        // the [VIDEO_FPS] breakdown stays meaningful; `t_convert`
                        // captures the libyuv -> RGBA pass.
                        let mut t_i420 = std::time::Duration::ZERO;
                        let t_convert;

                        // Destination buffer: reuse the recycled Arc in place via
                        // `Arc::get_mut`, otherwise allocate a fresh one.
                        let rgba_arc = match frame.buffer.buffer_type() {
                            VideoBufferType::I420 => {
                                let i420 = frame.buffer.as_i420().expect("I420 buffer_type");
                                let t1 = std::time::Instant::now();
                                let arc = convert_i420_into(recycled, i420, width, height, rgba_len);
                                t_convert = t1.elapsed();
                                arc
                            }
                            VideoBufferType::NV12 => {
                                let nv12 = frame.buffer.as_nv12().expect("NV12 buffer_type");
                                let t1 = std::time::Instant::now();
                                let arc = convert_nv12_into(recycled, nv12, width, height, rgba_len);
                                t_convert = t1.elapsed();
                                arc
                            }
                            _ => {
                                let t0 = std::time::Instant::now();
                                let i420_buffer = frame.buffer.to_i420();
                                t_i420 = t0.elapsed();
                                let t1 = std::time::Instant::now();
                                let arc =
                                    convert_i420_into(recycled, &i420_buffer, width, height, rgba_len);
                                t_convert = t1.elapsed();
                                arc
                            }
                        };

                        // Store the frame for the UI. `shared_frames` is the
                        // ONLY consumer path (UI subscribes to it); the former
                        // per-frame write into a second local `self.frames`
                        // HashMap was dead weight — its accessors have no
                        // callers — and the extra Arc clone it held also fought
                        // the recycle pool below. Dropped entirely.
                        let t2 = std::time::Instant::now();
                        if let Some(ref shared) = shared_frames {
                            shared.set_frame(
                                participant_id_clone.clone(),
                                rgba_arc.clone(),
                                width,
                                height,
                                is_screen_share,
                            );
                        }
                        let t_store = t2.elapsed();

                        // The frame we stored *last* iteration has now been
                        // overwritten in the shared store, so the only remaining
                        // clone (if any) is one the UI is reading. Offer it back
                        // to the recycle pool:
                        // if the UI already released it, next iteration's
                        // `Arc::get_mut` reclaims it; if not, it's harmlessly
                        // dropped when the pool is full or skipped while in use.
                        if let Some(old) = prev_arc.replace(rgba_arc) {
                            if spare_pool.len() < SPARE_POOL_CAP {
                                spare_pool.push(old);
                            }
                        }

                        frames_in_window += 1;
                        to_i420_total += t_i420;
                        convert_total += t_convert;
                        store_total += t_store;

                        let window_elapsed = window_start.elapsed();
                        if window_elapsed.as_millis() >= 1000 {
                            let secs = window_elapsed.as_secs_f64();
                            let avg_i420_us = to_i420_total.as_micros() as f64 / frames_in_window as f64;
                            let avg_conv_us = convert_total.as_micros() as f64 / frames_in_window as f64;
                            let avg_store_us = store_total.as_micros() as f64 / frames_in_window as f64;
                            tracing::info!(
                                "[VIDEO_FPS] {} ({}x{} ss={}): {:.1} fps over {:.2}s — avg/frame to_i420={:.0}us convert={:.0}us store={:.0}us",
                                participant_id_clone,
                                width, height, is_screen_share,
                                frames_in_window as f64 / secs,
                                secs,
                                avg_i420_us, avg_conv_us, avg_store_us,
                            );
                            frames_in_window = 0;
                            to_i420_total = std::time::Duration::ZERO;
                            convert_total = std::time::Duration::ZERO;
                            store_total = std::time::Duration::ZERO;
                            window_start = std::time::Instant::now();
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
            });

        // Store the active stream under the same key the frames use, so
        // remove_track targets the right one (camera vs screen share).
        let mut streams = self.streams.write();
        streams.insert(
            frame_key,
            ActiveVideoStream {
                track_sid,
                stop_signal,
                is_screen_share,
            },
        );
    }

    /// Remove a participant's video track. `is_screen_share` selects which of the
    /// peer's two possible tracks (camera or screen) to drop.
    pub fn remove_track(&self, participant_id: &str, is_screen_share: bool) {
        let frame_key = screen_share_frame_id(participant_id, is_screen_share);
        tracing::info!("Removing video track: {}", frame_key);

        // Stop the stream receiver
        let mut streams = self.streams.write();
        if let Some(stream) = streams.remove(&frame_key) {
            stream.stop_signal.store(true, Ordering::Relaxed);
        }

        // Remove stored frame
        let mut frames = self.frames.write();
        frames.remove(&frame_key);

        // Remove from shared frames
        if let Some(ref shared) = self.shared_frames {
            shared.remove_frame(&frame_key);
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
                (
                    participant_id.clone(),
                    VideoFrameData {
                        rgba_data: frame.rgba_data.clone(),
                        width: frame.width,
                        height: frame.height,
                        is_screen_share: frame.is_screen_share,
                        frame_id: 0,
                        rotation_degrees: frame.rotation_degrees,
                    },
                )
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
                (
                    participant_id.clone(),
                    VideoFrameData {
                        rgba_data: frame.rgba_data.clone(),
                        width: frame.width,
                        height: frame.height,
                        is_screen_share: frame.is_screen_share,
                        frame_id: 0,
                        rotation_degrees: frame.rotation_degrees,
                    },
                )
            })
            .collect()
    }

    /// Locally pause/resume a remote participant's video stream. While
    /// paused, frames still decode but are discarded before storage so the
    /// tile falls back to the avatar placeholder.
    pub fn set_peer_paused(&self, participant_id: &str, paused: bool) {
        if paused {
            self.paused_peers.write().insert(participant_id.to_string());
            // Drop the last cached frame so the UI flips to avatar immediately.
            self.frames.write().remove(participant_id);
            if let Some(ref shared) = self.shared_frames {
                shared.remove_frame(participant_id);
            }
        } else {
            self.paused_peers.write().remove(participant_id);
        }
    }

    pub fn is_peer_paused(&self, participant_id: &str) -> bool {
        self.paused_peers.read().contains(participant_id)
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

/// Frame-store / participant-tile key for a peer's video track. A screen share
/// gets a synthetic id (`{peer}\u{1}screen`) so it coexists with the peer's
/// camera as a separate tile instead of overwriting it. The `\u{1}` suffix sorts
/// immediately after the bare peer id, keeping the id-sorted frame list aligned
/// with the id-sorted participant tiles. The UI builds the matching pseudo-
/// participant with the same id (see `room_events.rs`).
pub fn screen_share_frame_id(participant_id: &str, is_screen_share: bool) -> String {
    if is_screen_share {
        format!("{participant_id}\u{1}screen")
    } else {
        participant_id.to_string()
    }
}

/// Convert I420 (YUV 4:2:0) buffer to RGBA8 (memory byte order R, G, B, A).
///
/// Uses libwebrtc's vendored libyuv (SIMD-accelerated on x86_64/ARMv8).
/// Verified by a one-off example (`examples/yuv_check.rs`, since removed; see
/// `git log -- crates/chatter-core/examples`): libyuv names destinations by the
/// little-endian 32-bit word value, so the in-memory byte order is the
/// reverse of the function name. `i420_to_abgr` writes bytes in
/// R, G, B, A order — exactly what Slint's `Rgba8Pixel` consumes.
///
/// We benchmarked a rayon row-chunk split and removed it: at debug build
/// the per-chunk slice splitting + rayon dispatch overhead beat the gain,
/// pushing 5120×1440 convert from 5ms to 17–53ms. Single-thread libyuv
/// already lands ~0.8 ms in release / ~5 ms in debug at 5120×1440 — both
/// under one 60-fps frame budget (measured by `examples/yuv_threaded_bench.rs`,
/// since removed; see `git log -- crates/chatter-core/examples`).
///
/// Writes into a caller-provided destination of exactly `width*height*4` bytes
/// (so the receive loop can recycle a pooled buffer instead of allocating).
fn i420_to_rgba_buf(
    buffer: &livekit::webrtc::video_frame::I420Buffer,
    width: u32,
    height: u32,
    dst: &mut [u8],
) {
    let (y_stride, u_stride, v_stride) = buffer.strides();
    let (y_data, u_data, v_data) = buffer.data();
    let dst_stride = width * 4;

    livekit::webrtc::native::yuv_helper::i420_to_abgr(
        y_data,
        y_stride,
        u_data,
        u_stride,
        v_data,
        v_stride,
        dst,
        dst_stride,
        width as i32,
        height as i32,
    );
}

/// NV12 (Y plane + interleaved UV plane) -> RGBA into a caller-provided
/// destination of exactly `width*height*4`. Mirrors `i420_to_rgba_buf`: libyuv
/// names destinations by the little-endian 32-bit word, so `nv12_to_abgr`
/// writes bytes in R, G, B, A order — exactly Slint's `Rgba8Pixel`.
fn nv12_to_rgba_buf(
    buffer: &livekit::webrtc::video_frame::NV12Buffer,
    width: u32,
    height: u32,
    dst: &mut [u8],
) {
    let (y_stride, uv_stride) = buffer.strides();
    let (y_data, uv_data) = buffer.data();
    let dst_stride = width * 4;

    livekit::webrtc::native::yuv_helper::nv12_to_abgr(
        y_data,
        y_stride,
        uv_data,
        uv_stride,
        dst,
        dst_stride,
        width as i32,
        height as i32,
    );
}

/// Convert an I420 buffer to RGBA, reusing `recycled` in place when present
/// (it is uniquely owned and correctly sized — the caller guarantees both).
///
/// `pub(crate)` so the capture path can reuse it to tee the local camera feed
/// into `SharedVideoFrames` for the self-preview tile.
pub(crate) fn convert_i420_into(
    recycled: Option<Arc<Vec<u8>>>,
    buffer: &livekit::webrtc::video_frame::I420Buffer,
    width: u32,
    height: u32,
    rgba_len: usize,
) -> Arc<Vec<u8>> {
    match recycled {
        Some(mut arc) => {
            // SAFETY of correctness: caller only hands us an Arc that passed
            // `Arc::get_mut` (strong_count == 1) and whose len == rgba_len.
            let dst = Arc::get_mut(&mut arc).expect("recycled buffer uniquely owned");
            i420_to_rgba_buf(buffer, width, height, dst.as_mut_slice());
            arc
        }
        None => {
            let mut rgba = vec![0u8; rgba_len];
            i420_to_rgba_buf(buffer, width, height, &mut rgba);
            Arc::new(rgba)
        }
    }
}

/// Convert an NV12 buffer to RGBA for playback or camera preview, reusing
/// `recycled` in place when present.
pub(crate) fn convert_nv12_into(
    recycled: Option<Arc<Vec<u8>>>,
    buffer: &livekit::webrtc::video_frame::NV12Buffer,
    width: u32,
    height: u32,
    rgba_len: usize,
) -> Arc<Vec<u8>> {
    match recycled {
        Some(mut arc) => {
            let dst = Arc::get_mut(&mut arc).expect("recycled buffer uniquely owned");
            nv12_to_rgba_buf(buffer, width, height, dst.as_mut_slice());
            arc
        }
        None => {
            let mut rgba = vec![0u8; rgba_len];
            nv12_to_rgba_buf(buffer, width, height, &mut rgba);
            Arc::new(rgba)
        }
    }
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
