//! Message gateway.
//!
//! This module re-exports from the modular gateway service.
//! See `gateway/` for the implementation.

pub use crate::services::gateway::{
    // Content builders
    build_text_content, build_data_message_content, build_reply_content,
    build_typing_content, build_typing_content_with_channel,
    build_read_receipt_content, build_read_receipt_content_with_channel,
    build_delivery_receipt_content, build_delivery_receipt_content_with_channel,
    build_reaction_content, build_reaction_content_with_channel,
    build_pin_content, build_pin_content_with_channel,
    build_edit_content, build_edit_content_with_channel,
    build_delete_for_me_content,
    build_delete_for_everyone_content, build_delete_for_everyone_content_with_channel,
    // DM call key exchange (ephemeral keys for PFS)
    build_call_key_offer, build_call_key_answer,
    // Group call key distribution
    build_group_call_key_content,
    // Call hangup
    build_call_hangup_content,
    // Types
    CallHangupReason, DmContent, DecryptResult, RouteResult, SealedSenderResult,
    // Struct
    MessageGateway,
};
