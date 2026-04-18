//! Call orchestration (DM / group / anonymous) plus call history and lifecycle.

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
#[cfg(feature = "desktop-media")]
use crate::services::call_worker::CallWorkerHandle;
use crate::services::channel::ChannelService;
use crate::services::dm_bootstrap_service::DmBootstrapService;
use crate::services::gateway::outgoing::{
    OwnedOutgoingServices, OutgoingIntent, QuoteIntent, SendError, SendResult,
    route_outgoing_owned,
};
use crate::services::jarvis::JarvisService;
use crate::services::ai::LocalAIService;
use crate::services::mailbox_manager::MailboxManager;
use crate::services::message::MessageService;
use crate::services::pin_service::PinService;
use crate::services::session_service::{SessionError, SessionService};
use crate::services::signal_service::SignalService;
use crate::services::section_service::SectionService;
use crate::services::typing_service::TypingService;
use crate::services::user::UserService;
use crate::services::conversation::ConversationHandle;
use crate::state::{
    ActiveConversation, AuthState, ChatState, JarvisState, MessageState, OfflineState,
    OrgState, PinnedState, PresenceState, PrivacyState, StreamingState, UIState, UserState,
};
use crate::state::chat_state::PendingAttachment;
use crate::storage::settings_keys;
use crate::system::Directories;
use super::{ChatterClient, ClientError};

impl ChatterClient {
    // ── DM Call Orchestration ──────────────────────────────────────────

    /// Start a DM call: generate ephemeral key, create call on server, send offer.
    ///
    /// Returns the call ID. The UI should show "Calling..." and wait for
    /// `complete_dm_call()` when the peer answers.
    #[cfg(feature = "desktop-media")]
    pub async fn start_dm_call(
        &self,
        peer_id: &crate::domain::ids::UserId,
        channel_id: crate::domain::ids::ChannelId,
        call_type: crate::domain::call::CallType,
        user_id: crate::domain::ids::UserId,
    ) -> Result<crate::domain::ids::CallId, ClientError> {
        let call_worker = self.call_service().get()
            .ok_or_else(|| ClientError::Inbox("Call service not initialized".into()))?
            .clone();
        self.call_signaling()
            .start_dm_call(peer_id, channel_id, call_type, &call_worker, user_id)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    /// Start a group call in a channel: generate key, initiate on server, distribute key.
    ///
    /// Returns a `CallSetupResult` with call_id and verification emojis for the UI.
    #[cfg(feature = "desktop-media")]
    pub async fn start_group_call(
        &self,
        channel_id: crate::domain::ids::ChannelId,
        call_type: crate::domain::call::CallType,
        user_id: crate::domain::ids::UserId,
    ) -> Result<crate::domain::call::CallSetupResult, ClientError> {
        use crate::crypto::generate_group_call_key;
        use crate::services::call_verification::CallVerification;

        let call_worker = self.call_service().get()
            .ok_or_else(|| ClientError::Inbox("Call service not initialized".into()))?
            .clone();

        let group_key = generate_group_call_key();

        let call_type_str = match call_type {
            crate::domain::call::CallType::Voice => "Voice Call",
            crate::domain::call::CallType::Video => "Video Call",
            crate::domain::call::CallType::Group => "Group Call",
        };

        let call = call_worker
            .initiate_call(channel_id, call_type_str, call_type, &group_key, user_id)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))?;

        // Publish microphone immediately
        if let Err(e) = call_worker.publish_microphone(None).await {
            tracing::error!("Failed to publish microphone: {}", e);
        }

        // Distribute key to all channel members (fire-and-forget)
        let call_id_str = call.id.to_string();
        let signaling = self.call_signaling().clone();
        let ch_id = channel_id;
        tokio::spawn(async move {
            match signaling.distribute_group_call_key(&ch_id, &call_id_str, group_key, 0).await {
                Ok(count) => tracing::info!(call_id = %call_id_str, sent = count, "Group call key distributed"),
                Err(e) => tracing::error!(call_id = %call_id_str, error = %e, "Failed to distribute group call key"),
            }
        });

        let verification = CallVerification::derive_verification_emojis(&group_key, &call.id.to_string());
        let emojis = CallVerification::to_slint_tuple(&verification);

        Ok(crate::domain::call::CallSetupResult {
            call_id: call.id,
            channel_id,
            verification_emojis: [emojis.0, emojis.1, emojis.2, emojis.3],
            is_video: matches!(call_type, crate::domain::call::CallType::Video),
        })
    }

    /// Complete an outgoing DM call after receiving the peer's ephemeral key answer.
    ///
    /// Returns verification data (emojis) for the UI.
    #[cfg(feature = "desktop-media")]
    pub async fn complete_dm_call(
        &self,
        peer_ephemeral_pk: [u8; 32],
        call_id: &str,
        peer_id: &crate::domain::ids::UserId,
    ) -> Result<crate::domain::call::CallSetupResult, ClientError> {
        let call_worker = self.call_service().get()
            .ok_or_else(|| ClientError::Inbox("Call service not initialized".into()))?
            .clone();
        self.call_signaling()
            .complete_dm_call(
                peer_ephemeral_pk,
                call_id,
                peer_id,
                self.signal(),
                &call_worker,
            )
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    /// Accept an incoming DM call: generate ephemeral key, derive shared secret, join call.
    ///
    /// Returns verification data (emojis) for the UI.
    #[cfg(feature = "desktop-media")]
    pub async fn accept_dm_call(
        &self,
        caller_ephemeral_pk: [u8; 32],
        call_id: &str,
        caller_id: &crate::domain::ids::UserId,
        user_id: crate::domain::ids::UserId,
    ) -> Result<crate::domain::call::CallSetupResult, ClientError> {
        let call_worker = self.call_service().get()
            .ok_or_else(|| ClientError::Inbox("Call service not initialized".into()))?
            .clone();
        self.call_signaling()
            .accept_dm_call(
                caller_ephemeral_pk,
                call_id,
                caller_id,
                self.signal(),
                &call_worker,
                user_id,
            )
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    // ── Anonymous Call Orchestration ─────────────────────────────────

    /// Start an anonymous DM call: acquire token, send offer, store pending state.
    #[cfg(feature = "desktop-media")]
    pub async fn start_anon_dm_call(
        &self,
        anon_dm_id: &crate::domain::anon_dm::AnonDmId,
        is_video: bool,
        display_name: &str,
    ) -> Result<crate::services::anon_dm::AnonCallOfferResult, ClientError> {
        self.call_signaling()
            .start_anon_dm_call(anon_dm_id, is_video, display_name)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    /// Start an anonymous group call: acquire token, send offer to all members, store pending state.
    pub async fn start_anon_group_call(
        &self,
        group_id: &crate::domain::anon_group::AnonGroupId,
        is_video: bool,
        display_name: &str,
    ) -> Result<crate::services::anon_dm::AnonCallOfferResult, ClientError> {
        self.call_signaling()
            .start_anon_group_call(group_id, is_video, display_name)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    /// Accept an incoming anonymous call: acquire token, send accept, join room, connect LiveKit.
    #[cfg(feature = "desktop-media")]
    pub async fn accept_anon_call(
        &self,
        room_name: &str,
    ) -> Result<crate::domain::call::AnonCallSetupResult, ClientError> {
        let call_worker = self.call_service().get()
            .ok_or_else(|| ClientError::Inbox("Call service not initialized".into()))?
            .clone();
        self.call_signaling()
            .accept_anon_call(room_name, &call_worker)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    /// Handle an anonymous call answer (caller side): connect to LiveKit if accepted.
    #[cfg(feature = "desktop-media")]
    pub async fn handle_anon_call_answer(
        &self,
        call_room_id: &[u8],
        accepted: bool,
        callee_display_name: &str,
    ) -> Result<Option<crate::domain::call::AnonCallSetupResult>, ClientError> {
        let call_worker = self.call_service().get()
            .ok_or_else(|| ClientError::Inbox("Call service not initialized".into()))?
            .clone();
        self.call_signaling()
            .handle_anon_call_answer(call_room_id, accepted, callee_display_name, &call_worker)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    /// Decline an incoming anonymous call: send decline answer, store missed call history.
    pub async fn decline_anon_call(
        &self,
        room_name: &str,
    ) -> Result<(), ClientError> {
        self.call_signaling()
            .decline_anon_call(room_name)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    // ── Mobile Call Methods (no desktop-media dependency) ─────────────
    //
    // Thin delegates to `CallSignalingService` — the real orchestration
    // (gRPC create_call / join_call / leave_call, ephemeral key ECDH, sealed
    // sender signaling, pending-call state) lives in the service. These
    // wrappers just translate the service's `CallSignalingError` into
    // `ClientError` for the public SDK surface.

    pub async fn start_dm_call_mobile(
        &self,
        peer_id: &crate::domain::ids::UserId,
        channel_id: crate::domain::ids::ChannelId,
        call_type: crate::domain::call::CallType,
        _user_id: crate::domain::ids::UserId,
    ) -> Result<String, ClientError> {
        self.call_signaling()
            .start_dm_call_mobile(peer_id, channel_id, call_type)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    pub async fn complete_dm_call_mobile(
        &self,
        peer_ephemeral_pk: [u8; 32],
        call_id: &str,
        peer_id: &crate::domain::ids::UserId,
    ) -> Result<crate::domain::call::MobileCallSetup, ClientError> {
        self.call_signaling()
            .complete_dm_call_mobile(peer_ephemeral_pk, call_id, peer_id)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    #[cfg(feature = "desktop-media")]
    pub async fn accept_dm_call_mobile(
        &self,
        caller_ephemeral_pk: [u8; 32],
        call_id: &str,
        caller_id: &crate::domain::ids::UserId,
    ) -> Result<crate::domain::call::MobileCallSetup, ClientError> {
        self.call_signaling()
            .accept_dm_call_mobile(caller_ephemeral_pk, call_id, caller_id)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    pub async fn start_group_call_mobile(
        &self,
        channel_id: crate::domain::ids::ChannelId,
        call_type: crate::domain::call::CallType,
        _user_id: crate::domain::ids::UserId,
    ) -> Result<crate::domain::call::MobileCallSetup, ClientError> {
        self.call_signaling()
            .start_group_call_mobile(channel_id, call_type)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    #[cfg(feature = "desktop-media")]
    pub async fn end_dm_call_mobile(
        &self,
        call_id: &str,
        channel_id: &crate::domain::ids::ChannelId,
        duration_secs: i64,
    ) -> Result<(), ClientError> {
        self.call_signaling()
            .end_dm_call_mobile(call_id, channel_id, duration_secs)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    #[cfg(feature = "desktop-media")]
    pub async fn end_anon_call_mobile(
        &self,
        room_name: &str,
        duration_secs: i64,
    ) -> Result<Option<crate::domain::anon_dm::AnonDmId>, ClientError> {
        self.call_signaling()
            .end_anon_call_mobile(room_name, duration_secs)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    pub async fn handle_remote_hangup_mobile(
        &self,
        call_id: &str,
    ) -> Result<(), ClientError> {
        self.call_signaling()
            .handle_remote_hangup_mobile(call_id)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    pub async fn accept_anon_call_mobile(
        &self,
        room_name: &str,
    ) -> Result<crate::domain::call::MobileCallSetup, ClientError> {
        self.call_signaling()
            .accept_anon_call_mobile(room_name)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }
    // ── Call history ──────────────────────────────────────────────────

    /// Store a call history message for a standard DM/channel call.
    pub fn store_call_history(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        call_id: &str,
        initiator_id: &crate::domain::ids::UserId,
        initiator_name: &str,
        _is_outgoing: bool,
        duration_secs: i64,
        is_missed: bool,
    ) {
        use crate::domain::message::*;

        let call_type = if is_missed { CallMessageType::Missed } else { CallMessageType::Ended };
        let duration = if duration_secs > 0 {
            Some(chrono::Duration::seconds(duration_secs))
        } else {
            None
        };

        let call_msg = CallMessage {
            call_id: call_id.to_string(),
            call_type,
            initiator_id: *initiator_id,
            initiator_name: initiator_name.to_string(),
            started_at: None,
            ended_at: Some(chrono::Utc::now()),
            duration,
            participant_count: 2,
        };

        let message = Message {
            id: crate::domain::ids::MessageId::new(),
            conversation_id: *channel_id,
            conversation_type: ConversationType::AnonGroup,
            sender_id: Some(*initiator_id),
            device_id: None,
            content: MessageContent::Call(call_msg),
            encrypted_content: None,
            thread_id: None,
            reply_to: None,
            reply_to_author_id: None,
            reply_to_text: None,
            reply_to_thumbnail: None,
            reply_to_thumbnail_mime: None,
            attachments: vec![],
            is_pinned: false,
            is_edited: false,
            is_deleted: false,
            is_read: false,
            sent_at: crate::util::time::TimestampNanos::now().as_u64(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            edited_at: None,
            reaction_counts: vec![],
            current_user_reactions: vec![],
            mentioned_integrations: vec![],
            targets_ai_bot: false,
            delivery_status: String::new(),
        };

        if let Err(e) = self.messages().store_message_local(&message) {
            tracing::error!(error = %e, "Failed to save call history");
        }
    }

    /// Store a call history entry in an anon DM conversation.
    pub fn store_anon_call_history(
        &self,
        conversation_id: &crate::domain::anon_dm::AnonDmId,
        is_outgoing: bool,
        duration_seconds: i64,
    ) {
        let duration_str = if duration_seconds > 0 {
            format!("{}:{:02}", duration_seconds / 60, duration_seconds % 60)
        } else {
            "0:00".to_string()
        };
        let label = if is_outgoing { "Outgoing call" } else { "Incoming call" };
        let text = format!("\u{1F4DE} {} \u{00B7} {}", label, duration_str);
        let sent_at = crate::util::time::TimestampNanos::now().as_u64();
        let user_id = self.auth_state().get().current_user_id
            .unwrap_or_else(crate::domain::ids::UserId::new);

        if let Err(e) = self.anon_dms().store_text_message(
            conversation_id, &user_id, &text, sent_at,
        ) {
            tracing::error!(error = %e, "Failed to store anon call history");
        }
    }

    // ── Call lifecycle ─────────────────────────────────────────────

    /// End an active DM/channel call: send hangup, store history, leave.
    ///
    /// `duration_secs` is the call duration from the UI timer.
    #[cfg(feature = "desktop-media")]
    pub async fn end_dm_call(&self, duration_secs: i64) -> Result<(), ClientError> {
        let call_worker = self.call_service().get()
            .ok_or_else(|| ClientError::Inbox("call service not initialized".into()))?;
        self.call_signaling()
            .end_dm_call(call_worker, duration_secs)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    /// End an active anonymous call: send hangup, store history, leave.
    ///
    /// Returns the conversation_id for UI bubble insertion.
    #[cfg(feature = "desktop-media")]
    pub async fn end_anon_call(&self, duration_secs: i64) -> Result<Option<crate::domain::anon_dm::AnonDmId>, ClientError> {
        let call_worker = self.call_service().get()
            .ok_or_else(|| ClientError::Inbox("call service not initialized".into()))?;
        self.call_signaling()
            .end_anon_call(call_worker, duration_secs)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    /// Handle a participant departing from a group call.
    ///
    /// If we are the initiator of a channel-based group call, generates a new
    /// random encryption key, rotates it locally, and redistributes to remaining
    /// members so the departed participant can't decrypt future frames.
    ///
    /// Only the initiator rotates to avoid race conditions.
    #[cfg(feature = "desktop-media")]
    pub async fn handle_participant_departed(&self) -> Result<(), ClientError> {
        let call_worker = self.call_service().get()
            .ok_or_else(|| ClientError::Inbox("call service not initialized".into()))?;

        let call = match call_worker.active_call().await {
            Some(c) => c,
            None => return Ok(()), // No active call
        };

        // Only rotate for channel-based group calls (not anon, not DM).
        // Anonymous calls have a random channel_id that won't resolve.
        let channel = match self.channels().get_channel_local(&call.channel_id) {
            Ok(Some(c)) => c,
            _ => return Ok(()),
        };

        use crate::domain::channel::ChannelType;
        if matches!(channel.channel_type, ChannelType::DirectMessage | ChannelType::SavedMessages) {
            return Ok(());
        }

        // Only the initiator rotates to prevent race conditions
        let my_id = self.sessions().current_user_id()
            .ok_or_else(|| ClientError::Inbox("not logged in".into()))?;
        if my_id != call.initiator_id {
            return Ok(());
        }

        // Generate new random key and rotate locally
        let new_key = crate::crypto::generate_group_call_key();
        let epoch = call_worker.rotate_key(new_key).await
            .map_err(|e| ClientError::Inbox(format!("key rotation failed: {e}")))?;

        // Brief delay for LiveKit key propagation before peers receive new-key frames
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        // Redistribute the new key to remaining channel members
        let call_id = call.id.to_string();
        match self.call_signaling()
            .distribute_group_call_key(&call.channel_id, &call_id, new_key, epoch)
            .await
        {
            Ok(count) => {
                tracing::info!(epoch, sent = count, "Rotated group call key after participant departure");
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to redistribute rotated group call key");
            }
        }

        Ok(())
    }

}
