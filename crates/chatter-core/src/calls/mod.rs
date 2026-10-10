//! Call management module.
//!
//! Provides video/audio calling functionality via LiveKit integration.

#[cfg(target_os = "android")]
pub mod android_camera;
mod audio_capture;
mod audio_playback;
pub mod audio_route;
mod room_manager;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
mod screen_capture;
pub mod shared_frames;
pub mod tiles;
pub(crate) mod timing;
mod video_capture;
mod video_playback;

pub use audio_capture::{AudioCapture, AudioDevice};
pub use audio_playback::AudioPlayback;
pub use room_manager::{RoomEvent as LiveKitRoomEvent, RoomManager, RoomState};
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub use screen_capture::{ScreenCapture, ScreenCaptureConfig, ScreenSource, ScreenSourceType};
pub use shared_frames::{SharedVideoFrames, VideoFrameData};
pub use video_capture::{VideoCapture, VideoCaptureConfig, VideoDevice};
pub use video_playback::{VideoPlayback, VideoStreamInfo, screen_share_frame_id};
