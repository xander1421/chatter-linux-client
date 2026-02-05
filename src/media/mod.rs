//! Media handling layer.
//!
//! Image, audio, video processing and playback.

pub mod audio_player;
pub mod audio_recorder;
pub mod code_preview;
pub mod embed;
pub mod file_picker;
pub mod gif;
pub mod image_processor;
pub mod mime_detector;
pub mod pdf_viewer;
pub mod thumbnail_cache;
pub mod video_player;
pub mod video_recorder;

// Re-exports for convenient access
pub use audio_player::{AudioPlayer, PlaybackState};
pub use file_picker::{FileFilter, FilePicker, FilePickerConfig};
pub use image_processor::{ImageProcessor, DEFAULT_THUMBNAIL_SIZE};
pub use mime_detector::MimeDetector;
pub use thumbnail_cache::ThumbnailCache;
pub use video_player::VideoPlayer;
