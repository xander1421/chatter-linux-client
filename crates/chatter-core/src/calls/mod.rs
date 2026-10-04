//! Call management module.
//!
//! Provides video/audio calling functionality via LiveKit integration.

mod room_manager;
mod audio_capture;
mod audio_playback;
pub mod audio_route;
mod video_capture;
mod video_playback;
#[cfg(target_os = "android")]
pub mod android_camera;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
mod screen_capture;
pub mod shared_frames;

pub use room_manager::{RoomManager, RoomState, RoomEvent as LiveKitRoomEvent};
pub use audio_capture::{AudioCapture, AudioDevice};
pub use audio_playback::AudioPlayback;
pub use video_capture::{VideoCapture, VideoDevice, VideoCaptureConfig};
pub use video_playback::{VideoPlayback, VideoStreamInfo, screen_share_frame_id};
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub use screen_capture::{ScreenCapture, ScreenSource, ScreenSourceType, ScreenCaptureConfig};
pub use shared_frames::{SharedVideoFrames, VideoFrameData};
