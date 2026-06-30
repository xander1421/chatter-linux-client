//! Core message operations.
//!
//! Send, edit, delete, reply, forward, and ephemeral message operations.

use std::path::Path;

use crate::domain::ephemeral::MessageTTL;
use crate::domain::ids::{AttachmentId, ChannelId, IntegrationId, MessageId};
use crate::domain::message::{Message, MessageContent};
use crate::mailbox::group::GroupFanout;
use crate::network::grpc::auth_client::AuthClient;
use crate::network::grpc::channel_client::ChannelClient;
use crate::network::grpc::message_client::MessageClient;
use crate::proto::chat_v1;
use crate::proto::content_v1::AttachmentPointer;
use crate::services::attachment::AttachmentService;
use crate::services::current_user::CurrentUserProvider;

use super::{MessageError, MessageService};

impl MessageService {
    /// Send a message to a channel.
    ///
    /// Encrypts with sender keys if channel requires encryption. When
    /// `group_fanout` is provided and channel is encrypted, the message
    /// will be delivered to member mailboxes IN ADDITION to the channel-based
    /// delivery. This provides traffic analysis resistance per NDSS '21 paper.
    #[allow(clippy::too_many_arguments)]
    pub async fn send_message(
        &self,
        message_client: &mut MessageClient,
        auth_client: &mut AuthClient,
        channel_client: &mut ChannelClient,
        group_fanout: Option<&GroupFanout>,
        channel_id: &ChannelId,
        text: &str,
        attachment_ids: Vec<AttachmentId>,
        attachment_pointers: Vec<AttachmentPointer>,
        thread_id: Option<&MessageId>,
        mentioned_integrations: Vec<IntegrationId>,
        targets_ai_bot: bool,
    ) -> Result<Message, MessageError> {
        // Check logged in
        let sender_id = self.settings_repo.current_user_id()?;

        // Validate content - must have text or attachments
        let trimmed_text = text.trim();
        if trimmed_text.is_empty() && attachment_ids.is_empty() {
            return Err(MessageError::InvalidContent(
                "message must have text or attachments".to_string(),
            ));
        }

        // Server-visible content is the text-only placeholder — the server
        // never sees plaintext on encrypted channels anyway, and unencrypted
        // channels only ever ship text.
        let content = chat_v1::message::Content {
            r#type: Some(chat_v1::message::content::Type::Text(trimmed_text.to_string())),
        };

        // Get encryption settings from channel config
        let (is_dm, require_encryption) = self
            .get_channel_encryption_settings(channel_id, channel_client)
            .await?;

        // E2E plaintext that rides inside the sender-key ciphertext: use the
        // pairwise `content_v1::DataMessage` proto so attachment pointers
        // (CDN key + per-blob AES key) travel with the message and receivers
        // can actually download + decrypt the blob.
        let plaintext_bytes = {
            use prost::Message as _;
            let dm = crate::proto::content_v1::DataMessage {
                body: trimmed_text.to_string(),
                attachments: attachment_pointers.clone(),
                channel_id: channel_id.to_string(),
                timestamp: crate::util::time::TimestampNanos::now().as_u64(),
                mentioned_integrations: mentioned_integrations
                    .iter()
                    .map(|id| id.to_string())
                    .collect(),
                targets_ai_bot,
                ..Default::default()
            };
            let wrap = crate::proto::content_v1::Content {
                timestamp: dm.timestamp,
                content: Some(crate::proto::content_v1::content::Content::DataMessage(dm)),
                ..Default::default()
            };
            wrap.encode_to_vec()
        };

        let (content_to_send, skdm_opt) = self.prepare_encrypted_content(
            auth_client,
            channel_id,
            &plaintext_bytes,
            content,
            is_dm,
            require_encryption,
        ).await?;

        // If a new sender key was created, distribute SKDM to channel members
        if let Some(skdm) = skdm_opt {
            self.distribute_sender_key(auth_client, message_client, channel_id, &skdm).await?;
        }

        // Call server
        tracing::debug!(
            channel_id = %channel_id,
            text_len = trimmed_text.len(),
            "[SEND_MESSAGE] Sending message to server API"
        );
        let proto_message = message_client
            .send_message(
                &channel_id.to_string(),
                content_to_send.clone(),
                thread_id.map(|id| id.to_string()).as_deref(),
                false, // ephemeral (Phase 34)
                None,  // ephemeral_ttl
                None,  // crypto_options
                mentioned_integrations.iter().map(|id| id.to_string()).collect(),
                targets_ai_bot,
                attachment_ids.iter().map(|id| id.to_string()).collect(),
            )
            .await?;
        tracing::info!(
            channel_id = %channel_id,
            message_id = %proto_message.id,
            "[SEND_MESSAGE] Server API success, message_id={}", proto_message.id
        );

        // Signal-style multi-device self-sync for DMs: the channel send above
        // delivers ONE ciphertext (encrypted for recipient's primary device).
        // Every other device that needs to see this message — recipient's other
        // devices + our own linked devices — gets its own sealed-sender copy
        // addressed directly via pairwise endpoint.
        if is_dm {
            if let Err(e) = self
                .fanout_dm_sync_copies(auth_client, message_client, channel_id, trimmed_text.as_bytes())
                .await
            {
                tracing::warn!(
                    channel_id = %channel_id,
                    error = %e,
                    "DM multi-device sync fanout failed (primary delivery still succeeded)"
                );
            }
        }

        // Server-managed channels: delivery happens via server → device inbox fanout
        // only. Pairwise group-mailbox fanout is reserved for anon groups (which go
        // through AnonGroupService::send_message). Sending both here causes receivers
        // to store the same message twice — once with plaintext (after SKDM arrives),
        // once as an "[Encrypted message]" placeholder (race where pairwise copy
        // arrives before the sender key).
        let _ = group_fanout;
        let _ = attachment_pointers;

        // Convert to domain message and preserve attachment IDs.
        // Derive ConversationType from the channel row so identified
        // channels don't get mistagged as AnonGroup.
        let conv_type = super::conversion::conversation_type_for_channel(&self.channel_repo, channel_id);
        let mut message = proto_to_domain_message(&proto_message, sender_id, conv_type)?;
        message.attachments = attachment_ids;

        // IMPORTANT: Check if server returned a different channel_id
        if message.conversation_id != *channel_id {
            tracing::warn!(
                input_channel_id = %channel_id,
                server_channel_id = %message.conversation_id,
                "[SEND_MESSAGE] CHANNEL_ID MISMATCH! Server returned different channel_id"
            );
            // Fix: Use the input channel_id, not the server's
            message.conversation_id = *channel_id;
        }

        // For sent messages, store original plaintext locally (not encrypted server response)
        message.content = MessageContent::Text(trimmed_text.to_string());

        // Store locally
        tracing::info!(
            channel_id = %message.conversation_id,
            message_id = %message.id,
            text_len = trimmed_text.len(),
            "[SEND_MESSAGE] Storing message locally"
        );
        self.message_repo.insert(&message)?;
        tracing::info!(
            channel_id = %channel_id,
            message_id = %message.id,
            "[SEND_MESSAGE] Message stored successfully"
        );

        Ok(message)
    }

    /// Fan a DM message out to every device that needs a sync copy.
    ///
    /// Signal-style multi-device: the primary channel send addresses the
    /// recipient's primary device only. This helper delivers the same
    /// plaintext (sealed-sender encrypted per device) to:
    ///
    /// * recipient's non-primary devices
    /// * our own linked devices other than this one
    ///
    /// Failures here do not abort the outer send — the primary device already
    /// got the message via the channel path; sync is best-effort.
    async fn fanout_dm_sync_copies(
        &self,
        auth_client: &mut AuthClient,
        message_client: &mut MessageClient,
        channel_id: &ChannelId,
        plaintext: &[u8],
    ) -> Result<(), MessageError> {
        use crate::proto::chat_v1::SealedSenderMessage;
        use crate::storage::repositories::settings_repo::keys;

        const PRIMARY_DEVICE_ID: u32 = 1;

        let our_user_id = self.settings_repo
            .get_string(keys::CURRENT_USER_ID)
            .map_err(MessageError::Storage)?
            .ok_or(MessageError::NotLoggedIn)?;
        let our_device_id = self.settings_repo
            .get::<u32>(keys::CURRENT_DEVICE_ID)
            .map_err(MessageError::Storage)?
            .ok_or(MessageError::NotLoggedIn)?;

        // Find the DM peer (the other channel member).
        let members = self.channel_repo.list_members(channel_id).unwrap_or_default();
        let recipient_id = members
            .iter()
            .find(|m| m.user_id.to_string() != our_user_id)
            .map(|m| m.user_id.to_string());

        let timestamp = crate::util::time::TimestampNanos::now().as_u64();
        let server_key_id = self.signal_service.get_server_key_id();

        // Recipient fanout: every device EXCEPT primary (primary handled by channel send).
        if let Some(rid) = recipient_id {
            match self.signal_service
                .encrypt_message_all_devices(auth_client, &rid, plaintext)
                .await
            {
                Ok(ciphertexts) => {
                    for (device_id, sealed_bytes) in ciphertexts {
                        if device_id == PRIMARY_DEVICE_ID {
                            continue;
                        }
                        let sealed_message = SealedSenderMessage {
                            sealed_body: sealed_bytes,
                            server_key_id,
                            timestamp,
                        };
                        if let Err(e) = message_client
                            .send_sealed_message(&rid, device_id, sealed_message)
                            .await
                        {
                            tracing::warn!(
                                recipient = %rid,
                                device_id,
                                error = %e,
                                "DM sync: failed to deliver to recipient device"
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        recipient = %rid,
                        error = %e,
                        "DM sync: failed to encrypt for recipient's other devices"
                    );
                }
            }
        }

        // Self fanout: every own device EXCEPT this one.
        match self.signal_service
            .encrypt_message_all_devices(auth_client, &our_user_id, plaintext)
            .await
        {
            Ok(ciphertexts) => {
                for (device_id, sealed_bytes) in ciphertexts {
                    if device_id == our_device_id {
                        continue;
                    }
                    let sealed_message = SealedSenderMessage {
                        sealed_body: sealed_bytes,
                        server_key_id,
                        timestamp,
                    };
                    if let Err(e) = message_client
                        .send_sealed_message(&our_user_id, device_id, sealed_message)
                        .await
                    {
                        tracing::warn!(
                            device_id,
                            error = %e,
                            "DM sync: failed to deliver to own other device"
                        );
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "DM sync: failed to encrypt for our own other devices"
                );
            }
        }

        Ok(())
    }

    /// Send a message to a channel (simplified API - no client params).
    ///
    /// This is the preferred method - uses internal gRPC clients.
    ///
    /// # Arguments
    /// * `attachment_ids` - IDs for server API and local storage
    /// * `attachment_pointers` - Full AttachmentPointer protos for E2E encrypted group delivery
    #[allow(clippy::too_many_arguments)]
    pub async fn send(
        &self,
        group_fanout: Option<&GroupFanout>,
        channel_id: &ChannelId,
        text: &str,
        attachment_ids: Vec<AttachmentId>,
        attachment_pointers: Vec<AttachmentPointer>,
        thread_id: Option<&MessageId>,
        mentioned_integrations: Vec<IntegrationId>,
        targets_ai_bot: bool,
    ) -> Result<Message, MessageError> {
        let mut message_client = self.message_client();
        let mut auth_client = self.auth_client();
        let mut channel_client = self.channel_client();

        self.send_message(
            &mut message_client,
            &mut auth_client,
            &mut channel_client,
            group_fanout,
            channel_id,
            text,
            attachment_ids,
            attachment_pointers,
            thread_id,
            mentioned_integrations,
            targets_ai_bot,
        )
        .await
    }

    /// Send a message with file attachments (high-level API).
    ///
    /// This method handles the complete flow:
    /// 1. Upload each file to CDN with encryption
    /// 2. Build AttachmentPointer protos
    /// 3. Send message via server API
    /// 4. Send via group mailboxes (if encrypted channel)
    ///
    /// # Arguments
    /// * `group_fanout` - Group member mailbox info for E2E encrypted delivery
    /// * `channel_id` - Target channel
    /// * `text` - Message text
    /// * `file_paths` - Local file paths to upload as attachments
    /// * `thread_id` - Optional thread root for thread replies
    /// * `mentioned_integrations` - IDs of mentioned integrations (bots)
    /// * `targets_ai_bot` - Whether message targets AI bot
    #[allow(clippy::too_many_arguments)]
    pub async fn send_with_files(
        &self,
        group_fanout: Option<&GroupFanout>,
        channel_id: &ChannelId,
        text: &str,
        file_paths: Vec<&Path>,
        thread_id: Option<&MessageId>,
        mentioned_integrations: Vec<IntegrationId>,
        targets_ai_bot: bool,
    ) -> Result<Message, MessageError> {
        // Upload attachments with encryption
        let attachment_service = AttachmentService::new(self.attachment_repo.clone(), self.grpc_pool.clone());
        let mut attachment_ids = Vec::with_capacity(file_paths.len());
        let mut attachment_pointers = Vec::with_capacity(file_paths.len());

        for file_path in &file_paths {
            tracing::info!(
                path = %file_path.display(),
                "Encrypting and uploading attachment to CDN"
            );

            match attachment_service.upload_to_cdn_encrypted(file_path).await {
                Ok(result) => {
                    tracing::info!(
                        cdn_key = %result.cdn_key,
                        attachment_id = %result.attachment_id,
                        filename = %result.file_name,
                        "Attachment uploaded to CDN"
                    );
                    attachment_ids.push(result.attachment_id);
                    attachment_pointers.push(result.to_attachment_pointer());
                }
                Err(e) => {
                    tracing::error!(
                        path = %file_path.display(),
                        error = %e,
                        "Failed to upload attachment to CDN"
                    );
                    return Err(MessageError::AttachmentError(e.to_string()));
                }
            }
        }

        // Send message with uploaded attachments
        self.send(
            group_fanout,
            channel_id,
            text,
            attachment_ids,
            attachment_pointers,
            thread_id,
            mentioned_integrations,
            targets_ai_bot,
        )
        .await
    }

    /// Send a message with pre-uploaded attachment pointers.
    ///
    /// Like `send_with_files` but skips the upload step.
    pub async fn send_with_pointers(
        &self,
        group_fanout: Option<&GroupFanout>,
        channel_id: &ChannelId,
        text: &str,
        attachment_pointers: Vec<AttachmentPointer>,
        attachment_ids: Vec<AttachmentId>,
        thread_id: Option<&MessageId>,
        mentioned_integrations: Vec<IntegrationId>,
        targets_ai_bot: bool,
    ) -> Result<Message, MessageError> {
        self.send(
            group_fanout,
            channel_id,
            text,
            attachment_ids,
            attachment_pointers,
            thread_id,
            mentioned_integrations,
            targets_ai_bot,
        )
        .await
    }

    /// Send control content (reactions, typing, pin, edit, delete, …) to
    /// every channel member's device inbox as a pairwise sealed-sender
    /// envelope.
    ///
    /// Replaces the former group-mailbox transport: the server-managed
    /// channels already expose membership, so the additional mailbox buys
    /// no privacy and costs a second polling loop.
    pub async fn send_control_content(
        &self,
        _message_client: &mut MessageClient,
        auth_client: &mut AuthClient,
        _channel_client: &mut ChannelClient,
        channel_id: &ChannelId,
        content: crate::proto::content_v1::Content,
        _group_fanout: Option<&GroupFanout>,
    ) -> Result<(), MessageError> {
        use prost::Message as ProstMessage;

        let _ = self.settings_repo.current_user_id()?;

        let plaintext = content.encode_to_vec();
        self.fanout_control_to_device_inboxes(auth_client, channel_id, &plaintext).await
    }

    /// Send control content to a channel (simplified API — no client params).
    pub async fn send_control(
        &self,
        channel_id: &ChannelId,
        content: crate::proto::content_v1::Content,
        group_fanout: Option<&GroupFanout>,
    ) -> Result<(), MessageError> {
        let mut message_client = self.message_client();
        let mut auth_client = self.auth_client();
        let mut channel_client = self.channel_client();

        self.send_control_content(
            &mut message_client,
            &mut auth_client,
            &mut channel_client,
            channel_id,
            content,
            group_fanout,
        )
        .await
    }

    /// Encrypt pre-serialized control content per (member, device) and ship
    /// each ciphertext through the device inbox with an anonymous credential.
    ///
    /// Skips our own current device (we already applied the change locally)
    /// but fans to our own linked devices for Signal-style multi-device sync.
    async fn fanout_control_to_device_inboxes(
        &self,
        auth_client: &mut AuthClient,
        channel_id: &ChannelId,
        plaintext: &[u8],
    ) -> Result<(), MessageError> {
        use crate::proto::chat_v1::SealedSenderMessage;
        use crate::storage::repositories::settings_repo::keys;

        let our_user_id = self.settings_repo
            .get_string(keys::CURRENT_USER_ID)
            .map_err(MessageError::Storage)?
            .ok_or(MessageError::NotLoggedIn)?;
        let our_device_id = self.settings_repo
            .get::<u32>(keys::CURRENT_DEVICE_ID)
            .map_err(MessageError::Storage)?
            .ok_or(MessageError::NotLoggedIn)?;

        // Always refresh membership from the server before fanning out.
        // `channel_event_service` only streams member-added events for
        // channels the user was already in; joiners that arrive later have
        // no local row, so a stale cache would silently drop reactions,
        // pins, etc. for them.
        self.refresh_channel_members(channel_id).await;

        let members = self.channel_repo.list_members(channel_id).unwrap_or_default();
        if members.is_empty() {
            return Ok(());
        }

        let timestamp = crate::util::time::TimestampNanos::now().as_u64();
        let server_key_id = self.signal_service.get_server_key_id();
        let token_manager = self.token_manager.clone();
        let mut anon_client = self.grpc_pool.anonymous_message_client();

        let mut success = 0u32;
        let mut errors = 0u32;

        for member in &members {
            let member_id = member.user_id.to_string();
            let is_self = member_id == our_user_id;

            let ciphertexts = match self.signal_service
                .encrypt_message_all_devices(auth_client, &member_id, plaintext)
                .await
            {
                Ok(cts) => cts,
                Err(e) => {
                    errors += 1;
                    tracing::warn!(
                        channel_id = %channel_id,
                        member_id = %member_id,
                        error = %e,
                        "Failed to encrypt control content for member"
                    );
                    continue;
                }
            };

            for (device_id, sealed_bytes) in ciphertexts {
                if is_self && device_id == our_device_id {
                    continue;
                }

                let token = match self.get_or_request_token_for_skdm(&token_manager, &member_id).await {
                    Some(t) => t,
                    None => {
                        errors += 1;
                        continue;
                    }
                };

                let sealed_message = SealedSenderMessage {
                    sealed_body: sealed_bytes,
                    server_key_id,
                    timestamp,
                };

                match anon_client
                    .send_with_anonymous_credential(
                        &member_id,
                        device_id,
                        sealed_message,
                        &token.public_key,
                        &token.signature,
                        token.randomizer.as_deref(),
                    )
                    .await
                {
                    Ok(_) => success += 1,
                    Err(e) => {
                        errors += 1;
                        tracing::warn!(
                            channel_id = %channel_id,
                            member_id = %member_id,
                            device_id,
                            error = %e,
                            "Failed to deliver control content to device"
                        );
                    }
                }
            }
        }

        tracing::debug!(
            channel_id = %channel_id,
            success,
            errors,
            "Control content fanout via device inbox complete"
        );
        Ok(())
    }

    /// Pull the current member list from the server and upsert it into the
    /// local `channel_memberships` table. Fire-and-forget: a failure to
    /// refresh doesn't block the outgoing control send — the fanout simply
    /// falls back to whatever the cache holds.
    async fn refresh_channel_members(&self, channel_id: &ChannelId) {
        let mut channel_client = self.grpc_pool.channel_client();
        let response = match channel_client
            .get_channel_members(&channel_id.to_string(), 100, 0)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!(
                    channel_id = %channel_id,
                    error = %e,
                    "Skipping member refresh (RPC failed)"
                );
                return;
            }
        };

        for pb_member in &response.members {
            let user_id = match pb_member.user_id.parse::<crate::domain::UserId>() {
                Ok(id) => id,
                Err(_) => continue,
            };
            let membership = crate::domain::channel::Membership {
                channel_id: *channel_id,
                user_id,
                role: crate::domain::channel::MemberRole::Member,
                notification_preference: crate::domain::channel::NotificationPreference::All,
                joined_at: chrono::Utc::now(),
                last_read_message_id: None,
                is_muted: false,
                muted_until: None,
            };
            let _ = self.channel_repo.insert_or_ignore_membership(&membership);
        }
    }

    /// Send an ephemeral (self-destructing) message to a channel.
    #[allow(clippy::too_many_arguments)]
    pub async fn send_ephemeral_message(
        &self,
        message_client: &mut MessageClient,
        channel_id: &ChannelId,
        text: &str,
        ttl: MessageTTL,
        attachment_ids: Vec<AttachmentId>,
        thread_id: Option<&MessageId>,
    ) -> Result<Message, MessageError> {
        // Check logged in
        let sender_id = self.settings_repo.current_user_id()?;

        // Validate content - must have text or attachments
        let trimmed_text = text.trim();
        if trimmed_text.is_empty() && attachment_ids.is_empty() {
            return Err(MessageError::InvalidContent(
                "message must have text or attachments".to_string(),
            ));
        }

        // Build proto content
        let content = chat_v1::message::Content {
            r#type: Some(chat_v1::message::content::Type::Text(trimmed_text.to_string())),
        };

        // Convert TTL to prost Duration
        let ephemeral_ttl = Some(prost_types::Duration {
            seconds: ttl.as_secs() as i64,
            nanos: 0,
        });

        // Call server with ephemeral flag
        let proto_message = message_client
            .send_message(
                &channel_id.to_string(),
                content,
                thread_id.map(|id| id.to_string()).as_deref(),
                true,          // ephemeral = true
                ephemeral_ttl, // TTL duration
                None,          // crypto_options
                Vec::new(),    // mentioned_integrations (not typically used with ephemeral)
                false,         // targets_ai_bot
                attachment_ids.iter().map(|id| id.to_string()).collect(),
            )
            .await?;

        // Convert to domain message and preserve attachment IDs
        let conv_type = super::conversion::conversation_type_for_channel(&self.channel_repo, channel_id);
        let mut message = proto_to_domain_message(&proto_message, sender_id, conv_type)?;
        message.attachments = attachment_ids;

        // IMPORTANT: Ensure channel_id matches (server might return different format)
        if message.conversation_id != *channel_id {
            tracing::warn!(
                input_channel_id = %channel_id,
                server_channel_id = %message.conversation_id,
                "[SEND_EPHEMERAL] CHANNEL_ID MISMATCH! Server returned different channel_id"
            );
            message.conversation_id = *channel_id;
        }

        // Store locally
        self.message_repo.insert(&message)?;

        Ok(message)
    }

    /// Edit a message in a channel.
    pub async fn edit_message(
        &self,
        channel_id: &ChannelId,
        message_id: &MessageId,
        new_text: &str,
    ) -> Result<Message, MessageError> {
        // Check logged in
        let current_user = self.settings_repo.current_user_id()?;

        // Validate new content
        let trimmed_text = new_text.trim();
        if trimmed_text.is_empty() {
            return Err(MessageError::InvalidContent(
                "message content cannot be empty".to_string(),
            ));
        }

        // Get existing message from local DB
        let existing = self
            .message_repo
            .get(message_id)?
            .ok_or_else(|| MessageError::MessageNotFound(*message_id))?;

        // Verify sender matches current user (only if sender_id is known)
        if let Some(sender_id) = existing.sender_id {
            if sender_id != current_user {
                return Err(MessageError::NotAuthor);
            }
        }

        // Build proto content
        let content = chat_v1::message::Content {
            r#type: Some(chat_v1::message::content::Type::Text(trimmed_text.to_string())),
        };

        // Call server
        let mut message_client = self.message_client();
        let proto_message = message_client
            .edit_message(&message_id.to_string(), &channel_id.to_string(), content)
            .await?;

        // Convert to domain message (use server response for consistency)
        let conv_type = super::conversion::conversation_type_for_channel(&self.channel_repo, channel_id);
        let mut updated_message = proto_to_domain_message(&proto_message, current_user, conv_type)?;

        // Mark edited locally (in case server didn't set it)
        updated_message.mark_edited();

        // Update local storage
        self.message_repo.update(&updated_message)?;

        Ok(updated_message)
    }

    /// Delete a message from a channel.
    pub async fn delete_message(
        &self,
        channel_id: &ChannelId,
        message_id: &MessageId,
    ) -> Result<(), MessageError> {
        // Check logged in
        let current_user = self.settings_repo.current_user_id()?;

        // Get existing message to verify ownership
        let existing = self
            .message_repo
            .get(message_id)?
            .ok_or_else(|| MessageError::MessageNotFound(*message_id))?;

        // Verify sender matches current user (only if sender_id is known)
        if let Some(sender_id) = existing.sender_id {
            if sender_id != current_user {
                return Err(MessageError::NotAuthor);
            }
        }

        // Call server
        let mut message_client = self.message_client();
        message_client
            .delete_message(&message_id.to_string(), &channel_id.to_string())
            .await?;

        // Soft delete locally - keep for history/sync
        let mut message = existing;
        message.mark_deleted();
        self.message_repo.update(&message)?;

        Ok(())
    }

    /// Patch an already-stored outgoing message with reply-quote context.
    ///
    /// Used by the outgoing router: channel sends (`send_with_pointers` /
    /// `send_with_files`) do not carry the quote, so the local row is written
    /// without `reply_to`/`reply_to_author_id`/`reply_to_text`. When the user
    /// is replying we fill those columns in here so the quote bubble survives
    /// a channel reload.
    ///
    /// Returns the updated message on success, or `(original, error)` if the
    /// update failed — callers can choose to log and continue.
    pub fn persist_outgoing_reply_context(
        &self,
        mut message: Message,
        quote: &crate::services::gateway::QuoteIntent,
    ) -> Result<Message, (Message, crate::error::StorageError)> {
        // Resolve quoted message locally to fill in any missing fields.
        let original = self
            .message_repo
            .get_by_sender_timestamp(&quote.target_author_id, quote.target_timestamp)
            .ok()
            .flatten();
        let original_attachments: Vec<_> = original
            .as_ref()
            .map(|m| {
                m.attachments
                    .iter()
                    .filter_map(|id| self.attachment_repo.get(id).ok().flatten())
                    .collect()
            })
            .unwrap_or_default();
        let reply_to_id = original
            .as_ref()
            .map(|m| m.id)
            .unwrap_or_else(|| crate::domain::ids::MessageId::from_timestamp(quote.target_timestamp));
        let reply_to_text = quote
            .target_preview
            .clone()
            .or_else(|| {
                original.as_ref().map(|m| {
                    crate::services::gateway::shared::message_reply_preview(m, &original_attachments)
                })
            })
            .filter(|t| !t.is_empty());
        let quote_atts =
            crate::services::gateway::shared::build_quoted_attachments(&original_attachments);
        let (reply_thumb, reply_thumb_mime) = quote_atts
            .first()
            .map(|qa| {
                let thumb = (!qa.thumbnail.is_empty()).then(|| qa.thumbnail.clone());
                let mime = (!qa.content_type.is_empty()).then(|| qa.content_type.clone());
                (thumb, mime)
            })
            .unwrap_or((None, None));

        message.reply_to = Some(reply_to_id);
        message.reply_to_author_id = Some(quote.target_author_id);
        message.reply_to_text = reply_to_text;
        message.reply_to_thumbnail = reply_thumb;
        message.reply_to_thumbnail_mime = reply_thumb_mime;

        match self.message_repo.update(&message) {
            Ok(()) => Ok(message),
            Err(e) => Err((message, e)),
        }
    }

    /// Send a reply (quote) to an existing message.
    #[allow(clippy::too_many_arguments)]
    pub async fn send_reply(
        &self,
        message_client: &mut MessageClient,
        auth_client: &mut AuthClient,
        channel_client: &mut ChannelClient,
        channel_id: &ChannelId,
        text: &str,
        reply_to_id: &MessageId,
        attachment_ids: Vec<AttachmentId>,
        mentioned_integrations: Vec<IntegrationId>,
        targets_ai_bot: bool,
    ) -> Result<Message, MessageError> {
        // Check logged in
        let sender_id = self.settings_repo.current_user_id()?;

        // Validate reply_to exists locally (informational - server validates)
        if self.message_repo.get(reply_to_id)?.is_none() {
            // Log but don't fail - server will validate
            // The message might exist on server but not locally
        }

        // Validate content - must have text or attachments
        let trimmed_text = text.trim();
        if trimmed_text.is_empty() && attachment_ids.is_empty() {
            return Err(MessageError::InvalidContent(
                "reply must have text or attachments".to_string(),
            ));
        }

        // Server-visible placeholder (E2E plaintext rides inside the
        // sender-key ciphertext as content_v1::DataMessage below).
        let content = chat_v1::message::Content {
            r#type: Some(chat_v1::message::content::Type::Text(trimmed_text.to_string())),
        };

        let (is_dm, require_encryption) = self
            .get_channel_encryption_settings(channel_id, channel_client)
            .await?;

        // Load attachment pointers from local repo so the E2E plaintext
        // carries CDN keys + per-blob AES keys to the receiver.
        let pointers: Vec<crate::proto::content_v1::AttachmentPointer> = attachment_ids
            .iter()
            .filter_map(|id| self.attachment_repo.get(id).ok().flatten())
            .filter_map(|att| att.to_pointer())
            .collect();

        let plaintext_bytes = {
            use prost::Message as _;
            let dm = crate::proto::content_v1::DataMessage {
                body: trimmed_text.to_string(),
                attachments: pointers,
                channel_id: channel_id.to_string(),
                timestamp: crate::util::time::TimestampNanos::now().as_u64(),
                mentioned_integrations: mentioned_integrations
                    .iter()
                    .map(|id| id.to_string())
                    .collect(),
                targets_ai_bot,
                ..Default::default()
            };
            let wrap = crate::proto::content_v1::Content {
                timestamp: dm.timestamp,
                content: Some(crate::proto::content_v1::content::Content::DataMessage(dm)),
                ..Default::default()
            };
            wrap.encode_to_vec()
        };

        let (content_to_send, skdm_opt) = self.prepare_encrypted_content(
            auth_client,
            channel_id,
            &plaintext_bytes,
            content,
            is_dm,
            require_encryption,
        ).await?;

        // If a new sender key was created, distribute SKDM to channel members
        if let Some(skdm) = skdm_opt {
            self.distribute_sender_key(auth_client, message_client, channel_id, &skdm).await?;
        }

        // Call server - note: reply_to is stored locally, not sent to server
        // Server's parent_id is for threads, not quotes
        let proto_message = message_client
            .send_message(
                &channel_id.to_string(),
                content_to_send,
                None,  // parent_id is for threads, not replies
                false, // ephemeral
                None,  // ephemeral_ttl
                None,  // crypto_options
                mentioned_integrations.iter().map(|id| id.to_string()).collect(),
                targets_ai_bot,
                attachment_ids.iter().map(|id| id.to_string()).collect(),
            )
            .await?;

        // Convert to domain message and set reply_to + attachments
        let conv_type = super::conversion::conversation_type_for_channel(&self.channel_repo, channel_id);
        let mut message = proto_to_domain_message(&proto_message, sender_id, conv_type)?;
        message.reply_to = Some(*reply_to_id);
        message.attachments = attachment_ids;

        // Populate reply quote context so reply bubbles survive reload
        if let Ok(Some(original)) = self.message_repo.get(reply_to_id) {
            message.reply_to_author_id = original.sender_id;
            message.reply_to_text = original.as_text().map(|t| t.to_string());
            let atts: Vec<_> = original
                .attachments
                .iter()
                .filter_map(|id| self.attachment_repo.get(id).ok().flatten())
                .collect();
            let quote_atts = crate::services::gateway::shared::build_quoted_attachments(&atts);
            if let Some(qa) = quote_atts.first() {
                message.reply_to_thumbnail = (!qa.thumbnail.is_empty()).then(|| qa.thumbnail.clone());
                message.reply_to_thumbnail_mime = (!qa.content_type.is_empty()).then(|| qa.content_type.clone());
            }
        }

        // IMPORTANT: Ensure channel_id matches (server might return different format)
        if message.conversation_id != *channel_id {
            tracing::warn!(
                input_channel_id = %channel_id,
                server_channel_id = %message.conversation_id,
                "[SEND_REPLY] CHANNEL_ID MISMATCH! Server returned different channel_id"
            );
            message.conversation_id = *channel_id;
        }

        // For sent messages, store original plaintext locally (not encrypted server response)
        message.content = MessageContent::Text(trimmed_text.to_string());

        // Store locally with reply_to set
        self.message_repo.insert(&message)?;

        Ok(message)
    }

    /// Helper to encrypt content based on channel type.
    pub(crate) async fn prepare_encrypted_content(
        &self,
        auth_client: &mut AuthClient,
        channel_id: &ChannelId,
        plaintext: &[u8],
        original_content: chat_v1::message::Content,
        is_dm: bool,
        require_encryption: bool,
    ) -> Result<(chat_v1::message::Content, Option<crate::proto::content_v1::SenderKeyDistribution>), MessageError> {
        // DM: Use sealed sender (pairwise encryption)
        // Multi-device: encrypt for ALL recipient devices, send primary ciphertext
        // Server handles device routing via channel subscription
        if is_dm {
            if let Ok(recipient_id) = channel_id.to_string().parse::<uuid::Uuid>() {
                // Encrypt for ALL recipient devices (Signal multi-device)
                match self.signal_service.encrypt_message_all_devices(
                    auth_client,
                    &recipient_id.to_string(),
                    plaintext
                ).await {
                    Ok(device_ciphertexts) => {
                        // For channel DMs, server handles device addressing
                        // Use primary device (1) ciphertext, fallback to first available
                        let primary_ciphertext = device_ciphertexts.get(&1)
                            .or_else(|| device_ciphertexts.values().next())
                            .cloned();

                        if let Some(ciphertext) = primary_ciphertext {
                            tracing::debug!(
                                recipient = %recipient_id,
                                device_count = device_ciphertexts.len(),
                                "Channel DM encrypted for devices"
                            );

                            let encrypted_content = chat_v1::message::Content {
                                r#type: Some(chat_v1::message::content::Type::SealedSender(
                                    chat_v1::SealedSenderMessage {
                                        sealed_body: ciphertext,
                                        ..Default::default()
                                    }
                                )),
                            };
                            return Ok((encrypted_content, None));
                        }
                        // Fail closed: never downgrade a DM to cleartext.
                        return Err(MessageError::EncryptionFailed(
                            "DM encryption produced no ciphertexts".into(),
                        ));
                    },
                    Err(e) => {
                        return Err(MessageError::EncryptionFailed(format!(
                            "DM encryption failed: {}", e
                        )));
                    }
                }
            }
            // DM target channel_id was not a valid recipient UUID — fail closed.
            return Err(MessageError::EncryptionFailed(
                "DM recipient could not be resolved for encryption".into(),
            ));
        }

        // Encrypted channel: Use sender keys (group encryption)
        if require_encryption {
            match self.signal_service.encrypt_for_group(channel_id, plaintext).await {
                Ok((group_encrypted, skdm_opt)) => {
                    use prost::Message;
                    // Serialize GroupEncrypted proto and pack into EncryptedData
                    let group_bytes = group_encrypted.encode_to_vec();

                    let encrypted_content = chat_v1::message::Content {
                        r#type: Some(chat_v1::message::content::Type::Encrypted(
                            chat_v1::EncryptedData {
                                version: chat_v1::CryptoVersion::SignalProtocol as i32,
                                ciphertext: group_bytes,
                                // Mark as sender key message in associated_data
                                associated_data: b"sender_key".to_vec(),
                                // Other fields empty for sender key
                                nonce: vec![],
                                auth_tag: vec![],
                                signature: vec![],
                                device_id: String::new(),
                            }
                        )),
                    };
                    return Ok((encrypted_content, skdm_opt));
                },
                Err(e) => {
                    // Fail closed: an encryption-required channel must never
                    // downgrade to cleartext.
                    return Err(MessageError::EncryptionFailed(format!(
                        "Group encryption failed for channel {}: {}", channel_id, e
                    )));
                }
            }
        }

        // Regular (non-encrypted) channel: content intentionally sent as-is.
        Ok((original_content, None))
    }
}

// Re-export proto conversion functions from conversion module
pub use super::conversion::{proto_to_domain_message, proto_to_domain_message_with_sender, proto_content_to_domain};
