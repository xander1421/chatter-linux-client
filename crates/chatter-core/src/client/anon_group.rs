//! anon_group façade — thin delegates to services.

#![allow(unused_imports)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::task::JoinHandle;

use crate::app::context::{AppContext, ContextError};
use crate::app::core_services::CoreServices;
use crate::domain::attachment::Attachment;
use crate::domain::channel::{Channel, ChannelType};
use crate::domain::message::{ConversationType, Message};
use crate::error::NetworkError;
use crate::events::{DomainEvent, EventBus};
use crate::services::auth_service::{AuthError, LoginResult};
use crate::inbox::DeviceInboxProvider;
use crate::mailbox::group::GroupFanout;
use crate::proto::content_v1::AttachmentPointer;
use crate::services::ai_service::AIService;
use crate::services::anon_dm::AnonDmService;
use crate::services::anon_group::AnonGroupService;
use crate::services::attachment::AttachmentService;
use crate::services::auth_service::AuthService;
#[cfg(feature = "media")]
use crate::services::call_worker::CallWorkerHandle;
use crate::services::channel::ChannelService;
use crate::services::dm_bootstrap_service::DmBootstrapService;
use crate::services::gateway::outgoing::{
    OwnedOutgoingServices, OutgoingIntent, QuoteIntent, SendError, SendResult,
    route_outgoing_owned,
};
use crate::services::ai::LocalAIService;
use crate::services::mailbox_manager::MailboxManager;
use crate::services::message::MessageService;
use crate::services::pin_service::PinService;
use crate::services::session_service::{SessionError, SessionService};
use crate::services::signal_service::SignalService;
use crate::services::section_service::SectionService;
use crate::services::user::UserService;
use crate::services::conversation::ConversationHandle;
use crate::state::{
    ActiveConversation, AuthState, ChatState, MessageState, OfflineState,
    PrivacyState, UserState,
};
use crate::state::chat_state::PendingAttachment;
use crate::storage::settings_keys;
use crate::system::Directories;
use super::{ChatterClient, ClientError};

impl ChatterClient {
    // ── Anon group façade ────────────────────────────────────────────────

    pub fn list_anon_groups(
        &self,
    ) -> Result<Vec<crate::domain::anon_group::AnonGroup>, crate::services::anon_group::AnonGroupError> {
        self.anon_groups().list_groups()
    }

    pub fn get_anon_group(
        &self,
        group_id: &crate::domain::anon_group::AnonGroupId,
    ) -> Option<crate::domain::anon_group::AnonGroup> {
        self.anon_groups().get_group(group_id).ok().flatten()
    }

    pub fn get_anon_group_members(
        &self,
        group_id: &crate::domain::anon_group::AnonGroupId,
    ) -> Vec<crate::domain::anon_group::AnonGroupMember> {
        self.anon_groups().get_members(group_id).unwrap_or_default()
    }

    pub fn get_anon_group_member_display_name(
        &self,
        group_id: &crate::domain::anon_group::AnonGroupId,
        user_id: &crate::domain::ids::UserId,
    ) -> Option<String> {
        self.anon_groups().get_member_display_name(group_id, user_id).ok().flatten()
    }

    /// Build the synthetic system `Message` shown for an anon-group membership
    /// change ("X joined", "X left", "X removed by Y"). The membership events
    /// arrive as bare metadata; constructing the domain `Message` is core's
    /// job, not the UI's.
    pub fn anon_group_system_message(
        &self,
        group_id: crate::domain::anon_group::AnonGroupId,
        actor_id: crate::domain::ids::UserId,
        content: crate::domain::message::SystemMessage,
    ) -> crate::domain::message::Message {
        use crate::domain::ids::MessageId;
        use crate::domain::message::{ConversationType, Message, MessageContent};
        let now = chrono::Utc::now();
        Message {
            id: MessageId::new(),
            conversation_id: group_id.into(),
            conversation_type: ConversationType::AnonGroup,
            sender_id: Some(actor_id),
            device_id: None,
            content: MessageContent::System(content),
            encrypted_content: None,
            thread_id: None,
            reply_to: None,
            reply_to_author_id: None,
            reply_to_text: None,
            reply_to_thumbnail: None,
            reply_to_thumbnail_mime: None,
            attachments: Vec::new(),
            is_pinned: false,
            is_edited: false,
            is_deleted: false,
            is_read: true,
            sent_at: now.timestamp_nanos_opt().unwrap_or(0) as u64,
            created_at: now,
            updated_at: now,
            edited_at: None,
            reaction_counts: Vec::new(),
            current_user_reactions: Vec::new(),
            mentioned_integrations: Vec::new(),
            targets_ai_bot: false,
            delivery_status: String::new(),
        }
    }

    pub fn get_latest_anon_group_message(
        &self,
        group_id: &crate::domain::anon_group::AnonGroupId,
    ) -> Option<crate::domain::message::Message> {
        self.anon_groups().get_latest_message(group_id).ok().flatten()
    }

    pub fn get_anon_group_messages_asc(
        &self,
        group_id: &crate::domain::anon_group::AnonGroupId,
        before: Option<&chrono::DateTime<chrono::Utc>>,
        limit: i32,
    ) -> Result<Vec<crate::domain::message::Message>, crate::services::anon_group::AnonGroupError> {
        let mut messages = self.anon_groups().get_messages_asc(group_id, before, limit)?;
        // Join reaction counts here (keyed by sent_at) so callers render a
        // complete Message. Previously the UI issued a second call and merged —
        // duplicated across sidebar sites and a data-shaping leak into the view.
        let reactions = self.anon_groups().get_all_reaction_counts(group_id).unwrap_or_default();
        for msg in &mut messages {
            if let Some(counts) = reactions.get(&msg.sent_at) {
                msg.reaction_counts = counts
                    .iter()
                    .map(|(emoji, count)| crate::domain::message::ReactionCount {
                        emoji: emoji.clone(),
                        count: *count,
                    })
                    .collect();
            }
        }
        Ok(messages)
    }

    pub fn list_anon_group_pinned(
        &self,
        group_id: &crate::domain::anon_group::AnonGroupId,
    ) -> Vec<crate::domain::message::Message> {
        self.anon_groups().list_pinned(group_id).unwrap_or_default()
    }

    pub async fn refresh_expiring_group_mailboxes(
        &self,
    ) -> Result<Vec<crate::domain::anon_group::AnonGroupId>, crate::services::anon_group::AnonGroupError> {
        self.anon_groups().refresh_expiring_group_mailboxes().await
    }

    pub async fn broadcast_profile_to_owned_groups(
        &self,
    ) -> Result<(), crate::services::anon_group::AnonGroupError> {
        self.anon_groups().broadcast_profile_to_owned_groups().await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn accept_anon_group_invite(
        &self,
        group_id: crate::domain::anon_group::AnonGroupId,
        group_name: String,
        owner_id: crate::domain::ids::UserId,
        inviter_id: crate::domain::ids::UserId,
        inviter_mailbox_pk: [u8; 32],
        inviter_profile_key: Option<[u8; 32]>,
        inviter_display_name: Option<String>,
        inviter_avatar: Option<Vec<u8>>,
    ) -> Result<crate::domain::anon_group::AnonGroup, crate::services::anon_group::AnonGroupError> {
        self.anon_groups()
            .accept_invite(
                group_id,
                group_name,
                owner_id,
                inviter_id,
                inviter_mailbox_pk,
                inviter_profile_key,
                inviter_display_name,
                inviter_avatar,
            )
            .await
    }
}
