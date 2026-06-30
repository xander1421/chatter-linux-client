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

    /// Subscribe to real-time message events for a specific channel.
    ///
    /// Starts a background task that streams message events from the server
    /// and emits `DomainEvent::MessageReceived` for each new message.
    /// This is called automatically by `create_channel` and `join_channel`,
    /// but can also be called explicitly for channels discovered via other
    /// means (e.g. DM channels created by friend accept).
    pub fn subscribe_channel_messages(self: &Arc<Self>, channel_id: &crate::domain::ids::ChannelId) {
        use tokio_stream::StreamExt;
        use prost::Message as _;
        use crate::proto::chat_v1::{self, message_event::Event as MsgEventType};
        use crate::proto::content_v1::GroupEncrypted;
        use crate::services::message::conversion::proto_to_domain_message_with_sender;

        let mut msg_client = self.ui.grpc_pool.message_client();
        let event_bus = self.ui.event_bus.clone();
        let channel_id_owned = *channel_id;
        let channel_id_str = channel_id.to_string();
        // Capture current user ID to filter out own messages from the stream.
        // Own messages are already displayed optimistically at send time.
        let current_user_id = self.ui.auth_state.get().current_user_id.clone();
        let signal = self.signal().clone();
        let messages = self.messages().clone();
        let attachment_service = self.attachments().clone();
        // Clone channel_repo so the spawned stream task can resolve
        // ConversationType per incoming message without borrowing self.
        let channel_repo_for_stream = self.ui.channel_repo.clone();

        let handle = tokio::spawn(async move {
            let stream = match msg_client.stream_message_events(&channel_id_str, None).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        channel_id = %channel_id_str,
                        error = %e,
                        "Failed to start channel message stream"
                    );
                    return;
                }
            };

            tokio::pin!(stream);

            while let Some(result) = stream.next().await {
                match result {
                    Ok(event) => {
                        if let Some(MsgEventType::MessageSent(ref proto_msg_ref)) = event.event {
                            let proto_msg = proto_msg_ref.clone();
                            let sender_id = if proto_msg.author_id.is_empty() {
                                None
                            } else {
                                proto_msg.author_id.parse::<uuid::Uuid>().ok()
                                    .map(crate::domain::ids::UserId::from)
                            };

                            // Skip own messages — already displayed optimistically at send time.
                            // Without this, the sender sees a duplicate (often as "[Encrypted message]"
                            // because we can't decrypt our own sender-key ciphertext).
                            if let (Some(sid), Some(cur)) = (&sender_id, &current_user_id) {
                                if sid == cur {
                                    tracing::debug!(
                                        channel_id = %channel_id_str,
                                        "Skipping own message from channel stream (already shown optimistically)"
                                    );
                                    continue;
                                }
                            }

                            // Server-managed group channel: Content::Encrypted wraps a
                            // GroupEncrypted proto inside `EncryptedData.ciphertext`. Decrypt
                            // via SenderKeyManager before converting — otherwise
                            // `proto_content_to_domain` would dump the literal
                            // "[Encrypted message]" placeholder into the UI.
                            let decrypted_proto = if let Some(chat_v1::message::content::Type::Encrypted(enc)) =
                                proto_msg.content.as_ref().and_then(|c| c.r#type.as_ref())
                            {
                                let sid = match sender_id {
                                    Some(id) => id,
                                    None => {
                                        tracing::debug!(
                                            channel_id = %channel_id_str,
                                            "Encrypted stream message missing sender_id; skipping"
                                        );
                                        continue;
                                    }
                                };
                                let group_encrypted = match GroupEncrypted::decode(enc.ciphertext.as_slice()) {
                                    Ok(ge) => ge,
                                    Err(e) => {
                                        tracing::warn!(
                                            channel_id = %channel_id_str,
                                            error = %e,
                                            "Failed to decode GroupEncrypted from stream message"
                                        );
                                        continue;
                                    }
                                };
                                // Signal-style: proto must carry sender's source_device
                                // (`author_device_id`). Sender-keys are per (uuid, device).
                                let device_id = proto_msg.author_device_id;
                                if device_id == 0 {
                                    tracing::warn!(
                                        channel_id = %channel_id_str,
                                        sender_id = %sid,
                                        message_id = %proto_msg.id,
                                        "Encrypted stream message missing author_device_id; dropping"
                                    );
                                    continue;
                                }
                                match signal.decrypt_from_group(&sid, device_id, &group_encrypted).await {
                                    Ok(plaintext) => {
                                        use prost::Message as _;
                                        // Sender ships a serialized
                                        // `content_v1::Content::DataMessage`
                                        // as the sender-key plaintext so
                                        // attachment pointers (CDN key + AES
                                        // key) travel with the message.
                                        let dm = match crate::proto::content_v1::Content::decode(plaintext.as_slice())
                                            .ok()
                                            .and_then(|c| c.content)
                                        {
                                            Some(crate::proto::content_v1::content::Content::DataMessage(dm)) => dm,
                                            _ => {
                                                tracing::warn!(
                                                    channel_id = %channel_id_str,
                                                    sender_id = %sid,
                                                    "Decrypted stream plaintext is not a valid DataMessage; dropping"
                                                );
                                                continue;
                                            }
                                        };

                                        // Persist incoming attachment pointers
                                        // so the receiver can download + decrypt.
                                        if !dm.attachments.is_empty() {
                                            attachment_service.create_from_pointers(&dm.attachments);
                                        }

                                        // Rebuild the wire proto with the
                                        // decrypted text + attachment ids so
                                        // `proto_to_domain_message_with_sender`
                                        // picks them up through the Mixed path.
                                        let files: Vec<chat_v1::FileMetadata> = dm
                                            .attachments
                                            .iter()
                                            .filter(|p| !p.cdn_key.is_empty())
                                            .map(|p| chat_v1::FileMetadata {
                                                id: crate::domain::AttachmentId::from_cdn_key(&p.cdn_key).to_string(),
                                                name: p.file_name.clone(),
                                                mime_type: p.content_type.clone(),
                                                size: p.plaintext_size,
                                                ..Default::default()
                                            })
                                            .collect();
                                        let new_type = if files.is_empty() {
                                            chat_v1::message::content::Type::Text(dm.body.clone())
                                        } else {
                                            chat_v1::message::content::Type::Mixed(
                                                chat_v1::message::MixedContent {
                                                    text: dm.body.clone(),
                                                    files,
                                                },
                                            )
                                        };
                                        let mut patched = proto_msg.clone();
                                        patched.content = Some(chat_v1::message::Content {
                                            r#type: Some(new_type),
                                        });
                                        patched
                                    }
                                    Err(e) => {
                                        let es = e.to_string();
                                        if es.contains("no sender key")
                                            || es.contains("missing sender key")
                                            || es.contains("SenderKeyRecord")
                                            || es.contains("not found")
                                        {
                                            tracing::warn!(
                                                channel_id = %channel_id_str,
                                                sender_id = %sid,
                                                device_id,
                                                "Missing sender key; deferring message and requesting SKDM"
                                            );
                                            // Enqueue so we can retry once the SKDM lands.
                                            messages.defer_pending_group_decrypt(
                                                channel_id_owned,
                                                sid,
                                                device_id,
                                                proto_msg.clone(),
                                            );
                                            let msg_svc = messages.clone();
                                            let cid = channel_id_owned;
                                            tokio::spawn(async move {
                                                if let Err(e) = msg_svc.send_sender_key_request(&cid, &sid).await {
                                                    tracing::warn!(
                                                        channel_id = %cid,
                                                        sender_id = %sid,
                                                        error = %e,
                                                        "Failed to send SenderKeyRequest"
                                                    );
                                                }
                                            });
                                        } else {
                                            tracing::warn!(
                                                channel_id = %channel_id_str,
                                                sender_id = %sid,
                                                error = %es,
                                                "Failed to decrypt stream message"
                                            );
                                        }
                                        continue;
                                    }
                                }
                            } else {
                                proto_msg
                            };

                            // Streaming decrypt path — derive
                            // ConversationType from the channel the
                            // subscription is bound to.
                            let conv_type = crate::services::message::conversion::conversation_type_for_channel(
                                &channel_repo_for_stream,
                                &channel_id_owned,
                            );
                            match proto_to_domain_message_with_sender(&decrypted_proto, sender_id, conv_type) {
                                Ok(mut message) => {
                                    // Persist to DB so the message survives a
                                    // channel switch (`load_channel_into_ui`
                                    // reads from `messages` table). Without
                                    // this the bubble only lives in the
                                    // in-memory state and disappears on nav.
                                    match messages.get_message_local(&message.id) {
                                        Ok(Some(existing)) => {
                                            // Preserve attachments + reactions
                                            // already stored for this message.
                                            if message.attachments.is_empty() {
                                                message.attachments = existing.attachments;
                                            }
                                            if message.reaction_counts.is_empty() {
                                                message.reaction_counts = existing.reaction_counts;
                                            }
                                            if let Err(e) = messages.update_message(&message) {
                                                tracing::warn!(
                                                    channel_id = %channel_id_str,
                                                    message_id = %message.id,
                                                    error = %e,
                                                    "Failed to update stream message"
                                                );
                                            }
                                        }
                                        Ok(None) => {
                                            if let Err(e) = messages.store_message_local(&message) {
                                                tracing::warn!(
                                                    channel_id = %channel_id_str,
                                                    message_id = %message.id,
                                                    error = %e,
                                                    "Failed to insert stream message"
                                                );
                                            }
                                        }
                                        Err(e) => {
                                            tracing::warn!(
                                                channel_id = %channel_id_str,
                                                message_id = %message.id,
                                                error = %e,
                                                "Failed to read existing message before upsert"
                                            );
                                        }
                                    }

                                    let _ = event_bus.domain_sender().send(
                                        DomainEvent::MessageReceived { message },
                                    );
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        channel_id = %channel_id_str,
                                        error = %e,
                                        "Failed to convert stream message to domain"
                                    );
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            channel_id = %channel_id_str,
                            error = %e,
                            "Channel message stream error"
                        );
                        break;
                    }
                }
            }
        });

        self.tasks.lock().push(handle);
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
        // Stream subscription owns tasks + the pending-decrypt queue held by
        // this client, so it stays here rather than inside the service.
        self.subscribe_channel_messages(&channel.id);
        Ok(channel)
    }

    /// Builder entry-point for creating a channel.
    ///
    /// Replaces the 7-positional-arg `create_channel` /
    /// `create_channel_with_icon` calls with a fluent builder so callers
    /// don't have to remember the order or pass `None`/`vec![]` placeholders
    /// for fields they don't care about.
    ///
    /// ```ignore
    /// client
    ///     .new_channel("general", ChannelType::Public)
    ///     .description("welcome")
    ///     .members(vec![alice, bob])
    ///     .icon_path(Path::new("logo.png"))
    ///     .create()
    ///     .await?;
    /// ```
    pub fn new_channel(
        self: &Arc<Self>,
        name: impl Into<String>,
        channel_type: ChannelType,
    ) -> ChannelBuilder {
        ChannelBuilder {
            client: self.clone(),
            name: name.into(),
            channel_type,
            config: None,
            description: None,
            icon: None,
            icon_path: None,
            password: None,
            initial_members: Vec::new(),
        }
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
        self.subscribe_channel_messages(&channel.id);

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
    /// `duration` is the ban length — `None` means permanent.
    pub async fn ban_user(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        user_id: &crate::domain::ids::UserId,
        duration: Option<std::time::Duration>,
        reason: Option<&str>,
    ) -> Result<(), crate::services::channel::ChannelError> {
        self.channels().ban_user_complete(channel_id, user_id, duration, reason).await
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

    /// Mute a user in a channel (local-only enforcement — flips the
    /// membership's `is_muted` flag and sets `muted_until`). Server sends
    /// channel-moderation events for peer coordination.
    pub fn mute_user(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        user_id: &crate::domain::ids::UserId,
        duration: std::time::Duration,
    ) -> Result<(), crate::services::channel::ChannelError> {
        self.channels().mute_user(channel_id, user_id, duration)
    }

    /// Unmute a user — clears the local mute flag.
    pub fn unmute_user(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        user_id: &crate::domain::ids::UserId,
    ) -> Result<(), crate::services::channel::ChannelError> {
        self.channels().unmute_user(channel_id, user_id)
    }

    /// Update a channel's name and/or description.
    pub async fn update_channel(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        name: Option<&str>,
        description: Option<&str>,
    ) -> Result<Channel, crate::services::channel::ChannelError> {
        self.channels().update_channel(channel_id, name, description, None, None).await
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
        self.channels().leave_channel_complete(channel_id).await
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

/// Fluent builder for `ChatterClient::new_channel(...)`.
///
/// All fields default to "unset" — only `name` and `channel_type` are
/// required (passed at construction). Call `.create()` to send the
/// CreateChannel RPC and run the post-create wiring (mailbox announce,
/// SKDM distribution, stream subscription).
pub struct ChannelBuilder {
    client: Arc<crate::client::ChatterClient>,
    name: String,
    channel_type: ChannelType,
    config: Option<crate::domain::channel::ChannelConfig>,
    description: Option<String>,
    icon: Option<Vec<u8>>,
    icon_path: Option<std::path::PathBuf>,
    password: Option<String>,
    initial_members: Vec<crate::domain::ids::UserId>,
}

impl ChannelBuilder {
    /// Override the default `ChannelConfig` (encryption, persistence, …).
    pub fn config(mut self, config: crate::domain::channel::ChannelConfig) -> Self {
        self.config = Some(config);
        self
    }

    /// Set the channel topic / description.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Set the channel icon as raw image bytes (PNG/JPEG/WebP, ≤64KB).
    /// Mutually exclusive with `icon_path`; the last setter wins.
    pub fn icon(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.icon = Some(bytes.into());
        self.icon_path = None;
        self
    }

    /// Read the icon from a local file before creating the channel.
    /// Mutually exclusive with `icon`.
    pub fn icon_path(mut self, icon_path: impl Into<std::path::PathBuf>) -> Self {
        self.icon_path = Some(icon_path.into());
        self.icon = None;
        self
    }

    /// Set a join password (private channels).
    pub fn password(mut self, password: impl Into<String>) -> Self {
        self.password = Some(password.into());
        self
    }

    /// Members to invite at creation time.
    pub fn members(mut self, members: Vec<crate::domain::ids::UserId>) -> Self {
        self.initial_members = members;
        self
    }

    /// Add a single member.
    pub fn add_member(mut self, member: crate::domain::ids::UserId) -> Self {
        self.initial_members.push(member);
        self
    }

    /// Execute the create. Reads the icon from disk if `icon_path` is set.
    pub async fn create(
        self,
    ) -> Result<Channel, crate::services::channel::ChannelError> {
        if let Some(path) = self.icon_path {
            self.client
                .create_channel_with_icon(
                    &self.name,
                    self.channel_type,
                    self.config,
                    self.description.as_deref(),
                    Some(path.as_path()),
                    self.password.as_deref(),
                    self.initial_members,
                )
                .await
        } else {
            let fitted = self.icon
                .as_deref()
                .map(ChatterClient::fit_icon_bytes)
                .transpose()?;
            self.client
                .create_channel(
                    &self.name,
                    self.channel_type,
                    self.config,
                    self.description.as_deref(),
                    fitted.as_deref(),
                    self.password.as_deref(),
                    self.initial_members,
                )
                .await
        }
    }
}
