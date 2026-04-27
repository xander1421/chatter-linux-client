//! Link embeds.
//!
//! YouTube embeds and link previews for URLs shared in messages.

pub mod link_preview;
pub mod unfurl;
pub mod youtube;

// Re-exports for convenient access
pub use link_preview::{LinkPreviewGenerator, PreviewFetcher};
pub use unfurl::UrlUnfurler;
pub use youtube::YouTubeExtractor;
