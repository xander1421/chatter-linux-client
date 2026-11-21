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

use std::path::PathBuf;
use std::sync::Arc;

use crate::domain::anon_dm::AnonDmId;
use crate::domain::anon_group::AnonGroupId;
use crate::domain::ids::{AttachmentId, ChannelId, IntegrationId, MessageId, UserId};
use crate::domain::message::Message;
use crate::mailbox::group::GroupFanout;
use chatter_proto::content_v1::AttachmentPointer;
use crate::services::anon_dm::{AnonDmError, AnonDmService};
use crate::services::anon_group::{AnonGroupError, AnonGroupService};
use crate::services::attachment::AttachmentError;
use crate::services::message::{MessageError, MessageService};
use crate::services::user::UserError;

// =============================================================================
// Intent
// =============================================================================

/// What the user wants to send.
pub enum OutgoingIntent {
    /// Send a message in a friend DM (channel-free, per-device sealed
    /// sender). See `services::message::server_dm`.
    ServerDm {
        peer_id: UserId,
        text: String,
        /// Pre-uploaded attachment pointers only — no raw `file_paths`
        /// support. Callers pre-upload via `upload_pending_attachments()`.
        attachment_pointers: Vec<AttachmentPointer>,
        quote: Option<QuoteIntent>,
    },

    /// Send a message in a regular channel.
    Channel {
        channel_id: ChannelId,
        text: String,
        /// Pre-uploaded attachment pointers. If empty and file_paths is set,
        /// the gateway uploads them.
        attachment_pointers: Vec<AttachmentPointer>,
        file_paths: Vec<PathBuf>,
        thread_id: Option<MessageId>,
        mentioned_integrations: Vec<IntegrationId>,
        targets_ai_bot: bool,
        /// Optional quote context for reply messages. When present, the
        /// stored local message will have `reply_to`, `reply_to_author_id`,
        /// and `reply_to_text` populated so the quote bubble survives reload.
        quote: Option<QuoteIntent>,
    },

    /// Send a message in an anonymous DM (NDSS '21 pairwise).
    AnonDm {
        conversation_id: AnonDmId,
        text: String,
        attachment_pointers: Vec<AttachmentPointer>,
        quote: Option<QuoteIntent>,
    },

    /// Send a message in an anonymous group (NDSS '21 pairwise fanout).
    AnonGroup {
        group_id: AnonGroupId,
        text: String,
        attachment_pointers: Vec<AttachmentPointer>,
        quote: Option<QuoteIntent>,
    },

    /// Save a message to the SavedMessages channel (Note to Self).
    SavedMessages {
        text: String,
        attachment_ids: Vec<AttachmentId>,
        /// Reply context. Saved Messages are stored locally only, so the
        /// row written by `save_message` is the one every reload reads:
        /// without `reply_to*` on it the quote strip vanishes at restart.
        quote: Option<QuoteIntent>,
    },
}

/// Quote/reply context for outgoing messages (all conversation types).
///
/// Field names use the `target_*` prefix so callers can tell at a glance
/// that these refer to the *quoted* (target) message, not the new outgoing
/// one. The Slint UI mental model already says `target_*` everywhere, so
/// the SDK matches.
pub struct QuoteIntent {
    pub target_author_id: UserId,
    pub target_timestamp: u64,
    /// Preview text for the quoted message. None for anon paths where
    /// the recipient already has the message locally.
    pub target_preview: Option<String>,
}

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
    /// The message was stored but not delivered (status `"failed"`, retryable)
    /// — friend DMs only. A refused or failed channel send is also `Ok`, but
    /// carries no error here: `message` comes back stored with
    /// `delivery_status == "failed"` (`MessageService::send`, core.rs). Other
    /// paths report a failure as `Err`.
    pub send_error: Option<MessageError>,
}

// =============================================================================
// Error
// =============================================================================

/// Unified send error across all channel types.
#[derive(Debug, thiserror::Error)]
pub enum SendError {
    /// Construct only at a boundary proven to precede message submission.
    #[error(transparent)]
    BeforeSubmission(Box<SendError>),
    #[error("Delivery is unconfirmed.")]
    DeliveryUnconfirmed,
    #[error("An earlier message has unconfirmed delivery. This new draft has not been sent.")]
    SendInProgress,
    #[error("This send attempt was already confirmed. It was not sent again.")]
    AttemptAlreadyConfirmed,
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

impl SendError {
    pub(crate) fn before_submission(self) -> Self {
        Self::BeforeSubmission(Box::new(self))
    }

    /// The server's `send_message` check refused us: in a channel, a
    /// moderator mute or a removal (schema.zed). Retrying cannot succeed.
    pub fn is_refused(&self) -> bool {
        matches!(
            self,
            Self::Message(MessageError::Network(
                crate::error::NetworkError::PermissionDenied
            ))
        )
    }
}

// =============================================================================
// Router
// =============================================================================

/// Route an outgoing message intent to the correct service.
pub fn route_outgoing<'a>(
    intent: OutgoingIntent,
    services: &'a OutgoingServices<'a>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<SendResult, SendError>> + Send + 'a>> {
    // Avoid embedding every transport's send future into the caller's stack.
    Box::pin(async move {
        match intent {
            OutgoingIntent::ServerDm {
                peer_id,
                text,
                attachment_pointers,
                quote,
            } => Ok(route_server_dm(
                services.message_service,
                &peer_id,
                &text,
                attachment_pointers,
                quote,
            )
            .await?),

            OutgoingIntent::Channel {
                channel_id,
                text,
                attachment_pointers,
                file_paths,
                thread_id,
                mentioned_integrations,
                targets_ai_bot,
                quote,
            } => Ok(route_channel(
                services.message_service,
                services.group_fanout,
                &channel_id,
                &text,
                attachment_pointers,
                file_paths,
                thread_id,
                mentioned_integrations,
                targets_ai_bot,
                quote,
            )
            .await?),

            OutgoingIntent::AnonDm {
                conversation_id,
                text,
                attachment_pointers,
                quote,
            } => {
                let anon_dm_service = services
                    .anon_dm_service
                    .ok_or_else(|| SendError::NotLoggedIn.before_submission())?;

                // Safety Check: Centralized Block/Rate Limit
                anon_dm_service
                    .check_conversation_not_blocked(&conversation_id)
                    .map_err(|e| SendError::from(e).before_submission())?;
                anon_dm_service
                    .check_rate_limit(&conversation_id)
                    .map_err(|e| SendError::from(e).before_submission())?;

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
                let anon_group_service = services
                    .anon_group_service
                    .ok_or_else(|| SendError::NotLoggedIn.before_submission())?;

                // Safety Check: Centralized Block/Rate Limit
                anon_group_service
                    .check_group_not_blocked(&group_id)
                    .map_err(|e| SendError::from(e).before_submission())?;
                anon_group_service
                    .check_rate_limit(&group_id)
                    .map_err(|e| SendError::from(e).before_submission())?;

                route_anon_group(
                    anon_group_service,
                    &group_id,
                    &text,
                    attachment_pointers,
                    quote,
                )
                .await
            }

            OutgoingIntent::SavedMessages {
                text,
                attachment_ids,
                quote,
            } => {
                // Unlike `route_channel`, nothing to patch afterwards:
                // `save_message` puts the quote on the row before its insert and
                // syncs it only once stored, so an insert error surfaces here.
                let message = services
                    .message_service
                    .save_message(&text, attachment_ids, quote.as_ref())
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
                    send_error: None,
                })
            }
        }
    })
}

/// Services needed by the outgoing router.
pub struct OutgoingServices<'a> {
    pub message_service: &'a MessageService,
    pub group_fanout: Option<&'a GroupFanout>,
    pub anon_dm_service: Option<&'a AnonDmService>,
    pub anon_group_service: Option<&'a AnonGroupService>,
}

/// Owned variant of [`OutgoingServices`] for use across await points.
pub struct OwnedOutgoingServices {
    pub message_service: Arc<MessageService>,
    pub group_fanout: Option<Arc<GroupFanout>>,
    pub anon_dm_service: Option<Arc<AnonDmService>>,
    pub anon_group_service: Option<Arc<AnonGroupService>>,
}

impl OwnedOutgoingServices {
    fn as_borrowed(&self) -> OutgoingServices<'_> {
        OutgoingServices {
            message_service: &self.message_service,
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
}

// =============================================================================
// Server DM dispatch (channel-free, per-device sealed sender)
// =============================================================================

async fn route_server_dm(
    message_service: &MessageService,
    peer_id: &UserId,
    text: &str,
    attachment_pointers: Vec<AttachmentPointer>,
    quote: Option<QuoteIntent>,
) -> Result<SendResult, MessageError> {
    let sent_at = crate::util::time::TimestampNanos::now().as_u64();
    // `send_server_dm` carries the quote on the wire (via build_reply_content)
    // AND stores the reply metadata on the local echo, so no separate
    // persist-patch is needed here.
    let dm_result = message_service
        .send_server_dm(peer_id, text, attachment_pointers, sent_at, quote.as_ref())
        .await?;

    Ok(SendResult {
        sent_at: dm_result.sent_at,
        message: dm_result.message,
        attachment_ids: dm_result.attachment_ids,
        gif_attachment_ids: dm_result.gif_attachment_ids,
        bootstrap_initiated: dm_result.bootstrap_initiated,
        anon_message_id: None,
        send_error: dm_result.send_error,
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
    quote: Option<QuoteIntent>,
) -> Result<SendResult, MessageError> {
    // Use pre-uploaded pointers if available, otherwise upload from file paths
    let message = if !attachment_pointers.is_empty() {
        let attachment_ids = attachment_pointers
            .iter()
            .map(|p| crate::domain::AttachmentId::from_cdn_key(&p.cdn_key))
            .collect();
        message_service
            .send_with_pointers(
                group_fanout,
                channel_id,
                text,
                attachment_pointers,
                attachment_ids,
                thread_id.as_ref(),
                mentioned_integrations,
                targets_ai_bot,
                quote.as_ref(),
            )
            .await?
    } else {
        let path_refs: Vec<&std::path::Path> = file_paths.iter().map(|p| p.as_path()).collect();
        message_service
            .send_with_files(
                group_fanout,
                channel_id,
                text,
                path_refs,
                thread_id.as_ref(),
                mentioned_integrations,
                targets_ai_bot,
                quote.as_ref(),
            )
            .await?
    };

    // Persist quote/reply context so reply bubbles survive a channel reload.
    // The server doesn't echo reply_to metadata for our own sent messages,
    // so we patch the local row after the insert performed by the send* method.
    let message = if let Some(q) = quote {
        message_service
            .persist_outgoing_reply_context(message, &q)
            .unwrap_or_else(|(msg, err)| {
                tracing::warn!(
                    error = %err,
                    "Failed to persist outgoing reply context; quote will be visible this session only"
                );
                msg
            })
    } else {
        message
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
        send_error: None,
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
    quote: Option<QuoteIntent>,
) -> Result<SendResult, SendError> {
    // Extract attachment IDs before pointers are moved into the service call
    let attachment_ids = super::shared::extract_attachment_ids(&attachment_pointers);

    let (message_id, sent_at) = if let Some(q) = quote {
        anon_dm_service
            .send_reply(
                conversation_id,
                text,
                attachment_pointers,
                &q.target_author_id,
                q.target_timestamp,
            )
            .await?
    } else {
        anon_dm_service
            .send_with_attachments(conversation_id, text, attachment_pointers)
            .await?
    };

    // Pull the just-stored Message so callers get reply-quote metadata
    // (reply_to_thumbnail, preview, etc.).
    let message = anon_dm_service.message_repo.get(&message_id).ok().flatten();

    Ok(SendResult {
        sent_at,
        message,
        attachment_ids,
        gif_attachment_ids: vec![],
        bootstrap_initiated: false,
        anon_message_id: Some(message_id.to_string()),
        send_error: None,
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
    quote: Option<QuoteIntent>,
) -> Result<SendResult, SendError> {
    // Extract attachment IDs before pointers are moved into the service call
    let attachment_ids = super::shared::extract_attachment_ids(&attachment_pointers);

    let (message_id, sent_at) = if let Some(q) = quote {
        anon_group_service
            .send_reply(
                group_id,
                text,
                attachment_pointers,
                &q.target_author_id,
                q.target_timestamp,
            )
            .await?
    } else if attachment_pointers.is_empty() {
        anon_group_service.send_text(group_id, text).await?
    } else {
        anon_group_service
            .send_with_attachments(group_id, text, attachment_pointers)
            .await?
    };

    // Pull the just-stored Message so callers get reply-quote metadata
    // (reply_to_thumbnail, preview, etc.).
    let message = message_id
        .as_str()
        .parse::<crate::domain::ids::MessageId>()
        .ok()
        .and_then(|mid| {
            anon_group_service
                .message_service
                .get_message_local(&mid)
                .ok()
                .flatten()
        });

    let complete = crate::services::anon_group::send_ledger::row_complete(
        &anon_group_service.message_service.database,
        message_id.as_str(),
    )
    .map_err(|e| SendError::Message(MessageError::Storage(e)))?;

    Ok(SendResult {
        sent_at,
        message,
        attachment_ids,
        gif_attachment_ids: vec![],
        bootstrap_initiated: complete != Some(true),
        anon_message_id: Some(message_id.as_str().to_string()),
        send_error: None,
    })
}
