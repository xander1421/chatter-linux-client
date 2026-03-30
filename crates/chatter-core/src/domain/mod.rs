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
pub mod anon_primitives;
pub mod anon_group;
pub mod anon_dm;
pub mod anon_dm_init;
pub mod private_friend_code;
pub mod jarvis;
pub mod section;

// Re-export core types for convenience
pub use ids::{
    AttachmentId, CallId, ChannelId, DeviceId, DraftId, IntegrationId, MessageId, NotificationId,
    ReactionId, SavedMessageId, SessionId, ThreadId, UserId,
};
pub use user::{Device, Friendship, FriendshipStatus, PresenceStatus, Profile, StatusMessage, User};
pub use message::{
    AIMessage, CallMessage, CallMessageType, ConversationType, EncryptedContent,
    EncryptedMessageType, Message, MessageContent, MixedContent, ReactionCount,
    SealedSenderContent, SystemMessage, Thread,
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
pub use anon_primitives::{AnonId, AnonMailbox, AnonMessageId, AnonRateLimitEntry};
pub use anon_group::{
    AnonGroup, AnonGroupId, AnonGroupMailbox, AnonGroupMember, AnonGroupPending,
    AnonGroupRole, PendingActionType,
};
pub use anon_dm::{
    AnonDm, AnonDmId, AnonDmMailbox, AnonDmMessage, AnonDmMessageId, AnonDmStatus,
};
pub use anon_dm_init::{AnonDmInitMessage, InitMessageError};
pub use private_friend_code::{PrivateFriendCode, PrivateFriendCodeError};
pub use section::{CustomSection, SectionConversation};
pub use jarvis::{
    ContextType, EntityKnowledge, EntityType, JarvisContext, JarvisConversation,
    JarvisConversationId, JarvisEntity, JarvisEntityId, JarvisEntityMention,
    JarvisFact, JarvisFactId, JarvisRelationship, JarvisRelationshipId,
    JarvisSummary, JarvisSummaryId, Role, SubjectType,
};
