//! DM bootstrap operations.
//!
//! Handles initial DM conversation setup using the Sesame algorithm
//! from the NDSS '21 paper. The lower UUID always initiates the bootstrap.

use std::sync::Arc;

use prost::Message as ProstMessage;

use crate::domain::ids::{AttachmentId, ChannelId, UserId};
use crate::mailbox::dm::{
    release_bootstrap_lock, try_acquire_bootstrap_lock, DmBootstrap,
};
use crate::mailbox::TokenManager;
use crate::network::grpc::auth_client::AuthClient;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::network::grpc::message_client::MessageClient;
use crate::network::grpc::MailboxClient;
use crate::proto::chat_v1::SealedSenderMessage;
use crate::services::current_user::CurrentUserProvider;
use crate::util::time::TimestampNanos;

use super::{MessageError, MessageService};

impl MessageService {
    /// Handle DM bootstrap when conversation is not ready.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn handle_dm_bootstrap(
        &self,
        auth_client: &mut AuthClient,
        peer_id: &UserId,
        content: crate::proto::content_v1::Content,
        grpc_pool: Option<&Arc<GrpcClientPool>>,
        attachment_ids: Option<Vec<AttachmentId>>,
        channel_id: Option<ChannelId>,
        conversation: Option<&crate::mailbox::dm::DmConversation>,
    ) -> Result<(), MessageError> {
        // Check if this is an ephemeral message (typing indicator, receipt) that should be dropped
        let is_ephemeral = matches!(
            &content.content,
            Some(crate::proto::content_v1::content::Content::Typing(_)) |
            Some(crate::proto::content_v1::content::Content::Receipt(_))
        );

        if is_ephemeral {
            tracing::debug!(
                peer_id = %peer_id,
                "Dropping ephemeral message (typing/receipt) - bootstrap not complete"
            );
            return Err(MessageError::BootstrapInitiated);
        }

        // Get our user ID for Sesame deterministic initiator selection
        let our_user_id = self.settings_repo.current_user_id()?;

        // Sesame algorithm: lower UUID initiates bootstrap, higher UUID waits
        let we_are_initiator = our_user_id.as_uuid() < peer_id.as_uuid();

        if !we_are_initiator {
            return self.handle_dm_bootstrap_responder(
                auth_client,
                peer_id,
                content,
                grpc_pool,
                attachment_ids,
                channel_id,
                conversation,
                &our_user_id,
            ).await;
        }

        // We're the lower UUID - we are the rightful initiator
        tracing::info!(
            peer_id = %peer_id,
            our_id = %our_user_id,
            "Sesame: We're lower UUID - initiating DM bootstrap"
        );

        // Need grpc_pool for bootstrap
        let grpc_pool = grpc_pool.ok_or_else(|| {
            tracing::error!(peer_id = %peer_id, "No gRPC pool for bootstrap");
            MessageError::DmError("gRPC pool required for DM bootstrap".into())
        })?;

        let dm_bootstrap = DmBootstrap::new(self.database.clone());
        let mut mailbox_client =
            grpc_pool.mailbox_client();
        let mut anon_mailbox_client =
            grpc_pool.anonymous_mailbox_client();

        match dm_bootstrap.initiate_dm(*peer_id, &mut mailbox_client, &mut anon_mailbox_client).await {
            Ok(bootstrap_result) => {
                self.send_bootstrap_message(
                    auth_client,
                    peer_id,
                    &bootstrap_result.bootstrap_message,
                    grpc_pool,
                ).await?;

                // Queue the original message to be sent once bootstrap completes
                let content_bytes = content.encode_to_vec();
                if let Err(e) = self.sesame_manager.queue_pending_message(
                    peer_id,
                    content_bytes,
                    attachment_ids.unwrap_or_default(),
                    channel_id,
                ) {
                    tracing::warn!(
                        peer_id = %peer_id,
                        error = %e,
                        "Failed to queue message for post-bootstrap delivery"
                    );
                } else {
                    tracing::info!(
                        peer_id = %peer_id,
                        "Message queued for delivery after bootstrap completes"
                    );
                }

                Err(MessageError::BootstrapInitiated)
            }
            Err(crate::error::MailboxError::ConversationExists) => {
                // Conversation exists but not ready - waiting for key exchange
                let content_bytes = content.encode_to_vec();
                let _ = self.sesame_manager.queue_pending_message(
                    peer_id,
                    content_bytes,
                    attachment_ids.unwrap_or_default(),
                    channel_id,
                );
                tracing::info!(peer_id = %peer_id, "Message queued - waiting for key exchange");
                Err(MessageError::BootstrapInitiated)
            }
            Err(e) => {
                tracing::error!(peer_id = %peer_id, error = %e, "Failed to initiate DM bootstrap");
                Err(MessageError::DmError(format!("Bootstrap failed: {}", e)))
            }
        }
    }

    /// Handle DM bootstrap as the responder (higher UUID).
    #[allow(clippy::too_many_arguments)]
    async fn handle_dm_bootstrap_responder(
        &self,
        auth_client: &mut AuthClient,
        peer_id: &UserId,
        content: crate::proto::content_v1::Content,
        grpc_pool: Option<&Arc<GrpcClientPool>>,
        attachment_ids: Option<Vec<AttachmentId>>,
        channel_id: Option<ChannelId>,
        conversation: Option<&crate::mailbox::dm::DmConversation>,
        our_user_id: &UserId,
    ) -> Result<(), MessageError> {
        // Acquire bootstrap lock EARLY to prevent concurrent REQUEST sends
        if !try_acquire_bootstrap_lock(peer_id) {
            tracing::debug!(
                peer_id = %peer_id,
                "BOOTSTRAP[1/4] Skipping - bootstrap already in progress for this peer"
            );
            // Still queue the message even if we skip the REQUEST
            let content_bytes = content.encode_to_vec();
            let _ = self.sesame_manager.queue_pending_message(
                peer_id,
                content_bytes,
                attachment_ids.unwrap_or_default(),
                channel_id,
            );
            return Err(MessageError::BootstrapInitiated);
        }

        // Queue the message so it will be sent once bootstrap completes
        let content_bytes = content.encode_to_vec();
        if let Err(e) = self.sesame_manager.queue_pending_message(
            peer_id,
            content_bytes,
            attachment_ids.unwrap_or_default(),
            channel_id,
        ) {
            tracing::warn!(
                peer_id = %peer_id,
                error = %e,
                "Sesame: Failed to queue message while waiting for peer bootstrap"
            );
        } else {
            tracing::info!(
                peer_id = %peer_id,
                "Sesame: Message queued - will send after peer initiates bootstrap"
            );
        }

        // Check if we already received their bootstrap (conversation state might be pending)
        let already_pending = conversation
            .map(|c| !c.needs_bootstrap())
            .unwrap_or(false);

        if already_pending {
            tracing::info!(
                peer_id = %peer_id,
                our_id = %our_user_id,
                "Sesame: Bootstrap pending from peer (we're higher UUID) - message queued"
            );
            release_bootstrap_lock(peer_id);
            return Err(MessageError::BootstrapInitiated);
        }

        // Not pending - send a bootstrap REQUEST to prompt peer (lower UUID) to initiate
        if let Some(grpc_pool) = grpc_pool {
            self.send_bootstrap_request(auth_client, peer_id, our_user_id, grpc_pool).await;
            // DON'T release lock here - keep it held until peer's bootstrap MESSAGE arrives
        } else {
            tracing::info!(
                peer_id = %peer_id,
                our_id = %our_user_id,
                "Sesame: No gRPC pool for bootstrap request - message queued, waiting for peer"
            );
            release_bootstrap_lock(peer_id);
        }

        Err(MessageError::BootstrapInitiated)
    }

    /// Send a bootstrap request (empty KeyExchange) to prompt the peer to initiate.
    async fn send_bootstrap_request(
        &self,
        auth_client: &mut AuthClient,
        peer_id: &UserId,
        our_user_id: &UserId,
        grpc_pool: &Arc<GrpcClientPool>,
    ) {
        tracing::info!(
            peer_id = %peer_id,
            our_id = %our_user_id,
            "BOOTSTRAP[1/4] Higher UUID sending REQUEST to lower UUID peer"
        );

        // Build bootstrap request: KeyExchange with empty ephemeral_pk
        let bootstrap_request = crate::proto::content_v1::KeyExchange {
            ephemeral_pk: vec![], // 0 bytes = bootstrap REQUEST
            conversation_id: our_user_id.to_string(),
            timestamp: TimestampNanos::now().as_u64(),
        };

        let request_content = crate::proto::content_v1::Content {
            content: Some(crate::proto::content_v1::content::Content::KeyExchange(bootstrap_request)),
            ephemeral_reply_to: None,
            timestamp: TimestampNanos::now().as_u64(),
            padding: vec![],
        };

        let request_bytes = request_content.encode_to_vec();

        // Encrypt and send to peer's device inbox
        let peer_device_id = 1u32;
        let peer_id_str = peer_id.to_string();

        match self.signal_service.encrypt_message(
            auth_client,
            &peer_id_str,
            peer_device_id,
            &request_bytes,
        ).await {
            Ok(sealed_ciphertext) => {
                // Get anonymous credential for the request
                let token_manager = self.token_manager.clone();
                let token = self.get_or_request_token(&token_manager, peer_id, grpc_pool).await;

                if let Some(token) = token {
                    let mut message_client = MessageClient::new(
                        grpc_pool.channel(),
                        grpc_pool.auth_clone(),
                    );

                    let sealed_msg = SealedSenderMessage {
                        sealed_body: sealed_ciphertext,
                        server_key_id: self.signal_service.get_server_key_id(),
                        timestamp: TimestampNanos::now().as_u64(),
                    };

                    if let Err(e) = message_client.send_sealed_message_with_credential(
                        &peer_id_str,
                        peer_device_id,
                        sealed_msg,
                        &token.public_key,
                        &token.signature,
                        token.randomizer.as_deref(),
                    ).await {
                        tracing::error!(
                            peer_id = %peer_id,
                            error = %e,
                            "BOOTSTRAP[1/4] FAILED to send REQUEST"
                        );
                    } else {
                        let _ = token_manager.mark_token_used(&token.id);
                        tracing::info!(
                            peer_id = %peer_id,
                            "BOOTSTRAP[1/4] SUCCESS - REQUEST sent to peer's device inbox"
                        );
                    }
                } else {
                    tracing::error!(
                        peer_id = %peer_id,
                        "BOOTSTRAP[1/4] FAILED - No anonymous tokens available"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    peer_id = %peer_id,
                    error = %e,
                    "Failed to encrypt bootstrap request"
                );
            }
        }
    }

    /// Send the bootstrap message to the peer's device inbox.
    pub(super) async fn send_bootstrap_message(
        &self,
        auth_client: &mut AuthClient,
        peer_id: &UserId,
        bootstrap_message: &[u8],
        grpc_pool: &Arc<GrpcClientPool>,
    ) -> Result<(), MessageError> {
        let peer_device_id = 1u32;
        let peer_id_str = peer_id.to_string();

        let sealed_ciphertext = self
            .signal_service
            .encrypt_message(auth_client, &peer_id_str, peer_device_id, bootstrap_message)
            .await
            .map_err(|e| MessageError::EncryptionFailed(format!("Bootstrap encrypt: {}", e)))?;

        // Get anonymous credential for initial contact
        let token_manager = self.token_manager.clone();

        let credential_token = match token_manager.get_available_token() {
            Ok(Some(token)) => token,
            Ok(None) => {
                // No tokens available - request new ones from server
                tracing::info!(peer_id = %peer_id, "No tokens available, requesting from server");
                let mut mailbox_client = grpc_pool.mailbox_client();
                if let Err(e) = token_manager.request_tokens(&mut mailbox_client, 5).await {
                    tracing::error!(peer_id = %peer_id, error = %e, "Failed to request anonymous tokens");
                    return Err(MessageError::DmError(format!("Failed to get anonymous token: {}", e)));
                }

                match token_manager.get_available_token() {
                    Ok(Some(t)) => t,
                    Ok(None) => return Err(MessageError::DmError("No tokens available after request".into())),
                    Err(e) => return Err(MessageError::DmError(format!("Failed to get token: {}", e))),
                }
            }
            Err(e) => return Err(MessageError::DmError(format!("Failed to get anonymous token: {}", e))),
        };

        // Send to peer's device inbox via MessageClient
        let mut message_client = grpc_pool.message_client();

        let sealed_msg = SealedSenderMessage {
            sealed_body: sealed_ciphertext,
            server_key_id: self.signal_service.get_server_key_id(),
            timestamp: TimestampNanos::now().as_u64(),
        };

        message_client
            .send_sealed_message_with_credential(
                &peer_id_str,
                peer_device_id,
                sealed_msg,
                &credential_token.public_key,
                &credential_token.signature,
                credential_token.randomizer.as_deref(),
            )
            .await
            .map_err(|e| MessageError::DmError(format!("Failed to send bootstrap: {}", e)))?;

        let _ = token_manager.mark_token_used(&credential_token.id);
        tracing::info!(
            peer_id = %peer_id,
            "DM bootstrap sent successfully - waiting for key exchange response"
        );

        Ok(())
    }

    /// Get an available token or request new ones from the server.
    pub(super) async fn get_or_request_token(
        &self,
        token_manager: &TokenManager,
        peer_id: &UserId,
        grpc_pool: &Arc<GrpcClientPool>,
    ) -> Option<crate::mailbox::MailboxToken> {
        match token_manager.get_available_token() {
            Ok(Some(t)) => Some(t),
            Ok(None) => {
                tracing::info!(peer_id = %peer_id, "No tokens for bootstrap request, requesting from server");
                let mut mailbox_client = MailboxClient::new(
                    grpc_pool.channel(),
                    grpc_pool.auth_clone(),
                );
                match token_manager.request_tokens(&mut mailbox_client, 5).await {
                    Ok(count) if count > 0 => token_manager.get_available_token().ok().flatten(),
                    _ => None,
                }
            }
            Err(e) => {
                tracing::warn!(peer_id = %peer_id, error = %e, "Failed to check tokens");
                None
            }
        }
    }
}
