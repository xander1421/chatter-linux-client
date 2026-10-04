//! Media handling layer.
//!
//! Image, audio, video processing and playback.
//! Uses GStreamer for cross-platform video:
//! - Desktop: Linux, Windows, macOS
//! - Mobile: Android, iOS (via GStreamer SDK)
//! - Hardware acceleration on all platforms

pub mod audio_player;
pub mod audio_recorder;
pub mod decoder;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub mod file_picker;
pub mod gif;
pub mod image_processor;
pub mod video_playback;
#[cfg(feature = "desktop-gstreamer")]
pub mod video_processor;
pub mod waveform;

pub use audio_player::{AudioPlayer, PlaybackState};
pub use audio_recorder::{AudioRecorder, RecordingState};
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub use file_picker::{FileFilter, FilePicker, FilePickerConfig};
pub use image_processor::{ImageProcessor, DEFAULT_THUMBNAIL_SIZE};
pub use video_playback::{
    format_duration, PlayerState, SharedPlayerState,
    VideoPlayer as InAppVideoPlayer,
};
#[cfg(feature = "desktop-gstreamer")]
pub use video_processor::{
    extract_video_thumbnail, extract_video_thumbnail_from_memory,
    get_video_dimensions, get_video_dimensions_from_memory,
    get_video_duration, get_video_duration_from_memory,
};
pub use waveform::{extract_waveform, extract_waveform_from_memory, WAVEFORM_BAR_COUNT};

