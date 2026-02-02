//! Saved message domain entity.
//!
//! Represents messages saved to personal vault.

/// Saved message in personal vault.
pub struct SavedMessage;

/// Saved content types.
pub enum SavedContent {
    /// Saved text message.
    Text,
    /// Saved file.
    File,
    /// Saved link.
    Link,
}
