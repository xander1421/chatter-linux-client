//! Media handling layer.
//!
//! Image, audio, video processing and playback.
//! Uses platform-native APIs for video decoding:
//! - Linux: GStreamer
//! - Android: MediaCodec (future)
//! - iOS/macOS: VideoToolbox (future)
//! - Windows: Media Foundation (future)

pub mod audio_player;
pub mod audio_recorder;
pub mod code_preview;
pub mod decoder;
pub mod embed;
pub mod file_picker;
pub mod gif;
pub mod image_processor;
pub mod mime_detector;
pub mod pdf_viewer;
pub mod thumbnail_cache;
pub mod video_playback;
pub mod video_player;
pub mod video_processor;
pub mod video_recorder;
pub mod waveform;

// Re-exports for convenient access
pub use audio_player::{AudioPlayer, PlaybackState};
pub use audio_recorder::{AudioRecorder, RecordingState};
pub use code_preview::{CodePreview, HighlightedLine, HighlightedSegment, SegmentStyle};
pub use file_picker::{FileFilter, FilePicker, FilePickerConfig};
pub use gif::{GifCache, GifClient, GifPicker, GifResult};
pub use image_processor::{ImageProcessor, DEFAULT_THUMBNAIL_SIZE};
pub use mime_detector::MimeDetector;
pub use pdf_viewer::PdfViewer;
pub use thumbnail_cache::ThumbnailCache;
pub use video_playback::{
    format_duration, rgb_to_slint_image, PlayerState, SharedPlayerState,
    VideoPlayer as InAppVideoPlayer,
};
pub use video_player::VideoPlayer;
pub use video_processor::{
    extract_video_thumbnail, extract_video_thumbnail_from_memory,
    get_video_dimensions, get_video_dimensions_from_memory,
    get_video_duration, get_video_duration_from_memory,
};
pub use video_recorder::VideoRecorder;
pub use waveform::{extract_waveform, extract_waveform_from_memory, WAVEFORM_BAR_COUNT};

// Embed extractors
pub use embed::{LinkPreviewGenerator, UrlUnfurler, YouTubeExtractor};
