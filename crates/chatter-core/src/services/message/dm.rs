//! DM (Direct Message) operations.
//!
//! Signal Protocol encryption for DM messages, including bootstrap,
//! session management, and pending message queue.

use prost::Message as ProstMessage;

use crate::crypto::encrypt_dm_bytes;
use crate::domain::ids::UserId;
use crate::mailbox::dm::{DmConversation, DmConversationManager, PeerMailbox};
use crate::network::grpc::auth_client::AuthClient;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::proto::mailbox_v1::EnvelopeType;
use crate::services::current_user::CurrentUserProvider;
use crate::services::mailbox_manager::MailboxManager;

use super::{queued_bubble_timestamp, MessageError, MessageService, PendingFlush, SendDmContentParams};

impl MessageService {
    /// Send a Content proto to a DM peer via two-layer encryption.
    ///
    /// Uses two encryption layers for privacy:
    /// 1. Signal Protocol (forward secrecy via Double Ratchet)
    /// 2. Mailbox encryption (sender anonymity via XChaCha20-Poly1305)
    ///
    /// If profile sharing is enabled for this friendship, includes our
    /// profile key in DataMessage so the recipient can decrypt our display_name.
    pub async fn send_dm_content(
        &self,
        params: SendDmContentParams<'_>,
    ) -> Result<(), MessageError> {
        let SendDmContentParams {
            auth_client,
            peer_id,
            content,
            mailbox_manager,
            grpc_pool,
            attachment_ids,
            channel_id,
        } = params;

        tracing::info!(
            peer_id = %peer_id,
            has_grpc_pool = grpc_pool.is_some(),
            "MessageService: send_dm_content called"
        );

        // Inject profile key if profile sharing is enabled for this peer
        let content = self.inject_profile_key_if_sharing(peer_id, content)?;

        // Load conversation to get peer's mailbox and shared secret
        tracing::debug!(peer_id = %peer_id, "MessageService: loading conversation state");
        let conversation_manager = DmConversationManager::new(self.database.clone());
        let conversation = conversation_manager.load_conversation(peer_id).map_err(|e| {
            tracing::error!(peer_id = %peer_id, error = %e, "MessageService: failed to load conversation");
            MessageError::DmError(format!("Failed to load conversation: {}", e))
        })?;

        tracing::debug!(
            peer_id = %peer_id,
            conversation_exists = conversation.is_some(),
            has_our_mailbox = conversation.as_ref().map(|c| c.our_mailbox.is_some()).unwrap_or(false),
            peer_devices = conversation.as_ref().map(|c| c.peer_mailboxes.len()).unwrap_or(0),
            is_ready = conversation.as_ref().map(|c| c.is_ready_to_send()).unwrap_or(false),
            "MessageService: conversation state loaded"
        );

        // Check if conversation exists and is ready
        let needs_bootstrap = match &conversation {
            None => true,
            Some(conv) => !conv.is_ready_to_send(),
        };

        if needs_bootstrap {
            // DIAGNOSTIC: a ServerDm (regular friend) may have no ready mailbox
            // conversation, so any mailbox-transport send (incl. call signaling)
            // bootstraps instead of delivering. Surfaces the transport mismatch.
            tracing::info!(
                peer_id = %peer_id,
                conversation_exists = conversation.is_some(),
                "[DM_TRANSPORT] mailbox send needs bootstrap — content queued, not delivered now"
            );
            return self.handle_dm_bootstrap(
                auth_client,
                peer_id,
                content,
                grpc_pool,
                attachment_ids,
                channel_id,
            ).await;
        }

        // Conversation is ready - proceed with sending
        let conversation = conversation.unwrap();
        self.snapshot_dm_content_targets(*peer_id, &content,
            &conversation.ready_devices().into_iter().map(|(mailbox, _)| mailbox.device_id).collect::<Vec<_>>())?;
        let plaintext = content.encode_to_vec();

        // Every ready peer device gets its own copy: sealed for that device
        // only (Layer 1, Signal), wrapped under that device's secret (Layer 2)
        // and deposited in that device's mailbox, the one only it polls.
        // Delivered once any device has it, as Signal counts a send.
        //
        // Never a prekey-bundle fetch: in Shield that identified RPC names the
        // recipient at the moment we message them — the link the mailbox
        // exists to hide. The sessions exist (each bootstrap made one).
        let mut delivered = 0usize;
        let mut last_error = None;
        for (mailbox, send_key) in conversation.ready_devices() {
            match self
                .send_to_peer_device(auth_client, mailbox_manager, peer_id, mailbox, &send_key, &plaintext)
                .await
            {
                Ok(()) => delivered += 1,
                Err(e) => {
                    tracing::warn!(peer_id = %peer_id, device_id = mailbox.device_id, error = %e, "Mailbox DM to a peer device failed");
                    last_error = Some(e);
                }
            }
        }
        if delivered == 0 {
            return Err(last_error.unwrap_or_else(|| MessageError::DmError("No ready peer device".into())));
        }
        tracing::debug!(peer_id = %peer_id, delivered, "Sent DM to the peer's ready devices");

        if !is_ephemeral_content(&content) {
            self.owe_unready_devices(auth_client, peer_id, &conversation, &plaintext, grpc_pool)
                .await;
        }
        self.sync_sent_to_own_devices(auth_client, peer_id, &content).await;
        Ok(())
    }

    /// Copy `content`, just delivered to `peer_id`, to our own other devices
    /// if [`own_device_copy`] gives it one. Every DM delivery — Shield, normal
    /// mode, the responder's post-bootstrap flush — ends here: one policy.
    pub(crate) async fn sync_sent_to_own_devices(
        &self,
        auth_client: &mut AuthClient,
        peer_id: &UserId,
        content: &crate::proto::content_v1::Content,
    ) -> usize {
        match own_device_copy(peer_id, content, self.current_device_id_or_zero()) {
            Some(copy) => self.sync_to_own_devices(auth_client, copy).await,
            None => 0,
        }
    }

    /// Send `sync` to our own devices other than this one — every own-device
    /// copy (`SyncSent`, `ReadSync`, the call ACCEPTED/DECLINED_ELSEWHERE
    /// hangup) goes through here. Returns how many devices it reached.
    ///
    /// Identified (Basic auth) on the authenticated channel, never with our
    /// UAK on the anonymous channel: that channel carries our mailbox
    /// deposits/polls and our sealed sends, and a deposit to our OWN account
    /// on it would tie them to us. Signal sends sync transcripts identified
    /// for the same reason — the sender is no secret to ourselves.
    ///
    /// Shield: no prekey-bundle fetch, only own devices we already hold a
    /// session with (`session_devices`, local), so a linked device we have
    /// never messaged is skipped until a normal-mode send makes a session.
    /// Normal mode fetches our own bundles (identified, naming only us), which
    /// is how a newly linked device gets its first session, and names its
    /// device set so the server reports one linked since
    /// (`deposit_sealed_to_all_devices`).
    ///
    /// Best effort, like Signal's sync: failures are logged.
    pub(crate) async fn sync_to_own_devices(
        &self,
        auth_client: &mut AuthClient,
        mut sync: crate::proto::content_v1::Content,
    ) -> usize {
        use crate::storage::repositories::settings_repo::keys;

        if let Err(error) = self.populate_sync_receipt_targets(&mut sync) {
            tracing::warn!(%error, "Could not snapshot own-device receipt targets");
            return 0;
        }
        let Ok(me) = self.current_user_id() else { return 0 };
        let me = me.to_string();
        if !self.shield_mode.load(std::sync::atomic::Ordering::Acquire) {
            Self::pad_content_to_bucket(&mut sync);
            return self
                .deposit_sealed_to_all_devices(auth_client, &mut self.message_client(), &me, &sync.encode_to_vec())
                .await
                .delivered;
        }
        let this_device = self.settings_repo.get::<u32>(keys::CURRENT_DEVICE_ID).ok().flatten();
        let devices = self
            .signal_service
            .session_devices(&me)
            .await
            .map(|devices| devices.into_iter().filter(|d| Some(*d) != this_device).collect());
        let devices: Vec<u32> = match devices {
            Ok(devices) => devices,
            Err(e) => {
                tracing::warn!(error = %e, "Own-device sync: device lookup failed");
                return 0;
            }
        };
        if devices.is_empty() {
            return 0;
        }
        Self::pad_content_to_bucket(&mut sync);
        let ciphertexts = match self
            .signal_service
            .encrypt_message_session_devices(auth_client, &me, devices, &sync.encode_to_vec())
            .await
        {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "Own-device sync: encrypt failed");
                return 0;
            }
        };
        let outcome = self
            .deposit_sealed_ciphertexts(&mut self.message_client(), &me, ciphertexts, None)
            .await;
        // What that device's DecryptionErrorMessage will name
        // (`resend_dm_on_decryption_error`).
        if let Ok(me) = me.parse() {
            self.dm_sent_log.record(me, &sync, &outcome.deposits);
        }
        outcome.delivered
    }

    /// Seal `plaintext` for one peer device and deposit it in its mailbox.
    async fn send_to_peer_device(
        &self,
        auth_client: &mut AuthClient,
        mailbox_manager: &MailboxManager,
        peer_id: &UserId,
        mailbox: &PeerMailbox,
        send_key: &[u8; 32],
        plaintext: &[u8],
    ) -> Result<(), MessageError> {
        let device_id = mailbox.device_id;
        let peer_ciphertexts = self
            .signal_service
            .encrypt_message_session_devices(auth_client, &peer_id.to_string(), vec![device_id], plaintext)
            .await
            .map_err(|e| MessageError::from_signal_send(e, "Mailbox DM encrypt"))?;
        for signal_ciphertext in peer_ciphertexts.into_values() {
            let mailbox_encrypted = encrypt_dm_bytes(&signal_ciphertext, send_key)
                .map_err(|e| MessageError::EncryptionFailed(format!("Mailbox encrypt: {}", e)))?;
            self.send_to_mailbox_retried(
                mailbox_manager, &mailbox.public_key, mailbox_encrypted, peer_id, device_id,
            ).await?;
        }
        Ok(())
    }

    /// Hold `plaintext` for every live peer device that has no half of the
    /// conversation yet, flushed when it answers the bootstrap
    /// ([`Self::send_pending_messages`], `dm_bootstrap_handler`). A device
    /// is asked to answer until an ask reaches it (`was_device_asked`), and
    /// only on a live send (`grpc_pool`); the post-bootstrap flush follows a
    /// bootstrap that already asked every device. No RPC beyond that ask.
    pub(super) async fn owe_unready_devices(
        &self,
        auth_client: &mut AuthClient,
        peer_id: &UserId,
        conversation: &DmConversation,
        plaintext: &[u8],
        grpc_pool: Option<&std::sync::Arc<GrpcClientPool>>,
    ) {
        let devices = match self.live_session_devices(peer_id).await {
            Ok(devices) => devices,
            Err(e) => {
                tracing::warn!(peer_id = %peer_id, error = %e, "Owed copies: device lookup failed");
                return;
            }
        };
        let conversations = DmConversationManager::new(self.database.clone());
        for device in devices {
            if conversation.peer_mailboxes.iter().any(|m| m.device_id == device) {
                continue;
            }
            // Asked only if an ask actually reached it: one that failed
            // (empty token batch) is repeated on the next send.
            let asked = conversations.was_device_asked(peer_id, device).unwrap_or(true);
            if let Err(e) = conversations.queue_for_device(peer_id, device, plaintext) {
                tracing::warn!(peer_id = %peer_id, device, error = %e, "Owed copies: queue failed");
                continue;
            }
            if let (false, Some(grpc_pool)) = (asked, grpc_pool) {
                if let Err(e) = self.prompt_peer_device(auth_client, peer_id, device, grpc_pool).await {
                    tracing::warn!(peer_id = %peer_id, device, error = %e, "Owed copies: could not ask the device to bootstrap");
                }
            }
        }
    }

    /// Deliver what is owed to every peer device that is ready now. Failures
    /// are logged, not announced: each of these messages already reached
    /// another device, so its bubble is already "sent".
    async fn flush_owed_copies(
        &self,
        auth_client: &mut AuthClient,
        peer_id: &UserId,
        mailbox_manager: &MailboxManager,
    ) {
        let conversations = DmConversationManager::new(self.database.clone());
        let conversation = match conversations.load_conversation(peer_id) {
            Ok(Some(conversation)) => conversation,
            Ok(None) => return,
            Err(e) => {
                tracing::warn!(peer_id = %peer_id, error = %e, "Owed copies: conversation load failed");
                return;
            }
        };
        for (mailbox, send_key) in conversation.ready_devices() {
            let owed = match conversations.drain_device_queue(peer_id, mailbox.device_id) {
                Ok(owed) => owed,
                Err(e) => {
                    tracing::warn!(peer_id = %peer_id, device = mailbox.device_id, error = %e, "Owed copies: drain failed");
                    continue;
                }
            };
            for plaintext in owed {
                if let Err(e) = self
                    .send_to_peer_device(auth_client, mailbox_manager, peer_id, mailbox, &send_key, &plaintext)
                    .await
                {
                    tracing::warn!(peer_id = %peer_id, device = mailbox.device_id, error = %e, "Owed copy to a late device lost");
                }
            }
        }
    }

    /// Send a Content proto to a DM peer (simplified API - no client params).
    pub async fn send_dm(
        &self,
        peer_id: &UserId,
        content: crate::proto::content_v1::Content,
        mailbox_manager: &MailboxManager,
    ) -> Result<(), MessageError> {
        let mut auth_client = self.auth_client();

        self.send_dm_content(SendDmContentParams {
            auth_client: &mut auth_client,
            peer_id,
            content,
            mailbox_manager,
            grpc_pool: Some(&self.grpc_pool),
            attachment_ids: None,
            channel_id: None,
        })
        .await
    }

    /// Send pending messages after bootstrap completes.
    ///
    /// Every queued message is attempted, and the outcome of each is in the
    /// returned [`PendingFlush`] — the queue is already drained, so the caller
    /// must announce it (`PendingFlush::emit`) or a failure is a message lost
    /// without a word.
    pub async fn send_pending_messages(
        &self,
        auth_client: &mut AuthClient,
        peer_id: &UserId,
        mailbox_manager: &MailboxManager,
    ) -> PendingFlush {
        let mut flush = PendingFlush::default();
        let pending = match self.sesame_manager.drain_pending_messages(peer_id) {
            Ok(pending) => pending,
            Err(e) => {
                tracing::error!(peer_id = %peer_id, error = %e, "PENDING_MESSAGES: Failed to drain pending messages");
                flush.error = Some(format!("queued messages could not be read: {e}"));
                return flush;
            }
        };
        let pending_count = pending.len();
        tracing::info!(peer_id = %peer_id, count = pending_count, "PENDING_MESSAGES: draining Sesame queue");

        for msg in pending {
            let content = match crate::proto::content_v1::Content::decode(msg.content.as_slice()) {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!(peer_id = %peer_id, error = %e, "Failed to deserialize pending message content");
                    flush.record(None, Err(format!("undecodable queued message: {e}")));
                    continue;
                }
            };
            let bubble = queued_bubble_timestamp(&content);

            let result = match self
                .send_dm_content(SendDmContentParams {
                    auth_client,
                    peer_id,
                    content,
                    mailbox_manager,
                    grpc_pool: None,
                    attachment_ids: Some(msg.attachment_ids.clone()),
                    channel_id: msg.channel_id,
                })
                .await
            {
                // Re-queued behind a bootstrap still in progress: still "sending".
                Err(MessageError::BootstrapInitiated) => continue,
                Err(e) => {
                    tracing::error!(peer_id = %peer_id, error = %e, "PENDING_MESSAGES: FAILED to send message");
                    Err(e.to_string())
                }
                Ok(()) => Ok(()),
            };
            flush.record(bubble, result);
        }

        // A device that answered after another got these copies catches up.
        self.flush_owed_copies(auth_client, peer_id, mailbox_manager).await;

        tracing::info!(
            peer_id = %peer_id,
            sent = flush.sent.len(),
            failed = flush.failed.len(),
            total = pending_count,
            "PENDING_MESSAGES: Complete"
        );
        flush
    }

    /// Reset session with a peer and attempt to re-establish.
    pub async fn reset_and_establish_session(
        &self,
        auth_client: &mut AuthClient,
        peer_id: &UserId,
        device_id: u32,
    ) -> Result<(), MessageError> {
        tracing::info!(
            peer_id = %peer_id,
            device_id,
            "Resetting session with peer"
        );

        // Archive all sessions in Sesame
        if let Err(e) = self.sesame_manager.archive_all_sessions(peer_id, device_id) {
            tracing::warn!(
                peer_id = %peer_id,
                error = %e,
                "Failed to archive Sesame sessions (continuing anyway)"
            );
        }

        // Mark peer as stale
        if let Err(e) = self.sesame_manager.mark_user_stale(peer_id) {
            tracing::warn!(
                peer_id = %peer_id,
                error = %e,
                "Failed to mark peer as stale (continuing anyway)"
            );
        }

        // Establish a fresh session by fetching their prekey bundle
        self.signal_service
            .ensure_session(auth_client, &peer_id.to_string(), device_id)
            .await
            .map_err(|e| {
                tracing::error!(
                    peer_id = %peer_id,
                    error = %e,
                    "Failed to establish new session after reset"
                );
                MessageError::from_signal_send(e, "Session establishment failed")
            })?;

        tracing::info!(
            peer_id = %peer_id,
            device_id,
            "Successfully reset and established new session"
        );

        Ok(())
    }

    /// Reset sessions with ALL devices of a peer and re-establish.
    ///
    /// Used for convergence failure recovery in multi-device scenarios.
    /// Archives existing sessions and establishes fresh ones for all known devices.
    pub async fn reset_and_establish_sessions_all_devices(
        &self,
        auth_client: &mut AuthClient,
        peer_id: &UserId,
    ) -> Result<Vec<u32>, MessageError> {
        tracing::info!(
            peer_id = %peer_id,
            "Resetting sessions for all peer devices"
        );

        // Mark peer as stale (affects all devices)
        if let Err(e) = self.sesame_manager.mark_user_stale(peer_id) {
            tracing::warn!(
                peer_id = %peer_id,
                error = %e,
                "Failed to mark peer as stale (continuing anyway)"
            );
        }

        // Establish fresh sessions for ALL devices
        self.signal_service.forget_device_list(&peer_id.to_string());
        let device_ids = self.signal_service
            .ensure_sessions_all_devices(auth_client, &peer_id.to_string())
            .await
            .map_err(|e| {
                tracing::error!(
                    peer_id = %peer_id,
                    error = %e,
                    "Failed to establish sessions for peer devices"
                );
                MessageError::from_signal_send(e, "Multi-device session establishment failed")
            })?;

        tracing::info!(
            peer_id = %peer_id,
            device_count = device_ids.len(),
            "Reset sessions for all peer devices"
        );

        Ok(device_ids)
    }

    /// Send to a mailbox with retry for transient network failures.
    ///
    /// Retries up to 3 times with exponential backoff (500ms, 1s, 2s)
    /// for retryable errors (timeout, unavailable, rate limit, server error).
    /// Non-retryable errors (auth, not found, invalid request) fail immediately.
    async fn send_to_mailbox_retried(
        &self,
        mailbox_manager: &MailboxManager,
        public_key: &[u8],
        encrypted_envelope: Vec<u8>,
        peer_id: &UserId,
        device_id: u32,
    ) -> Result<(), MessageError> {
        use crate::services::mailbox_manager::MailboxError;

        const MAX_ATTEMPTS: u32 = 4; // 1 initial + 3 retries

        for attempt in 0..MAX_ATTEMPTS {
            if attempt > 0 {
                let delay = std::time::Duration::from_millis(500 * 2u64.pow(attempt - 1));
                tokio::time::sleep(delay).await;
            }

            match mailbox_manager
                .send_to_mailbox(
                    public_key,
                    encrypted_envelope.clone(),
                    EnvelopeType::SealedSender,
                )
                .await
            {
                Ok(()) => return Ok(()),
                Err(MailboxError::Network(net_err))
                    if net_err.is_retryable() && attempt + 1 < MAX_ATTEMPTS =>
                {
                    tracing::warn!(
                        peer_id = %peer_id,
                        device_id,
                        attempt = attempt + 1,
                        max_attempts = MAX_ATTEMPTS,
                        error = %net_err,
                        "Mailbox send failed, retrying"
                    );
                }
                Err(e) => {
                    return Err(MessageError::DmError(format!(
                        "Failed to send to mailbox: {}",
                        e
                    )));
                }
            }
        }

        unreachable!("retry loop always returns")
    }

    /// Inject our profile key into the Content if profile sharing is enabled.
    ///
    /// If this is a DM to a friend, auto-enables profile sharing on first message
    /// and injects our profile key into any DataMessage or EditMessage
    /// so the recipient can decrypt our display_name.
    pub(crate) fn inject_profile_key_if_sharing(
        &self,
        peer_id: &UserId,
        mut content: crate::proto::content_v1::Content,
    ) -> Result<crate::proto::content_v1::Content, MessageError> {
        use crate::domain::user::FriendshipStatus;
        use crate::proto::content_v1::content;

        // Get current user ID
        let our_user_id = self.settings_repo.current_user_id()?;

        // Check if this is a friendship
        let friendship = self
            .user_repo
            .get_friendship(&our_user_id, peer_id)
            .ok()
            .flatten();

        // Only share profile key with accepted friends
        let is_friend = friendship
            .as_ref()
            .map(|f| matches!(f.status, FriendshipStatus::Accepted))
            .unwrap_or(false);

        if !is_friend {
            tracing::trace!(
                peer_id = %peer_id,
                "Not a friend, skipping profile key injection"
            );
            return Ok(content);
        }

        // Check if profile sharing is enabled for this friendship
        let is_sharing = self
            .user_repo
            .is_profile_sharing(&our_user_id, peer_id)
            .unwrap_or(false);

        // Auto-enable profile sharing on first message to a friend
        if !is_sharing {
            if let Err(e) = self.user_repo.set_profile_sharing(&our_user_id, peer_id, true) {
                tracing::warn!(
                    peer_id = %peer_id,
                    error = %e,
                    "Failed to auto-enable profile sharing"
                );
                return Ok(content);
            }
            tracing::info!(
                peer_id = %peer_id,
                "Auto-enabled profile sharing for friendship"
            );
        }

        // Whether this friend may hold it is the visibility setting's call
        // ("nobody", a block, a rotation in flight).
        let Some(profile_key) = crate::services::profile_visibility::shareable_profile_key(
            &self.settings_repo,
            &self.user_repo,
            peer_id,
        ) else {
            tracing::trace!(peer_id = %peer_id, "Profile key not shareable, skipping injection");
            return Ok(content);
        };
        let profile_key = profile_key.to_vec();

        // Inject profile key into DataMessage or EditMessage
        match &mut content.content {
            Some(content::Content::DataMessage(dm)) => {
                dm.profile_key = Some(profile_key);
                tracing::debug!(
                    peer_id = %peer_id,
                    "Injected profile key into DataMessage"
                );
            }
            Some(content::Content::Edit(edit)) => {
                if let Some(dm) = edit.data_message.as_mut() {
                    dm.profile_key = Some(profile_key);
                    tracing::debug!(
                        peer_id = %peer_id,
                        "Injected profile key into EditMessage"
                    );
                }
            }
            Some(content::Content::Attachment(_)) => {
                // AttachmentMessage doesn't have profile_key field,
                // but the attachment is usually accompanied by a DataMessage
                tracing::trace!(
                    peer_id = %peer_id,
                    "AttachmentMessage - profile key not applicable"
                );
            }
            _ => {
                // Other message types (typing, receipts, reactions, etc.)
                // don't need profile key
                tracing::trace!(
                    peer_id = %peer_id,
                    "Non-data message type, profile key not applicable"
                );
            }
        }

        Ok(content)
    }
}

/// What our own other devices get for `content` sent to `peer_id` over
/// either DM transport, or `None`: the `SyncSent` for a message
/// ([`sync_sent_copy`]), the ACCEPTED/DECLINED_ELSEWHERE hangup when this
/// device answers or declines a call, so our other devices stop ringing, or
/// the bare copy of a message-anchored control ([`bare_self_copy`]).
pub(super) fn own_device_copy(
    peer_id: &UserId,
    content: &crate::proto::content_v1::Content,
    our_device_id: u32,
) -> Option<crate::proto::content_v1::Content> {
    sync_sent_copy(peer_id, content)
        .or_else(|| crate::services::gateway::build_call_self_sync(content, our_device_id))
        .or_else(|| bare_self_copy(content))
}

/// The bare copy our own other devices get of a reaction, pin, edit or
/// delete-for-everyone: it names its target by (author, sent timestamp), so
/// the linked device applies it to the message wherever it is filed
/// (`dispatch_control_content` → `inbox_processor`), no destination needed.
///
/// An allow-list, never "everything else": mailbox-internal and anon content
/// (bootstrap `KeyExchange`, `DmMailboxRotation`, `AnonGroup*`/`AnonDm*`,
/// `SenderKey*`, `GroupMailboxAddress`) would hand our devices state that is
/// this conversation's, not theirs; typing and receipts are not synced
/// (Signal does not); `ReadSync`/`SyncSent` already are own-device content.
fn bare_self_copy(content: &crate::proto::content_v1::Content) -> Option<crate::proto::content_v1::Content> {
    use crate::proto::content_v1::{content, Content};

    let inner = match &content.content {
        Some(content::Content::Reaction(_) | content::Content::Pin(_) | content::Content::Edit(_)) => {
            content.content.clone()
        }
        Some(content::Content::DataMessage(dm)) if dm.delete.is_some() => content.content.clone(),
        _ => return None,
    };
    Some(Content { timestamp: content.timestamp, content: inner, ..Default::default() })
}

/// The `SyncSent` our own other devices get for `content` sent to `peer_id`,
/// or `None` when it is not a message they should file (F8, V13): a
/// text/attachment `DataMessage` or a timer update, never a delete carrier.
pub(super) fn sync_sent_copy(
    peer_id: &UserId,
    content: &crate::proto::content_v1::Content,
) -> Option<crate::proto::content_v1::Content> {
    use crate::proto::content_v1::{content, Content, SyncSent};

    let Some(content::Content::DataMessage(dm)) = &content.content else { return None };
    let is_message = !dm.body.is_empty()
        || !dm.attachments.is_empty()
        || crate::services::disappearing::is_timer_update(dm);
    (is_message && dm.delete.is_none()).then(|| Content {
        timestamp: content.timestamp,
        content: Some(content::Content::SyncSent(SyncSent {
            destination_user_id: peer_id.to_string(),
            timestamp: dm.timestamp,
            message: Some(dm.clone()),
            receipt_targets: Vec::new(),
        })),
        ..Default::default()
    })
}

/// Typing indicators and receipts: worth nothing late, so never queued.
pub(super) fn is_ephemeral_content(content: &crate::proto::content_v1::Content) -> bool {
    matches!(
        &content.content,
        Some(crate::proto::content_v1::content::Content::Typing(_))
            | Some(crate::proto::content_v1::content::Content::Receipt(_))
    )
}

/// Reply metadata assembled from a quoted message, feeding both the outgoing
/// wire `Quote` and the local echo's stored reply fields. Shared across DM
/// transports so reply assembly lives in one place.
pub(crate) struct OutboundReplyParts {
    pub quote_attachments: Vec<crate::proto::content_v1::QuotedAttachment>,
    pub reply_to: crate::domain::ids::MessageId,
    pub reply_to_text: Option<String>,
    pub reply_to_thumbnail: Option<Vec<u8>>,
    pub reply_to_thumbnail_mime: Option<String>,
}

impl MessageService {
    /// Resolve a quoted message into wire + local reply metadata from a single
    /// source of truth. Used by every DM reply send (mailbox + ServerDm) so the
    /// quote-assembly logic isn't duplicated per transport.
    pub(crate) fn assemble_reply_parts(
        &self,
        quote_author: &crate::domain::ids::UserId,
        quote_timestamp: u64,
        quote_preview: &str,
    ) -> OutboundReplyParts {
        let original = self
            .message_repo
            .get_by_sender_timestamp(quote_author, quote_timestamp)
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
        let quote_attachments =
            crate::services::gateway::shared::build_quoted_attachments(&original_attachments);
        let (reply_to_thumbnail, reply_to_thumbnail_mime) = quote_attachments
            .first()
            .map(|qa| {
                let thumb = (!qa.thumbnail.is_empty()).then(|| qa.thumbnail.clone());
                let mime = (!qa.content_type.is_empty()).then(|| qa.content_type.clone());
                (thumb, mime)
            })
            .unwrap_or((None, None));
        let reply_to = original
            .as_ref()
            .map(|m| m.id)
            .unwrap_or_else(|| crate::domain::ids::MessageId::from_timestamp(quote_timestamp));
        let reply_to_text = if !quote_preview.is_empty() {
            Some(crate::services::gateway::shared::truncate_preview(quote_preview))
        } else {
            original.as_ref().map(|m| {
                crate::services::gateway::shared::message_reply_preview(m, &original_attachments)
            })
        }
        .filter(|t| !t.is_empty());

        OutboundReplyParts {
            quote_attachments,
            reply_to,
            reply_to_text,
            reply_to_thumbnail,
            reply_to_thumbnail_mime,
        }
    }
}

#[cfg(test)]
mod sync_sent_copy_tests {
    use crate::domain::ids::UserId;
    use crate::proto::content_v1::{content, Content, DataMessage};

    #[test]
    fn wraps_messages_only_and_names_the_peer() {
        let peer = UserId::new();
        let copy = super::sync_sent_copy(&peer, &crate::services::build_text_content("hi", 7)).unwrap();
        let Some(content::Content::SyncSent(sync)) = copy.content else { panic!("not a SyncSent") };
        assert_eq!(sync.destination_user_id, peer.to_string());
        assert_eq!(sync.message.unwrap().body, "hi");

        let empty = Content {
            content: Some(content::Content::DataMessage(DataMessage::default())),
            ..Default::default()
        };
        assert!(super::sync_sent_copy(&peer, &empty).is_none());
        assert!(super::sync_sent_copy(&peer, &Content::default()).is_none());
    }

    /// F62: reactions, pins, edits and deletes-for-everyone reach our own
    /// devices bare; nothing mailbox-internal, anon or ephemeral does.
    #[test]
    fn bare_copies_only_the_allow_list() {
        use crate::proto::content_v1::*;
        let peer = UserId::new();
        let wrap = |c| Content { timestamp: 9, content: Some(c), ephemeral_reply_to: Some(vec![1; 32]), ..Default::default() };
        let delete = DataMessage { delete: Some(Delete { target_sent_timestamp: 3 }), ..Default::default() };

        for allowed in [
            content::Content::Reaction(Reaction::default()),
            content::Content::Pin(PinMessage::default()),
            content::Content::Edit(EditMessage::default()),
            content::Content::DataMessage(delete),
        ] {
            let copy = super::own_device_copy(&peer, &wrap(allowed.clone()), 1).expect("allowed variant not copied");
            assert_eq!(copy.content, Some(allowed));
            assert_eq!(copy.timestamp, 9);
            assert!(copy.ephemeral_reply_to.is_none(), "our mailbox address must not ride the copy");
        }

        for internal in [
            content::Content::KeyExchange(KeyExchange::default()),
            content::Content::DmMailboxRotation(DmMailboxRotation::default()),
            content::Content::AnonGroupInvite(AnonGroupInvite::default()),
            content::Content::AnonDmRequest(AnonDmRequest::default()),
            content::Content::AnonDmMessage(Box::default()),
            content::Content::SenderKey(SenderKeyDistribution::default()),
            content::Content::SenderKeyRequest(SenderKeyRequest::default()),
            content::Content::GroupMailboxAddress(GroupMailboxAddress::default()),
            content::Content::Typing(TypingMessage::default()),
            content::Content::Receipt(ReceiptMessage::default()),
            content::Content::ReadSync(ReadSync::default()),
            content::Content::SyncSent(SyncSent::default()),
            content::Content::DeleteForMeSync(DeleteForMeSync::default()),
            content::Content::DataMessage(DataMessage::default()),
        ] {
            assert!(super::own_device_copy(&peer, &wrap(internal.clone()), 1).is_none(), "{internal:?} copied");
        }
    }
}
