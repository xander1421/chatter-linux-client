//! Embed domain entity.
//!
//! Represents link embeds and previews for URLs shared in messages.
//! Supports generic OpenGraph previews as well as platform-specific
//! embeds for YouTube, Twitter, and GitHub.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;

// ============================================================================
// LinkEmbed
// ============================================================================

/// Rich embed for a link shared in a message.
///
/// Different platforms have different embed types with platform-specific
/// metadata for optimal display.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum LinkEmbed {
    /// Generic OpenGraph preview for any URL.
    Generic(LinkPreview),
    /// YouTube video embed.
    YouTube(YouTubeEmbed),
    /// Twitter/X tweet embed.
    Twitter(TwitterEmbed),
    /// GitHub repository, issue, or PR embed.
    GitHub(GitHubEmbed),
    /// Direct image embed.
    Image {
        /// URL to the image.
        url: String,
        /// Image width in pixels.
        width: Option<u32>,
        /// Image height in pixels.
        height: Option<u32>,
    },
    /// Animated GIF embed.
    Gif {
        /// URL to the full-size GIF.
        url: String,
        /// URL to a smaller preview (for thumbnails).
        preview_url: Option<String>,
        /// GIF width in pixels.
        width: Option<u32>,
        /// GIF height in pixels.
        height: Option<u32>,
    },
}

/// Type hint for URL pattern detection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmbedType {
    /// Generic website (OpenGraph).
    Generic,
    /// YouTube video.
    YouTube,
    /// Twitter/X tweet.
    Twitter,
    /// GitHub repository.
    GitHubRepo,
    /// GitHub issue.
    GitHubIssue,
    /// GitHub pull request.
    GitHubPullRequest,
    /// Direct image link.
    Image,
    /// Animated GIF.
    Gif,
}

impl LinkEmbed {
    /// Detect the embed type from a URL pattern.
    ///
    /// Returns the detected type if the URL matches a known pattern,
    /// or `Generic` as fallback.
    #[must_use]
    pub fn detect_type(url: &str) -> EmbedType {
        let url_lower = url.to_lowercase();

        // YouTube patterns
        if url_lower.contains("youtube.com/watch")
            || url_lower.contains("youtu.be/")
            || url_lower.contains("youtube.com/shorts")
        {
            return EmbedType::YouTube;
        }

        // Twitter/X patterns
        if (url_lower.contains("twitter.com/") || url_lower.contains("x.com/"))
            && url_lower.contains("/status/")
        {
            return EmbedType::Twitter;
        }

        // GitHub patterns
        if url_lower.contains("github.com/") {
            if url_lower.contains("/pull/") {
                return EmbedType::GitHubPullRequest;
            }
            if url_lower.contains("/issues/") {
                return EmbedType::GitHubIssue;
            }
            // Check if it looks like a repo URL (owner/repo pattern)
            // but not a file path (owner/repo/blob, owner/repo/tree, etc.)
            let parts: Vec<&str> = url_lower
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .trim_start_matches("github.com/")
                .split('/')
                .collect();
            if parts.len() == 2
                || (parts.len() == 3 && parts[2].is_empty())
            {
                return EmbedType::GitHubRepo;
            }
        }

        // GIF patterns - check before generic images
        // Known GIF CDN domains
        let gif_domains = [
            "klipy.co",
            "media.klipy.co",
            "giphy.com",
            "media.giphy.com",
            "tenor.com",
            "media.tenor.com",
            "gfycat.com",
            "thumbs.gfycat.com",
        ];
        for domain in &gif_domains {
            if url_lower.contains(domain) {
                return EmbedType::Gif;
            }
        }
        // .gif extension
        if url_lower.ends_with(".gif") {
            return EmbedType::Gif;
        }

        // Direct image patterns (non-GIF)
        let image_extensions = [".jpg", ".jpeg", ".png", ".webp", ".svg"];
        for ext in &image_extensions {
            if url_lower.ends_with(ext) {
                return EmbedType::Image;
            }
        }

        // Also check for image query strings (e.g., imgur)
        if url_lower.contains("i.imgur.com/") || url_lower.contains("pbs.twimg.com/media/") {
            return EmbedType::Image;
        }

        EmbedType::Generic
    }

    /// Check if this embed is media (video, image, or GIF).
    #[must_use]
    pub fn is_media(&self) -> bool {
        matches!(self, Self::YouTube(_) | Self::Image { .. } | Self::Gif { .. })
    }

    /// Get the preview image URL if available.
    #[must_use]
    pub fn preview_image(&self) -> Option<&str> {
        match self {
            Self::Generic(preview) => preview.image_url.as_deref(),
            Self::YouTube(yt) => Some(&yt.thumbnail_url),
            Self::Twitter(_) => None, // Could add author avatar later
            Self::GitHub(_) => None,  // Could add repo owner avatar later
            Self::Image { url, .. } => Some(url),
            Self::Gif { preview_url, url, .. } => preview_url.as_deref().or(Some(url)),
        }
    }

    /// Get the title if available.
    #[must_use]
    pub fn title(&self) -> Option<&str> {
        match self {
            Self::Generic(preview) => preview.title.as_deref(),
            Self::YouTube(yt) => Some(&yt.title),
            Self::Twitter(tw) => Some(&tw.content),
            Self::GitHub(gh) => gh.title(),
            Self::Image { .. } => None,
            Self::Gif { .. } => None,
        }
    }

    /// Get the source URL.
    ///
    /// For YouTube and Twitter, returns the ID string (use dedicated URL methods
    /// on those types for full URLs). For GitHub, constructs the full URL.
    #[must_use]
    pub fn source_url(&self) -> String {
        match self {
            Self::Generic(preview) => preview.url.clone(),
            Self::YouTube(yt) => yt.watch_url(),
            Self::Twitter(tw) => tw.tweet_url(),
            Self::GitHub(gh) => gh.url(),
            Self::Image { url, .. } => url.clone(),
            Self::Gif { url, .. } => url.clone(),
        }
    }
}

// ============================================================================
// LinkPreview
// ============================================================================

/// Generic link preview using OpenGraph metadata.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LinkPreview {
    /// The URL being previewed.
    pub url: String,
    /// Page title (og:title).
    pub title: Option<String>,
    /// Page description (og:description).
    pub description: Option<String>,
    /// Preview image URL (og:image).
    pub image_url: Option<String>,
    /// Site name (og:site_name).
    pub site_name: Option<String>,
    /// Favicon URL.
    pub favicon_url: Option<String>,
}

impl LinkPreview {
    /// Create a new link preview with just the URL.
    #[must_use]
    pub fn new(url: String) -> Self {
        Self {
            url,
            title: None,
            description: None,
            image_url: None,
            site_name: None,
            favicon_url: None,
        }
    }

    /// Set the title.
    #[must_use]
    pub fn with_title(mut self, title: String) -> Self {
        self.title = Some(title);
        self
    }

    /// Set the description.
    #[must_use]
    pub fn with_description(mut self, description: String) -> Self {
        self.description = Some(description);
        self
    }

    /// Set the preview image.
    #[must_use]
    pub fn with_image(mut self, url: String) -> Self {
        self.image_url = Some(url);
        self
    }

    /// Set the site name.
    #[must_use]
    pub fn with_site_name(mut self, name: String) -> Self {
        self.site_name = Some(name);
        self
    }

    /// Set the favicon.
    #[must_use]
    pub fn with_favicon(mut self, url: String) -> Self {
        self.favicon_url = Some(url);
        self
    }

    /// Check if this preview has any metadata beyond the URL.
    #[must_use]
    pub fn has_metadata(&self) -> bool {
        self.title.is_some()
            || self.description.is_some()
            || self.image_url.is_some()
            || self.site_name.is_some()
    }
}

// ============================================================================
// YouTubeEmbed
// ============================================================================

/// YouTube video embed metadata.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct YouTubeEmbed {
    /// YouTube video ID (e.g., "dQw4w9WgXcQ").
    pub video_id: String,
    /// Video title.
    pub title: String,
    /// Thumbnail image URL.
    pub thumbnail_url: String,
    /// Channel name.
    pub channel_name: Option<String>,
    /// Video duration.
    pub duration: Option<Duration>,
}

impl YouTubeEmbed {
    /// Create a new YouTube embed.
    #[must_use]
    pub fn new(video_id: String, title: String, thumbnail_url: String) -> Self {
        Self {
            video_id,
            title,
            thumbnail_url,
            channel_name: None,
            duration: None,
        }
    }

    /// Set the channel name.
    #[must_use]
    pub fn with_channel(mut self, name: String) -> Self {
        self.channel_name = Some(name);
        self
    }

    /// Set the duration.
    #[must_use]
    pub fn with_duration(mut self, duration: Duration) -> Self {
        self.duration = Some(duration);
        self
    }

    /// Get the full YouTube watch URL.
    #[must_use]
    pub fn watch_url(&self) -> String {
        format!("https://www.youtube.com/watch?v={}", self.video_id)
    }

    /// Get the embed URL for iframe embedding.
    #[must_use]
    pub fn embed_url(&self) -> String {
        format!("https://www.youtube.com/embed/{}", self.video_id)
    }

    /// Format the duration as a human-readable string.
    #[must_use]
    pub fn duration_display(&self) -> Option<String> {
        self.duration.map(|d| {
            let secs = d.as_secs();
            if secs < 60 {
                format!("0:{:02}", secs)
            } else if secs < 3600 {
                format!("{}:{:02}", secs / 60, secs % 60)
            } else {
                format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
            }
        })
    }

    /// Extract video ID from a YouTube URL.
    ///
    /// Supports formats:
    /// - `https://www.youtube.com/watch?v=VIDEO_ID`
    /// - `https://youtu.be/VIDEO_ID`
    /// - `https://www.youtube.com/shorts/VIDEO_ID`
    #[must_use]
    pub fn extract_video_id(url: &str) -> Option<String> {
        // youtu.be format
        if let Some(idx) = url.find("youtu.be/") {
            let start = idx + 9;
            let end = url[start..]
                .find(['?', '&', '/'])
                .map(|i| start + i)
                .unwrap_or(url.len());
            return Some(url[start..end].to_string());
        }

        // youtube.com/watch format
        if let Some(idx) = url.find("v=") {
            let start = idx + 2;
            let end = url[start..]
                .find(['&', '/', '#'])
                .map(|i| start + i)
                .unwrap_or(url.len());
            return Some(url[start..end].to_string());
        }

        // youtube.com/shorts format
        if let Some(idx) = url.find("/shorts/") {
            let start = idx + 8;
            let end = url[start..]
                .find(['?', '&', '/'])
                .map(|i| start + i)
                .unwrap_or(url.len());
            return Some(url[start..end].to_string());
        }

        None
    }
}

// ============================================================================
// TwitterEmbed
// ============================================================================

/// Twitter/X tweet embed metadata.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TwitterEmbed {
    /// Tweet ID.
    pub tweet_id: String,
    /// Author display name.
    pub author_name: String,
    /// Author handle (without @).
    pub author_handle: String,
    /// Tweet content/text.
    pub content: String,
    /// When the tweet was posted.
    pub created_at: Option<DateTime<Utc>>,
}

impl TwitterEmbed {
    /// Create a new Twitter embed.
    #[must_use]
    pub fn new(
        tweet_id: String,
        author_name: String,
        author_handle: String,
        content: String,
    ) -> Self {
        Self {
            tweet_id,
            author_name,
            author_handle,
            content,
            created_at: None,
        }
    }

    /// Set the creation timestamp.
    #[must_use]
    pub fn with_created_at(mut self, created_at: DateTime<Utc>) -> Self {
        self.created_at = Some(created_at);
        self
    }

    /// Get the full tweet URL.
    #[must_use]
    pub fn tweet_url(&self) -> String {
        format!("https://twitter.com/{}/status/{}", self.author_handle, self.tweet_id)
    }

    /// Get the author mention text.
    #[must_use]
    pub fn author_mention(&self) -> String {
        format!("@{}", self.author_handle)
    }

    /// Extract tweet ID from a Twitter/X URL.
    #[must_use]
    pub fn extract_tweet_id(url: &str) -> Option<String> {
        // Look for /status/ followed by digits
        if let Some(idx) = url.find("/status/") {
            let start = idx + 8;
            let end = url[start..]
                .find(|c: char| !c.is_ascii_digit())
                .map(|i| start + i)
                .unwrap_or(url.len());
            let id = &url[start..end];
            if !id.is_empty() {
                return Some(id.to_string());
            }
        }
        None
    }
}

// ============================================================================
// GitHubEmbed
// ============================================================================

/// GitHub embed for repositories, issues, and pull requests.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum GitHubEmbed {
    /// GitHub repository.
    Repository {
        /// Repository owner.
        owner: String,
        /// Repository name.
        repo: String,
        /// Repository description.
        description: Option<String>,
        /// Star count.
        stars: Option<u32>,
    },
    /// GitHub issue.
    Issue {
        /// Repository owner.
        owner: String,
        /// Repository name.
        repo: String,
        /// Issue number.
        number: u32,
        /// Issue title.
        title: String,
        /// Issue state (open, closed).
        state: String,
    },
    /// GitHub pull request.
    PullRequest {
        /// Repository owner.
        owner: String,
        /// Repository name.
        repo: String,
        /// PR number.
        number: u32,
        /// PR title.
        title: String,
        /// PR state (open, closed, merged).
        state: String,
    },
}

impl GitHubEmbed {
    /// Create a repository embed.
    #[must_use]
    pub fn repository(owner: String, repo: String) -> Self {
        Self::Repository {
            owner,
            repo,
            description: None,
            stars: None,
        }
    }

    /// Create an issue embed.
    #[must_use]
    pub fn issue(owner: String, repo: String, number: u32, title: String, state: String) -> Self {
        Self::Issue {
            owner,
            repo,
            number,
            title,
            state,
        }
    }

    /// Create a pull request embed.
    #[must_use]
    pub fn pull_request(
        owner: String,
        repo: String,
        number: u32,
        title: String,
        state: String,
    ) -> Self {
        Self::PullRequest {
            owner,
            repo,
            number,
            title,
            state,
        }
    }

    /// Get the title for display.
    #[must_use]
    pub fn title(&self) -> Option<&str> {
        match self {
            Self::Repository { repo, .. } => Some(repo),
            Self::Issue { title, .. } | Self::PullRequest { title, .. } => Some(title),
        }
    }

    /// Get the full GitHub URL.
    #[must_use]
    pub fn url(&self) -> String {
        match self {
            Self::Repository { owner, repo, .. } => {
                format!("https://github.com/{}/{}", owner, repo)
            }
            Self::Issue {
                owner,
                repo,
                number,
                ..
            } => {
                format!("https://github.com/{}/{}/issues/{}", owner, repo, number)
            }
            Self::PullRequest {
                owner,
                repo,
                number,
                ..
            } => {
                format!("https://github.com/{}/{}/pull/{}", owner, repo, number)
            }
        }
    }

    /// Get a short reference string (e.g., "owner/repo#123").
    #[must_use]
    pub fn short_ref(&self) -> String {
        match self {
            Self::Repository { owner, repo, .. } => format!("{}/{}", owner, repo),
            Self::Issue {
                owner,
                repo,
                number,
                ..
            }
            | Self::PullRequest {
                owner,
                repo,
                number,
                ..
            } => format!("{}/{}#{}", owner, repo, number),
        }
    }

    /// Check if the issue/PR is open.
    #[must_use]
    pub fn is_open(&self) -> bool {
        match self {
            Self::Repository { .. } => true, // Repos are always "open"
            Self::Issue { state, .. } | Self::PullRequest { state, .. } => {
                state.to_lowercase() == "open"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_type_youtube() {
        assert_eq!(
            LinkEmbed::detect_type("https://www.youtube.com/watch?v=dQw4w9WgXcQ"),
            EmbedType::YouTube
        );
        assert_eq!(
            LinkEmbed::detect_type("https://youtu.be/dQw4w9WgXcQ"),
            EmbedType::YouTube
        );
        assert_eq!(
            LinkEmbed::detect_type("https://youtube.com/shorts/abc123"),
            EmbedType::YouTube
        );
    }

    #[test]
    fn test_detect_type_twitter() {
        assert_eq!(
            LinkEmbed::detect_type("https://twitter.com/user/status/123456"),
            EmbedType::Twitter
        );
        assert_eq!(
            LinkEmbed::detect_type("https://x.com/user/status/789"),
            EmbedType::Twitter
        );
    }

    #[test]
    fn test_detect_type_github() {
        assert_eq!(
            LinkEmbed::detect_type("https://github.com/rust-lang/rust"),
            EmbedType::GitHubRepo
        );
        assert_eq!(
            LinkEmbed::detect_type("https://github.com/rust-lang/rust/issues/123"),
            EmbedType::GitHubIssue
        );
        assert_eq!(
            LinkEmbed::detect_type("https://github.com/rust-lang/rust/pull/456"),
            EmbedType::GitHubPullRequest
        );
    }

    #[test]
    fn test_detect_type_image() {
        assert_eq!(
            LinkEmbed::detect_type("https://example.com/image.png"),
            EmbedType::Image
        );
        assert_eq!(
            LinkEmbed::detect_type("https://example.com/photo.jpg"),
            EmbedType::Image
        );
        assert_eq!(
            LinkEmbed::detect_type("https://i.imgur.com/abc123"),
            EmbedType::Image
        );
    }

    #[test]
    fn test_detect_type_gif() {
        // .gif extension
        assert_eq!(
            LinkEmbed::detect_type("https://example.com/funny.gif"),
            EmbedType::Gif
        );
        // Klipy CDN
        assert_eq!(
            LinkEmbed::detect_type("https://media.klipy.co/abc123.webp"),
            EmbedType::Gif
        );
        // Giphy
        assert_eq!(
            LinkEmbed::detect_type("https://media.giphy.com/media/xyz/giphy.gif"),
            EmbedType::Gif
        );
        // Tenor
        assert_eq!(
            LinkEmbed::detect_type("https://media.tenor.com/images/abc.gif"),
            EmbedType::Gif
        );
    }

    #[test]
    fn test_detect_type_generic() {
        assert_eq!(
            LinkEmbed::detect_type("https://example.com/article"),
            EmbedType::Generic
        );
        assert_eq!(
            LinkEmbed::detect_type("https://news.ycombinator.com"),
            EmbedType::Generic
        );
    }

    #[test]
    fn test_link_preview_builder() {
        let preview = LinkPreview::new("https://example.com".to_string())
            .with_title("Example Site".to_string())
            .with_description("A great example".to_string())
            .with_site_name("Example".to_string());

        assert_eq!(preview.url, "https://example.com");
        assert_eq!(preview.title, Some("Example Site".to_string()));
        assert_eq!(preview.description, Some("A great example".to_string()));
        assert!(preview.has_metadata());
    }

    #[test]
    fn test_link_preview_no_metadata() {
        let preview = LinkPreview::new("https://example.com".to_string());
        assert!(!preview.has_metadata());
    }

    #[test]
    fn test_youtube_embed() {
        let embed = YouTubeEmbed::new(
            "dQw4w9WgXcQ".to_string(),
            "Rick Astley - Never Gonna Give You Up".to_string(),
            "https://i.ytimg.com/vi/dQw4w9WgXcQ/hqdefault.jpg".to_string(),
        )
        .with_channel("Rick Astley".to_string())
        .with_duration(Duration::from_secs(213));

        assert_eq!(embed.watch_url(), "https://www.youtube.com/watch?v=dQw4w9WgXcQ");
        assert_eq!(embed.embed_url(), "https://www.youtube.com/embed/dQw4w9WgXcQ");
        assert_eq!(embed.duration_display(), Some("3:33".to_string()));
    }

    #[test]
    fn test_youtube_extract_video_id() {
        assert_eq!(
            YouTubeEmbed::extract_video_id("https://www.youtube.com/watch?v=dQw4w9WgXcQ"),
            Some("dQw4w9WgXcQ".to_string())
        );
        assert_eq!(
            YouTubeEmbed::extract_video_id("https://youtu.be/dQw4w9WgXcQ"),
            Some("dQw4w9WgXcQ".to_string())
        );
        assert_eq!(
            YouTubeEmbed::extract_video_id("https://youtube.com/shorts/abc123"),
            Some("abc123".to_string())
        );
        assert_eq!(
            YouTubeEmbed::extract_video_id("https://www.youtube.com/watch?v=abc&t=123"),
            Some("abc".to_string())
        );
    }

    #[test]
    fn test_twitter_embed() {
        let embed = TwitterEmbed::new(
            "123456789".to_string(),
            "Elon Musk".to_string(),
            "elonmusk".to_string(),
            "Hello Twitter!".to_string(),
        );

        assert_eq!(
            embed.tweet_url(),
            "https://twitter.com/elonmusk/status/123456789"
        );
        assert_eq!(embed.author_mention(), "@elonmusk");
    }

    #[test]
    fn test_twitter_extract_tweet_id() {
        assert_eq!(
            TwitterEmbed::extract_tweet_id("https://twitter.com/user/status/123456789"),
            Some("123456789".to_string())
        );
        assert_eq!(
            TwitterEmbed::extract_tweet_id("https://x.com/user/status/987654321"),
            Some("987654321".to_string())
        );
        assert_eq!(
            TwitterEmbed::extract_tweet_id("https://twitter.com/user/status/123?s=20"),
            Some("123".to_string())
        );
    }

    #[test]
    fn test_github_repository() {
        let embed = GitHubEmbed::repository("rust-lang".to_string(), "rust".to_string());

        assert_eq!(embed.url(), "https://github.com/rust-lang/rust");
        assert_eq!(embed.short_ref(), "rust-lang/rust");
        assert_eq!(embed.title(), Some("rust"));
        assert!(embed.is_open());
    }

    #[test]
    fn test_github_issue() {
        let embed = GitHubEmbed::issue(
            "rust-lang".to_string(),
            "rust".to_string(),
            12345,
            "Fix compilation error".to_string(),
            "open".to_string(),
        );

        assert_eq!(embed.url(), "https://github.com/rust-lang/rust/issues/12345");
        assert_eq!(embed.short_ref(), "rust-lang/rust#12345");
        assert_eq!(embed.title(), Some("Fix compilation error"));
        assert!(embed.is_open());
    }

    #[test]
    fn test_github_pull_request() {
        let embed = GitHubEmbed::pull_request(
            "rust-lang".to_string(),
            "rust".to_string(),
            67890,
            "Add new feature".to_string(),
            "closed".to_string(),
        );

        assert_eq!(embed.url(), "https://github.com/rust-lang/rust/pull/67890");
        assert_eq!(embed.short_ref(), "rust-lang/rust#67890");
        assert!(!embed.is_open());
    }

    #[test]
    fn test_link_embed_is_media() {
        let youtube = LinkEmbed::YouTube(YouTubeEmbed::new(
            "test".to_string(),
            "Test".to_string(),
            "https://example.com/thumb.jpg".to_string(),
        ));
        assert!(youtube.is_media());

        let image = LinkEmbed::Image {
            url: "https://example.com/image.png".to_string(),
            width: Some(800),
            height: Some(600),
        };
        assert!(image.is_media());

        let generic = LinkEmbed::Generic(LinkPreview::new("https://example.com".to_string()));
        assert!(!generic.is_media());
    }

    #[test]
    fn test_link_embed_preview_image() {
        let preview = LinkPreview::new("https://example.com".to_string())
            .with_image("https://example.com/og.png".to_string());
        let embed = LinkEmbed::Generic(preview);
        assert_eq!(embed.preview_image(), Some("https://example.com/og.png"));

        let youtube = LinkEmbed::YouTube(YouTubeEmbed::new(
            "test".to_string(),
            "Test".to_string(),
            "https://i.ytimg.com/thumb.jpg".to_string(),
        ));
        assert_eq!(youtube.preview_image(), Some("https://i.ytimg.com/thumb.jpg"));
    }
}
