//! Domain layer - core business entities.
//!
//! Pure Rust structs with no external dependencies. These are the source of truth
//! for all business logic in the application.

pub mod ids;
pub mod user;
pub mod message;
pub mod channel;
pub mod attachment;
pub mod reaction;
pub mod call;
pub mod notification;
pub mod ai;
pub mod moderation;
pub mod saved_message;
pub mod draft;
pub mod ephemeral;
pub mod mention;
pub mod embed;

// Re-export core types for convenience
pub use ids::{
    AttachmentId, CallId, ChannelId, DeviceId, DraftId, IntegrationId, MessageId, NotificationId,
    ReactionId, SavedMessageId, SessionId, ThreadId, UserId,
};
pub use user::{Device, Friendship, FriendshipStatus, PresenceStatus, Profile, StatusMessage, User};
pub use message::{
    AIMessage, CallMessage, CallMessageType, EncryptedContent, EncryptedMessageType, Message,
    MessageContent, MixedContent, ReactionCount, SealedSenderContent, SystemMessage, Thread,
};
pub use channel::{
    Channel, ChannelConfig, ChannelType, MemberRole, Membership, MessagePersistence,
    NotificationPreference, Permissions, Template,
};
pub use attachment::{
    Attachment, AttachmentStatus, FileCategory, FileMetadata, GifMetadata, GifProvider, Thumbnail,
};
pub use reaction::{Emoji, EmojiCategory, Reaction};
pub use notification::{
    CallNotificationType, Notification, NotificationPriority, NotificationType,
};
pub use call::{Call, CallEndReason, CallState, CallType, Participant, VerificationBadge};
pub use moderation::{Ban, ModerationAction, ModerationLogEntry, Mute};
pub use draft::{Draft, DraftKey};
pub use ephemeral::{EphemeralMessage, EphemeralState, MessageTTL};
pub use saved_message::{SavedContent, SavedMessage};
pub use mention::{Mention, MentionExtractor, MentionSuggestion, MentionType};
pub use embed::{EmbedType, GitHubEmbed, LinkEmbed, LinkPreview, TwitterEmbed, YouTubeEmbed};
