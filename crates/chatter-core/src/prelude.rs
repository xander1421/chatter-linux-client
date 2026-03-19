//! Convenience re-exports for SDK consumers.
//!
//! ```rust,no_run
//! use chatter_core::prelude::*;
//! ```

// Client
pub use crate::client::{ChatterClient, ClientBuilder, ClientError, SendOptions, UploadedAttachments};
pub use crate::services::auth_service::LoginResult;

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
pub use crate::events::{DomainEvent, EventBus, UiEvent};

// Conversation operations
pub use crate::services::conversation::{ConversationHandle, ConversationError};
pub use crate::services::gateway::outgoing::{QuoteIntent, SendError, SendResult};

// State
pub use crate::state::{ActiveConversation, AuthState, ChatState, MessageState};
pub use crate::state::chat_state::PendingAttachment;

// Protocol types
pub use crate::proto::UserPresence;

// Errors
pub use crate::app::ContextError;
