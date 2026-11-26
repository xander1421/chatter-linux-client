//! GIF gRPC client.
//!
//! Client for GIF service operations including search and trending.

use crate::network::grpc::transport::GrpcTransport;
use tonic::service::interceptor::InterceptedService;

use crate::error::NetworkError;
use crate::mailbox::token_manager::TokenManager;
use crate::network::grpc::interceptors::AuthInterceptor;
use chatter_proto::chat_v1::{
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

/// The picker drops its connection after one RPC, including errors.
pub(crate) struct AnonymousGifClient {
    inner: GifServiceClient<GrpcTransport>,
    tokens: TokenManager,
}

impl AnonymousGifClient {
    pub(crate) fn new(channel: GrpcTransport, tokens: TokenManager) -> Self {
        Self {
            inner: GifServiceClient::new(channel).max_decoding_message_size(MAX_GIF_MEDIA_MESSAGE),
            tokens,
        }
    }

    fn request<T>(&self, payload: T) -> Result<(tonic::Request<T>, String), NetworkError> {
        let mut request = tonic::Request::new(payload);
        let token = self.tokens.claim_send_credential()?;
        if let Err(status) = super::interceptors::insert_blind_credential(
            &mut request,
            &token.public_key,
            &token.signature,
        ) {
            self.restore_credential(&token.id);
            return Err(NetworkError::from_status(&status));
        }
        Ok((request, token.id))
    }

    fn restore_credential(&self, token_id: &str) {
        if let Err(error) = self.tokens.return_unspent_gif_credential(token_id) {
            tracing::warn!(%error, "Could not restore an unspent GIF credential");
        }
    }

    fn response<T>(
        &self,
        token_id: &str,
        result: Result<tonic::Response<T>, tonic::Status>,
    ) -> Result<T, NetworkError> {
        result.map(tonic::Response::into_inner).map_err(|status| {
            // Local transport failures and ambiguous server errors can arrive
            // after admission. Reuse only on an explicit server refusal.
            if status.code() == tonic::Code::ResourceExhausted
                && status
                    .metadata()
                    .get("x-gif-credential-unspent")
                    .is_some_and(|value| value == "1")
            {
                self.restore_credential(token_id);
            }
            NetworkError::from_status(&status)
        })
    }

    pub(crate) async fn media(mut self, url: &str) -> Result<Vec<u8>, NetworkError> {
        let (request, token_id) = self.request(GetGifMediaRequest { url: url.into() })?;
        let result = self.inner.get_private_gif_media(request).await;
        self.response(&token_id, result).map(|r| r.data)
    }

    pub(crate) async fn search(
        mut self,
        query: &str,
        limit: i32,
    ) -> Result<Vec<GifResult>, NetworkError> {
        let (request, token_id) = self.request(SearchGifsRequest {
            query: query.into(),
            limit,
            content_filter: "low".into(),
            ..Default::default()
        })?;
        let result = self.inner.search_private_gifs(request).await;
        self.response(&token_id, result).map(|r| r.results)
    }

    pub(crate) async fn trending(mut self, limit: i32) -> Result<Vec<GifResult>, NetworkError> {
        let (request, token_id) = self.request(GetTrendingGifsRequest {
            limit,
            content_filter: "low".into(),
            ..Default::default()
        })?;
        let result = self.inner.get_private_trending_gifs(request).await;
        self.response(&token_id, result).map(|r| r.results)
    }
}
