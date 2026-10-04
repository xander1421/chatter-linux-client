//! Channel CRUD and membership operations.

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
    // ── Conversation listing ────────────────────────────────────────

    // ── Channel operations ────────────────────────────────────────

    /// Tell the live channel-message stream that this device's channel
    /// membership changed, so it reconnects and the server re-runs its
    /// membership-based auto-subscribe.
    ///
    /// Replaces the old per-channel `subscribe_channel_messages`, which
    /// spawned one `StreamMessageEvents` task per channel from exactly two
    /// call sites, never re-subscribed at login, and returned for good on the
    /// first stream error (INFRA-1). Delivery now lives in
    /// `services::channel_stream::ChannelStreamService`.
    pub fn resubscribe_channel_messages(self: &Arc<Self>) {
        self.services.channel_stream_service.request_resubscribe();
    }

    /// Create a channel and set up group encryption (mailbox + sender keys).
    ///
    /// Handles the full protocol: create on server, announce mailbox,
    /// distribute SKDM to members. The caller doesn't need to know about
    /// mailbox setup or sender key distribution.
    pub async fn create_channel(
        self: &Arc<Self>,
        name: &str,
        channel_type: ChannelType,
        config: Option<crate::domain::channel::ChannelConfig>,
        description: Option<&str>,
        icon: Option<&[u8]>,
        password: Option<&str>,
        initial_members: Vec<crate::domain::ids::UserId>,
    ) -> Result<Channel, crate::services::channel::ChannelError> {
        let (channel, _mailbox_setup) = self.channels()
            .create_channel(name, channel_type, config, description, icon, password, initial_members, true)
            .await?;
        // The global message stream subscribes from the server's membership
        // list; nudge it so the channel we just created is picked up now
        // rather than at the next reconnect.
        self.resubscribe_channel_messages();
        Ok(channel)
    }

    /// Create a channel, reading the icon file from disk if one is
    /// provided. UI may pass `icon_path` and let core handle the read +
    /// size validation. The bytes are sent inline in the channel proto
    /// — server stores them plaintext alongside the channel name.
    ///
    /// If the icon read fails or the file exceeds the 64KB cap, the
    /// channel is not created.
    pub async fn create_channel_with_icon(
        self: &Arc<Self>,
        name: &str,
        channel_type: ChannelType,
        config: Option<crate::domain::channel::ChannelConfig>,
        description: Option<&str>,
        icon_path: Option<&std::path::Path>,
        password: Option<&str>,
        initial_members: Vec<crate::domain::ids::UserId>,
    ) -> Result<Channel, crate::services::channel::ChannelError> {
        let icon_bytes: Option<Vec<u8>> = match icon_path {
            Some(path) => match tokio::fs::read(path).await {
                Ok(bytes) => Some(Self::fit_icon_bytes(&bytes)?),
                Err(e) => {
                    return Err(crate::services::channel::ChannelError::InvalidInput(format!(
                        "icon read failed: {}",
                        e
                    )));
                }
            },
            None => None,
        };
        self.create_channel(
            name,
            channel_type,
            config,
            description,
            icon_bytes.as_deref(),
            password,
            initial_members,
        )
        .await
    }

    /// Join a channel and set up group encryption.
    ///
    /// Handles mailbox announcement, SKDM distribution, and requesting
    /// existing members' SKDMs automatically. Without receiving existing
    /// members' SKDMs, the new member cannot decrypt their messages.
    pub async fn join_channel(
        self: &Arc<Self>,
        channel_id: &crate::domain::ids::ChannelId,
        password: Option<&str>,
    ) -> Result<Channel, crate::services::channel::ChannelError> {
        let (channel, _mailbox_setup) = self.channels()
            .join_channel(channel_id, password, true)
            .await?;
        self.resubscribe_channel_messages();

        // Pull SKDMs from existing members so we can decrypt their next
        // messages without the no-sender-key defer/retry round-trip.
        if let Err(e) = self.messages().request_skdms_from_channel_members(channel_id).await {
            tracing::warn!(
                channel_id = %channel_id,
                error = %e,
                "Failed to request SKDMs on join"
            );
        }

        Ok(channel)
    }

    /// Kick a user from a channel and re-distribute sender keys.
    ///
    /// After kicking, a new SKDM is distributed so the kicked user
    /// can no longer decrypt future messages. Message sending is blocked
    /// during this operation and unblocked only after SKDM distribution.
    pub async fn kick_user(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        user_id: &crate::domain::ids::UserId,
        reason: Option<&str>,
    ) -> Result<(), crate::services::channel::ChannelError> {
        self.channels().kick_user_complete(channel_id, user_id, reason).await
    }

    /// Ban a user from a channel (server-side) and re-distribute sender keys.
    ///
    /// Like kick, ban removes local membership and rotates the sender key.
    /// Permanent until [`Self::unban_user`] (no UI offers a timed ban).
    pub async fn ban_user(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        user_id: &crate::domain::ids::UserId,
        reason: Option<&str>,
    ) -> Result<(), crate::services::channel::ChannelError> {
        self.channels().ban_user_complete(channel_id, user_id, reason).await
    }

    /// Unban a user — clears the server-side ban. The user can rejoin via
    /// invite or public-channel flow afterwards.
    pub async fn unban_user(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        user_id: &crate::domain::ids::UserId,
    ) -> Result<(), crate::services::channel::ChannelError> {
        self.channels().unban_user(channel_id, user_id).await
    }

    /// Make `new_owner` the channel's owner; we become an admin and may then
    /// leave. Only the owner may (the server checks it too).
    pub async fn transfer_channel_ownership(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        new_owner: &crate::domain::ids::UserId,
    ) -> Result<(), crate::services::channel::ChannelError> {
        self.channels().transfer_ownership(channel_id, new_owner).await
    }

    /// Mute a member for `duration`, or until unmuted when `None`.
    /// Server-enforced: their sends are refused until it is lifted or lapses.
    pub async fn mute_user(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        user_id: &crate::domain::ids::UserId,
        duration: Option<std::time::Duration>,
    ) -> Result<(), crate::services::channel::ChannelError> {
        self.channels().mute_user(channel_id, user_id, duration).await
    }

    /// Lift a mute.
    pub async fn unmute_user(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        user_id: &crate::domain::ids::UserId,
    ) -> Result<(), crate::services::channel::ChannelError> {
        self.channels().unmute_user(channel_id, user_id).await
    }

    /// The notice that replaces the composer when we may not post in this
    /// channel, or "" when we may. Read from our stored membership, which a
    /// removal event deletes and a member fetch refreshes.
    pub fn channel_send_notice(&self, channel_id: &crate::domain::ids::ChannelId) -> String {
        if !self.is_moderated_channel(channel_id) {
            return String::new();
        }
        let Some(me) = self.session_current_user_id() else { return String::new() };
        let members = self.list_channel_members_local(channel_id);
        let own = members.iter().find(|m| m.user_id == me);
        crate::domain::channel::SendBlock::for_viewer(own, !members.is_empty(), self.former_channel_state(channel_id))
            .map(|b| b.notice())
            .unwrap_or_default()
    }

    /// `Some(deleted)` for a channel we are no longer in (left, removed,
    /// banned, deleted), which stays listed read-only; `None` otherwise.
    fn former_channel_state(&self, channel_id: &crate::domain::ids::ChannelId) -> Option<bool> {
        self.channels().channel_repo.former_state(channel_id).ok().flatten()
    }

    /// A channel we are no longer in, listed read-only: its row offers
    /// "Delete chat" (a live channel is left, not deleted — a membership
    /// outlives a local delete, the next sync brings it back) and it is no
    /// forward destination.
    pub fn is_former_channel(&self, channel_id: &crate::domain::ids::ChannelId) -> bool {
        let Some(me) = self.session_current_user_id() else { return false };
        self.former_channel_state(channel_id).is_some()
            && self.channels().channel_repo.get_membership(channel_id, &me).ok().flatten().is_none()
    }

    /// Whether to offer "Leave channel": we are still a member and not the
    /// owner (see [`crate::domain::channel::Channel::can_user_leave`]). An
    /// unsynced roster counts as membership, as in [`Self::channel_send_notice`].
    pub fn can_leave_channel(&self, channel_id: &crate::domain::ids::ChannelId) -> bool {
        let (Some(channel), Some(me)) = (self.get_channel_local(channel_id), self.session_current_user_id()) else {
            return false;
        };
        let members = self.list_channel_members_local(channel_id);
        let own = members.iter().find(|m| m.user_id == me);
        let still_member = !matches!(
            crate::domain::channel::SendBlock::for_viewer(own, !members.is_empty(), self.former_channel_state(channel_id)),
            Some(crate::domain::channel::SendBlock::Removed | crate::domain::channel::SendBlock::Deleted)
        );
        channel.can_user_leave(&me, still_member)
    }

    /// [`Self::channel_send_notice`] after re-reading the roster from the
    /// server: on channel open and after a refused send, in case a live
    /// `MemberUpdated` was missed. A failed fetch keeps the local
    /// answer.
    // refetches the full roster; a "my membership" RPC if that
    // shows up in large channels.
    pub async fn refresh_channel_send_notice(&self, channel_id: &crate::domain::ids::ChannelId) -> String {
        if !self.is_moderated_channel(channel_id) {
            return String::new();
        }
        if let Err(e) = self.channels().get_all_channel_members(channel_id).await {
            tracing::debug!(%channel_id, error = %e, "Roster refresh for send notice failed");
        }
        self.channel_send_notice(channel_id)
    }

    /// DMs and Saved Messages have no moderation (the server refuses it).
    fn is_moderated_channel(&self, channel_id: &crate::domain::ids::ChannelId) -> bool {
        self.get_channel_local(channel_id).is_some_and(|c| {
            !c.is_dm() && !matches!(c.channel_type, ChannelType::SavedMessages)
        })
    }

    /// A channel's active bans shaped for display, or `None` when our role
    /// may not see them (only owner/admin can unban). Call after a member
    /// fetch so our local role is current.
    pub async fn list_banned_members(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
    ) -> Result<Option<Vec<crate::domain::channel::BannedMember>>, crate::services::channel::ChannelError> {
        let Some(bans) = self.channels().list_banned_users(channel_id).await? else {
            return Ok(None);
        };
        Ok(Some(
            bans.into_iter()
                .map(|b| crate::domain::channel::BannedMember {
                    display_name: self.display_name(&b.user_id),
                    avatar_bytes: self.get_profile_local(&b.user_id).and_then(|p| p.avatar_bytes),
                    ..b
                })
                .collect(),
        ))
    }

    /// Update a channel's name, description and/or icon.
    ///
    /// `icon` is raw image bytes; oversized images are auto-resized and
    /// re-encoded via `fit_icon_bytes` to the 64KB inline-proto cap (same
    /// path as channel creation). `None` leaves the current icon unchanged.
    pub async fn update_channel(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        name: Option<&str>,
        description: Option<&str>,
        icon: Option<&[u8]>,
    ) -> Result<Channel, crate::services::channel::ChannelError> {
        let fitted = icon.map(Self::fit_icon_bytes).transpose()?;
        self.channels()
            .update_channel(channel_id, name, description, fitted.as_deref(), None)
            .await
    }

    /// Archive or unarchive a channel.
    pub async fn archive_channel(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        archive: bool,
    ) -> Result<Channel, crate::services::channel::ChannelError> {
        if archive {
            self.channels().archive_channel(channel_id).await
        } else {
            self.channels().unarchive_channel(channel_id).await
        }
    }

    /// Leave a channel.
    ///
    /// Before leaving, rotates the sender key and distributes the new SKDM
    /// to remaining members. This ensures forward secrecy — the departed
    /// member can no longer decrypt future messages.
    pub async fn leave_channel(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
    ) -> Result<(), crate::services::channel::ChannelError> {
        self.channels().leave_channel_complete(channel_id).await?;
        self.services.channel_event_service.record_own_leave(channel_id);
        Ok(())
    }

    /// Search public channels.
    pub async fn search_channels(
        &self,
        query: &str,
        limit: u32,
    ) -> Result<Vec<Channel>, crate::services::channel::ChannelError> {
        self.channels().search_channels(query, limit, 0, vec![ChannelType::Public]).await
    }

    const MAX_ICON_BYTES: usize = 65_536;

    fn fit_icon_bytes(raw: &[u8]) -> Result<Vec<u8>, crate::services::channel::ChannelError> {
        if raw.len() <= Self::MAX_ICON_BYTES {
            return Ok(raw.to_vec());
        }

        let img = image::load_from_memory(raw).map_err(|e| {
            crate::services::channel::ChannelError::InvalidInput(format!(
                "icon decode failed: {e}"
            ))
        })?;

        let mut img = img;
        for max_dim in [256u32, 192, 128, 96, 64] {
            if img.width() > max_dim || img.height() > max_dim {
                img = img.resize(max_dim, max_dim, image::imageops::FilterType::Lanczos3);
            }
            let mut buf = std::io::Cursor::new(Vec::new());
            img.write_to(&mut buf, image::ImageFormat::WebP).map_err(|e| {
                crate::services::channel::ChannelError::InvalidInput(format!(
                    "icon encode failed: {e}"
                ))
            })?;
            let encoded = buf.into_inner();
            if encoded.len() <= Self::MAX_ICON_BYTES {
                tracing::info!(
                    original_bytes = raw.len(),
                    final_bytes = encoded.len(),
                    final_dim = max_dim,
                    "auto-converted icon to WebP"
                );
                return Ok(encoded);
            }
        }

        Err(crate::services::channel::ChannelError::InvalidInput(
            "icon still too large after conversion to 64×64 WebP".into(),
        ))
    }
}

