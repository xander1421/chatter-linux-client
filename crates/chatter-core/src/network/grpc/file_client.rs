//! File gRPC client.
//!
//! Client for Signal-style encrypted attachments: presigned upload/download
//! URLs. Bytes go directly to RustFS; the server only brokers URLs.

use crate::network::grpc::transport::GrpcTransport;
use tonic::service::interceptor::InterceptedService;

use crate::error::NetworkError;
use crate::network::grpc::interceptors::AuthInterceptor;
use crate::proto::chat_v1::{
    AbortAttachmentMultipartUploadRequest, CompleteAttachmentMultipartUploadRequest,
    ConfirmAttachmentUploadRequest, ConfirmAttachmentUploadResponse,
    CreateAttachmentMultipartUploadRequest, CreateAttachmentMultipartUploadResponse,
    GetAttachmentDownloadUrlRequest, GetAttachmentDownloadUrlResponse,
    GetAttachmentUploadUrlRequest, GetAttachmentUploadUrlResponse,
    file_service_client::FileServiceClient,
};

/// File service client wrapper.
///
/// Wraps the generated FileServiceClient with ergonomic APIs and error mapping.
pub struct FileClient {
    inner: FileServiceClient<InterceptedService<GrpcTransport, AuthInterceptor>>,
}

impl FileClient {
    /// Create a new FileClient from pool components.
    pub fn new(channel: GrpcTransport, interceptor: AuthInterceptor) -> Self {
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
        let inner = self.inner.clone();
        confirm_with_retry(cdn_key, move |request| {
            let mut inner = inner.clone();
            async move {
                inner
                    .confirm_attachment_upload(request)
                    .await
                    .map(|r| r.into_inner())
            }
        })
        .await
        .map_err(|status| NetworkError::from_status(&status))
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

    // ==================== Multipart Upload RPCs ====================

    /// Start a multipart upload; returns upload_id + presigned part URLs.
    pub async fn create_attachment_multipart_upload(
        &mut self,
        encrypted_size: u64,
    ) -> Result<CreateAttachmentMultipartUploadResponse, NetworkError> {
        let request = CreateAttachmentMultipartUploadRequest { encrypted_size };
        self.inner
            .create_attachment_multipart_upload(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Finalize a multipart upload with the collected part ETags.
    pub async fn complete_attachment_multipart_upload(
        &mut self,
        cdn_key: &str,
        upload_id: &str,
        parts: Vec<crate::proto::chat_v1::CompletedPart>,
    ) -> Result<(), NetworkError> {
        let request = CompleteAttachmentMultipartUploadRequest {
            cdn_key: cdn_key.to_string(),
            upload_id: upload_id.to_string(),
            parts,
        };
        self.inner
            .complete_attachment_multipart_upload(request)
            .await
            .map(|_| ())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Abort a multipart upload (best-effort cleanup on client give-up).
    pub async fn abort_attachment_multipart_upload(
        &mut self,
        cdn_key: &str,
        upload_id: &str,
    ) -> Result<(), NetworkError> {
        let request = AbortAttachmentMultipartUploadRequest {
            cdn_key: cdn_key.to_string(),
            upload_id: upload_id.to_string(),
        };
        self.inner
            .abort_attachment_multipart_upload(request)
            .await
            .map(|_| ())
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
        fn _verify_types(channel: GrpcTransport, interceptor: AuthInterceptor) {
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

    #[test]
    fn test_create_multipart_request_construction() {
        let request = CreateAttachmentMultipartUploadRequest {
            encrypted_size: 200 * 1024 * 1024,
        };
        assert_eq!(request.encrypted_size, 200 * 1024 * 1024);
    }
}

// The server may retain an uncertain immutable promotion for its five-minute
// lease. The public error filter hides response text, so retry transient statuses:
// never create a new upload, key, destination or message from this loop.
async fn confirm_with_retry<F, Fut>(
    cdn_key: &str,
    mut confirm: F,
) -> Result<ConfirmAttachmentUploadResponse, tonic::Status>
where
    F: FnMut(ConfirmAttachmentUploadRequest) -> Fut,
    Fut: std::future::Future<Output = Result<ConfirmAttachmentUploadResponse, tonic::Status>>,
{
    use std::time::Duration;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(330);
    let mut delay = Duration::from_millis(250);
    loop {
        let request = ConfirmAttachmentUploadRequest {
            cdn_key: cdn_key.to_owned(),
        };
        let result = tokio::time::timeout_at(deadline, confirm(request))
            .await
            .map_err(|_| tonic::Status::deadline_exceeded("Attachment confirmation timed out"))?;
        match result {
            Ok(response) => return Ok(response),
            Err(status) => {
                let retry_same = matches!(
                    status.code(),
                    tonic::Code::Unavailable | tonic::Code::DeadlineExceeded
                );
                if !retry_same || tokio::time::Instant::now() + delay >= deadline {
                    return Err(status);
                }
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(5));
            }
        }
    }
}

#[cfg(test)]
mod confirmation_retry_tests {
    use super::*;
    use std::{
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };
    fn busy() -> tonic::Status {
        tonic::Status::unavailable("service temporarily unavailable")
    }
    #[tokio::test(start_paused = true)]
    async fn confirmation_survives_promotion_lease_using_the_same_key() {
        let start = tokio::time::Instant::now();
        let keys = Arc::new(Mutex::new(Vec::new()));
        let observed = keys.clone();
        let response = confirm_with_retry("fixture-cdn-key", move |request| {
            observed.lock().unwrap().push(request.cdn_key);
            async move {
                if start.elapsed() < Duration::from_secs(305) {
                    Err(busy())
                } else {
                    Ok(ConfirmAttachmentUploadResponse { confirmed: true })
                }
            }
        })
        .await
        .unwrap();
        assert!(response.confirmed);
        let keys = keys.lock().unwrap();
        assert!(keys.len() > 2 && keys.len() < 75);
        assert!(keys.iter().all(|key| key == "fixture-cdn-key"));
    }
    #[tokio::test(start_paused = true)]
    async fn terminal_errors_do_not_retry() {
        for status in [
            tonic::Status::unauthenticated("fixture"),
            tonic::Status::invalid_argument("fixture"),
            tonic::Status::resource_exhausted("fixture"),
            tonic::Status::not_found("fixture"),
        ] {
            let calls = AtomicUsize::new(0);
            let expected = status.code();
            let result = confirm_with_retry("fixture", |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                let status = status.clone();
                async move { Err(status) }
            })
            .await;
            assert_eq!(result.unwrap_err().code(), expected);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
    }
    #[tokio::test(start_paused = true)]
    async fn transient_confirmation_does_not_depend_on_sanitized_error_text() {
        for first in [
            tonic::Status::unavailable("service temporarily unavailable"),
            tonic::Status::unavailable("generic transport failure"),
            tonic::Status::deadline_exceeded("request deadline exceeded"),
        ] {
            let mut first = Some(first);
            let response = confirm_with_retry("same-key", |request| {
                assert_eq!(request.cdn_key, "same-key");
                let first = first.take();
                async move {
                    match first {
                        Some(status) => Err(status),
                        None => Ok(ConfirmAttachmentUploadResponse { confirmed: true }),
                    }
                }
            })
            .await
            .unwrap();
            assert!(response.confirmed);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn perpetual_busy_and_hung_rpc_are_bounded() {
        let start = tokio::time::Instant::now();
        let calls = AtomicUsize::new(0);
        assert!(
            confirm_with_retry("fixture", |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Err(busy()) }
            })
            .await
            .is_err()
        );
        assert!(start.elapsed() <= Duration::from_secs(330));
        assert!(calls.load(Ordering::SeqCst) < 75);
        let start = tokio::time::Instant::now();
        let result = confirm_with_retry("fixture", |_| std::future::pending()).await;
        assert_eq!(result.unwrap_err().code(), tonic::Code::DeadlineExceeded);
        assert_eq!(start.elapsed(), Duration::from_secs(330));
    }
    #[tokio::test(start_paused = true)]
    async fn dropping_confirmation_stops_future_attempts() {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let task = tokio::spawn(async move {
            confirm_with_retry("fixture", move |_| {
                observed.fetch_add(1, Ordering::SeqCst);
                async { Err(busy()) }
            })
            .await
        });
        tokio::task::yield_now().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let before = calls.load(Ordering::SeqCst);
        tokio::time::advance(Duration::from_secs(600)).await;
        assert_eq!(calls.load(Ordering::SeqCst), before);
    }
}
