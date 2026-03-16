//! Shared utilities for anonymous messaging (DM and group).
//!
//! Contains the common primitives used by both `AnonDmService` and
//! `AnonGroupService`: mailbox delivery, profile helpers, message
//! field construction, and content envelope building.

use prost::Message as ProstMessage;

use crate::domain::{AttachmentId, UserId};
use crate::proto::content_v1::{self, Content};
use crate::proto::mailbox_v1::EnvelopeType;
use crate::services::gateway::shared as gateway_shared;
use crate::services::mailbox_manager::MailboxManager;
use crate::services::pairwise::PairwiseService;
use crate::storage::repositories::settings_keys;
use crate::storage::repositories::SettingsRepository;

/// Encrypt and send content to an anonymous mailbox.
///
/// This is the shared core for both anon DM and anon group sending:
/// serialize -> encrypt (pairwise) -> send to mailbox.
///
/// Returns a `String` error so callers can map it to their own error type.
pub(crate) async fn send_to_anon_mailbox(
    pairwise_service: &PairwiseService,
    mailbox_manager: &MailboxManager,
    peer_id: &UserId,
    mailbox_pk: &[u8; 32],
    content: Content,
) -> Result<(), String> {
    let plaintext = content.encode_to_vec();

    let ciphertext = pairwise_service
        .encrypt(peer_id, &plaintext)
        .await
        .map_err(|e| e.to_string())?;

    mailbox_manager
        .send_to_mailbox(mailbox_pk, ciphertext, EnvelopeType::SealedSender)
        .await
        .map_err(|e| e.to_string())?;

    Ok(())
}

/// Get our profile key (32 bytes) from settings, if available.
pub(crate) fn get_my_profile_key(settings_repo: &SettingsRepository) -> Option<[u8; 32]> {
    settings_repo
        .get_bytes(settings_keys::PROFILE_KEY)
        .ok()
        .flatten()
        .and_then(|v| {
            if v.len() == 32 {
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&v);
                Some(arr)
            } else {
                None
            }
        })
}

/// Get our display name from settings.
pub(crate) fn get_my_display_name(settings_repo: &SettingsRepository) -> Option<String> {
    settings_repo
        .get_string(settings_keys::DISPLAY_NAME)
        .ok()
        .flatten()
}

/// Get our avatar bytes from settings (raw image data stored locally).
pub(crate) fn get_my_avatar(settings_repo: &SettingsRepository) -> Option<Vec<u8>> {
    settings_repo
        .get_bytes(settings_keys::AVATAR)
        .ok()
        .flatten()
}

/// Get encrypted avatar for sharing during bootstrap/profile updates.
///
/// Encrypts avatar with profile key for privacy-preserving sharing.
/// Used by both DM and Group services during handshake flows.
pub(crate) fn get_encrypted_avatar(
    settings_repo: &SettingsRepository,
    profile_key: &[u8; 32],
) -> Option<Vec<u8>> {
    let avatar = get_my_avatar(settings_repo)?;
    crate::crypto::profile_crypto::encrypt_avatar(profile_key, &avatar).ok()
}

/// Get our Signal Protocol identity key (33 bytes, compressed Ed25519).
pub(crate) fn get_my_identity_key(settings_repo: &SettingsRepository) -> Result<[u8; 33], String> {
    let key_bytes = settings_repo
        .get_bytes(settings_keys::IDENTITY_KEY_PUBLIC)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "not logged in".to_string())?;

    if key_bytes.len() != 33 {
        return Err(format!("invalid identity key length: {}", key_bytes.len()));
    }

    let mut key = [0u8; 33];
    key.copy_from_slice(&key_bytes);
    Ok(key)
}

/// Generate random padding to reach a target size.
///
/// Delegates to the centralized NDSS '21 padding in `gateway::builders`.
pub(crate) fn generate_padding(boundary: usize) -> Vec<u8> {
    crate::services::gateway::builders::generate_padding(boundary)
}

// ============================================================================
// Message field construction (shared between DM and Group storage)
// ============================================================================

/// Pre-computed message fields ready for storage in either anon_dm_messages
/// or anon_group_messages table.
///
/// Both tables have identical field structure for message content.
pub(crate) struct AnonMessageFields {
    pub content_type: i32,
    pub content_json: String,
    pub attachments_blob: Vec<u8>,
}

/// Build the common storage fields for an anonymous message.
///
/// Handles text, attachments, and optional quote metadata — the logic
/// that was duplicated in `AnonDmService::send_with_attachments_and_quote`
/// and `AnonGroupService::store_message_with_attachments`.
pub(crate) fn build_message_fields(
    text: &str,
    attachment_ids: &[AttachmentId],
    quote: Option<&content_v1::Quote>,
    quote_author_name: Option<&str>,
) -> AnonMessageFields {
    let content_json = if let Some(q) = quote {
        let author_name = quote_author_name.unwrap_or(&q.author_aci);
        gateway_shared::build_reply_content_json(
            text,
            &q.author_aci,
            author_name,
            q.id,
            &q.text,
        )
    } else {
        gateway_shared::build_content_json(text, attachment_ids)
    };

    let content_type = if quote.is_some() {
        gateway_shared::CONTENT_TYPE_MIXED
    } else {
        gateway_shared::determine_content_type(!text.is_empty(), !attachment_ids.is_empty())
    };

    let attachments_blob = gateway_shared::serialize_attachment_ids(attachment_ids);

    AnonMessageFields {
        content_type,
        content_json,
        attachments_blob,
    }
}

// ============================================================================
// Content envelope construction
// ============================================================================

/// Build the outer `Content` envelope that wraps an inner proto message.
///
/// Both DM and Group wrap their inner proto (AnonDmMessage / AnonGroupMessage)
/// in a `Content` envelope with timestamp and padding. This function provides
/// the shared wrapping logic.
pub(crate) fn build_outer_content(
    inner: content_v1::content::Content,
    padding_boundary: usize,
) -> Content {
    Content {
        content: Some(inner),
        ephemeral_reply_to: None,
        timestamp: crate::util::time::TimestampNanos::now().as_u64(),
        padding: generate_padding(padding_boundary),
    }
}

/// Fan out a content envelope to multiple recipients' mailboxes.
///
/// Sends the same content to each recipient, skipping the current user.
/// Errors for individual recipients are logged but don't fail the operation.
/// Returns the number of successfully sent messages.
pub(crate) async fn fan_out_to_mailboxes(
    pairwise_service: &PairwiseService,
    mailbox_manager: &MailboxManager,
    current_user: &UserId,
    recipients: &[(UserId, [u8; 32])],
    content: &Content,
) -> usize {
    let mut sent_count = 0;
    for (user_id, mailbox_pk) in recipients {
        if user_id == current_user {
            continue;
        }
        match send_to_anon_mailbox(pairwise_service, mailbox_manager, user_id, mailbox_pk, content.clone()).await {
            Ok(()) => sent_count += 1,
            Err(e) => tracing::warn!("Failed to send to member {}: {}", user_id, e),
        }
    }
    sent_count
}
