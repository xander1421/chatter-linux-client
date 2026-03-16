//! Convenience re-exports for SDK consumers.
//!
//! ```rust,no_run
//! use chatter_core::prelude::*;
//! ```

pub use crate::client::{ChatterClient, ClientBuilder, ClientError, SendOptions, UploadedAttachments};
pub use crate::domain::{
    Attachment, AttachmentId, Channel, ChannelId, ChannelType, Message, MessageContent, MessageId,
    Profile, User, UserId,
};
pub use crate::events::{DomainEvent, EventBus, UiEvent};
pub use crate::services::auth_service::LoginResult;
pub use crate::services::conversation::{ConversationHandle, ConversationError};
pub use crate::services::gateway::outgoing::{QuoteIntent, SendError, SendResult};
pub use crate::state::{ActiveConversation, AuthState, ChatState, MessageState};
pub use crate::state::chat_state::PendingAttachment;
pub use crate::proto::UserPresence;
pub use crate::app::ContextError;
