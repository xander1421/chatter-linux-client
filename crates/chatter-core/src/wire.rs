//! Wire-format types needed by UI for message construction/parsing.
//!
//! Re-exports protobuf types so consumers don't import from `proto` directly.

pub use crate::proto::content_v1::{
    AttachmentPointer, Content, DataMessage, SenderKeyDistribution,
    AnonGroupMemberInfo,
};
pub use crate::proto::content_v1::content;
pub use crate::proto::content_v1::anon_call_hangup;
pub use crate::proto::chat_v1::{IntegrationConfig, IntegrationType};
pub use crate::proto::UserPresence;
