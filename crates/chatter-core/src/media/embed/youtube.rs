//! YouTube embed extractor.
//!
//! Extracts YouTube video metadata using the oEmbed API.

use crate::domain::embed::YouTubeEmbed;
use crate::error::media_error::MediaError;
use serde::Deserialize;

/// YouTube metadata extractor using oEmbed API.
pub struct YouTubeExtractor {
    client: reqwest::Client,
}

/// oEmbed response from YouTube.
#[derive(Debug, Deserialize)]
struct OEmbedResponse {
    title: String,
    author_name: String,
    thumbnail_url: String,
}

impl YouTubeExtractor {
    /// Create a new YouTube extractor.
    #[must_use]
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    /// Extract YouTube video metadata from a URL.
    ///
    /// Returns `Ok(None)` if the URL doesn't contain a valid video ID.
    /// Returns `Ok(Some(embed))` with metadata from oEmbed, or fallback data
    /// if oEmbed request fails.
    pub async fn extract(&self, url: &str) -> Result<Option<YouTubeEmbed>, MediaError> {
        // Extract video ID
        let video_id = match YouTubeEmbed::extract_video_id(url) {
            Some(id) => id,
            None => return Ok(None),
        };

        // Try oEmbed API
        let oembed_url = format!(
            "https://www.youtube.com/oembed?url={}&format=json",
            url
        );

        match self.fetch_oembed(&oembed_url).await {
            Ok(response) => {
                let embed = YouTubeEmbed::new(
                    video_id,
                    response.title,
                    response.thumbnail_url,
                )
                .with_channel(response.author_name);
                Ok(Some(embed))
            }
            Err(_) => {
                // Fallback: return embed with default values
                let thumbnail_url = format!(
                    "https://i.ytimg.com/vi/{}/hqdefault.jpg",
                    video_id
                );
                let embed = YouTubeEmbed::new(
                    video_id,
                    "YouTube Video".to_string(),
                    thumbnail_url,
                );
                Ok(Some(embed))
            }
        }
    }

    /// Fetch oEmbed data from YouTube.
    async fn fetch_oembed(&self, url: &str) -> Result<OEmbedResponse, MediaError> {
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| MediaError::EmbedError(format!("HTTP request failed: {}", e)))?;

        if !response.status().is_success() {
            return Err(MediaError::EmbedError(format!(
                "oEmbed request failed with status: {}",
                response.status()
            )));
        }

        response
            .json::<OEmbedResponse>()
            .await
            .map_err(|e| MediaError::EmbedError(format!("Failed to parse oEmbed response: {}", e)))
    }
}

impl Default for YouTubeExtractor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extractor_creation() {
        let extractor = YouTubeExtractor::new();
        assert!(std::mem::size_of_val(&extractor) > 0);
    }

    #[test]
    fn test_default_implementation() {
        let _extractor = YouTubeExtractor::default();
    }
}
