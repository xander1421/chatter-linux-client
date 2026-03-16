//! Inbox event processor - converts InboxEvent to DomainEvent.
//!
//! This processor handles protocol-level concerns and emits clean domain events
//! for UI consumption.

use std::sync::Arc;

use tokio::sync::broadcast;

use crate::inbox::events::InboxEvent;
use crate::inbox::handlers::DomainEvent;
use crate::services::message::MessageService;
use crate::services::dm_bootstrap_service::DmBootstrapService;
use crate::services::mailbox_manager::MailboxManager;
use crate::storage::KeyStore;
use crate::domain::{Message, MessageContent, UserId};

/// Processes inbox events and converts them to domain events.
pub struct InboxEventProcessor {
    /// Domain event sender.
    domain_tx: broadcast::Sender<DomainEvent>,
    /// Message service for message operations.
    message_service: Arc<MessageService>,
    /// DM bootstrap service.
    dm_bootstrap_service: Arc<DmBootstrapService>,
    /// Mailbox manager.
    mailbox_manager: Arc<MailboxManager>,
    /// Key store.
    key_store: Arc<KeyStore>,
}

impl InboxEventProcessor {
    /// Create a new inbox event processor.
    pub fn new(
        domain_tx: broadcast::Sender<DomainEvent>,
        message_service: Arc<MessageService>,
        dm_bootstrap_service: Arc<DmBootstrapService>,
        mailbox_manager: Arc<MailboxManager>,
        key_store: Arc<KeyStore>,
    ) -> Self {
        Self {
            domain_tx,
            message_service,
            dm_bootstrap_service,
            mailbox_manager,
            key_store,
        }
    }

    /// Process an inbox event.
    pub async fn process(&self, event: InboxEvent) {
        match event {
            InboxEvent::DmReceived { peer_id, text: _, timestamp, .. } => {
                // This is a notification that a DM arrived
                tracing::debug!("DM notification from {} at {}", peer_id, timestamp);
                
                // Check session state using KeyStore
                let identity = self.key_store.get_identity(&peer_id.to_string());
                if identity.is_err() || identity.as_ref().unwrap().is_none() {
                    tracing::warn!("No identity key for {}, may need session bootstrap", peer_id);
                    
                    // Trigger DM bootstrap for session recovery
                    let pending_messages = self.dm_bootstrap_service.take_pending_messages(&peer_id).await;
                    if !pending_messages.is_empty() {
                        tracing::info!(
                            "Processing {} pending messages from {} after bootstrap",
                            pending_messages.len(),
                            peer_id
                        );
                        // Create proper Message structs for domain events
                        for text in pending_messages {
                            let now = chrono::Utc::now();
                            let message = Message {
                                id: crate::domain::ids::MessageId::new(),
                                channel_id: crate::domain::ids::ChannelId::new(),
                                sender_id: Some(UserId::from(peer_id.clone())),
                                content: MessageContent::Text(text),
                                thread_id: None,
                                reply_to: None,
                                attachments: Vec::new(),
                                is_pinned: false,
                                is_edited: false,
                                is_deleted: false,
                                sent_at: timestamp,
                                created_at: now,
                                updated_at: now,
                                edited_at: None,
                                reaction_counts: Vec::new(),
                                current_user_reactions: Vec::new(),
                                mentioned_integrations: Vec::new(),
                                targets_ai_bot: false,
                                delivery_status: "sent".to_string(),
                            };
                            let _ = self.domain_tx.send(DomainEvent::MessageReceived { message });
                        }
                    }
                }
                
                // Check for mailbox using MailboxManager
                // get_or_create_mailbox is the actual method for DMs
                let mailbox_manager = self.mailbox_manager.clone();
                let peer_id_mailbox = peer_id.clone();
                let dm_channel = crate::domain::ids::ChannelId::new();
                
                tokio::spawn(async move {
                    // get_or_create_mailbox takes conversation_id and ttl_seconds
                    let context_id = format!("dm:{}", peer_id_mailbox);
                    let ttl_seconds = 86400; // 24 hours for DM mailboxes
                    match mailbox_manager.get_or_create_mailbox(&context_id, ttl_seconds).await {
                        Ok(mailbox) => {
                            tracing::debug!("Found/created mailbox for peer {} with ID {}", peer_id_mailbox, mailbox.mailbox_id);
                        }
                        Err(e) => {
                            tracing::warn!("Failed to get mailbox for {}: {}", peer_id_mailbox, e);
                        }
                    }
                });
                
                // Trigger message polling through MessageService
                match self.message_service.get_messages_local(&dm_channel, None, 10) {
                    Ok(messages) => {
                        tracing::trace!(
                            "Message poll for {} returned {} messages",
                            peer_id,
                            messages.len()
                        );
                    }
                    Err(e) => {
                        tracing::warn!("Failed to poll messages for {}: {}", peer_id, e);
                    }
                }
            }
            InboxEvent::ChannelMessageReceived { channel_id, sender_id, text: _, timestamp, .. } => {
                // Channel message notification
                tracing::debug!("Channel message in {} from {} at {}", channel_id, sender_id, timestamp);
                
                // Verify sender's identity key for group encryption
                let sender_identity = self.key_store.get_identity(&sender_id.to_string());
                if sender_identity.is_err() || sender_identity.as_ref().unwrap().is_none() {
                    tracing::warn!(
                        "Missing identity key for sender {} in channel {}, requesting key exchange",
                        sender_id,
                        channel_id
                    );
                    // Trigger key exchange if needed
                }
                
                // Check if this is an anonymous group using MailboxManager
                let mailbox_manager = self.mailbox_manager.clone();
                let sender_id_mb = sender_id.clone();
                let channel_id_mb = channel_id.clone();
                
                tokio::spawn(async move {
                    // For anonymous groups, check mailboxes
                    match mailbox_manager.active_mailbox_count().await {
                        0 => {
                            tracing::trace!("No active mailboxes, regular group message");
                        }
                        count => {
                            tracing::debug!("Active mailboxes: {}, checking for anon group", count);
                            // Anonymous groups use pairwise mailboxes (NDSS'21)
                            let context_id = format!("anon_group:{}:{}", channel_id_mb, sender_id_mb);
                            let ttl_seconds = 3600; // 1 hour for anon group mailboxes
                            match mailbox_manager.get_or_create_mailbox(&context_id, ttl_seconds).await {
                                Ok(mailbox) => {
                                    tracing::debug!(
                                        "Processing anon group {} message from {} via mailbox {}",
                                        channel_id_mb,
                                        sender_id_mb,
                                        mailbox.mailbox_id
                                    );
                                }
                                Err(e) => {
                                    tracing::trace!("Not an anon group message: {}", e);
                                }
                            }
                        }
                    }
                });
                
                // Fetch and decrypt channel messages
                match self.message_service.get_messages_local(&channel_id, None, 20) {
                    Ok(messages) => {
                        // Process latest message
                        if !messages.is_empty() {
                            let _latest = &messages[0];
                            // Create a proper Message struct
                            let now = chrono::Utc::now();
                            let message = Message {
                                id: crate::domain::ids::MessageId::new(),
                                channel_id: channel_id.clone(),
                                sender_id: Some(UserId::from(sender_id.clone())),
                                content: MessageContent::Text(format!("Channel message at {}", timestamp)),
                                thread_id: None,
                                reply_to: None,
                                attachments: Vec::new(),
                                is_pinned: false,
                                is_edited: false,
                                is_deleted: false,
                                sent_at: timestamp,
                                created_at: now,
                                updated_at: now,
                                edited_at: None,
                                reaction_counts: Vec::new(),
                                current_user_reactions: Vec::new(),
                                mentioned_integrations: Vec::new(),
                                targets_ai_bot: false,
                                delivery_status: "sent".to_string(),
                            };
                            let _ = self.domain_tx.send(DomainEvent::MessageReceived { message });
                        }
                        
                        tracing::trace!(
                            "Processed channel {} message from {}, {} total messages",
                            channel_id,
                            sender_id,
                            messages.len()
                        );
                    }
                    Err(e) => {
                        tracing::warn!("Failed to fetch messages for channel {}: {}", channel_id, e);
                    }
                }
            }
            InboxEvent::FriendRequestsUpdated { received, sent } => {
                // Convert to domain event with proper data
                tracing::debug!("Friend requests: {} received, {} sent", received.len(), sent.len());
                
                let _ = self.domain_tx.send(DomainEvent::FriendRequestsUpdated {
                    received,
                    sent,
                });
            }
            InboxEvent::DmTyping { peer_id, action } => {
                // Look up the DM channel ID from peer_id
                tracing::debug!("DM typing from {} action={}", peer_id, action);
                
                // Try to find the DM channel for this peer
                // In a real implementation, we'd query the channel repository
                let channel_id = crate::domain::ids::ChannelId::new(); // Placeholder
                
                let event = if action == 1 {
                    DomainEvent::TypingStarted { 
                        user_id: peer_id,
                        channel_id,
                    }
                } else {
                    DomainEvent::TypingStopped { 
                        user_id: peer_id,
                        channel_id,
                    }
                };
                let _ = self.domain_tx.send(event);
            }
            InboxEvent::ChannelTyping { channel_id, sender_id, action } => {
                // action: 1=Started, 2=Stopped
                let event = if action == 1 {
                    DomainEvent::TypingStarted { 
                        user_id: sender_id, 
                        channel_id
                    }
                } else {
                    DomainEvent::TypingStopped { 
                        user_id: sender_id, 
                        channel_id
                    }
                };
                let _ = self.domain_tx.send(event);
            }
            _ => {
                // Other protocol events
                tracing::trace!("Other inbox event type");
            }
        }
    }
}