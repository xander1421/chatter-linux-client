//! Attachment repository.
//!
//! Data access for attachment entities.

use crate::domain::attachment::{Attachment, AttachmentStatus, ContentLocation, FileMetadata, Thumbnail};
use crate::domain::ids::AttachmentId;
use crate::error::StorageError;
use crate::storage::serialization::{db_deserialize, db_serialize};
use crate::storage::Database;
use chrono::{DateTime, Utc};
use rusqlite::params;
use std::sync::Arc;

/// Attachment data repository.
pub struct AttachmentRepository {
    db: Arc<Database>,
}

impl AttachmentRepository {
    /// Create a new AttachmentRepository.
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    // ========================================================================
    // Attachment CRUD
    // ========================================================================

    /// Insert a new attachment.
    pub fn insert(&self, attachment: &Attachment) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        // Use bincode for efficient BLOB storage
        let metadata_blob = db_serialize(&attachment.metadata)?;

        // Extract thumbnail bytes for BLOB storage (don't store in metadata)
        let thumbnail_bytes = attachment.metadata.thumbnail.as_deref();

        conn.execute(
            "INSERT INTO attachments (
                id, metadata_blob, status, local_path, remote_url,
                encryption_key, encryption_iv, thumbnail_bytes, content_location, created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                attachment.id.to_string(),
                metadata_blob,
                status_to_int(attachment.status),
                attachment.local_path,
                attachment.remote_url,
                attachment.encryption_key,
                attachment.encryption_iv,
                thumbnail_bytes,
                content_location_to_int(attachment.content_location),
                attachment.created_at.to_rfc3339(),
            ],
        )?;

        Ok(())
    }

    /// Insert an attachment with encrypted content stored in database.
    ///
    /// This stores the encrypted ciphertext directly in the database for
    /// secure in-memory-only decryption during playback.
    pub fn insert_with_content(
        &self,
        attachment: &Attachment,
        encrypted_content: &[u8],
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        // Use bincode for efficient BLOB storage
        let metadata_blob = db_serialize(&attachment.metadata)?;

        let thumbnail_bytes = attachment.metadata.thumbnail.as_deref();

        conn.execute(
            "INSERT INTO attachments (
                id, metadata_blob, status, local_path, remote_url,
                encryption_key, encryption_iv, thumbnail_bytes,
                encrypted_content, content_location, created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                attachment.id.to_string(),
                metadata_blob,
                status_to_int(attachment.status),
                attachment.local_path,
                attachment.remote_url,
                attachment.encryption_key,
                attachment.encryption_iv,
                thumbnail_bytes,
                encrypted_content,
                content_location_to_int(ContentLocation::InDatabase),
                attachment.created_at.to_rfc3339(),
            ],
        )?;

        Ok(())
    }

    /// Get an attachment by ID.
    pub fn get(&self, id: &AttachmentId) -> Result<Option<Attachment>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, metadata_blob, status, local_path, remote_url,
                    encryption_key, encryption_iv, thumbnail_bytes, content_location, created_at
             FROM attachments WHERE id = ?1",
        )?;

        let result = stmt.query_row(params![id.to_string()], row_to_attachment);

        match result {
            Ok(attachment) => Ok(Some(attachment?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Get encrypted content for an attachment.
    ///
    /// Returns the encrypted ciphertext blob for decryption in memory.
    /// Returns None if content is not stored in database (needs CDN download).
    pub fn get_encrypted_content(&self, id: &AttachmentId) -> Result<Option<Vec<u8>>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT encrypted_content FROM attachments WHERE id = ?1 AND content_location = 1",
        )?;

        let result = stmt.query_row(params![id.to_string()], |row| {
            let content: Option<Vec<u8>> = row.get(0)?;
            Ok(content)
        });

        match result {
            Ok(content) => Ok(content),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Store encrypted content for an existing attachment.
    ///
    /// Updates the attachment with downloaded encrypted ciphertext and
    /// marks content_location as InDatabase.
    pub fn store_encrypted_content(
        &self,
        id: &AttachmentId,
        encrypted_content: &[u8],
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;

        // Set content_location AND status atomically to avoid race condition
        // where user clicks attachment between content stored and status update
        let rows = conn.execute(
            "UPDATE attachments SET
                encrypted_content = ?1,
                content_location = ?2,
                status = ?3
             WHERE id = ?4",
            params![
                encrypted_content,
                content_location_to_int(ContentLocation::InDatabase),
                status_to_int(AttachmentStatus::Ready),
                id.to_string(),
            ],
        )?;

        if rows == 0 {
            return Err(StorageError::NotFound(format!(
                "Attachment {} not found",
                id
            )));
        }

        Ok(())
    }

    /// Update an existing attachment.
    pub fn update(&self, attachment: &Attachment) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        // Use bincode for efficient BLOB storage
        let metadata_blob = db_serialize(&attachment.metadata)?;

        // Extract thumbnail bytes for BLOB storage
        let thumbnail_bytes = attachment.metadata.thumbnail.as_deref();

        let rows = conn.execute(
            "UPDATE attachments SET
                metadata_blob = ?1, status = ?2, local_path = ?3, remote_url = ?4,
                encryption_key = ?5, encryption_iv = ?6, thumbnail_bytes = ?7,
                content_location = ?8
             WHERE id = ?9",
            params![
                metadata_blob,
                status_to_int(attachment.status),
                attachment.local_path,
                attachment.remote_url,
                attachment.encryption_key,
                attachment.encryption_iv,
                thumbnail_bytes,
                content_location_to_int(attachment.content_location),
                attachment.id.to_string(),
            ],
        )?;

        if rows == 0 {
            return Err(StorageError::NotFound(format!(
                "Attachment {} not found",
                attachment.id
            )));
        }

        Ok(())
    }

    /// Delete an attachment by ID.
    pub fn delete(&self, id: &AttachmentId) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "DELETE FROM attachments WHERE id = ?1",
            params![id.to_string()],
        )?;

        if rows == 0 {
            return Err(StorageError::NotFound(format!(
                "Attachment {} not found",
                id
            )));
        }

        Ok(())
    }

    // ========================================================================
    // Queries
    // ========================================================================

    /// List attachments by status.
    pub fn list_by_status(&self, status: AttachmentStatus) -> Result<Vec<Attachment>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, metadata_blob, status, local_path, remote_url,
                    encryption_key, encryption_iv, thumbnail_bytes, content_location, created_at
             FROM attachments WHERE status = ?1
             ORDER BY created_at DESC",
        )?;

        let mut rows = stmt.query(params![status_to_int(status)])?;
        collect_attachments(&mut rows)
    }

    /// List attachments pending upload (Pending or Uploading status).
    ///
    /// Note: Excludes thumbnail_bytes for memory efficiency. Use get() for full data.
    pub fn list_pending_uploads(&self) -> Result<Vec<Attachment>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, metadata_blob, status, local_path, remote_url,
                    encryption_key, encryption_iv, thumbnail_bytes, content_location, created_at
             FROM attachments WHERE status IN (?1, ?2)
             ORDER BY created_at ASC",
        )?;

        let pending = status_to_int(AttachmentStatus::Pending);
        let uploading = status_to_int(AttachmentStatus::Uploading);
        let mut rows = stmt.query(params![pending, uploading])?;
        collect_attachments(&mut rows)
    }

    /// List attachments that need content downloaded from CDN.
    ///
    /// Note: Uses read connection for concurrent access.
    pub fn list_pending_downloads(&self) -> Result<Vec<Attachment>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, metadata_blob, status, local_path, remote_url,
                    encryption_key, encryption_iv, thumbnail_bytes, content_location, created_at
             FROM attachments
             WHERE content_location = 0
               AND remote_url IS NOT NULL
               AND encryption_key IS NOT NULL
               AND status != ?1
             ORDER BY created_at ASC",
        )?;

        let failed = status_to_int(AttachmentStatus::Failed);
        let mut rows = stmt.query(params![failed])?;
        collect_attachments(&mut rows)
    }

    // ========================================================================
    // Thumbnail operations
    // ========================================================================

    /// Get thumbnail for an attachment.
    pub fn get_thumbnail(&self, attachment_id: &AttachmentId) -> Result<Option<Thumbnail>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT attachment_id, local_path, width, height, created_at
             FROM thumbnails WHERE attachment_id = ?1",
        )?;

        let result = stmt.query_row(params![attachment_id.to_string()], row_to_thumbnail);

        match result {
            Ok(thumbnail) => Ok(Some(thumbnail?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Insert or update a thumbnail.
    pub fn upsert_thumbnail(&self, thumbnail: &Thumbnail) -> Result<(), StorageError> {
        let conn = self.db.connection()?;

        conn.execute(
            "INSERT INTO thumbnails (attachment_id, local_path, width, height, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(attachment_id) DO UPDATE SET
                 local_path = excluded.local_path,
                 width = excluded.width,
                 height = excluded.height,
                 created_at = excluded.created_at",
            params![
                thumbnail.attachment_id.to_string(),
                thumbnail.local_path,
                thumbnail.width,
                thumbnail.height,
                thumbnail.created_at.to_rfc3339(),
            ],
        )?;

        Ok(())
    }

    /// Delete a thumbnail.
    pub fn delete_thumbnail(&self, attachment_id: &AttachmentId) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "DELETE FROM thumbnails WHERE attachment_id = ?1",
            params![attachment_id.to_string()],
        )?;
        // Don't error if not found - idempotent delete
        Ok(())
    }
}

// ============================================================================
// Helper functions
// ============================================================================

/// Convert AttachmentStatus to integer for database storage.
fn status_to_int(status: AttachmentStatus) -> i32 {
    match status {
        AttachmentStatus::Pending => 0,
        AttachmentStatus::Uploading => 1,
        AttachmentStatus::Downloading => 6,
        AttachmentStatus::Processing => 2,
        AttachmentStatus::Ready => 3,
        AttachmentStatus::Failed => 4,
        AttachmentStatus::Expired => 5,
    }
}

/// Convert integer to AttachmentStatus.
fn int_to_status(value: i32) -> AttachmentStatus {
    match value {
        0 => AttachmentStatus::Pending,
        1 => AttachmentStatus::Uploading,
        2 => AttachmentStatus::Processing,
        3 => AttachmentStatus::Ready,
        4 => AttachmentStatus::Failed,
        5 => AttachmentStatus::Expired,
        6 => AttachmentStatus::Downloading,
        _ => AttachmentStatus::Pending, // Default fallback
    }
}

/// Convert ContentLocation to integer for database storage.
fn content_location_to_int(location: ContentLocation) -> i32 {
    match location {
        ContentLocation::NotDownloaded => 0,
        ContentLocation::InDatabase => 1,
    }
}

/// Convert integer to ContentLocation.
fn int_to_content_location(value: i32) -> ContentLocation {
    match value {
        1 => ContentLocation::InDatabase,
        _ => ContentLocation::NotDownloaded,
    }
}

/// Parse an RFC3339 datetime string, falling back to now if invalid.
fn parse_datetime(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

/// Convert a database row to Attachment.
fn row_to_attachment(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<Attachment, StorageError>> {
    let id_str: String = row.get(0)?;
    // Read from BLOB column (bincode serialized)
    let metadata_blob: Vec<u8> = row.get(1)?;
    let status_int: i32 = row.get(2)?;
    let local_path: Option<String> = row.get(3)?;
    let remote_url: Option<String> = row.get(4)?;
    let encryption_key: Option<Vec<u8>> = row.get(5)?;
    let encryption_iv: Option<Vec<u8>> = row.get(6)?;
    let thumbnail_bytes: Option<Vec<u8>> = row.get(7)?;
    let content_location_int: i32 = row.get(8).unwrap_or(0); // Default to NotDownloaded for old rows
    let created_at_str: String = row.get(9)?;

    let id: AttachmentId = match id_str.parse() {
        Ok(id) => id,
        Err(e) => return Ok(Err(StorageError::InvalidData(format!("Invalid attachment ID: {}", e)))),
    };

    // Deserialize BLOB field from bincode
    let mut metadata: FileMetadata = match db_deserialize(&metadata_blob) {
        Ok(m) => m,
        Err(e) => return Ok(Err(e)),
    };

    // Load thumbnail bytes from BLOB column (more efficient than metadata)
    metadata.thumbnail = thumbnail_bytes;

    Ok(Ok(Attachment {
        id,
        metadata,
        status: int_to_status(status_int),
        content_location: int_to_content_location(content_location_int),
        local_path,
        remote_url,
        encryption_key,
        encryption_iv,
        created_at: parse_datetime(&created_at_str),
    }))
}

/// Convert a database row to Thumbnail.
fn row_to_thumbnail(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<Thumbnail, StorageError>> {
    let attachment_id_str: String = row.get(0)?;
    let local_path: String = row.get(1)?;
    let width: u32 = row.get(2)?;
    let height: u32 = row.get(3)?;
    let created_at_str: String = row.get(4)?;

    let attachment_id: AttachmentId = match attachment_id_str.parse() {
        Ok(id) => id,
        Err(e) => return Ok(Err(StorageError::InvalidData(format!("Invalid attachment ID: {}", e)))),
    };

    Ok(Ok(Thumbnail {
        attachment_id,
        local_path,
        width,
        height,
        created_at: parse_datetime(&created_at_str),
    }))
}

/// Collect attachments from a Rows iterator.
fn collect_attachments(rows: &mut rusqlite::Rows<'_>) -> Result<Vec<Attachment>, StorageError> {
    let mut attachments = Vec::new();
    while let Some(row) = rows.next()? {
        let attachment_result = row_to_attachment(row)?;
        attachments.push(attachment_result?);
    }
    Ok(attachments)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::serialization::{db_deserialize, db_serialize};

    // ========================================================================
    // FileMetadata bincode serialization tests
    // ========================================================================

    #[test]
    fn test_bincode_file_metadata_roundtrip() {
        let metadata = FileMetadata {
            name: "test_image.jpg".to_string(),
            mime_type: "image/jpeg".to_string(),
            size: 1024 * 1024, // 1MB
            sha256: vec![0u8; 32],
            chunk_size: 0,
            width: Some(1920),
            height: Some(1080),
            duration_seconds: None,
            waveform_samples: None,
            thumbnail: None,
            thumbnail_width: None,
            thumbnail_height: None,
        };

        let blob = db_serialize(&metadata).unwrap();
        let deserialized: FileMetadata = db_deserialize(&blob).unwrap();

        assert_eq!(metadata.name, deserialized.name);
        assert_eq!(metadata.mime_type, deserialized.mime_type);
        assert_eq!(metadata.size, deserialized.size);
        assert_eq!(metadata.width, deserialized.width);
        assert_eq!(metadata.height, deserialized.height);
        assert_eq!(metadata.duration_seconds, deserialized.duration_seconds);
    }

    #[test]
    fn test_bincode_file_metadata_video() {
        let metadata = FileMetadata {
            name: "video.mp4".to_string(),
            mime_type: "video/mp4".to_string(),
            size: 50 * 1024 * 1024, // 50MB
            sha256: vec![0u8; 32],
            chunk_size: 0,
            width: Some(1920),
            height: Some(1080),
            duration_seconds: Some(120.0), // 2 minutes
            waveform_samples: None,
            thumbnail: None,
            thumbnail_width: None,
            thumbnail_height: None,
        };

        let blob = db_serialize(&metadata).unwrap();
        let deserialized: FileMetadata = db_deserialize(&blob).unwrap();

        assert_eq!(metadata.duration_seconds, deserialized.duration_seconds);
    }

    #[test]
    fn test_bincode_file_metadata_audio() {
        let metadata = FileMetadata {
            name: "audio.mp3".to_string(),
            mime_type: "audio/mpeg".to_string(),
            size: 5 * 1024 * 1024, // 5MB
            sha256: vec![0u8; 32],
            chunk_size: 0,
            width: None,
            height: None,
            duration_seconds: Some(180.0), // 3 minutes
            waveform_samples: Some(vec![50u8; 32]),
            thumbnail: None,
            thumbnail_width: None,
            thumbnail_height: None,
        };

        let blob = db_serialize(&metadata).unwrap();
        let deserialized: FileMetadata = db_deserialize(&blob).unwrap();

        assert_eq!(metadata.name, deserialized.name);
        assert_eq!(metadata.width, deserialized.width);
        assert_eq!(metadata.height, deserialized.height);
    }

    #[test]
    fn test_bincode_file_metadata_document() {
        let metadata = FileMetadata {
            name: "document.pdf".to_string(),
            mime_type: "application/pdf".to_string(),
            size: 2 * 1024 * 1024, // 2MB
            sha256: vec![0u8; 32],
            chunk_size: 0,
            width: None,
            height: None,
            duration_seconds: None,
            waveform_samples: None,
            thumbnail: None,
            thumbnail_width: None,
            thumbnail_height: None,
        };

        let blob = db_serialize(&metadata).unwrap();
        let deserialized: FileMetadata = db_deserialize(&blob).unwrap();

        assert_eq!(metadata.name, deserialized.name);
        assert_eq!(metadata.mime_type, deserialized.mime_type);
    }

    // ========================================================================
    // Size comparison tests
    // ========================================================================

    #[test]
    fn test_bincode_smaller_than_json_for_metadata() {
        let metadata = FileMetadata {
            name: "test_image_with_a_very_long_filename_that_is_realistic.jpg".to_string(),
            mime_type: "image/jpeg".to_string(),
            size: 1024 * 1024,
            sha256: vec![0u8; 32],
            chunk_size: 0,
            width: Some(1920),
            height: Some(1080),
            duration_seconds: None,
            waveform_samples: None,
            thumbnail: None,
            thumbnail_width: None,
            thumbnail_height: None,
        };

        let bincode_size = db_serialize(&metadata).unwrap().len();
        let json_size = serde_json::to_vec(&metadata).unwrap().len();

        assert!(bincode_size < json_size, "Bincode should be smaller than JSON");
    }

    // ========================================================================
    // Unicode filename tests
    // ========================================================================

    #[test]
    fn test_bincode_unicode_filename() {
        let metadata = FileMetadata {
            name: "文档_日本語_한국어.pdf".to_string(),
            mime_type: "application/pdf".to_string(),
            size: 1024,
            sha256: vec![0u8; 32],
            chunk_size: 0,
            width: None,
            height: None,
            duration_seconds: None,
            waveform_samples: None,
            thumbnail: None,
            thumbnail_width: None,
            thumbnail_height: None,
        };

        let blob = db_serialize(&metadata).unwrap();
        let deserialized: FileMetadata = db_deserialize(&blob).unwrap();

        assert_eq!(metadata.name, deserialized.name);
    }

    #[test]
    fn test_bincode_emoji_filename() {
        let metadata = FileMetadata {
            name: "photo_📷_🎉.jpg".to_string(),
            mime_type: "image/jpeg".to_string(),
            size: 1024,
            sha256: vec![0u8; 32],
            chunk_size: 0,
            width: Some(640),
            height: Some(480),
            duration_seconds: None,
            waveform_samples: None,
            thumbnail: None,
            thumbnail_width: None,
            thumbnail_height: None,
        };

        let blob = db_serialize(&metadata).unwrap();
        let deserialized: FileMetadata = db_deserialize(&blob).unwrap();

        assert_eq!(metadata.name, deserialized.name);
    }

    // ========================================================================
    // Edge case tests
    // ========================================================================

    #[test]
    fn test_bincode_large_file_size() {
        let metadata = FileMetadata {
            name: "large_video.mp4".to_string(),
            mime_type: "video/mp4".to_string(),
            size: u64::MAX, // Maximum possible file size
            sha256: vec![0u8; 32],
            chunk_size: 0,
            width: Some(3840),
            height: Some(2160),
            duration_seconds: Some(f64::MAX),
            waveform_samples: None,
            thumbnail: None,
            thumbnail_width: None,
            thumbnail_height: None,
        };

        let blob = db_serialize(&metadata).unwrap();
        let deserialized: FileMetadata = db_deserialize(&blob).unwrap();

        assert_eq!(metadata.size, deserialized.size);
        assert_eq!(metadata.duration_seconds, deserialized.duration_seconds);
    }

    #[test]
    fn test_bincode_empty_filename() {
        let metadata = FileMetadata {
            name: "".to_string(),
            mime_type: "application/octet-stream".to_string(),
            size: 0,
            sha256: vec![],
            chunk_size: 0,
            width: None,
            height: None,
            duration_seconds: None,
            waveform_samples: None,
            thumbnail: None,
            thumbnail_width: None,
            thumbnail_height: None,
        };

        let blob = db_serialize(&metadata).unwrap();
        let deserialized: FileMetadata = db_deserialize(&blob).unwrap();

        assert_eq!(metadata.name, deserialized.name);
        assert_eq!(metadata.size, deserialized.size);
    }

    // ========================================================================
    // Error handling tests
    // ========================================================================

    #[test]
    fn test_bincode_invalid_metadata_blob() {
        let invalid_blob = vec![0xFF, 0x00, 0xFF, 0x00];

        let result: Result<FileMetadata, _> = db_deserialize(&invalid_blob);
        assert!(result.is_err());
    }

    #[test]
    fn test_bincode_truncated_metadata_blob() {
        let metadata = FileMetadata {
            name: "test.jpg".to_string(),
            mime_type: "image/jpeg".to_string(),
            size: 1024,
            sha256: vec![0u8; 32],
            chunk_size: 0,
            width: Some(640),
            height: Some(480),
            duration_seconds: None,
            waveform_samples: None,
            thumbnail: None,
            thumbnail_width: None,
            thumbnail_height: None,
        };

        let mut blob = db_serialize(&metadata).unwrap();
        blob.truncate(blob.len() / 2);

        let result: Result<FileMetadata, _> = db_deserialize(&blob);
        assert!(result.is_err());
    }
}
