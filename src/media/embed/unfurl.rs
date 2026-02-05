//! URL unfurling coordinator.
//!
//! Routes URLs to appropriate extractors based on detected type.

use crate::domain::embed::{EmbedType, LinkEmbed, LinkPreview};
use crate::error::media_error::MediaError;
use crate::media::embed::link_preview::LinkPreviewGenerator;
use crate::media::embed::youtube::YouTubeExtractor;

/// URL unfurler that coordinates embed extraction.
///
/// Routes URLs to the appropriate extractor based on detected type:
/// - YouTube URLs → YouTubeExtractor (oEmbed)
/// - Image URLs → Direct image embed
/// - Other URLs → LinkPreviewGenerator (OpenGraph)
pub struct UrlUnfurler {
    youtube: YouTubeExtractor,
    link_preview: LinkPreviewGenerator,
}

impl UrlUnfurler {
    /// Create a new URL unfurler.
    #[must_use]
    pub fn new() -> Self {
        Self {
            youtube: YouTubeExtractor::new(),
            link_preview: LinkPreviewGenerator::new(),
        }
    }

    /// Unfurl a URL into a rich link embed.
    ///
    /// Detects the URL type and routes to the appropriate extractor.
    pub async fn unfurl(&self, url: &str) -> Result<LinkEmbed, MediaError> {
        let embed_type = LinkEmbed::detect_type(url);

        match embed_type {
            EmbedType::YouTube => {
                match self.youtube.extract(url).await? {
                    Some(yt_embed) => Ok(LinkEmbed::YouTube(yt_embed)),
                    None => {
                        // Fallback to generic if video ID extraction failed
                        let preview = self.link_preview.generate(url).await?;
                        Ok(LinkEmbed::Generic(preview))
                    }
                }
            }
            EmbedType::Image => {
                Ok(LinkEmbed::Image {
                    url: url.to_string(),
                    width: None,
                    height: None,
                })
            }
            EmbedType::Twitter => {
                // Twitter API requires auth - fall back to generic OpenGraph
                let preview = self.link_preview.generate(url).await?;
                Ok(LinkEmbed::Generic(preview))
            }
            EmbedType::GitHubRepo | EmbedType::GitHubIssue | EmbedType::GitHubPullRequest => {
                // GitHub API is complex - fall back to generic OpenGraph
                let preview = self.link_preview.generate(url).await?;
                Ok(LinkEmbed::Generic(preview))
            }
            EmbedType::Generic => {
                let preview = self.link_preview.generate(url).await?;
                Ok(LinkEmbed::Generic(preview))
            }
        }
    }

    /// Unfurl multiple URLs concurrently.
    ///
    /// Returns results in the same order as input URLs.
    pub async fn unfurl_batch(&self, urls: &[&str]) -> Vec<Result<LinkEmbed, MediaError>> {
        let futures: Vec<_> = urls.iter().map(|url| self.unfurl(url)).collect();
        futures::future::join_all(futures).await
    }

    /// Unfurl a URL with a timeout.
    ///
    /// Returns an error if extraction takes longer than the specified duration.
    pub async fn unfurl_with_timeout(
        &self,
        url: &str,
        timeout: std::time::Duration,
    ) -> Result<LinkEmbed, MediaError> {
        tokio::time::timeout(timeout, self.unfurl(url))
            .await
            .map_err(|_| MediaError::EmbedError("URL unfurl timed out".to_string()))?
    }

    /// Try to unfurl a URL, returning a minimal preview on failure.
    ///
    /// Never fails - returns at least a basic LinkPreview with just the URL.
    pub async fn unfurl_or_minimal(&self, url: &str) -> LinkEmbed {
        match self.unfurl(url).await {
            Ok(embed) => embed,
            Err(_) => LinkEmbed::Generic(LinkPreview::new(url.to_string())),
        }
    }
}

impl Default for UrlUnfurler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unfurler_creation() {
        let unfurler = UrlUnfurler::new();
        assert!(std::mem::size_of_val(&unfurler) > 0);
    }

    #[test]
    fn test_default_implementation() {
        let _unfurler = UrlUnfurler::default();
    }

    #[tokio::test]
    async fn test_unfurl_or_minimal_on_invalid_url() {
        let unfurler = UrlUnfurler::new();
        let embed = unfurler.unfurl_or_minimal("not-a-valid-url").await;

        // Should return a minimal preview, not panic
        match embed {
            LinkEmbed::Generic(preview) => {
                assert_eq!(preview.url, "not-a-valid-url");
            }
            _ => panic!("Expected Generic embed"),
        }
    }

    #[test]
    fn test_detect_youtube_routes_correctly() {
        assert_eq!(
            LinkEmbed::detect_type("https://youtube.com/watch?v=abc123"),
            EmbedType::YouTube
        );
    }

    #[test]
    fn test_detect_image_routes_correctly() {
        assert_eq!(
            LinkEmbed::detect_type("https://example.com/image.png"),
            EmbedType::Image
        );
    }

    #[test]
    fn test_detect_generic_routes_correctly() {
        assert_eq!(
            LinkEmbed::detect_type("https://example.com/article"),
            EmbedType::Generic
        );
    }
}
