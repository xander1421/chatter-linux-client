//! Saved message domain entity.
//!
//! Represents messages saved to personal vault. Users can save text, files,
//! links, or entire messages from conversations for later reference.

use crate::domain::ids::{AttachmentId, ChannelId, MessageId, SavedMessageId, UserId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ============================================================================
// SavedContent
// ============================================================================

/// Content variants for saved items.
///
/// Each variant stores the actual data needed to display the saved item
/// independently of the original source.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SavedContent {
    /// Saved text content.
    Text {
        /// The text content that was saved.
        content: String,
    },
    /// Saved file reference.
    File {
        /// Reference to the attachment.
        attachment_id: AttachmentId,
        /// Original filename for display.
        filename: String,
    },
    /// Saved link/URL.
    Link {
        /// The URL that was saved.
        url: String,
        /// Optional title for the link.
        title: Option<String>,
    },
    /// Full saved message reference.
    Message {
        /// Reference to the original message.
        message_id: MessageId,
    },
}

impl SavedContent {

}

// ============================================================================
// SavedMessage
// ============================================================================

/// A message saved to the user's personal vault.
///
/// Saved messages allow users to bookmark important content from conversations
/// for later reference. They support:
/// - Multiple content types (text, files, links, full messages)
/// - Source tracking (which channel/message it came from)
/// - Personal notes
/// - Custom tags for organization
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedMessage {
    /// Unique identifier for this saved message.
    pub id: SavedMessageId,
    /// User who saved this message.
    pub user_id: UserId,
    /// The saved content.
    pub content: SavedContent,
    /// Channel where the content originated (if applicable).
    pub source_channel_id: Option<ChannelId>,
    /// Original message ID (if saved from a message).
    pub source_message_id: Option<MessageId>,
    /// User's personal note about this saved item.
    pub note: Option<String>,
    /// User-defined tags for organization.
    pub tags: Vec<String>,
    /// When the item was saved.
    pub saved_at: DateTime<Utc>,
}

impl SavedMessage {

}

