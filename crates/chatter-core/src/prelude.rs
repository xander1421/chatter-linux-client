//! Convenience re-exports for SDK consumers.
//!
//! ```rust,no_run
//! use chatter_core::prelude::*;
//! ```

// Client
pub use crate::client::{ChatterClient, ClientBuilder, ClientError, SendOptions, UploadedAttachments};
pub use crate::services::auth_service::LoginResult;

// Crypto-adjacent helpers commonly needed by callers (auth flows,
// decrypted-buffer handling). Surfaced here so UI code doesn't have to
// reach into the `crypto::` module — the layering rule is "UI never
// imports `crypto::`" and that is enforced via the prelude.
pub use crate::crypto::{SecretString, SecureBuffer};

// Service-level filters that show up in the public façade.
pub use crate::services::user::FriendRequestFilter;

// Jarvis context (used by `ChatterClient::jarvis_ask` etc.)
#[cfg(feature = "jarvis")]
pub use crate::domain::jarvis::{ContextType as JarvisContextType, JarvisContext};

// Device link approval flow — the pending struct is `!Clone` and feeds
// `ChatterClient::approve_link`.
pub use crate::services::link_device_service::PendingDeviceLink;

// Domain types
pub use crate::domain::{
    Attachment, AttachmentId, Channel, ChannelId, ChannelType, Message, MessageContent, MessageId,
    Profile, User, UserId,
};
pub use crate::domain::channel::{ChannelConfig, MessagePersistence};
pub use crate::domain::anon_dm::{AnonDm, AnonDmId};
pub use crate::domain::anon_group::{AnonGroup, AnonGroupId, AnonGroupRole};
pub use crate::domain::user::{Friendship, StatusMessage};

// Events
pub use crate::events::{DomainEvent, EventBus};

// Conversation operations
pub use crate::services::conversation::{ConversationHandle, ConversationError};
pub use crate::services::gateway::outgoing::{QuoteIntent, SendError, SendResult};

// State
pub use crate::state::{ActiveConversation, AuthState, ChatState, MessageState};
pub use crate::state::chat_state::PendingAttachment;

// Domain enums commonly needed at the public surface.
pub use crate::domain::user::PresenceStatus;

// Errors
pub use crate::app::ContextError;
