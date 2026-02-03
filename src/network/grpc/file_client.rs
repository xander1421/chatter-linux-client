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
    file_service_client::FileServiceClient, FileChunk, FileMetadata, FileRequest,
    ListChannelFilesRequest, ListChannelFilesResponse,
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
}
