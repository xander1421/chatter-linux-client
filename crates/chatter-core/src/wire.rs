//! Wire-format types needed by UI for message construction/parsing.
//!
//! Re-exports protobuf types so consumers don't import from `proto` directly.

pub use chatter_proto::UserPresence;
pub use chatter_proto::chat_v1::{IntegrationConfig, IntegrationType};
pub use chatter_proto::content_v1::content;
pub use chatter_proto::content_v1::{
    AnonGroupMemberInfo, AttachmentPointer, Content, DataMessage, SenderKeyDistribution,
};
