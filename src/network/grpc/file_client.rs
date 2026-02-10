//! File gRPC client.
//!
//! Client for file service operations including file upload/download,
//! multipart uploads, file sharing, and attachment management.

use tokio_stream::{Stream, StreamExt};
use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;

use crate::error::NetworkError;
use crate::network::grpc::interceptors::AuthInterceptor;
use crate::proto::chat_v1::{
    file_service_client::FileServiceClient, AbortUploadRequest, CompleteUploadRequest,
    ConfirmAttachmentUploadRequest, ConfirmAttachmentUploadResponse, FileChunk, FileMetadata,
    FilePermission, FileRequest, GetAttachmentDownloadUrlRequest, GetAttachmentDownloadUrlResponse,
    GetAttachmentUploadUrlRequest, GetAttachmentUploadUrlResponse, GetFilePermissionsRequest,
    GetFilePermissionsResponse, GetStorageStatsRequest, InitiateUploadRequest,
    ListChannelFilesRequest, ListChannelFilesResponse, ListSharedFilesRequest,
    ListSharedFilesResponse, RevokeFileAccessRequest, SearchFilesRequest, SearchFilesResponse,
    ShareFileRequest, StorageStats, UploadChunkRequest, UploadChunkResponse, UploadSession,
};

/// File service client wrapper.
///
/// Wraps the generated FileServiceClient with ergonomic APIs and error mapping.
///
/// # Example
///
/// ```ignore
/// let pool = GrpcClientPool::new("http://localhost:50051")?;
/// pool.connect().await?;
///
/// let mut file_client = FileClient::new(pool.channel(), pool.auth_clone());
/// let metadata = file_client.upload_file(chunks_stream).await?;
/// ```
pub struct FileClient {
    inner: FileServiceClient<InterceptedService<Channel, AuthInterceptor>>,
}

impl FileClient {
    /// Create a new FileClient from pool components.
    pub fn new(channel: Channel, interceptor: AuthInterceptor) -> Self {
        Self {
            inner: FileServiceClient::with_interceptor(channel, interceptor),
        }
    }

    // ==================== Core File RPCs ====================

    /// Download a file (server streaming).
    ///
    /// Returns a stream of file chunks for the requested file.
    ///
    /// # Arguments
    ///
    /// * `file_id` - The ID of the file to download
    pub async fn download_file(
        &mut self,
        file_id: &str,
    ) -> Result<impl Stream<Item = Result<FileChunk, NetworkError>>, NetworkError> {
        let request = FileRequest {
            file_id: file_id.to_string(),
        };

        let response = self
            .inner
            .download_file(request)
            .await
            .map_err(|s| NetworkError::from_status(&s))?;

        Ok(response
            .into_inner()
            .map(|r| r.map_err(|s| NetworkError::from_status(&s))))
    }

    /// Upload a file (client streaming).
    ///
    /// Sends a stream of file chunks and returns the resulting file metadata.
    ///
    /// # Arguments
    ///
    /// * `chunks` - Stream of FileChunk to upload
    pub async fn upload_file(
        &mut self,
        chunks: impl Stream<Item = FileChunk> + Send + 'static,
    ) -> Result<FileMetadata, NetworkError> {
        let request = tonic::Request::new(chunks);

        self.inner
            .upload_file(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Upload a file with metadata headers (client streaming).
    ///
    /// Sends a stream of file chunks with channel-id, file-name, and mime-type headers.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Target channel ID (required for permission check)
    /// * `filename` - Original filename
    /// * `mime_type` - MIME type of the file
    /// * `file_size` - Size of the file in bytes
    /// * `chunks` - Stream of FileChunk to upload
    pub async fn upload_file_with_metadata(
        &mut self,
        channel_id: &str,
        filename: &str,
        mime_type: &str,
        file_size: u64,
        chunks: impl Stream<Item = FileChunk> + Send + 'static,
    ) -> Result<FileMetadata, NetworkError> {
        tracing::debug!(
            channel_id = %channel_id,
            filename = %filename,
            mime_type = %mime_type,
            file_size = %file_size,
            "Starting file upload with metadata"
        );

        let mut request = tonic::Request::new(chunks);

        // Add required metadata headers
        request.metadata_mut().insert(
            "channel-id",
            channel_id.parse().map_err(|_| NetworkError::InvalidRequest("invalid channel-id".to_string()))?,
        );
        request.metadata_mut().insert(
            "file-name",
            filename.parse().map_err(|_| NetworkError::InvalidRequest("invalid file-name".to_string()))?,
        );
        request.metadata_mut().insert(
            "mime-type",
            mime_type.parse().map_err(|_| NetworkError::InvalidRequest("invalid mime-type".to_string()))?,
        );
        // NOTE: Do NOT set content-length for streaming requests!
        // The wire format includes protobuf framing overhead, so the actual
        // bytes sent will exceed the raw file size and cause PROTOCOL_ERROR.
        // The file_size is passed as a custom header for server-side validation.
        request.metadata_mut().insert(
            "x-file-size",
            file_size.to_string().parse().map_err(|_| NetworkError::InvalidRequest("invalid file-size".to_string()))?,
        );

        tracing::debug!("Sending upload request to server...");
        let start = std::time::Instant::now();

        let result = self.inner.upload_file(request).await;
        let elapsed = start.elapsed();

        match &result {
            Ok(response) => {
                tracing::debug!(
                    elapsed_ms = %elapsed.as_millis(),
                    file_id = %response.get_ref().id,
                    "Upload completed successfully"
                );
            }
            Err(status) => {
                tracing::error!(
                    elapsed_ms = %elapsed.as_millis(),
                    code = ?status.code(),
                    message = %status.message(),
                    details = ?status.details(),
                    source = ?std::error::Error::source(status),
                    "Upload failed with gRPC error"
                );
            }
        }

        result
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Delete a file.
    ///
    /// # Arguments
    ///
    /// * `file_id` - The ID of the file to delete
    pub async fn delete_file(&mut self, file_id: &str) -> Result<(), NetworkError> {
        let request = FileRequest {
            file_id: file_id.to_string(),
        };

        self.inner
            .delete_file(request)
            .await
            .map(|_| ())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// List files in a channel with pagination.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - The channel to list files from
    /// * `limit` - Max number of files to return
    /// * `offset` - Pagination offset
    pub async fn list_channel_files(
        &mut self,
        channel_id: &str,
        limit: i32,
        offset: i32,
    ) -> Result<ListChannelFilesResponse, NetworkError> {
        let request = ListChannelFilesRequest {
            channel_id: channel_id.to_string(),
            limit,
            offset,
        };

        self.inner
            .list_channel_files(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    // ==================== Multipart Upload RPCs ====================

    /// Initiate a resumable multipart upload.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Channel to upload to
    /// * `filename` - Name of the file
    /// * `mime_type` - MIME type of the file
    /// * `total_size` - Total size in bytes
    /// * `chunk_size` - Size of each chunk in bytes
    pub async fn initiate_upload(
        &mut self,
        channel_id: &str,
        filename: &str,
        mime_type: &str,
        total_size: u64,
        chunk_size: u32,
    ) -> Result<UploadSession, NetworkError> {
        let request = InitiateUploadRequest {
            channel_id: channel_id.to_string(),
            file_name: filename.to_string(),
            mime_type: mime_type.to_string(),
            total_size,
            chunk_size,
        };

        self.inner
            .initiate_upload(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Upload a single chunk.
    ///
    /// # Arguments
    ///
    /// * `transfer_id` - The upload session transfer ID
    /// * `sequence` - Chunk sequence number
    /// * `data` - Chunk data
    /// * `sha256` - SHA-256 hash of the chunk data
    pub async fn upload_chunk(
        &mut self,
        transfer_id: &str,
        sequence: u32,
        data: Vec<u8>,
        sha256: Vec<u8>,
    ) -> Result<UploadChunkResponse, NetworkError> {
        let request = UploadChunkRequest {
            transfer_id: transfer_id.to_string(),
            sequence,
            data,
            sha256,
        };

        self.inner
            .upload_chunk(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Complete the multipart upload.
    ///
    /// # Arguments
    ///
    /// * `transfer_id` - The upload session transfer ID
    /// * `file_sha256` - SHA-256 hash of the complete file
    pub async fn complete_upload(
        &mut self,
        transfer_id: &str,
        file_sha256: Vec<u8>,
    ) -> Result<FileMetadata, NetworkError> {
        let request = CompleteUploadRequest {
            transfer_id: transfer_id.to_string(),
            file_sha256,
        };

        self.inner
            .complete_upload(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Abort an in-progress upload.
    ///
    /// # Arguments
    ///
    /// * `transfer_id` - The upload session transfer ID
    pub async fn abort_upload(&mut self, transfer_id: &str) -> Result<(), NetworkError> {
        let request = AbortUploadRequest {
            transfer_id: transfer_id.to_string(),
        };

        self.inner
            .abort_upload(request)
            .await
            .map(|_| ())
            .map_err(|s| NetworkError::from_status(&s))
    }

    // ==================== Sharing RPCs ====================

    /// Share a file with another user.
    ///
    /// # Arguments
    ///
    /// * `file_id` - The file to share
    /// * `shared_with_id` - User ID to share with
    /// * `permission` - Permission level to grant
    /// * `expires_at` - Optional expiration timestamp (0 means no expiration)
    pub async fn share_file(
        &mut self,
        file_id: &str,
        shared_with_id: &str,
        permission: FilePermission,
        expires_at: i64,
    ) -> Result<(), NetworkError> {
        let request = ShareFileRequest {
            file_id: file_id.to_string(),
            shared_with_id: shared_with_id.to_string(),
            permission: permission.into(),
            expires_at,
        };

        self.inner
            .share_file(request)
            .await
            .map(|_| ())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Revoke file access from a user.
    ///
    /// # Arguments
    ///
    /// * `file_id` - The file to revoke access to
    /// * `shared_with_id` - User ID to revoke access from
    pub async fn revoke_file_access(
        &mut self,
        file_id: &str,
        shared_with_id: &str,
    ) -> Result<(), NetworkError> {
        let request = RevokeFileAccessRequest {
            file_id: file_id.to_string(),
            shared_with_id: shared_with_id.to_string(),
        };

        self.inner
            .revoke_file_access(request)
            .await
            .map(|_| ())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// List files shared with a user.
    ///
    /// # Arguments
    ///
    /// * `user_id` - Optional user ID (defaults to current user)
    /// * `limit` - Max number of files to return
    /// * `offset` - Pagination offset
    pub async fn list_shared_files(
        &mut self,
        user_id: Option<&str>,
        limit: i32,
        offset: i32,
    ) -> Result<ListSharedFilesResponse, NetworkError> {
        let request = ListSharedFilesRequest {
            user_id: user_id.unwrap_or("").to_string(),
            limit,
            offset,
        };

        self.inner
            .list_shared_files(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Get all permissions for a file.
    ///
    /// # Arguments
    ///
    /// * `file_id` - The file to get permissions for
    pub async fn get_file_permissions(
        &mut self,
        file_id: &str,
    ) -> Result<GetFilePermissionsResponse, NetworkError> {
        let request = GetFilePermissionsRequest {
            file_id: file_id.to_string(),
        };

        self.inner
            .get_file_permissions(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    // ==================== Search and Stats RPCs ====================

    /// Search files with filters.
    ///
    /// # Arguments
    ///
    /// * `query` - Search query
    /// * `channel_id` - Optional channel filter
    /// * `author_id` - Optional author filter
    /// * `mime_type` - Optional MIME type filter
    /// * `limit` - Max number of results to return
    /// * `offset` - Pagination offset
    pub async fn search_files(
        &mut self,
        query: &str,
        channel_id: Option<&str>,
        author_id: Option<&str>,
        mime_type: Option<&str>,
        limit: i32,
        offset: i32,
    ) -> Result<SearchFilesResponse, NetworkError> {
        let request = SearchFilesRequest {
            query: query.to_string(),
            channel_id: channel_id.unwrap_or("").to_string(),
            author_id: author_id.unwrap_or("").to_string(),
            mime_type: mime_type.unwrap_or("").to_string(),
            limit,
            offset,
            search_in_content: false,
            content_only: false,
            file_extensions: vec![],
        };

        self.inner
            .search_files(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Get storage statistics for a user.
    ///
    /// # Arguments
    ///
    /// * `user_id` - Optional user ID (defaults to current user)
    pub async fn get_storage_stats(
        &mut self,
        user_id: Option<&str>,
    ) -> Result<StorageStats, NetworkError> {
        let request = GetStorageStatsRequest {
            user_id: user_id.unwrap_or("").to_string(),
        };

        self.inner
            .get_storage_stats(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    // ==================== Attachment URL RPCs ====================

    /// Get a presigned upload URL for an attachment.
    ///
    /// # Arguments
    ///
    /// * `encrypted_size` - Size of the encrypted attachment in bytes
    pub async fn get_attachment_upload_url(
        &mut self,
        encrypted_size: u64,
    ) -> Result<GetAttachmentUploadUrlResponse, NetworkError> {
        let request = GetAttachmentUploadUrlRequest { encrypted_size };

        self.inner
            .get_attachment_upload_url(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Confirm an attachment upload completed.
    ///
    /// # Arguments
    ///
    /// * `cdn_key` - The CDN key from the upload URL response
    pub async fn confirm_attachment_upload(
        &mut self,
        cdn_key: &str,
    ) -> Result<ConfirmAttachmentUploadResponse, NetworkError> {
        let request = ConfirmAttachmentUploadRequest {
            cdn_key: cdn_key.to_string(),
        };

        self.inner
            .confirm_attachment_upload(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Get a presigned download URL for an attachment.
    ///
    /// # Arguments
    ///
    /// * `cdn_key` - The CDN key of the attachment to download
    pub async fn get_attachment_download_url(
        &mut self,
        cdn_key: &str,
    ) -> Result<GetAttachmentDownloadUrlResponse, NetworkError> {
        let request = GetAttachmentDownloadUrlRequest {
            cdn_key: cdn_key.to_string(),
        };

        self.inner
            .get_attachment_download_url(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Note: Full integration tests require a running server.
    // These tests verify compilation and basic structure.

    #[test]
    fn test_file_client_construction_types() {
        // This test verifies the type signatures compile correctly.
        // We can't actually create a client without a real channel,
        // but we verify the wrapper pattern is correct.
        fn _verify_types(channel: Channel, interceptor: AuthInterceptor) {
            let _client = FileClient::new(channel, interceptor);
        }
    }

    #[test]
    fn test_file_request_construction() {
        // Verify FileRequest struct can be constructed
        let request = FileRequest {
            file_id: "file-123".to_string(),
        };
        assert_eq!(request.file_id, "file-123");
    }

    #[test]
    fn test_initiate_upload_request_construction() {
        // Verify InitiateUploadRequest struct can be constructed
        let request = InitiateUploadRequest {
            channel_id: "channel-123".to_string(),
            file_name: "test.txt".to_string(),
            mime_type: "text/plain".to_string(),
            total_size: 1024,
            chunk_size: 256,
        };
        assert_eq!(request.file_name, "test.txt");
        assert_eq!(request.total_size, 1024);
        assert_eq!(request.chunk_size, 256);
    }

    #[test]
    fn test_share_file_request_construction() {
        // Verify ShareFileRequest struct can be constructed
        let request = ShareFileRequest {
            file_id: "file-123".to_string(),
            shared_with_id: "user-456".to_string(),
            permission: FilePermission::Read.into(),
            expires_at: 0,
        };
        assert_eq!(request.file_id, "file-123");
        assert_eq!(request.shared_with_id, "user-456");
    }

    #[test]
    fn test_get_attachment_upload_url_request_construction() {
        // Verify GetAttachmentUploadUrlRequest struct can be constructed
        let request = GetAttachmentUploadUrlRequest {
            encrypted_size: 2048,
        };
        assert_eq!(request.encrypted_size, 2048);
    }

    #[test]
    fn test_file_permission_values() {
        // Verify FilePermission enum is accessible and has expected values
        assert_eq!(FilePermission::Unspecified as i32, 0);
        assert_eq!(FilePermission::Read as i32, 1);
        assert_eq!(FilePermission::Write as i32, 2);
        assert_eq!(FilePermission::Admin as i32, 3);
    }

    #[test]
    fn test_file_chunk_construction() {
        // Verify FileChunk struct can be constructed
        let chunk = FileChunk {
            transfer_id: "transfer-123".to_string(),
            sequence: 1,
            data: vec![0u8; 256],
            auth_tag: vec![0u8; 16], // Poly1305 MAC
        };
        assert_eq!(chunk.transfer_id, "transfer-123");
        assert_eq!(chunk.sequence, 1);
        assert_eq!(chunk.data.len(), 256);
    }

    #[test]
    fn test_search_files_request_construction() {
        // Verify SearchFilesRequest struct can be constructed
        let request = SearchFilesRequest {
            query: "test".to_string(),
            channel_id: "".to_string(),
            author_id: "".to_string(),
            mime_type: "".to_string(),
            limit: 10,
            offset: 0,
            search_in_content: false,
            content_only: false,
            file_extensions: vec![],
        };
        assert_eq!(request.query, "test");
        assert_eq!(request.limit, 10);
    }
}
