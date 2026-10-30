//! GIF gRPC client.
//!
//! Client for GIF service operations including search and trending.

use crate::network::grpc::transport::GrpcTransport;
use tonic::service::interceptor::InterceptedService;

use crate::error::NetworkError;
use crate::network::grpc::interceptors::AuthInterceptor;
use crate::proto::chat_v1::{
    GetGifMediaRequest, GetTrendingGifsRequest, GetTrendingGifsResponse, GifResult,
    SearchGifsRequest, SearchGifsResponse, gif_service_client::GifServiceClient,
};

/// GIF service client wrapper.
///
/// Wraps the generated GifServiceClient with ergonomic APIs and error mapping.
pub struct GifClient {
    inner: GifServiceClient<InterceptedService<GrpcTransport, AuthInterceptor>>,
}

/// Largest GetGifMedia response accepted. Matches the server's 10 MiB media cap
/// (`gif.go` maxGifMediaBytes) plus framing; tonic's 4 MiB default would reject
/// the provider's larger "hd" GIFs.
const MAX_GIF_MEDIA_MESSAGE: usize = (10 << 20) + 1024;

impl GifClient {
    /// Create a new GifClient from pool components.
    pub fn new(channel: GrpcTransport, interceptor: AuthInterceptor) -> Self {
        Self {
            inner: GifServiceClient::with_interceptor(channel, interceptor)
                .max_decoding_message_size(MAX_GIF_MEDIA_MESSAGE),
        }
    }

    /// Fetch a GifResult media URL through the server proxy.
    ///
    /// The client trusts only the private CA, so it cannot reach the
    /// provider CDN itself; the server fetches it, and the provider never
    /// sees this device's IP.
    pub async fn media(&mut self, url: &str) -> Result<Vec<u8>, NetworkError> {
        self.inner
            .get_gif_media(GetGifMediaRequest {
                url: url.to_string(),
            })
            .await
            .map(|r| r.into_inner().data)
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Search for GIFs by query.
    ///
    /// # Arguments
    ///
    /// * `query` - Search query string
    /// * `limit` - Maximum number of results (default: 30, max: 50)
    ///
    /// # Returns
    ///
    /// List of GIF results with URLs and metadata.
    pub async fn search(
        &mut self,
        query: &str,
        limit: i32,
    ) -> Result<Vec<GifResult>, NetworkError> {
        let request = SearchGifsRequest {
            query: query.to_string(),
            limit,
            pos: String::new(),
            content_filter: "low".to_string(),
        };

        let response = self
            .inner
            .search_gifs(request)
            .await
            .map_err(|s| NetworkError::from_status(&s))?;

        Ok(response.into_inner().results)
    }

    /// Search for GIFs with pagination.
    pub async fn search_with_pagination(
        &mut self,
        query: &str,
        limit: i32,
        pos: &str,
        content_filter: &str,
    ) -> Result<SearchGifsResponse, NetworkError> {
        let request = SearchGifsRequest {
            query: query.to_string(),
            limit,
            pos: pos.to_string(),
            content_filter: content_filter.to_string(),
        };

        self.inner
            .search_gifs(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Get trending GIFs.
    pub async fn trending(&mut self, limit: i32) -> Result<Vec<GifResult>, NetworkError> {
        let request = GetTrendingGifsRequest {
            limit,
            pos: String::new(),
            content_filter: "low".to_string(),
        };

        let response = self
            .inner
            .get_trending_gifs(request)
            .await
            .map_err(|s| NetworkError::from_status(&s))?;

        Ok(response.into_inner().results)
    }

    /// Get trending GIFs with pagination.
    pub async fn trending_with_pagination(
        &mut self,
        limit: i32,
        pos: &str,
        content_filter: &str,
    ) -> Result<GetTrendingGifsResponse, NetworkError> {
        let request = GetTrendingGifsRequest {
            limit,
            pos: pos.to_string(),
            content_filter: content_filter.to_string(),
        };

        self.inner
            .get_trending_gifs(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }
}
