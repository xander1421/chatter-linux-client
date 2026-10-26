//! Domain layer - core business entities.
//!
//! Pure Rust structs with no external dependencies. These are the source of truth
//! for all business logic in the application.

pub mod anon_dm;
pub mod anon_dm_init;
pub mod anon_group;
pub mod anon_primitives;
pub mod attachment;
pub mod call;
pub mod channel;
pub mod ephemeral;
pub mod ids;
pub mod link;
#[cfg(feature = "jarvis")]
pub mod jarvis;
pub mod mention;
pub mod message;
pub mod moderation;
pub mod notification;
pub mod private_friend_code;
pub mod reaction;
pub mod saved_message;
pub mod section;
pub mod unified_conversation;
pub mod user;
pub mod username_link;

// Re-export core types for convenience
pub use anon_dm::{AnonDm, AnonDmId, AnonDmMailbox, AnonDmMessage, AnonDmMessageId, AnonDmStatus};
pub use anon_dm_init::{AnonDmInitMessage, InitMessageError};
pub use anon_group::{
    AnonGroup, AnonGroupId, AnonGroupMailbox, AnonGroupMember, AnonGroupPending, AnonGroupRole,
    PendingActionType,
};
pub use anon_primitives::{AnonId, AnonMailbox, AnonMessageId, AnonRateLimitEntry};
pub use attachment::{
    Attachment, AttachmentStatus, FileCategory, FileMetadata, GifMetadata, GifProvider,
    SharedCategory, Thumbnail, extract_links,
};
pub use call::{
    Call, CallDirection, CallEndReason, CallHistoryEntry, CallHistoryFilter, CallResult, CallState,
    CallType, Participant, VerificationBadge,
};
pub use channel::{
    Channel, ChannelConfig, ChannelType, MemberRole, Membership, MessagePersistence,
    NotificationPreference, Permissions, Template,
};
pub use ephemeral::{EphemeralMessage, EphemeralState, MessageTTL};
pub use ids::{
    AttachmentId, CallId, ChannelId, DeviceId, IntegrationId, MessageId, NotificationId,
    ReactionId, SavedMessageId, SessionId, ThreadId, UserId,
};
#[cfg(feature = "jarvis")]
pub use jarvis::{
    ContextType, EntityKnowledge, EntityType, JarvisContext, JarvisConversation,
    JarvisConversationId, JarvisEntity, JarvisEntityId, JarvisEntityMention, JarvisFact,
    JarvisFactId, JarvisRelationship, JarvisRelationshipId, JarvisSummary, JarvisSummaryId, Role,
    SubjectType,
};
pub use mention::{Mention, MentionExtractor, MentionSuggestion, MentionType};
pub use message::{
    AIMessage, CallMessage, CallMessageType, ConversationType, EncryptedContent,
    EncryptedMessageType, Message, MessageActions, MessageContent, MessageMenu, MixedContent,
    ReactionCount, SealedSenderContent, SystemMessage, Thread,
};
pub use moderation::{Ban, ModerationAction, ModerationLogEntry, Mute};
pub use notification::{
    CallNotificationType, Notification, NotificationPriority, NotificationType,
};
pub use private_friend_code::{PrivateFriendCode, PrivateFriendCodeError};
pub use reaction::{Emoji, EmojiCategory, Reaction};
pub use saved_message::{SavedContent, SavedMessage};
pub use section::{CustomSection, SectionConversation};
pub use unified_conversation::UnifiedConversation;
pub use user::{
    Device, Friendship, FriendshipStatus, PresenceStatus, Profile, StatusMessage, User,
};
pub use username_link::{UsernameLink, UsernameLinkParseError};
