//! Call management module.
//!
//! Provides video/audio calling functionality via LiveKit integration.

mod room_manager;
mod audio_capture;
mod audio_playback;
mod video_capture;
mod video_playback;
mod screen_capture;
mod shared_frames;

pub use room_manager::{RoomManager, RoomState, RoomEvent as LiveKitRoomEvent};
pub use audio_capture::{AudioCapture, AudioDevice, AudioOutputDevice as AudioInputDevice};
pub use audio_playback::{AudioPlayback, AudioOutputDevice};
pub use video_capture::{VideoCapture, VideoDevice, VideoCaptureConfig};
pub use video_playback::{VideoPlayback, VideoStreamInfo};
pub use screen_capture::{ScreenCapture, ScreenSource, ScreenSourceType, ScreenCaptureConfig};
pub use shared_frames::{SharedVideoFrames, VideoFrameData};
