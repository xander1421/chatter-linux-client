//! Channel bootstrap service for handling pending message flush.
//!
//! This service handles the business-critical synchronization logic for flushing
//! pending messages to new group members when they join. This was previously
//! in the UI layer but belongs in the service layer for resilience.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::broadcast;

use crate::domain::{ChannelId, UserId};
use crate::inbox::DomainEvent;
use crate::mailbox::group::{GroupFanout, GroupMailboxManager};
use crate::network::grpc::auth_client::AuthClient;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::proto::mailbox_v1::EnvelopeType;
use crate::services::signal::SignalService;

/// Channel bootstrap service for handling pending message flush.
///
/// This service subscribes to GroupMailboxReceived events and automatically
/// flushes any pending messages to new members, ensuring reliable delivery
/// even if the UI is closed or the app crashes.
pub struct ChannelBootstrapService {
    grpc_pool: Arc<GrpcClientPool>,
    signal_service: Arc<SignalService>,
    group_fanout: Arc<std::sync::OnceLock<Arc<GroupFanout>>>,
    group_mailbox_manager: Arc<GroupMailboxManager>,
}

impl ChannelBootstrapService {
    /// Create a new channel bootstrap service.
    pub fn new(
        grpc_pool: Arc<GrpcClientPool>,
        signal_service: Arc<SignalService>,
        group_fanout: Arc<std::sync::OnceLock<Arc<GroupFanout>>>,
        group_mailbox_manager: Arc<GroupMailboxManager>,
    ) -> Self {
        Self {
            grpc_pool,
            signal_service,
            group_fanout,
            group_mailbox_manager,
        }
    }

    /// Start the bootstrap service.
    ///
    /// This spawns a background task that listens for GroupMailboxReceived events
    /// and flushes pending messages to new members.
    pub fn start(&self, mut event_rx: broadcast::Receiver<DomainEvent>) -> tokio::task::JoinHandle<()> {
        let grpc_pool = self.grpc_pool.clone();
        let signal_service = self.signal_service.clone();
        let group_fanout = self.group_fanout.clone();
        let group_mailbox_manager = self.group_mailbox_manager.clone();

        tokio::spawn(async move {
            while let Ok(event) = event_rx.recv().await {
                if let DomainEvent::GroupMailboxReceived {
                    sender_id,
                    channel_id,
                    ..
                } = event
                {
                    // Get the group fanout if available
                    let fanout = match group_fanout.get() {
                        Some(f) => f.clone(),
                        None => {
                            tracing::debug!(
                                channel_id = %channel_id,
                                "GroupFanout not initialized yet - skipping pending message flush"
                            );
                            continue;
                        }
                    };

                    // Check for pending messages using nil member (broadcast queue)
                    let nil_member = UserId::from_uuid(uuid::Uuid::nil());
                    let pending_messages = match group_mailbox_manager.drain_pending_messages(
                        &channel_id,
                        &nil_member,
                    ) {
                        Ok(msgs) => msgs,
                        Err(e) => {
                            tracing::warn!(
                                channel_id = %channel_id,
                                error = %e,
                                "Failed to drain pending channel messages"
                            );
                            continue;
                        }
                    };

                    if pending_messages.is_empty() {
                        tracing::debug!(
                            channel_id = %channel_id,
                            sender_id = %sender_id,
                            "No pending messages to flush for channel"
                        );
                        continue;
                    }

                    tracing::info!(
                        channel_id = %channel_id,
                        sender_id = %sender_id,
                        count = pending_messages.len(),
                        "[BOOTSTRAP] Flushing {} pending message(s) to new member",
                        pending_messages.len()
                    );

                    // Flush each pending message in a separate task for resilience
                    let pool = grpc_pool.clone();
                    let signal = signal_service.clone();
                    let fanout_clone = fanout.clone();
                    let channel_id_clone = channel_id;
                    let sender_id_clone = sender_id;

                    tokio::spawn(async move {
                        if let Err(e) = Self::flush_pending_messages(
                            &pool,
                            &signal,
                            &fanout_clone,
                            &channel_id_clone,
                            &sender_id_clone,
                            pending_messages,
                        )
                        .await
                        {
                            tracing::error!(
                                channel_id = %channel_id_clone,
                                error = %e,
                                "[BOOTSTRAP] Failed to flush pending messages"
                            );
                        }
                    });
                }
            }

            tracing::info!("Channel bootstrap service stopped");
        })
    }

    /// Flush pending messages to a specific member.
    async fn flush_pending_messages(
        grpc_pool: &Arc<GrpcClientPool>,
        signal_service: &Arc<SignalService>,
        group_fanout: &Arc<GroupFanout>,
        channel_id: &ChannelId,
        member_id: &UserId,
        pending_messages: Vec<(Vec<u8>, u64)>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut auth_client = AuthClient::new(
            grpc_pool.channel(),
            grpc_pool.auth_clone(),
        );

        // Get the member's mailbox from group state
        let group_state = group_fanout
            .get_group_state(channel_id)?
            .ok_or("Group state not found")?;

        // Verify member has a mailbox
        if !group_state.member_mailboxes.contains_key(member_id) {
            return Err(format!("Member mailbox not found for {}", member_id).into());
        }

        for (plaintext, timestamp) in pending_messages {
            tracing::debug!(
                channel_id = %channel_id,
                member_id = %member_id,
                plaintext_len = plaintext.len(),
                timestamp = timestamp,
                "[BOOTSTRAP] Sending pending message"
            );

            // Encrypt pairwise for the member using sealed sender
            let sealed_ciphertext = match signal_service
                .encrypt_message(
                    &mut auth_client,
                    &member_id.to_string(),
                    1, // Default to primary device
                    &plaintext,
                )
                .await
            {
                Ok(ct) => ct,
                Err(e) => {
                    tracing::warn!(
                        channel_id = %channel_id,
                        member_id = %member_id,
                        error = %e,
                        "[BOOTSTRAP] Failed to encrypt pending message"
                    );
                    continue;
                }
            };

            // Send to the member's mailbox
            let mut member_ciphertexts: HashMap<UserId, Vec<u8>> = HashMap::new();
            member_ciphertexts.insert(*member_id, sealed_ciphertext);

            match group_fanout
                .send_to_group_pairwise(channel_id, member_ciphertexts, EnvelopeType::Message)
                .await
            {
                Ok(result) => {
                    tracing::info!(
                        channel_id = %channel_id,
                        member_id = %member_id,
                        successful = result.successful,
                        failed = result.failed,
                        "[BOOTSTRAP] Pending message sent to new member"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        channel_id = %channel_id,
                        member_id = %member_id,
                        error = %e,
                        "[BOOTSTRAP] Failed to send pending message to mailbox"
                    );
                }
            }
        }

        Ok(())
    }
}