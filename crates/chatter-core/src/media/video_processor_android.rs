//! Android attachment metadata. The UI installs the platform extractor once
//! during startup; the attachment service remains responsible for send metadata.

use std::path::Path;
use std::sync::OnceLock;

use crate::error::MediaError;

/// A bounded thumbnail and the source video's dimensions and duration.
pub struct VideoMetadata {
    pub thumbnail: Vec<u8>,
    pub thumbnail_width: u32,
    pub thumbnail_height: u32,
    pub width: u32,
    pub height: u32,
    pub duration_ms: u64,
}

type Extractor = fn(&Path) -> Result<VideoMetadata, String>;
static EXTRACTOR: OnceLock<Extractor> = OnceLock::new();

pub fn register_video_metadata_extractor(extractor: Extractor) {
    let _ = EXTRACTOR.set(extractor);
}

pub fn analyze_video(path: &Path) -> Result<VideoMetadata, MediaError> {
    let extractor = EXTRACTOR.get().ok_or_else(|| {
        MediaError::PlaybackError("Android video metadata extractor unavailable".into())
    })?;
    let result = extractor(path).map_err(MediaError::PlaybackError)?;
    if result.thumbnail.len() > 256 * 1024
        || (!result.thumbnail.is_empty()
            && (result.thumbnail_width == 0 || result.thumbnail_height == 0))
        || result.thumbnail_width > 384
        || result.thumbnail_height > 384
        || result.width == 0
        || result.height == 0
        || result.width > 8192
        || result.height > 8192
    {
        return Err(MediaError::InvalidFormat(
            "invalid Android video metadata".into(),
        ));
    }
    Ok(result)
}
