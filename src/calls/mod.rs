//! Call management module.
//!
//! Provides video/audio calling functionality via LiveKit integration.

mod room_manager;
mod audio_capture;
mod video_capture;
mod screen_capture;

pub use room_manager::{RoomManager, RoomState, RoomEvent as LiveKitRoomEvent};
pub use audio_capture::{AudioCapture, AudioDevice, AudioOutputDevice};
pub use video_capture::{VideoCapture, VideoDevice, VideoCaptureConfig};
pub use screen_capture::{ScreenCapture, ScreenSource, ScreenSourceType, ScreenCaptureConfig};
