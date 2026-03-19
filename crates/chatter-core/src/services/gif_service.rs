//! GIF service for searching and managing GIFs.

use std::sync::Arc;

use crate::network::grpc::GifClient;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::proto::chat_v1::{
    GifSearchRequest, GifSearchResponse,
    TrendingGifsRequest, TrendingGifsResponse,
};

#[derive(Debug, thiserror::Error)]
pub enum GifError {
    #[error("Network error: {0}")]
    Network(#[from] tonic::Status),
    #[error("Not connected to server")]
    NotConnected,
}

/// Service for GIF search and trending GIFs.
pub struct GifService {
    grpc_pool: Arc<GrpcClientPool>,
}

impl GifService {
    /// Create a new GIF service.
    pub fn new(grpc_pool: Arc<GrpcClientPool>) -> Self {
        Self { grpc_pool }
    }

    /// Search for GIFs.
    pub async fn search(&self, query: &str, limit: u32) -> Result<Vec<GifResult>, GifError> {
        let channel = self.grpc_pool.try_channel()
            .ok_or(GifError::NotConnected)?;
        
        let mut client = GifClient::new(channel, self.grpc_pool.auth_clone());
        
        let request = GifSearchRequest {
            query: query.to_string(),
            limit: limit as i32,
            offset: 0,
        };
        
        let response = client.inner_mut()
            .search(request)
            .await?
            .into_inner();
        
        Ok(response.results.into_iter().map(|g| GifResult {
            id: g.id,
            url: g.url,
            preview_url: g.preview_url,
            width: g.width as u32,
            height: g.height as u32,
            title: g.title,
        }).collect())
    }
    
    /// Get trending GIFs.
    pub async fn trending(&self, limit: u32) -> Result<Vec<GifResult>, GifError> {
        let channel = self.grpc_pool.try_channel()
            .ok_or(GifError::NotConnected)?;
        
        let mut client = GifClient::new(channel, self.grpc_pool.auth_clone());
        
        let request = TrendingGifsRequest {
            limit: limit as i32,
            offset: 0,
        };
        
        let response = client.inner_mut()
            .trending(request)
            .await?
            .into_inner();
        
        Ok(response.results.into_iter().map(|g| GifResult {
            id: g.id,
            url: g.url,
            preview_url: g.preview_url,
            width: g.width as u32,
            height: g.height as u32,
            title: g.title,
        }).collect())
    }
}

/// A GIF search result.
#[derive(Debug, Clone)]
pub struct GifResult {
    pub id: String,
    pub url: String,
    pub preview_url: String,
    pub width: u32,
    pub height: u32,
    pub title: String,
}