//! File gRPC client.
//!
//! Client for Signal-style encrypted attachments: presigned upload/download
//! URLs. Bytes go directly to RustFS; the server only brokers URLs.

use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;

use crate::error::NetworkError;
use crate::network::grpc::interceptors::AuthInterceptor;
use crate::proto::chat_v1::{
    file_service_client::FileServiceClient, ConfirmAttachmentUploadRequest,
    ConfirmAttachmentUploadResponse, GetAttachmentDownloadUrlRequest,
    GetAttachmentDownloadUrlResponse, GetAttachmentUploadUrlRequest, GetAttachmentUploadUrlResponse,
};

/// File service client wrapper.
///
/// Wraps the generated FileServiceClient with ergonomic APIs and error mapping.
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
    fn test_get_attachment_upload_url_request_construction() {
        // Verify GetAttachmentUploadUrlRequest struct can be constructed
        let request = GetAttachmentUploadUrlRequest {
            encrypted_size: 2048,
        };
        assert_eq!(request.encrypted_size, 2048);
    }
}
