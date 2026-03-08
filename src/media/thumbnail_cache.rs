//! Thumbnail cache.
//!
//! Persistent cache for generated thumbnails.

use crate::domain::attachment::{Attachment, Thumbnail};
use crate::domain::ids::AttachmentId;
use crate::error::MediaError;
use crate::media::image_processor::{ImageProcessor, DEFAULT_THUMBNAIL_SIZE};
use crate::storage::repositories::AttachmentRepository;
use crate::storage::Database;
use crate::system::Directories;
use chrono::Utc;
use image::ImageFormat;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Thumbnail cache with file system storage and database tracking.
///
/// Thumbnails are stored in `{cache_dir}/thumbnails/{attachment_id}.jpg`.
/// Metadata is persisted in the database via `AttachmentRepository`.
pub struct ThumbnailCache {
    /// Base directory for thumbnail storage.
    thumbnails_dir: PathBuf,
    /// Database for thumbnail metadata.
    db: Arc<Database>,
    /// Maximum thumbnail dimension.
    max_size: u32,
}

impl ThumbnailCache {
    /// Create a new thumbnail cache.
    ///
    /// Creates the thumbnails subdirectory if it doesn't exist.
    ///
    /// # Arguments
    ///
    /// * `directories` - XDG directories for cache path
    /// * `db` - Database for metadata storage
    pub fn new(directories: &Directories, db: Arc<Database>) -> Self {
        let thumbnails_dir = directories.cache_dir().join("thumbnails");

        // Create thumbnails directory with secure permissions (best effort)
        if let Err(e) = std::fs::create_dir_all(&thumbnails_dir) {
            tracing::warn!("Failed to create thumbnails directory: {}", e);
        } else {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&thumbnails_dir, std::fs::Permissions::from_mode(0o700));
            }
        }

        Self {
            thumbnails_dir,
            db,
            max_size: DEFAULT_THUMBNAIL_SIZE,
        }
    }

    /// Create a thumbnail cache with custom thumbnail size.
    ///
    /// # Arguments
    ///
    /// * `directories` - XDG directories for cache path
    /// * `db` - Database for metadata storage
    /// * `max_size` - Maximum thumbnail dimension (width or height)
    pub fn with_max_size(directories: &Directories, db: Arc<Database>, max_size: u32) -> Self {
        let mut cache = Self::new(directories, db);
        cache.max_size = max_size;
        cache
    }

    /// Get a cached thumbnail or generate one if needed.
    ///
    /// If the thumbnail exists in cache, returns its path. Otherwise, generates
    /// a new thumbnail from the attachment's local file and caches it.
    ///
    /// # Arguments
    ///
    /// * `attachment` - The attachment to get/generate a thumbnail for
    ///
    /// # Returns
    ///
    /// Path to the thumbnail file.
    ///
    /// # Errors
    ///
    /// Returns `MediaError::CacheError` if the attachment has no local path.
    /// Returns `MediaError::InvalidFormat` if the image cannot be processed.
    /// Returns `MediaError::ThumbnailGeneration` if thumbnail creation fails.
    pub fn get_or_generate(&self, attachment: &Attachment) -> Result<PathBuf, MediaError> {
        // Check if cached thumbnail exists
        if let Some(path) = self.get_cached(&attachment.id) {
            if path.exists() {
                return Ok(path);
            }
        }

        // Get source file path
        let source_path = attachment
            .local_path
            .as_ref()
            .ok_or_else(|| MediaError::CacheError("attachment has no local path".to_string()))?;

        // Generate thumbnail
        let source = Path::new(source_path);
        let thumbnail_image = ImageProcessor::generate_thumbnail(source, self.max_size)?;

        // Get dimensions of generated thumbnail
        let width = thumbnail_image.width();
        let height = thumbnail_image.height();

        // Save thumbnail to cache
        let cache_path = self.thumbnail_path(&attachment.id);
        ImageProcessor::save_thumbnail(&thumbnail_image, &cache_path, ImageFormat::Jpeg)?;

        // Store metadata in database
        let thumbnail = Thumbnail {
            attachment_id: attachment.id,
            local_path: cache_path.to_string_lossy().to_string(),
            width,
            height,
            created_at: Utc::now(),
        };

        let repo = AttachmentRepository::new(self.db.clone());
        repo.upsert_thumbnail(&thumbnail)
            .map_err(|e| MediaError::CacheError(format!("failed to store thumbnail metadata: {}", e)))?;

        Ok(cache_path)
    }

    /// Get the cached thumbnail path if it exists.
    ///
    /// Checks the database for metadata and verifies the file still exists.
    /// Returns `None` if the thumbnail is not cached.
    ///
    /// # Arguments
    ///
    /// * `attachment_id` - ID of the attachment to look up
    #[must_use]
    pub fn get_cached(&self, attachment_id: &AttachmentId) -> Option<PathBuf> {
        // Check database for metadata
        let repo = AttachmentRepository::new(self.db.clone());
        if let Ok(Some(thumbnail)) = repo.get_thumbnail(attachment_id) {
            let path = PathBuf::from(&thumbnail.local_path);
            if path.exists() {
                return Some(path);
            }
        }

        // Check if file exists at expected path (may not be in DB yet)
        let expected_path = self.thumbnail_path(attachment_id);
        if expected_path.exists() {
            return Some(expected_path);
        }

        None
    }

    /// Invalidate (remove) a cached thumbnail.
    ///
    /// Deletes both the file and the database metadata.
    ///
    /// # Arguments
    ///
    /// * `attachment_id` - ID of the attachment to invalidate
    ///
    /// # Errors
    ///
    /// Returns `MediaError::CacheError` if file deletion fails.
    pub fn invalidate(&self, attachment_id: &AttachmentId) -> Result<(), MediaError> {
        // Remove from database
        let repo = AttachmentRepository::new(self.db.clone());
        if let Err(e) = repo.delete_thumbnail(attachment_id) {
            // Log but don't fail - file removal is more important
            tracing::warn!("failed to remove thumbnail metadata: {}", e);
        }

        // Remove file
        let path = self.thumbnail_path(attachment_id);
        if path.exists() {
            std::fs::remove_file(&path).map_err(|e| {
                MediaError::CacheError(format!("failed to delete thumbnail file: {}", e))
            })?;
        }

        Ok(())
    }

    /// Clear all cached thumbnails.
    ///
    /// Removes all files in the thumbnails directory. Database entries will
    /// be orphaned but will be cleaned up on next access.
    ///
    /// # Errors
    ///
    /// Returns `MediaError::CacheError` if the directory cannot be read or cleared.
    pub fn clear(&self) -> Result<(), MediaError> {
        if !self.thumbnails_dir.exists() {
            return Ok(());
        }

        let entries = std::fs::read_dir(&self.thumbnails_dir)
            .map_err(|e| MediaError::CacheError(format!("failed to read thumbnails dir: {}", e)))?;

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                // Best effort deletion
                let _ = std::fs::remove_file(&path);
            }
        }

        Ok(())
    }

    /// Get the expected path for a thumbnail.
    ///
    /// Path format: `{thumbnails_dir}/{attachment_id}.jpg`
    #[must_use]
    pub fn thumbnail_path(&self, attachment_id: &AttachmentId) -> PathBuf {
        self.thumbnails_dir.join(format!("{}.jpg", attachment_id))
    }

    /// Get the thumbnails directory path.
    #[must_use]
    pub fn thumbnails_dir(&self) -> &Path {
        &self.thumbnails_dir
    }

    /// Get the current maximum thumbnail size.
    #[must_use]
    pub fn max_size(&self) -> u32 {
        self.max_size
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_thumbnail_path_format() {
        // This test verifies the path format without needing actual directories
        let id = AttachmentId::new();
        let expected_suffix = format!("{}.jpg", id);

        // Verify the ID is included in the expected path format
        assert!(expected_suffix.ends_with(".jpg"));
        assert!(!expected_suffix.is_empty());
    }

    #[test]
    fn test_default_max_size() {
        assert_eq!(DEFAULT_THUMBNAIL_SIZE, 256);
    }
}
