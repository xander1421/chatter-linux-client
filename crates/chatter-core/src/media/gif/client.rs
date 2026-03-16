//! GIF API client.
//!
//! Tenor v2 API client for GIF search, trending, and lookup.

use crate::error::MediaError;
use serde::Deserialize;

/// Base URL for Tenor API v2.
const TENOR_BASE_URL: &str = "https://tenor.googleapis.com/v2";

/// Default search/trending limit.
const DEFAULT_LIMIT: u32 = 20;

/// A GIF result from the Tenor API.
#[derive(Debug, Clone)]
pub struct GifResult {
    /// Tenor media ID.
    pub id: String,
    /// Content description/title.
    pub title: String,
    /// Preview URL (tinygif for thumbnails).
    pub preview_url: String,
    /// Full GIF URL for sending.
    pub url: String,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

/// Tenor API client for GIF operations.
pub struct GifClient {
    /// Tenor API key.
    api_key: String,
    /// HTTP client.
    client: reqwest::Client,
}

// Tenor API response structures
#[derive(Deserialize)]
struct TenorSearchResponse {
    results: Vec<TenorResult>,
}

#[derive(Deserialize)]
struct TenorResult {
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    content_description: String,
    media_formats: TenorMediaFormats,
}

#[derive(Deserialize)]
struct TenorMediaFormats {
    gif: Option<TenorMediaFormat>,
    mediumgif: Option<TenorMediaFormat>,
    tinygif: Option<TenorMediaFormat>,
    nanogif: Option<TenorMediaFormat>,
}

#[derive(Deserialize)]
struct TenorMediaFormat {
    url: String,
    dims: Vec<u32>,
}

impl GifClient {
    /// Create a new GIF client with the given API key.
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            client: reqwest::Client::new(),
        }
    }

    /// Search for GIFs matching the query.
    ///
    /// # Arguments
    /// * `query` - Search term
    /// * `limit` - Maximum number of results (default 20)
    ///
    /// # Returns
    /// A vector of GIF results
    pub async fn search(&self, query: &str, limit: u32) -> Result<Vec<GifResult>, MediaError> {
        let limit = if limit == 0 { DEFAULT_LIMIT } else { limit };

        let url = format!(
            "{}/search?q={}&key={}&limit={}&media_filter=gif,tinygif",
            TENOR_BASE_URL,
            urlencoding::encode(query),
            self.api_key,
            limit
        );

        self.fetch_and_parse(&url).await
    }

    /// Get trending GIFs.
    ///
    /// # Arguments
    /// * `limit` - Maximum number of results (default 20)
    ///
    /// # Returns
    /// A vector of trending GIF results
    pub async fn trending(&self, limit: u32) -> Result<Vec<GifResult>, MediaError> {
        let limit = if limit == 0 { DEFAULT_LIMIT } else { limit };

        let url = format!(
            "{}/featured?key={}&limit={}&media_filter=gif,tinygif",
            TENOR_BASE_URL,
            self.api_key,
            limit
        );

        self.fetch_and_parse(&url).await
    }

    /// Get a specific GIF by ID.
    ///
    /// # Arguments
    /// * `id` - Tenor media ID
    ///
    /// # Returns
    /// The GIF result if found, None otherwise
    pub async fn get_by_id(&self, id: &str) -> Result<Option<GifResult>, MediaError> {
        let url = format!(
            "{}/posts?key={}&ids={}&media_filter=gif,tinygif",
            TENOR_BASE_URL,
            self.api_key,
            urlencoding::encode(id)
        );

        let results = self.fetch_and_parse(&url).await?;
        Ok(results.into_iter().next())
    }

    /// Fetch and parse a Tenor API response.
    async fn fetch_and_parse(&self, url: &str) -> Result<Vec<GifResult>, MediaError> {
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| MediaError::GifError(format!("HTTP request failed: {}", e)))?;

        if !response.status().is_success() {
            return Err(MediaError::GifError(format!(
                "Tenor API error: {}",
                response.status()
            )));
        }

        let tenor_response: TenorSearchResponse = response
            .json()
            .await
            .map_err(|e| MediaError::GifError(format!("Failed to parse response: {}", e)))?;

        let results = tenor_response
            .results
            .into_iter()
            .filter_map(|r| self.convert_result(r))
            .collect();

        Ok(results)
    }

    /// Convert a Tenor API result to our GifResult format.
    fn convert_result(&self, result: TenorResult) -> Option<GifResult> {
        // Get the full GIF URL (prefer gif, fall back to mediumgif)
        let gif_format = result
            .media_formats
            .gif
            .or(result.media_formats.mediumgif)?;

        // Get the preview URL (prefer tinygif, fall back to nanogif)
        let preview_format = result
            .media_formats
            .tinygif
            .or(result.media_formats.nanogif)?;

        // Get dimensions from the full GIF
        let (width, height) = if gif_format.dims.len() >= 2 {
            (gif_format.dims[0], gif_format.dims[1])
        } else {
            (0, 0)
        };

        // Use content_description as title if title is empty
        let title = if result.title.is_empty() {
            result.content_description
        } else {
            result.title
        };

        Some(GifResult {
            id: result.id,
            title,
            preview_url: preview_format.url,
            url: gif_format.url,
            width,
            height,
        })
    }
}

// Simple URL encoding helper module
mod urlencoding {
    pub fn encode(s: &str) -> String {
        let mut result = String::with_capacity(s.len() * 3);
        for c in s.chars() {
            match c {
                'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '~' => {
                    result.push(c);
                }
                ' ' => {
                    result.push_str("%20");
                }
                _ => {
                    for byte in c.to_string().as_bytes() {
                        result.push_str(&format!("%{:02X}", byte));
                    }
                }
            }
        }
        result
    }
}
