//! Link preview generator.
//!
//! Extracts OpenGraph metadata for generic link previews.

use crate::domain::embed::LinkPreview;
use crate::error::media_error::MediaError;
use async_trait::async_trait;
use std::time::Duration;

/// Maximum response body size to download (100KB).
const MAX_BODY_SIZE: usize = 100 * 1024;

/// Abstraction over link-preview fetching.
///
/// This trait gives the caching service in `services::embed_service` a seam
/// it can mock out without touching the network. The real implementation
/// (`LinkPreviewGenerator`) performs an HTTP GET and parses OpenGraph tags.
#[async_trait]
pub trait PreviewFetcher: Send + Sync {
    /// Fetch and parse a `LinkPreview` for the given URL.
    async fn fetch_preview(&self, url: &str) -> Result<LinkPreview, MediaError>;
}

/// Link preview generator using OpenGraph meta tags.
pub struct LinkPreviewGenerator {
    client: reqwest::Client,
}

impl LinkPreviewGenerator {
    /// Create a new link preview generator.
    #[must_use]
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::limited(5))
            .user_agent("ChatterBot/1.0")
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self { client }
    }

    /// Generate a link preview from a URL.
    ///
    /// Fetches the URL and parses OpenGraph meta tags from the HTML.
    pub async fn generate(&self, url: &str) -> Result<LinkPreview, MediaError> {
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| MediaError::EmbedError(format!("HTTP request failed: {}", e)))?;

        if !response.status().is_success() {
            return Err(MediaError::EmbedError(format!(
                "Request failed with status: {}",
                response.status()
            )));
        }

        // Get response body with size limit
        let html = self.read_limited_body(response).await?;

        // Parse OpenGraph tags
        let mut preview = LinkPreview::new(url.to_string());

        // Extract og:title or fallback to <title>
        if let Some(title) = extract_meta_content(&html, "og:title") {
            preview = preview.with_title(title);
        } else if let Some(title) = extract_title_tag(&html) {
            preview = preview.with_title(title);
        }

        // Extract og:description
        if let Some(desc) = extract_meta_content(&html, "og:description") {
            preview = preview.with_description(desc);
        } else if let Some(desc) = extract_meta_content(&html, "description") {
            preview = preview.with_description(desc);
        }

        // Extract og:image
        if let Some(image) = extract_meta_content(&html, "og:image") {
            preview = preview.with_image(image);
        }

        // Extract og:site_name
        if let Some(site_name) = extract_meta_content(&html, "og:site_name") {
            preview = preview.with_site_name(site_name);
        }

        // Try to extract favicon
        if let Some(favicon) = extract_favicon(&html, url) {
            preview = preview.with_favicon(favicon);
        }

        Ok(preview)
    }

    /// Read response body with size limit.
    async fn read_limited_body(&self, response: reqwest::Response) -> Result<String, MediaError> {
        let bytes = response
            .bytes()
            .await
            .map_err(|e| MediaError::EmbedError(format!("Failed to read response: {}", e)))?;

        // Limit to first MAX_BODY_SIZE bytes
        let limited = if bytes.len() > MAX_BODY_SIZE {
            &bytes[..MAX_BODY_SIZE]
        } else {
            &bytes[..]
        };

        String::from_utf8_lossy(limited).into_owned().pipe(Ok)
    }
}

impl Default for LinkPreviewGenerator {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PreviewFetcher for LinkPreviewGenerator {
    async fn fetch_preview(&self, url: &str) -> Result<LinkPreview, MediaError> {
        self.generate(url).await
    }
}

/// Helper trait for pipe operations.
trait Pipe: Sized {
    fn pipe<F, R>(self, f: F) -> R
    where
        F: FnOnce(Self) -> R,
    {
        f(self)
    }
}

impl<T> Pipe for T {}

/// Extract meta tag content by property or name.
fn extract_meta_content(html: &str, property: &str) -> Option<String> {
    // Try property="..." format (OpenGraph)
    let property_pattern = format!("property=\"{}\"", property);
    if let Some(content) = find_meta_content(html, &property_pattern) {
        return Some(decode_html_entities(&content));
    }

    // Try property='...' format (single quotes)
    let property_pattern_sq = format!("property='{}'", property);
    if let Some(content) = find_meta_content(html, &property_pattern_sq) {
        return Some(decode_html_entities(&content));
    }

    // Try name="..." format (standard meta)
    let name_pattern = format!("name=\"{}\"", property);
    if let Some(content) = find_meta_content(html, &name_pattern) {
        return Some(decode_html_entities(&content));
    }

    // Try name='...' format (single quotes)
    let name_pattern_sq = format!("name='{}'", property);
    if let Some(content) = find_meta_content(html, &name_pattern_sq) {
        return Some(decode_html_entities(&content));
    }

    None
}

/// Find content attribute value in a meta tag.
fn find_meta_content(html: &str, property_match: &str) -> Option<String> {
    let html_lower = html.to_lowercase();
    let property_lower = property_match.to_lowercase();

    // Find the meta tag containing the property
    let meta_pos = html_lower.find(&property_lower)?;

    // Find the <meta tag start before this position
    let tag_start = html_lower[..meta_pos].rfind("<meta")?;

    // Find the tag end
    let tag_end = html_lower[tag_start..].find('>')? + tag_start;

    let tag = &html[tag_start..=tag_end];

    // Extract content="..." or content='...'
    if let Some(content_start) = tag.to_lowercase().find("content=\"") {
        let start = content_start + 9;
        if let Some(end) = tag[start..].find('"') {
            return Some(tag[start..start + end].to_string());
        }
    }

    if let Some(content_start) = tag.to_lowercase().find("content='") {
        let start = content_start + 9;
        if let Some(end) = tag[start..].find('\'') {
            return Some(tag[start..start + end].to_string());
        }
    }

    None
}

/// Extract <title> tag content.
fn extract_title_tag(html: &str) -> Option<String> {
    let html_lower = html.to_lowercase();
    let start = html_lower.find("<title")? + 6;
    let after_tag = html_lower[start..].find('>')? + 1;
    let content_start = start + after_tag;
    let content_end = html_lower[content_start..].find("</title>")? + content_start;

    let title = html[content_start..content_end].trim().to_string();
    if title.is_empty() {
        None
    } else {
        Some(decode_html_entities(&title))
    }
}

/// Extract favicon URL from HTML.
fn extract_favicon(html: &str, page_url: &str) -> Option<String> {
    let html_lower = html.to_lowercase();

    // Look for <link rel="icon" or <link rel="shortcut icon"
    for rel in ["rel=\"icon\"", "rel='icon'", "rel=\"shortcut icon\"", "rel='shortcut icon'"] {
        if let Some(pos) = html_lower.find(rel) {
            // Find the <link tag containing this
            if let Some(tag_start) = html_lower[..pos].rfind("<link") {
                let tag_end = html_lower[tag_start..].find('>')? + tag_start;
                let tag = &html[tag_start..=tag_end];

                // Extract href
                if let Some(href) = extract_href(tag) {
                    return Some(resolve_url(&href, page_url));
                }
            }
        }
    }

    // Fallback: try /favicon.ico at domain root
    if let Some(base) = extract_base_url(page_url) {
        return Some(format!("{}/favicon.ico", base));
    }

    None
}

/// Extract href attribute value.
fn extract_href(tag: &str) -> Option<String> {
    let tag_lower = tag.to_lowercase();

    if let Some(start) = tag_lower.find("href=\"") {
        let start = start + 6;
        if let Some(end) = tag[start..].find('"') {
            return Some(tag[start..start + end].to_string());
        }
    }

    if let Some(start) = tag_lower.find("href='") {
        let start = start + 6;
        if let Some(end) = tag[start..].find('\'') {
            return Some(tag[start..start + end].to_string());
        }
    }

    None
}

/// Extract base URL (scheme + host) from a full URL.
fn extract_base_url(url: &str) -> Option<String> {
    // Find the scheme
    let scheme_end = url.find("://")?;
    let scheme = &url[..scheme_end];

    // Find the host portion
    let after_scheme = &url[scheme_end + 3..];
    let host_end = after_scheme.find('/').unwrap_or(after_scheme.len());
    let host = &after_scheme[..host_end];

    if host.is_empty() {
        None
    } else {
        Some(format!("{}://{}", scheme, host))
    }
}

/// Resolve a potentially relative URL against a base URL.
fn resolve_url(href: &str, base_url: &str) -> String {
    // Already absolute
    if href.starts_with("http://") || href.starts_with("https://") {
        return href.to_string();
    }

    // Protocol-relative
    if href.starts_with("//") {
        let scheme = if base_url.starts_with("https://") {
            "https"
        } else {
            "http"
        };
        return format!("{}:{}", scheme, href);
    }

    // Root-relative URL
    if href.starts_with('/') {
        if let Some(base) = extract_base_url(base_url) {
            return format!("{}{}", base, href);
        }
    }

    // Relative URL - join with directory of base
    if let Some(last_slash) = base_url.rfind('/') {
        // Make sure we're not cutting into the scheme
        if last_slash > base_url.find("://").map(|i| i + 2).unwrap_or(0) {
            return format!("{}/{}", &base_url[..last_slash], href);
        }
    }

    // Fallback: just return the href
    href.to_string()
}

/// Decode basic HTML entities.
fn decode_html_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&#x27;", "'")
        .replace("&nbsp;", " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generator_creation() {
        let generator = LinkPreviewGenerator::new();
        assert!(std::mem::size_of_val(&generator) > 0);
    }

    #[test]
    fn test_default_implementation() {
        let _generator = LinkPreviewGenerator::default();
    }

    #[test]
    fn test_extract_meta_content_og() {
        let html = r#"<html><head><meta property="og:title" content="Test Title"></head></html>"#;
        assert_eq!(
            extract_meta_content(html, "og:title"),
            Some("Test Title".to_string())
        );
    }

    #[test]
    fn test_extract_meta_content_name() {
        let html = r#"<html><head><meta name="description" content="Test Description"></head></html>"#;
        assert_eq!(
            extract_meta_content(html, "description"),
            Some("Test Description".to_string())
        );
    }

    #[test]
    fn test_extract_meta_content_single_quotes() {
        let html = r#"<html><head><meta property='og:title' content='Single Quote Title'></head></html>"#;
        assert_eq!(
            extract_meta_content(html, "og:title"),
            Some("Single Quote Title".to_string())
        );
    }

    #[test]
    fn test_extract_title_tag() {
        let html = r#"<html><head><title>Page Title</title></head></html>"#;
        assert_eq!(extract_title_tag(html), Some("Page Title".to_string()));
    }

    #[test]
    fn test_extract_title_tag_with_whitespace() {
        let html = r#"<html><head><title>  Trimmed Title  </title></head></html>"#;
        assert_eq!(extract_title_tag(html), Some("Trimmed Title".to_string()));
    }

    #[test]
    fn test_decode_html_entities() {
        assert_eq!(decode_html_entities("&amp;"), "&");
        assert_eq!(decode_html_entities("&lt;"), "<");
        assert_eq!(decode_html_entities("&gt;"), ">");
        assert_eq!(decode_html_entities("&quot;"), "\"");
        assert_eq!(decode_html_entities("Tom &amp; Jerry"), "Tom & Jerry");
    }

    #[test]
    fn test_resolve_url_absolute() {
        assert_eq!(
            resolve_url("https://example.com/image.png", "https://other.com"),
            "https://example.com/image.png"
        );
    }

    #[test]
    fn test_resolve_url_relative() {
        assert_eq!(
            resolve_url("/images/logo.png", "https://example.com/page"),
            "https://example.com/images/logo.png"
        );
    }

    #[test]
    fn test_resolve_url_protocol_relative() {
        assert_eq!(
            resolve_url("//cdn.example.com/img.png", "https://example.com"),
            "https://cdn.example.com/img.png"
        );
    }

    #[test]
    fn test_extract_href() {
        let tag = r#"<link rel="icon" href="/favicon.ico">"#;
        assert_eq!(extract_href(tag), Some("/favicon.ico".to_string()));
    }

    #[test]
    fn test_extract_favicon_fallback() {
        let html = r#"<html><head><title>No Favicon</title></head></html>"#;
        let favicon = extract_favicon(html, "https://example.com/page");
        assert_eq!(favicon, Some("https://example.com/favicon.ico".to_string()));
    }
}
