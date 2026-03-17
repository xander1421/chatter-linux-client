//! Outgoing message router.
//!
//! Dispatches `OutgoingIntent` to the correct service based on channel type.
//!
//! # Design
//!
//! Uses enum dispatch (not traits) per the messaging consolidation plan.
//! Each variant carries exactly the data needed for that channel type.
//! The router handles uploads, proto construction, encryption, storage,
//! and returns a unified `SendResult`.
//!
//! # Adding new channel types
//!
//! 1. Add a variant to `OutgoingIntent`
//! 2. Add a match arm in `route_outgoing`
//! 3. Map the service result to `SendResult`

use std::sync::Arc;
use std::path::PathBuf;

use crate::domain::anon_dm::AnonDmId;
use crate::domain::anon_group::AnonGroupId;
use crate::domain::ids::{AttachmentId, ChannelId, IntegrationId, MessageId, UserId};
use crate::domain::message::Message;
use crate::mailbox::group::GroupFanout;
use crate::mailbox::CallRoomToken;
use crate::proto::content_v1::{AttachmentPointer, anon_call_answer, anon_call_hangup};
use crate::services::anon_dm::{AnonDmError, AnonDmService};
use crate::services::anon_group::{AnonGroupError, AnonGroupService};
use crate::domain::attachment::Attachment;
use crate::domain::channel::ChannelType;
use crate::services::mailbox_manager::MailboxManager;
use crate::services::attachment::{AttachmentError, AttachmentService};
use crate::services::channel::ChannelService;
use crate::services::message::{MessageError, MessageService};
use crate::services::user::{UserError, UserService};
use crate::state::ActiveConversation;
use crate::state::chat_state::PendingAttachment;

// =============================================================================
// Intent
// =============================================================================

/// What the user wants to send.
pub enum OutgoingIntent {
    /// Send a message in a channel DM (Signal Protocol).
    ChannelDm {
        peer_id: UserId,
        dm_channel_id: ChannelId,
        text: String,
        /// Pre-uploaded attachment pointers. If empty and file_paths is set,
        /// the gateway uploads them. Prefer pre-uploading via
        /// `upload_pending_attachments()` for consistency with anon paths.
        attachment_pointers: Vec<AttachmentPointer>,
        file_paths: Vec<PathBuf>,
        quote: Option<QuoteIntent>,
    },

    /// Send a message in a regular channel.
    Channel {
        channel_id: ChannelId,
        text: String,
        /// Pre-uploaded attachment pointers. See ChannelDm docs.
        attachment_pointers: Vec<AttachmentPointer>,
        file_paths: Vec<PathBuf>,
        thread_id: Option<MessageId>,
        mentioned_integrations: Vec<IntegrationId>,
        targets_ai_bot: bool,
    },

    /// Send a message in an anonymous DM (NDSS '21 pairwise).
    AnonDm {
        conversation_id: AnonDmId,
        text: String,
        attachment_pointers: Vec<AttachmentPointer>,
        quote: Option<AnonQuoteIntent>,
    },

    /// Send a message in an anonymous group (NDSS '21 pairwise fanout).
    AnonGroup {
        group_id: AnonGroupId,
        text: String,
        attachment_pointers: Vec<AttachmentPointer>,
        quote: Option<AnonQuoteIntent>,
    },

    /// Save a message to the SavedMessages channel (Note to Self).
    SavedMessages {
        text: String,
        attachment_ids: Vec<AttachmentId>,
    },

    /// Initiate a call.
    CallOffer {
        channel_id: CallChannelId,
        video: bool,
        display_name: String,
        token: Option<CallRoomToken>, // Required for Anon
    },

    /// Answer a call.
    CallAnswer {
        channel_id: CallChannelId,
        call_room_id: Vec<u8>,
        accept: bool,
        token: Option<CallRoomToken>, // Required for Anon
        decline_reason: Option<i32>,
        display_name: String,
    },

    /// Hang up a call.
    CallHangup {
        channel_id: CallChannelId,
        call_room_id: Vec<u8>,
        reason: i32,
    },

    /// Connect with a user (Friend Request or Anon DM Request).
    Connect {
        peer_id: UserId,
        message: Option<String>,
        use_anonymous_dm: bool, // If true, starts an Anon DM instead of Friend Request
        private_friend_code: Option<String>, // NDSS'21 compliant code
    },
}

/// Channel ID variant for call signaling.
pub enum CallChannelId {
    Standard(ChannelId),
    AnonDm(AnonDmId),
}

/// Quote/reply context for channel DM outgoing messages.
/// Quote/reply context for outgoing messages (all conversation types).
pub struct QuoteIntent {
    pub author_id: UserId,
    pub timestamp: u64,
    /// Preview text for the quoted message. None for anon paths where
    /// the recipient already has the message locally.
    pub preview_text: Option<String>,
}

/// Alias for backward compatibility.
pub type AnonQuoteIntent = QuoteIntent;

// =============================================================================
// Result
// =============================================================================

/// Unified result from sending a message through the router.
pub struct SendResult {
    pub sent_at: u64,
    pub message: Option<Message>,
    pub attachment_ids: Vec<AttachmentId>,
    pub gif_attachment_ids: Vec<AttachmentId>,
    pub bootstrap_initiated: bool,
    pub anon_message_id: Option<String>,
    pub call_result: Option<CallOfferResult>,
}

/// Result of initiating a call.
pub struct CallOfferResult {
    pub call_room_id: [u8; 16],
    pub room_name: String,
    pub call_encryption_key: Vec<u8>,
}

// =============================================================================
// Error
// =============================================================================

/// Unified send error across all channel types.
#[derive(Debug, thiserror::Error)]
pub enum SendError {
    #[error(transparent)]
    Message(#[from] MessageError),

    #[error(transparent)]
    AnonDm(#[from] AnonDmError),

    #[error(transparent)]
    AnonGroup(#[from] AnonGroupError),

    #[error(transparent)]
    User(#[from] UserError),

    #[error("Not logged in")]
    NotLoggedIn,

    #[error("Action not supported for this channel type")]
    UnsupportedAction,

    #[error("attachment upload failed: {0}")]
    Upload(#[from] AttachmentError),
}

// =============================================================================
// Router
// =============================================================================

/// Route an outgoing message intent to the correct service.
pub async fn route_outgoing(
    intent: OutgoingIntent,
    services: &OutgoingServices<'_>,
) -> Result<SendResult, SendError> {
    match intent {
        OutgoingIntent::ChannelDm {
            peer_id,
            dm_channel_id,
            text,
            attachment_pointers,
            file_paths,
            quote,
        } => {
            Ok(route_channel_dm(
                services.message_service,
                services.mailbox_manager,
                &peer_id,
                &dm_channel_id,
                &text,
                attachment_pointers,
                file_paths,
                quote,
            )
            .await?)
        }

        OutgoingIntent::Channel {
            channel_id,
            text,
            attachment_pointers,
            file_paths,
            thread_id,
            mentioned_integrations,
            targets_ai_bot,
        } => {
            Ok(route_channel(
                services.message_service,
                services.group_fanout,
                &channel_id,
                &text,
                attachment_pointers,
                file_paths,
                thread_id,
                mentioned_integrations,
                targets_ai_bot,
            )
            .await?)
        }

        OutgoingIntent::AnonDm {
            conversation_id,
            text,
            attachment_pointers,
            quote,
        } => {
            let anon_dm_service = services.anon_dm_service
                .ok_or_else(|| SendError::NotLoggedIn)?;
            
            // Safety Check: Centralized Block/Rate Limit
            anon_dm_service.check_conversation_not_blocked(&conversation_id)?;
            anon_dm_service.check_rate_limit(&conversation_id)?;

            route_anon_dm(
                anon_dm_service,
                &conversation_id,
                &text,
                attachment_pointers,
                quote,
            )
            .await
        }

        OutgoingIntent::AnonGroup {
            group_id,
            text,
            attachment_pointers,
            quote,
        } => {
            let anon_group_service = services.anon_group_service
                .ok_or_else(|| SendError::NotLoggedIn)?;
            
            // Safety Check: Centralized Block/Rate Limit
            anon_group_service.check_group_not_blocked(&group_id)?;
            anon_group_service.check_rate_limit(&group_id)?;

            route_anon_group(
                anon_group_service,
                &group_id,
                &text,
                attachment_pointers,
                quote,
            )
            .await
        }

        OutgoingIntent::SavedMessages { text, attachment_ids } => {
            let message = services.message_service
                .save_message(&text, attachment_ids)
                .await?;
            let sent_at = message.sent_at;
            let att_ids = message.attachments.clone();
            Ok(SendResult {
                sent_at,
                message: Some(message),
                attachment_ids: att_ids,
                gif_attachment_ids: vec![],
                bootstrap_initiated: false,
                anon_message_id: None,
                call_result: None,
            })
        }

        OutgoingIntent::CallOffer { channel_id, video, display_name, token } => {
            match channel_id {
                CallChannelId::AnonDm(cid) => {
                    let anon_dm_service = services.anon_dm_service.ok_or(SendError::NotLoggedIn)?;
                    let t = token.ok_or(SendError::UnsupportedAction)?;
                    let res = anon_dm_service.send_call_offer(&cid, &t, video, Some(&display_name)).await?;
                    Ok(SendResult::from_call(res))
                }
                CallChannelId::Standard(_cid) => {
                    // Standard Signal call signaling to be integrated here
                    Err(SendError::UnsupportedAction)
                }
            }
        }

        OutgoingIntent::CallAnswer { channel_id, call_room_id, accept, token, decline_reason, display_name } => {
            let room_id_arr: [u8; 16] = call_room_id.try_into().map_err(|_| SendError::UnsupportedAction)?;
            match channel_id {
                CallChannelId::AnonDm(cid) => {
                    let anon_dm_service = services.anon_dm_service.ok_or(SendError::NotLoggedIn)?;
                    let reason = decline_reason.and_then(|r| anon_call_answer::DeclineReason::try_from(r).ok());
                    anon_dm_service.send_call_answer(&cid, &room_id_arr, accept, token.as_ref(), reason, Some(&display_name)).await?;
                    Ok(SendResult::empty())
                }
                CallChannelId::Standard(_cid) => Err(SendError::UnsupportedAction),
            }
        }

        OutgoingIntent::CallHangup { channel_id, call_room_id, reason } => {
            let room_id_arr: [u8; 16] = call_room_id.try_into().map_err(|_| SendError::UnsupportedAction)?;
            match channel_id {
                CallChannelId::AnonDm(cid) => {
                    let anon_dm_service = services.anon_dm_service.ok_or(SendError::NotLoggedIn)?;
                    let r = anon_call_hangup::Reason::try_from(reason).unwrap_or(anon_call_hangup::Reason::Normal);
                    anon_dm_service.send_call_hangup(&cid, &room_id_arr, r).await?;
                    Ok(SendResult::empty())
                }
                CallChannelId::Standard(_cid) => Err(SendError::UnsupportedAction),
            }
        }

        OutgoingIntent::Connect { peer_id, message, use_anonymous_dm, private_friend_code } => {
            if use_anonymous_dm {
                let anon_dm_service = services.anon_dm_service.ok_or(SendError::NotLoggedIn)?;
                if let Some(code_str) = private_friend_code {
                    let code = crate::domain::private_friend_code::PrivateFriendCode::parse(&code_str)
                        .map_err(|e| SendError::AnonDm(AnonDmError::InvalidFriendCode(e.to_string())))?;
                    anon_dm_service.start_conversation_private(&code, message.as_deref(), None).await?;
                } else {
                    anon_dm_service.start_conversation(peer_id, message.as_deref()).await?;
                }
                Ok(SendResult::empty())
            } else {
                let user_service = services.user_service.ok_or(SendError::NotLoggedIn)?;
                user_service.send_friend_request(&peer_id, message.as_deref()).await?;
                Ok(SendResult::empty())
            }
        }
    }
}

/// Services needed by the outgoing router.
pub struct OutgoingServices<'a> {
    pub message_service: &'a MessageService,
    pub mailbox_manager: &'a MailboxManager,
    pub user_service: Option<&'a UserService>,
    pub group_fanout: Option<&'a GroupFanout>,
    pub anon_dm_service: Option<&'a AnonDmService>,
    pub anon_group_service: Option<&'a AnonGroupService>,
}

impl<'a> OutgoingServices<'a> {
    /// Build from UIContext — fills all service references, eliminating
    /// the per-branch Some/None assembly in send_message.rs.
    ///
    /// `group_fanout` is optional because it's a post-boot service.
    pub fn from_ctx(
        ctx: &'a crate::app::UIContext,
        group_fanout: Option<&'a GroupFanout>,
    ) -> Self {
        Self {
            message_service: &ctx.message_service,
            mailbox_manager: &ctx.mailbox_manager,
            user_service: Some(&ctx.user_service),
            group_fanout,
            anon_dm_service: Some(&ctx.anon_dm_service),
            anon_group_service: Some(&ctx.anon_group_service),
        }
    }
}

/// Owned variant of [`OutgoingServices`] for use across await points.
pub struct OwnedOutgoingServices {
    pub message_service: Arc<MessageService>,
    pub mailbox_manager: Arc<MailboxManager>,
    pub user_service: Option<Arc<UserService>>,
    pub group_fanout: Option<Arc<GroupFanout>>,
    pub anon_dm_service: Option<Arc<AnonDmService>>,
    pub anon_group_service: Option<Arc<AnonGroupService>>,
}

impl OwnedOutgoingServices {
    pub fn from_client(client: &crate::client::ChatterClient) -> Self {
        Self {
            message_service: client.messages().clone(),
            mailbox_manager: client.mailbox_manager().clone(),
            user_service: Some(client.users().clone()),
            group_fanout: client.group_fanout().get().cloned(),
            anon_dm_service: Some(client.anon_dms().clone()),
            anon_group_service: Some(client.anon_groups().clone()),
        }
    }

    fn as_borrowed(&self) -> OutgoingServices<'_> {
        OutgoingServices {
            message_service: &self.message_service,
            mailbox_manager: &self.mailbox_manager,
            user_service: self.user_service.as_deref(),
            group_fanout: self.group_fanout.as_deref(),
            anon_dm_service: self.anon_dm_service.as_deref(),
            anon_group_service: self.anon_group_service.as_deref(),
        }
    }
}

pub async fn route_outgoing_owned(
    intent: OutgoingIntent,
    services: &OwnedOutgoingServices,
) -> Result<SendResult, SendError> {
    route_outgoing(intent, &services.as_borrowed()).await
}

impl SendResult {
    /// Get a unified message ID string (works for all conversation types).
    pub fn message_id_string(&self) -> String {
        if let Some(ref msg) = self.message {
            msg.id.to_string()
        } else if let Some(ref anon_id) = self.anon_message_id {
            anon_id.clone()
        } else {
            String::new()
        }
    }

    /// Check if this was a text-only send (no attachments).
    pub fn is_text_only(&self) -> bool {
        self.attachment_ids.is_empty()
    }

    fn empty() -> Self {
        Self {
            sent_at: crate::util::time::TimestampNanos::now().as_u64(),
            message: None,
            attachment_ids: vec![],
            gif_attachment_ids: vec![],
            bootstrap_initiated: false,
            anon_message_id: None,
            call_result: None,
        }
    }

    fn from_call(res: crate::services::anon_dm::AnonCallOfferResult) -> Self {
        Self {
            sent_at: res.timestamp,
            message: None,
            attachment_ids: vec![],
            gif_attachment_ids: vec![],
            bootstrap_initiated: false,
            anon_message_id: None,
            call_result: Some(CallOfferResult {
                call_room_id: res.call_room_id,
                room_name: res.room_name,
                call_encryption_key: res.call_encryption_key,
            }),
        }
    }
}

// =============================================================================
// Channel DM dispatch
// =============================================================================

async fn route_channel_dm(
    message_service: &MessageService,
    mailbox_manager: &MailboxManager,
    peer_id: &UserId,
    dm_channel_id: &ChannelId,
    text: &str,
    attachment_pointers: Vec<AttachmentPointer>,
    file_paths: Vec<PathBuf>,
    quote: Option<QuoteIntent>,
) -> Result<SendResult, MessageError> {
    // Use pre-uploaded pointers if available, otherwise upload from file paths
    let dm_result = if !attachment_pointers.is_empty() {
        let attachment_ids = attachment_pointers.iter()
            .map(|p| crate::domain::AttachmentId::from_cdn_key(&p.cdn_key))
            .collect();
        message_service.send_dm_with_pointers(
            peer_id, dm_channel_id, text, attachment_pointers, attachment_ids, mailbox_manager,
        ).await?
    } else {
        let path_refs: Vec<&std::path::Path> = file_paths.iter().map(|p| p.as_path()).collect();
        if let Some(q) = quote {
            message_service.send_dm_with_reply(
                peer_id, dm_channel_id, text, path_refs,
                &q.author_id, q.timestamp, q.preview_text.as_deref().unwrap_or(""),
                mailbox_manager,
            ).await?
        } else {
            message_service.send_dm_with_files(peer_id, dm_channel_id, text, path_refs, mailbox_manager).await?
        }
    };

    Ok(SendResult {
        sent_at: dm_result.sent_at,
        message: dm_result.message,
        attachment_ids: dm_result.attachment_ids,
        gif_attachment_ids: dm_result.gif_attachment_ids,
        bootstrap_initiated: dm_result.bootstrap_initiated,
        anon_message_id: None,
        call_result: None,
    })
}

// =============================================================================
// Channel dispatch
// =============================================================================

async fn route_channel(
    message_service: &MessageService,
    group_fanout: Option<&GroupFanout>,
    channel_id: &ChannelId,
    text: &str,
    attachment_pointers: Vec<AttachmentPointer>,
    file_paths: Vec<PathBuf>,
    thread_id: Option<MessageId>,
    mentioned_integrations: Vec<IntegrationId>,
    targets_ai_bot: bool,
) -> Result<SendResult, MessageError> {
    // Use pre-uploaded pointers if available, otherwise upload from file paths
    let message = if !attachment_pointers.is_empty() {
        let attachment_ids = attachment_pointers.iter()
            .map(|p| crate::domain::AttachmentId::from_cdn_key(&p.cdn_key))
            .collect();
        message_service.send_with_pointers(
            group_fanout, channel_id, text, attachment_pointers, attachment_ids,
            thread_id.as_ref(), mentioned_integrations, targets_ai_bot,
        ).await?
    } else {
        let path_refs: Vec<&std::path::Path> = file_paths.iter().map(|p| p.as_path()).collect();
        message_service.send_with_files(
            group_fanout, channel_id, text, path_refs,
            thread_id.as_ref(), mentioned_integrations, targets_ai_bot,
        ).await?
    };

    let sent_at = message.sent_at;
    let attachment_ids = message.attachments.clone();

    Ok(SendResult {
        sent_at,
        message: Some(message),
        attachment_ids,
        gif_attachment_ids: vec![],
        bootstrap_initiated: false,
        anon_message_id: None,
        call_result: None,
    })
}

// =============================================================================
// Anonymous DM dispatch
// =============================================================================

async fn route_anon_dm(
    anon_dm_service: &AnonDmService,
    conversation_id: &AnonDmId,
    text: &str,
    attachment_pointers: Vec<AttachmentPointer>,
    quote: Option<AnonQuoteIntent>,
) -> Result<SendResult, SendError> {
    let (message_id, sent_at) = if let Some(q) = quote {
        anon_dm_service
            .send_reply(conversation_id, text, attachment_pointers, &q.author_id, q.timestamp)
            .await?
    } else {
        anon_dm_service
            .send_with_attachments(conversation_id, text, attachment_pointers)
            .await?
    };

    Ok(SendResult {
        sent_at,
        message: None, // Anon DM uses its own message type, not domain::Message
        attachment_ids: vec![],
        gif_attachment_ids: vec![],
        bootstrap_initiated: false,
        anon_message_id: Some(message_id.as_str().to_string()),
        call_result: None,
    })
}

// =============================================================================
// Anonymous Group dispatch
// =============================================================================

async fn route_anon_group(
    anon_group_service: &AnonGroupService,
    group_id: &AnonGroupId,
    text: &str,
    attachment_pointers: Vec<AttachmentPointer>,
    quote: Option<AnonQuoteIntent>,
) -> Result<SendResult, SendError> {
    let (message_id, sent_at) = if let Some(q) = quote {
        anon_group_service
            .send_reply(group_id, text, attachment_pointers, &q.author_id, q.timestamp)
            .await?
    } else if attachment_pointers.is_empty() {
        anon_group_service
            .send_text(group_id, text)
            .await?
    } else {
        anon_group_service
            .send_with_attachments(group_id, text, attachment_pointers)
            .await?
    };

    Ok(SendResult {
        sent_at,
        message: None, // Anon group uses its own message type, not domain::Message
        attachment_ids: vec![],
        gif_attachment_ids: vec![],
        bootstrap_initiated: false,
        anon_message_id: Some(message_id.as_str().to_string()),
        call_result: None,
    })
}

// =============================================================================
// Send options + upload helpers (shared by ChatterClient and UIContext)
// =============================================================================

/// Options for [`send_message()`].
#[derive(Debug, Default)]
pub struct SendOptions {
    /// Thread ID for thread replies (channels only).
    pub thread_id: Option<MessageId>,
    /// Integration IDs mentioned in the message (channels only).
    pub mentioned_integrations: Vec<IntegrationId>,
    /// Whether this message targets an AI bot (channels only).
    pub targets_ai_bot: bool,
}

/// Result of uploading pending attachments.
pub struct UploadedAttachments {
    /// AttachmentPointer protos for the Content proto.
    pub pointers: Vec<AttachmentPointer>,
    /// Full Attachment domain objects for display.
    pub attachments: Vec<Attachment>,
}

/// Upload pending attachments to CDN, returning pointers and display data.
pub async fn upload_attachments(
    pending: &[PendingAttachment],
    attachment_service: &AttachmentService,
) -> UploadedAttachments {
    let mut pointers = Vec::with_capacity(pending.len());
    let mut attachments = Vec::with_capacity(pending.len());

    for att in pending {
        if let Some(pointer) = &att.pointer {
            pointers.push(pointer.clone());
            if let Ok(Some(full_att)) = attachment_service.get(&att.id) {
                attachments.push(full_att);
            }
            continue;
        }

        match attachment_service.upload_to_cdn_encrypted(&att.path).await {
            Ok(res) => {
                pointers.push(res.to_attachment_pointer());
                if let Ok(Some(full_att)) = attachment_service.get(&res.attachment_id) {
                    attachments.push(full_att);
                }
            }
            Err(e) => {
                tracing::error!(
                    path = %att.path.display(),
                    error = %e,
                    "Failed to upload attachment"
                );
            }
        }
    }

    UploadedAttachments { pointers, attachments }
}

/// Map `ActiveConversation` to `OutgoingIntent`.
pub fn build_intent(
    conversation: ActiveConversation,
    text: &str,
    attachment_pointers: Vec<AttachmentPointer>,
    quote: Option<QuoteIntent>,
    options: &SendOptions,
    channel_service: &ChannelService,
) -> Result<OutgoingIntent, SendError> {
    match conversation {
        ActiveConversation::AnonDm(id) => Ok(OutgoingIntent::AnonDm {
            conversation_id: id,
            text: text.to_string(),
            attachment_pointers,
            quote,
        }),
        ActiveConversation::AnonGroup(id) => Ok(OutgoingIntent::AnonGroup {
            group_id: id,
            text: text.to_string(),
            attachment_pointers,
            quote,
        }),
        ActiveConversation::Dm { channel_id, peer_id } => Ok(OutgoingIntent::ChannelDm {
            peer_id,
            dm_channel_id: channel_id,
            text: text.to_string(),
            attachment_pointers,
            file_paths: vec![],
            quote,
        }),
        ActiveConversation::Channel(id) => {
            let is_saved = channel_service
                .get_channel_local(&id)
                .ok()
                .flatten()
                .map(|c| c.channel_type == ChannelType::SavedMessages)
                .unwrap_or(false);

            if is_saved {
                let att_ids = attachment_pointers
                    .iter()
                    .map(|p| crate::domain::AttachmentId::from_cdn_key(&p.cdn_key))
                    .collect();
                Ok(OutgoingIntent::SavedMessages {
                    text: text.to_string(),
                    attachment_ids: att_ids,
                })
            } else {
                Ok(OutgoingIntent::Channel {
                    channel_id: id,
                    text: text.to_string(),
                    attachment_pointers,
                    file_paths: vec![],
                    thread_id: options.thread_id,
                    mentioned_integrations: options.mentioned_integrations.clone(),
                    targets_ai_bot: options.targets_ai_bot,
                })
            }
        }
        ActiveConversation::None => Err(SendError::UnsupportedAction),
    }
}
