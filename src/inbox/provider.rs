//! Device inbox provider.
//!
//! Main coordinator for device inbox operations including configuration,
//! event broadcasting, and lifecycle management.

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use tokio::sync::{broadcast, watch};

use crate::domain::ids::{ChannelId, MessageId};
use crate::domain::message::Message;
use crate::domain::UserId;
use crate::mailbox::dm::{DmConversationManager, TwoWayAnonymity};
use crate::mailbox::group::{GroupCoordinator, GroupFanout, GroupMailboxManager};
use crate::network::grpc::{MailboxClient, MessageClient};
use crate::network::validation::MessageValidationPipeline;
use crate::proto::chat_v1::{DeviceMessage, EncryptedData, SealedSenderMessage};
use crate::proto::content_v1::{Content, DataMessage, content};
use crate::proto::mailbox_v1::EnvelopeType;
use crate::services::message_gateway::{DmContent, MessageGateway, RouteResult};
use prost::Message as ProstMessage;
use crate::error::ServiceError;
use crate::storage::{ChannelRepository, Database, MessageRepository, SettingsRepository};
use crate::storage::repositories::settings_repo::keys;

use super::poller::InboxPoller;

// ============================================================================
// Configuration
// ============================================================================

/// Configuration for the inbox polling system.
#[derive(Debug, Clone)]
pub struct InboxConfig {
    /// Base polling interval when active.
    pub poll_interval: Duration,
    /// Polling interval when idle (no recent messages).
    pub poll_interval_idle: Duration,
    /// Maximum messages to fetch per poll request.
    pub max_batch_size: u32,
    /// Number of retry attempts for failed messages.
    pub retry_attempts: u32,
    /// Number of messages to acknowledge per request.
    pub ack_batch_size: u32,
}

impl Default for InboxConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(5),
            poll_interval_idle: Duration::from_secs(30),
            max_batch_size: 100,
            retry_attempts: 3,
            ack_batch_size: 50,
        }
    }
}

impl InboxConfig {
    /// Create a new configuration builder.
    pub fn builder() -> InboxConfigBuilder {
        InboxConfigBuilder::new()
    }
}

/// Builder for `InboxConfig`.
#[derive(Debug, Clone)]
pub struct InboxConfigBuilder {
    config: InboxConfig,
}

impl InboxConfigBuilder {
    /// Create a new builder with default values.
    pub fn new() -> Self {
        Self {
            config: InboxConfig::default(),
        }
    }

    /// Set the base polling interval.
    pub fn poll_interval(mut self, interval: Duration) -> Self {
        self.config.poll_interval = interval;
        self
    }

    /// Set the idle polling interval.
    pub fn poll_interval_idle(mut self, interval: Duration) -> Self {
        self.config.poll_interval_idle = interval;
        self
    }

    /// Set the maximum batch size for polling.
    pub fn max_batch_size(mut self, size: u32) -> Self {
        self.config.max_batch_size = size;
        self
    }

    /// Set the number of retry attempts.
    pub fn retry_attempts(mut self, attempts: u32) -> Self {
        self.config.retry_attempts = attempts;
        self
    }

    /// Set the acknowledgment batch size.
    pub fn ack_batch_size(mut self, size: u32) -> Self {
        self.config.ack_batch_size = size;
        self
    }

    /// Build the configuration.
    pub fn build(self) -> InboxConfig {
        self.config
    }
}

impl Default for InboxConfigBuilder {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Events
// ============================================================================

/// Events emitted by the inbox system for monitoring and coordination.
#[derive(Debug, Clone)]
pub enum InboxEvent {
    /// A new message was received from the server.
    MessageReceived {
        /// Unique message identifier.
        message_id: String,
        /// Type of envelope (SealedSender, PrekeyMessage, etc.).
        envelope_type: EnvelopeType,
    },
    /// A message was successfully processed through the pipeline.
    MessageProcessed {
        /// Unique message identifier.
        message_id: String,
    },
    /// A message failed processing.
    MessageFailed {
        /// Unique message identifier.
        message_id: String,
        /// Error description.
        error: String,
    },
    /// A poll cycle completed.
    PollCompleted {
        /// Number of messages received.
        count: u32,
        /// Whether more messages are available.
        has_more: bool,
    },
    /// A poll request failed.
    PollFailed {
        /// Error description.
        error: String,
    },
    /// Message acknowledgment failed.
    AcknowledgeFailed {
        /// Message IDs that failed to acknowledge.
        message_ids: Vec<String>,
        /// Error description.
        error: String,
    },
    /// A DM was received and decrypted.
    DmReceived {
        /// Peer user ID.
        peer_id: UserId,
        /// Decrypted message text.
        text: String,
        /// Server timestamp.
        timestamp: u64,
        /// Message ID (for acknowledgment).
        message_id: String,
    },
    /// DM decryption failed.
    DmDecryptionFailed {
        /// Peer user ID.
        peer_id: UserId,
        /// Error description.
        error: String,
    },
    /// DM mailbox poll completed.
    DmPollCompleted {
        /// Total messages received across all mailboxes.
        total_messages: u32,
    },
    /// DM unread count was updated.
    DmUnreadCountUpdated {
        /// DM channel ID.
        channel_id: ChannelId,
        /// Peer user ID.
        peer_id: UserId,
        /// New unread count.
        unread_count: u32,
    },
    /// Typing indicator received.
    DmTyping {
        /// Peer user ID.
        peer_id: UserId,
        /// Typing action: 1=Started, 2=Stopped.
        action: i32,
    },
    /// Read/delivery receipt received.
    DmReceipt {
        /// Peer user ID.
        peer_id: UserId,
        /// Receipt type: 1=Delivery, 2=Read, 3=Viewed.
        receipt_type: i32,
        /// Message timestamps being acknowledged.
        timestamps: Vec<u64>,
    },
    /// Reaction received.
    DmReaction {
        /// Peer user ID (who reacted).
        peer_id: UserId,
        /// Emoji.
        emoji: String,
        /// True if removing reaction.
        remove: bool,
        /// Target message author.
        target_author_aci: String,
        /// Target message timestamp.
        target_sent_timestamp: u64,
    },
    /// Pin/unpin received.
    DmPin {
        /// Peer user ID.
        peer_id: UserId,
        /// Target message author.
        target_author_aci: String,
        /// Target message timestamp.
        target_sent_timestamp: u64,
        /// True if unpinning.
        unpin: bool,
    },
    /// Message edit received.
    DmEdit {
        /// Peer user ID.
        peer_id: UserId,
        /// Target message timestamp.
        target_sent_timestamp: u64,
        /// New message body.
        new_body: String,
    },
    /// Delete for me sync received.
    DmDeleteForMe {
        /// Conversation ID.
        conversation_id: String,
        /// Target message author.
        target_author_aci: String,
        /// Target message timestamp.
        target_sent_timestamp: u64,
    },
    /// Delete for everyone received (peer wants us to delete).
    DmDeleteForEveryone {
        /// Peer user ID (who sent the delete request).
        peer_id: UserId,
        /// Target message timestamp.
        target_sent_timestamp: u64,
    },
    // =========================================================================
    // Channel Control Events (via sender key encryption)
    // =========================================================================
    /// Typing indicator received from channel member.
    ChannelTyping {
        /// Channel ID.
        channel_id: ChannelId,
        /// User who is typing.
        sender_id: UserId,
        /// Typing action: 1=Started, 2=Stopped.
        action: i32,
    },
    /// Reaction received from channel member.
    ChannelReaction {
        /// Channel ID.
        channel_id: ChannelId,
        /// User who reacted.
        sender_id: UserId,
        /// Emoji.
        emoji: String,
        /// True if removing reaction.
        remove: bool,
        /// Target message author.
        target_author_aci: String,
        /// Target message timestamp.
        target_sent_timestamp: u64,
    },
    /// Pin/unpin received from channel member.
    ChannelPin {
        /// Channel ID.
        channel_id: ChannelId,
        /// User who pinned.
        sender_id: UserId,
        /// Target message author.
        target_author_aci: String,
        /// Target message timestamp.
        target_sent_timestamp: u64,
        /// True if unpinning.
        unpin: bool,
    },
    /// Message edit received from channel member.
    ChannelEdit {
        /// Channel ID.
        channel_id: ChannelId,
        /// User who edited.
        sender_id: UserId,
        /// Target message timestamp.
        target_sent_timestamp: u64,
        /// New message body.
        new_body: String,
    },
    /// Delete for everyone received from channel member.
    ChannelDeleteForEveryone {
        /// Channel ID.
        channel_id: ChannelId,
        /// User who deleted.
        sender_id: UserId,
        /// Target message timestamp.
        target_sent_timestamp: u64,
    },
    /// Delete for me sync received in channel context.
    ChannelDeleteForMe {
        /// Conversation/channel ID.
        conversation_id: String,
        /// Target message author.
        target_author_aci: String,
        /// Target message timestamp.
        target_sent_timestamp: u64,
    },
    // =========================================================================
    // Group Mailbox Events (ephemeral mailbox routing for privacy)
    // =========================================================================
    /// Group mailbox poll completed.
    GroupMailboxPollCompleted {
        /// Number of groups polled.
        total_groups: u32,
        /// Total messages received across all group mailboxes.
        total_messages: u32,
    },
    /// Group mailbox announcement received from another member.
    GroupMailboxAnnouncement {
        /// Channel ID.
        channel_id: ChannelId,
        /// Sender/member who announced their mailbox.
        member_id: UserId,
        /// Member's mailbox public key (32 bytes Ed25519).
        mailbox_pk: Vec<u8>,
        /// When the mailbox announcement was sent (ms since epoch).
        timestamp: u64,
    },
}

// ============================================================================
// State
// ============================================================================

/// Current state of the inbox polling system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[derive(Default)]
pub enum InboxState {
    /// Not currently polling.
    #[default]
    Idle,
    /// Actively polling the server.
    Polling,
    /// Processing received messages.
    Processing,
    /// Explicitly stopped.
    Stopped,
}


// ============================================================================
// Statistics
// ============================================================================

/// Statistics for monitoring inbox performance.
#[derive(Debug, Clone, Copy, Default)]
pub struct InboxStats {
    /// Total messages received.
    pub messages_received: u64,
    /// Total messages successfully processed.
    pub messages_processed: u64,
    /// Total messages that failed processing.
    pub messages_failed: u64,
    /// Time of last poll (if any).
    pub last_poll_at: Option<Instant>,
    /// Number of messages in last poll.
    pub last_poll_count: u32,
}

// ============================================================================
// Error Types
// ============================================================================

/// Errors that can occur during inbox operations.
#[derive(Debug, Clone, thiserror::Error)]
pub enum InboxError {
    /// Poll request failed.
    #[error("poll failed: {0}")]
    PollFailed(String),

    /// Challenge-response authentication failed.
    #[error("authentication failed: {0}")]
    AuthFailed(String),

    /// Message acknowledgment failed.
    #[error("acknowledge failed: {0}")]
    AcknowledgeFailed(String),

    /// Ed25519 signing failed.
    #[error("signing failed: {0}")]
    SigningFailed(String),

    /// Provider is not running.
    #[error("provider not running")]
    NotRunning,

    /// Provider is already running.
    #[error("provider already running")]
    AlreadyRunning,

    /// Shutdown timed out.
    #[error("shutdown timed out")]
    ShutdownTimeout,
}

// ============================================================================
// DeviceInboxProvider
// ============================================================================

/// Event channel capacity for inbox events.
const EVENT_CHANNEL_CAPACITY: usize = 64;

/// Timeout for graceful shutdown.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Main coordinator for device inbox operations.
///
/// Orchestrates the polling loop, message validation, and event broadcasting.
/// Messages flow through:
/// 1. `InboxPoller` fetches from server
/// 2. `MessageValidationPipeline` validates and orders
/// 3. Events emitted for handlers to process
///
/// Also handles DM mailbox polling for two-way anonymous messaging.
///
/// # Example
///
/// ```ignore
/// let config = InboxConfig::builder()
///     .poll_interval(Duration::from_secs(5))
///     .build();
///
/// let provider = DeviceInboxProvider::new(
///     config,
///     message_client,
///     mailbox_client,
///     pipeline,
///     message_gateway,
///     database,
/// );
///
/// // Subscribe to events before starting
/// let mut rx = provider.subscribe();
/// provider.start().await?;
///
/// // Handle events
/// while let Ok(event) = rx.recv().await {
///     match event {
///         InboxEvent::MessageReceived { message_id, .. } => {
///             // Process message
///         }
///         InboxEvent::DmReceived { peer_id, text, .. } => {
///             // Handle DM message
///         }
///         _ => {}
///     }
/// }
/// ```
pub struct DeviceInboxProvider {
    config: InboxConfig,
    poller: Arc<InboxPoller>,
    pipeline: Arc<MessageValidationPipeline>,
    event_tx: broadcast::Sender<InboxEvent>,
    shutdown_tx: watch::Sender<bool>,
    poller_handle: RwLock<Option<std::thread::JoinHandle<()>>>,
    stats: Arc<RwLock<InboxStats>>,
    state: Arc<RwLock<InboxState>>,
    message_gateway: Arc<MessageGateway>,
    /// Mailbox client for DM polling.
    mailbox_client: Arc<tokio::sync::Mutex<MailboxClient>>,
    /// Database for DM conversation state.
    database: Arc<Database>,
}

impl DeviceInboxProvider {
    /// Create a new device inbox provider.
    ///
    /// # Arguments
    ///
    /// * `config` - Polling and processing configuration
    /// * `message_client` - Client for message RPCs
    /// * `mailbox_client` - Client for DM mailbox polling
    /// * `pipeline` - Message validation pipeline
    /// * `message_gateway` - Gateway for decrypting and routing messages
    /// * `database` - Database for DM conversation state
    pub fn new(
        config: InboxConfig,
        message_client: Arc<tokio::sync::Mutex<MessageClient>>,
        mailbox_client: Arc<tokio::sync::Mutex<MailboxClient>>,
        pipeline: Arc<MessageValidationPipeline>,
        message_gateway: Arc<MessageGateway>,
        database: Arc<Database>,
    ) -> Self {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (event_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let stats = Arc::new(RwLock::new(InboxStats::default()));
        let state = Arc::new(RwLock::new(InboxState::Idle));

        let poller = Arc::new(InboxPoller::new(
            config.clone(),
            message_client,
            shutdown_rx,
            event_tx.clone(),
            stats.clone(),
            state.clone(),
        ));

        Self {
            config,
            poller,
            pipeline,
            event_tx,
            shutdown_tx,
            poller_handle: RwLock::new(None),
            stats,
            state,
            message_gateway,
            mailbox_client,
            database,
        }
    }

    /// Start the polling loop.
    ///
    /// Spawns a background thread that continuously polls for messages.
    /// Does nothing if already running.
    pub async fn start(&self) -> Result<(), InboxError> {
        {
            let handle = self.poller_handle.read();
            if handle.is_some() {
                return Err(InboxError::AlreadyRunning);
            }
        }

        let poller = self.poller.clone();
        let pipeline = self.pipeline.clone();
        let event_tx = self.event_tx.clone();
        let stats = self.stats.clone();
        let message_gateway = self.message_gateway.clone();
        let mailbox_client = self.mailbox_client.clone();
        let database = self.database.clone();

        let handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to create runtime");

            rt.block_on(async move {
                Self::run_loop(poller, pipeline, event_tx, stats, message_gateway, mailbox_client, database).await;
            });
        });

        *self.poller_handle.write() = Some(handle);
        Ok(())
    }

    /// Stop the polling loop.
    ///
    /// Sends shutdown signal and waits for graceful termination.
    pub async fn stop(&self) -> Result<(), InboxError> {
        // Signal shutdown
        let _ = self.shutdown_tx.send(true);

        // Wait for handle with timeout
        let handle = self.poller_handle.write().take();
        if let Some(handle) = handle {
            // Join the thread in a blocking task so we don't block logic
            let join_handle = tokio::task::spawn_blocking(move || {
                let _ = handle.join();
            });

            match tokio::time::timeout(SHUTDOWN_TIMEOUT, join_handle).await {
                Ok(_) => {
                    *self.state.write() = InboxState::Stopped;
                    Ok(())
                }
                Err(_) => Err(InboxError::ShutdownTimeout),
            }
        } else {
            Ok(())
        }
    }

    /// Main polling loop.
    async fn run_loop(
        poller: Arc<InboxPoller>,
        pipeline: Arc<MessageValidationPipeline>,
        event_tx: broadcast::Sender<InboxEvent>,
        stats: Arc<RwLock<InboxStats>>,
        message_gateway: Arc<MessageGateway>,
        mailbox_client: Arc<tokio::sync::Mutex<MailboxClient>>,
        database: Arc<Database>,
    ) {
        // Create TwoWayAnonymity for DM polling
        let two_way = TwoWayAnonymity::new(database.clone());
        // Create conversation manager for shared secret lookups
        let conversation_manager = DmConversationManager::new(database.clone());
        // Create repositories for message storage and unread tracking
        let settings_repo = Arc::new(SettingsRepository::new(database.clone()));
        let message_repo = Arc::new(MessageRepository::new(database.clone()));
        let channel_repo = Arc::new(ChannelRepository::new(database.clone()));
        // Create group mailbox infrastructure for group polling
        let group_mailbox_manager = Arc::new(GroupMailboxManager::new(database.clone()));
        let group_coordinator = Arc::new(GroupCoordinator::new(database.clone(), group_mailbox_manager.clone()));
        let group_fanout = Arc::new(GroupFanout::new(group_mailbox_manager, mailbox_client.clone()));

        loop {
            // 1. Poll device inbox for regular messages
            match poller.run_once().await {
                Ok(Some(messages)) => {
                    for msg in messages {
                        Self::process_message(
                            &msg,
                            &pipeline,
                            &event_tx,
                            &stats,
                            &message_gateway,
                            &message_repo,
                            &channel_repo,
                            &group_coordinator,
                        ).await;
                    }
                }
                Ok(None) => {
                    // Shutdown requested
                    break;
                }
                Err(_) => {
                    // Error already logged/emitted by poller
                }
            }

            // 2. Poll DM mailboxes
            Self::poll_dm_mailboxes(
                &two_way,
                &conversation_manager,
                &mailbox_client,
                &message_gateway,
                &event_tx,
                &settings_repo,
                &message_repo,
                &channel_repo,
            ).await;

            // 3. Poll group mailboxes (ephemeral per-group mailboxes)
            Self::poll_group_mailboxes(
                &group_fanout,
                &group_coordinator,
                &message_gateway,
                &event_tx,
                &settings_repo,
                &message_repo,
                &channel_repo,
            ).await;
        }
    }

    /// Poll all DM mailboxes and process received messages.
    ///
    /// DM messages are decrypted through the MessageGateway which handles:
    /// - Signal Protocol layer (forward secrecy via Double Ratchet)
    /// - Mailbox layer (sender anonymity via XChaCha20-Poly1305)
    ///
    /// Decrypted messages are stored in the local database for persistence.
    /// Unread counts are incremented for conversations not currently viewed.
    async fn poll_dm_mailboxes(
        two_way: &TwoWayAnonymity,
        conversation_manager: &DmConversationManager,
        mailbox_client: &Arc<tokio::sync::Mutex<MailboxClient>>,
        message_gateway: &Arc<MessageGateway>,
        event_tx: &broadcast::Sender<InboxEvent>,
        settings_repo: &Arc<SettingsRepository>,
        message_repo: &Arc<MessageRepository>,
        channel_repo: &Arc<ChannelRepository>,
    ) {
        // Get current user ID for DM channel derivation
        let current_user_id: Option<UserId> = settings_repo
            .get_string(keys::CURRENT_USER_ID)
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok());
        let mut client = mailbox_client.lock().await;

        match two_way.poll_all_mailboxes(&mut client).await {
            Ok(poll_results) => {
                let mut total_messages = 0u32;

                for result in poll_results {
                    let peer_id = result.peer_id;

                    // Collect message IDs for potential acknowledgment
                    let message_ids: Vec<String> = result.messages.iter()
                        .map(|m| m.message_id.clone())
                        .collect();

                    // Get shared secret for this conversation
                    let shared_secret = match conversation_manager.load_conversation(&peer_id) {
                        Ok(Some(conv)) => conv.conversation_key.and_then(|k| k.shared_secret),
                        Ok(None) => {
                            tracing::warn!(peer_id = %peer_id, "No conversation state for DM");
                            None
                        }
                        Err(e) => {
                            tracing::error!(peer_id = %peer_id, error = %e, "Failed to load conversation");
                            None
                        }
                    };

                    // Process each message through the gateway
                    for msg in result.messages {
                        total_messages += 1;

                        if let Some(ref secret) = shared_secret {
                            // Decrypt with Signal Protocol (forward secrecy via Double Ratchet)
                            match message_gateway
                                .process_dm_message(&msg.encrypted_envelope, secret)
                                .await
                            {
                                Ok((sender_id, dm_content)) => {
                                    // Dispatch by content type
                                    match dm_content {
                                        DmContent::Text { body, timestamp } => {
                                            // Store message in local database for persistence
                                            if let Some(ref current_user) = current_user_id {
                                                let dm_channel_id = ChannelId::from_dm_participants(current_user, &sender_id);
                                                let message_id = msg.message_id.parse::<MessageId>()
                                                    .unwrap_or_else(|_| MessageId::new());

                                                let mut message = Message::new_text(
                                                    dm_channel_id,
                                                    Some(sender_id),
                                                    body.clone(),
                                                );
                                                message.id = message_id;

                                                match message_repo.insert_or_ignore(&message) {
                                                    Ok(true) => {
                                                        if let Ok(new_count) = channel_repo.increment_unread_count(&dm_channel_id) {
                                                            let _ = event_tx.send(InboxEvent::DmUnreadCountUpdated {
                                                                channel_id: dm_channel_id,
                                                                peer_id,
                                                                unread_count: new_count,
                                                            });
                                                        }
                                                    }
                                                    Ok(false) => {
                                                        tracing::debug!(peer_id = %peer_id, "DM duplicate skipped");
                                                    }
                                                    Err(e) => {
                                                        tracing::warn!(peer_id = %peer_id, error = %e, "Failed to store DM");
                                                    }
                                                }
                                            }

                                            let _ = event_tx.send(InboxEvent::DmReceived {
                                                peer_id,
                                                text: body,
                                                timestamp: if timestamp > 0 { timestamp } else { msg.server_timestamp },
                                                message_id: msg.message_id.clone(),
                                            });
                                        }

                                        DmContent::Typing { action, .. } => {
                                            tracing::debug!(peer_id = %peer_id, action, "DM typing indicator");
                                            let _ = event_tx.send(InboxEvent::DmTyping { peer_id, action });
                                        }

                                        DmContent::Receipt { receipt_type, timestamps } => {
                                            tracing::debug!(peer_id = %peer_id, receipt_type, count = timestamps.len(), "DM receipt");
                                            let _ = event_tx.send(InboxEvent::DmReceipt { peer_id, receipt_type, timestamps });
                                        }

                                        DmContent::Reaction { emoji, remove, target_author_aci, target_sent_timestamp } => {
                                            tracing::debug!(peer_id = %peer_id, emoji = %emoji, remove, "DM reaction");
                                            // TODO: Store reaction in local database
                                            let _ = event_tx.send(InboxEvent::DmReaction {
                                                peer_id,
                                                emoji,
                                                remove,
                                                target_author_aci,
                                                target_sent_timestamp,
                                            });
                                        }

                                        DmContent::Pin { target_author_aci, target_sent_timestamp, unpin } => {
                                            tracing::debug!(peer_id = %peer_id, unpin, "DM pin");
                                            // TODO: Update pin status in local database
                                            let _ = event_tx.send(InboxEvent::DmPin {
                                                peer_id,
                                                target_author_aci,
                                                target_sent_timestamp,
                                                unpin,
                                            });
                                        }

                                        DmContent::Edit { target_sent_timestamp, new_body } => {
                                            tracing::debug!(peer_id = %peer_id, target = target_sent_timestamp, "DM edit");
                                            // TODO: Update message in local database
                                            let _ = event_tx.send(InboxEvent::DmEdit {
                                                peer_id,
                                                target_sent_timestamp,
                                                new_body,
                                            });
                                        }

                                        DmContent::DeleteForMe { conversation_id, target_author_aci, target_sent_timestamp } => {
                                            tracing::debug!(conversation_id = %conversation_id, "DM delete for me sync");
                                            // TODO: Delete message from local database
                                            let _ = event_tx.send(InboxEvent::DmDeleteForMe {
                                                conversation_id,
                                                target_author_aci,
                                                target_sent_timestamp,
                                            });
                                        }

                                        DmContent::DeleteForEveryone { target_sent_timestamp } => {
                                            tracing::debug!(
                                                peer_id = %peer_id,
                                                target_sent_timestamp = target_sent_timestamp,
                                                "DM delete for everyone received"
                                            );
                                            let _ = event_tx.send(InboxEvent::DmDeleteForEveryone {
                                                peer_id: peer_id.clone(),
                                                target_sent_timestamp,
                                            });
                                        }
                                    }
                                }
                                Err(e) => {
                                    tracing::error!(
                                        peer_id = %peer_id,
                                        message_id = %msg.message_id,
                                        error = %e,
                                        "DM decryption failed"
                                    );
                                    let _ = event_tx.send(InboxEvent::DmDecryptionFailed {
                                        peer_id,
                                        error: e.to_string(),
                                    });
                                }
                            }
                        } else {
                            tracing::warn!(
                                peer_id = %peer_id,
                                message_id = %msg.message_id,
                                "No shared secret for DM decryption"
                            );
                            let _ = event_tx.send(InboxEvent::DmDecryptionFailed {
                                peer_id,
                                error: "No shared secret established".to_string(),
                            });
                        }
                    }

                    // Acknowledge processed messages
                    if !message_ids.is_empty() {
                        if let Err(e) = two_way.acknowledge_messages(&peer_id, message_ids, &mut client).await {
                            tracing::warn!(peer_id = %peer_id, error = %e, "Failed to acknowledge DM messages");
                        }
                    }
                }

                if total_messages > 0 {
                    let _ = event_tx.send(InboxEvent::DmPollCompleted { total_messages });
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, "DM mailbox poll failed (may be normal if no active conversations)");
            }
        }
    }

    /// Poll all group mailboxes for new messages.
    ///
    /// Group messages are received via per-group ephemeral mailboxes (NDSS '21 paper).
    /// Messages are encrypted with sender keys and routed through anonymous mailboxes
    /// to prevent server-side correlation of group membership.
    ///
    /// This method handles:
    /// - Sender key encrypted channel messages
    /// - GroupMailboxAddress announcements (member mailbox exchange)
    async fn poll_group_mailboxes(
        group_fanout: &Arc<GroupFanout>,
        group_coordinator: &Arc<GroupCoordinator>,
        message_gateway: &Arc<MessageGateway>,
        event_tx: &broadcast::Sender<InboxEvent>,
        settings_repo: &Arc<SettingsRepository>,
        message_repo: &Arc<MessageRepository>,
        channel_repo: &Arc<ChannelRepository>,
    ) {
        // Get current user ID for message attribution
        let current_user_id: Option<UserId> = settings_repo
            .get_string(keys::CURRENT_USER_ID)
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok());

        match group_fanout.poll_all_group_mailboxes().await {
            Ok(poll_results) => {
                let mut total_groups = 0u32;
                let mut total_messages = 0u32;

                for (group_id, poll_result) in poll_results {
                    total_groups += 1;
                    let message_count = poll_result.messages.len();

                    if message_count == 0 {
                        continue;
                    }

                    // Collect message IDs for acknowledgment
                    let message_ids = poll_result.message_ids();

                    for msg in poll_result.messages {
                        total_messages += 1;

                        // Try to decode as EncryptedData (sender key encrypted)
                        if let Ok(encrypted_data) = EncryptedData::decode(msg.encrypted_envelope.as_slice()) {
                            // Extract sender from associated_data
                            // Format: "sender:{user_id}:device:{device_id}"
                            let sender_info = Self::extract_sender_from_encrypted_data(&encrypted_data);

                            if let Some(sender_id) = sender_info {
                                match message_gateway
                                    .route_encrypted_message(&encrypted_data, &sender_id, 1)
                                    .await
                                {
                                    Ok(RouteResult::Decrypted(plaintext)) => {
                                        // Parse as DataMessage and store
                                        if let Ok(data_message) = DataMessage::decode(plaintext.as_slice()) {
                                            if current_user_id.is_some() {
                                                let message_id = msg.message_id.parse::<MessageId>()
                                                    .unwrap_or_else(|_| MessageId::new());

                                                let mut message = Message::new_text(
                                                    group_id,
                                                    Some(sender_id),
                                                    data_message.body.clone(),
                                                );
                                                message.id = message_id;

                                                if data_message.timestamp > 0 {
                                                    if let Some(dt) = chrono::DateTime::from_timestamp_millis(data_message.timestamp as i64) {
                                                        message.created_at = dt;
                                                        message.updated_at = dt;
                                                    }
                                                }

                                                match message_repo.insert_or_ignore(&message) {
                                                    Ok(true) => {
                                                        if let Ok(new_count) = channel_repo.increment_unread_count(&group_id) {
                                                            tracing::debug!(
                                                                channel_id = %group_id,
                                                                unread_count = new_count,
                                                                "Incremented group unread count (via mailbox)"
                                                            );
                                                        }
                                                    }
                                                    Ok(false) => {
                                                        tracing::debug!(
                                                            message_id = %message.id,
                                                            "Group mailbox message already exists (skipped duplicate)"
                                                        );
                                                    }
                                                    Err(e) => {
                                                        tracing::warn!(
                                                            message_id = %message.id,
                                                            error = %e,
                                                            "Failed to store group mailbox message"
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    Ok(RouteResult::MailboxAnnouncement { channel_id, announcement }) => {
                                        // Gateway detected GroupMailboxAddress - process via coordinator
                                        if let Err(e) = group_coordinator.process_announcement(sender_id, &announcement) {
                                            tracing::error!(
                                                channel_id = %channel_id,
                                                sender_id = %sender_id,
                                                error = %e,
                                                "Failed to process group mailbox announcement"
                                            );
                                        } else {
                                            tracing::info!(
                                                channel_id = %channel_id,
                                                sender_id = %sender_id,
                                                "Processed group mailbox announcement from group mailbox"
                                            );
                                            let _ = event_tx.send(InboxEvent::GroupMailboxAnnouncement {
                                                channel_id,
                                                member_id: sender_id,
                                                mailbox_pk: announcement.mailbox_pk.clone(),
                                                timestamp: announcement.timestamp,
                                            });
                                        }
                                    }
                                    Ok(RouteResult::ControlContent(_plaintext)) => {
                                        // Control content (typing, reaction, etc.)
                                        // Note: GroupMailboxAddress is now returned as MailboxAnnouncement by gateway
                                        tracing::debug!(
                                            group_id = %group_id,
                                            "Group mailbox control content received"
                                        );
                                    }
                                    Ok(RouteResult::Processed) => {
                                        tracing::debug!(
                                            group_id = %group_id,
                                            "Group mailbox sender key control message processed"
                                        );
                                    }
                                    Ok(RouteResult::NeedsSenderKey { channel_id, sender_id }) => {
                                        tracing::info!(
                                            group_id = %group_id,
                                            channel_id = %channel_id,
                                            sender_id = %sender_id,
                                            "Need sender key for group mailbox message"
                                        );
                                    }
                                    Err(e) => {
                                        tracing::error!(
                                            group_id = %group_id,
                                            error = %e,
                                            "Failed to decrypt group mailbox message"
                                        );
                                    }
                                }
                            } else {
                                tracing::warn!(
                                    group_id = %group_id,
                                    "Could not extract sender from group mailbox message"
                                );
                            }
                        } else {
                            tracing::warn!(
                                group_id = %group_id,
                                message_id = %msg.message_id,
                                "Failed to decode group mailbox message as EncryptedData"
                            );
                        }
                    }

                    // Acknowledge processed messages
                    if !message_ids.is_empty() {
                        if let Err(e) = group_fanout.acknowledge_group_messages(&group_id, message_ids).await {
                            tracing::warn!(
                                group_id = %group_id,
                                error = %e,
                                "Failed to acknowledge group mailbox messages"
                            );
                        }
                    }
                }

                if total_messages > 0 || total_groups > 0 {
                    let _ = event_tx.send(InboxEvent::GroupMailboxPollCompleted {
                        total_groups,
                        total_messages,
                    });
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, "Group mailbox poll failed (may be normal if no active groups)");
            }
        }
    }

    /// Extract sender UserId from EncryptedData associated_data.
    fn extract_sender_from_encrypted_data(encrypted_data: &EncryptedData) -> Option<UserId> {
        // The associated_data may contain sender info in format:
        // "sender:{user_id}:channel:{channel_id}"
        // Or it could be a structured proto - depends on how we encode it
        let associated_str = String::from_utf8_lossy(&encrypted_data.associated_data);
        if associated_str.starts_with("sender:") {
            let parts: Vec<&str> = associated_str.split(':').collect();
            if parts.len() >= 2 {
                return parts[1].parse().ok();
            }
        }
        None
    }

    /// Process a single message through the validation pipeline.
    ///
    /// Handles:
    /// - Sender key encrypted channel messages (EncryptedData with associated_data)
    /// - Sealed sender messages (direct Signal Protocol encryption)
    /// - GroupMailboxAddress announcements (initial mailbox exchange via device inbox)
    async fn process_message(
        msg: &DeviceMessage,
        pipeline: &MessageValidationPipeline,
        event_tx: &broadcast::Sender<InboxEvent>,
        stats: &Arc<RwLock<InboxStats>>,
        message_gateway: &Arc<MessageGateway>,
        message_repo: &Arc<MessageRepository>,
        channel_repo: &Arc<ChannelRepository>,
        group_coordinator: &Arc<GroupCoordinator>,
    ) {
        use crate::network::validation::PipelineResult;

        // First, try to decode as EncryptedData (sender key encrypted channel message)
        if let Ok(encrypted_data) = EncryptedData::decode(msg.envelope.as_slice()) {
            // Check if this is a sender key message
            if MessageGateway::is_sender_key_message(&encrypted_data)
                || MessageGateway::is_sender_key_distribution(&encrypted_data)
                || MessageGateway::is_sender_key_request(&encrypted_data)
            {
                // Parse sender info from message
                let sender_id: UserId = match msg.source_user_id.parse() {
                    Ok(id) => id,
                    Err(e) => {
                        tracing::error!(
                            message_id = %msg.message_guid,
                            error = %e,
                            "Invalid sender ID in channel message"
                        );
                        let _ = event_tx.send(InboxEvent::MessageFailed {
                            message_id: msg.message_guid.clone(),
                            error: format!("Invalid sender ID: {}", e),
                        });
                        stats.write().messages_failed += 1;
                        return;
                    }
                };

                // Route through message gateway
                match message_gateway
                    .route_encrypted_message(&encrypted_data, &sender_id, msg.source_device_id)
                    .await
                {
                    Ok(RouteResult::Decrypted(plaintext)) => {
                        // Parse as DataMessage and store
                        match DataMessage::decode(plaintext.as_slice()) {
                            Ok(data_message) => {
                                // Extract channel ID
                                let channel_id: ChannelId = match data_message.channel_id.parse() {
                                    Ok(id) => id,
                                    Err(e) => {
                                        tracing::error!(
                                            message_id = %msg.message_guid,
                                            error = %e,
                                            "Invalid channel ID in DataMessage"
                                        );
                                        let _ = event_tx.send(InboxEvent::MessageFailed {
                                            message_id: msg.message_guid.clone(),
                                            error: format!("Invalid channel ID: {}", e),
                                        });
                                        stats.write().messages_failed += 1;
                                        return;
                                    }
                                };

                                // Create message ID (use message_guid or generate new)
                                let message_id = msg.message_guid.parse::<MessageId>()
                                    .unwrap_or_else(|_| MessageId::new());

                                // Create domain message
                                let mut message = Message::new_text(
                                    channel_id,
                                    Some(sender_id),
                                    data_message.body.clone(),
                                );
                                message.id = message_id;

                                // Set timestamp if available
                                if data_message.timestamp > 0 {
                                    if let Some(dt) = chrono::DateTime::from_timestamp_millis(data_message.timestamp as i64) {
                                        message.created_at = dt;
                                        message.updated_at = dt;
                                    }
                                }

                                // Store message with deduplication
                                match message_repo.insert_or_ignore(&message) {
                                    Ok(true) => {
                                        tracing::debug!(
                                            message_id = %message.id,
                                            channel_id = %channel_id,
                                            sender_id = %sender_id,
                                            "Stored channel message locally"
                                        );

                                        // Increment unread count
                                        if let Ok(new_count) = channel_repo.increment_unread_count(&channel_id) {
                                            tracing::debug!(
                                                channel_id = %channel_id,
                                                unread_count = new_count,
                                                "Incremented channel unread count"
                                            );
                                        }

                                        let _ = event_tx.send(InboxEvent::MessageReceived {
                                            message_id: msg.message_guid.clone(),
                                            envelope_type: EnvelopeType::SealedSender,
                                        });
                                        let _ = event_tx.send(InboxEvent::MessageProcessed {
                                            message_id: msg.message_guid.clone(),
                                        });
                                        let mut s = stats.write();
                                        s.messages_received += 1;
                                        s.messages_processed += 1;
                                    }
                                    Ok(false) => {
                                        tracing::debug!(
                                            message_id = %message.id,
                                            "Channel message already exists (skipped duplicate)"
                                        );
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            message_id = %message.id,
                                            error = %e,
                                            "Failed to store channel message"
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::error!(
                                    message_id = %msg.message_guid,
                                    error = %e,
                                    "Failed to parse DataMessage from decrypted content"
                                );
                                let _ = event_tx.send(InboxEvent::MessageFailed {
                                    message_id: msg.message_guid.clone(),
                                    error: format!("Failed to parse DataMessage: {}", e),
                                });
                                stats.write().messages_failed += 1;
                            }
                        }
                    }
                    Ok(RouteResult::ControlContent(plaintext)) => {
                        // Parse as Content proto (typing, reaction, edit, pin, delete)
                        match Content::decode(plaintext.as_slice()) {
                            Ok(content_proto) => {
                                // Dispatch based on content type
                                if let Some(inner) = content_proto.content {
                                    Self::dispatch_channel_control_content(
                                        inner,
                                        &sender_id,
                                        &msg.message_guid,
                                        event_tx,
                                        stats,
                                    );
                                } else {
                                    tracing::warn!(
                                        message_id = %msg.message_guid,
                                        "Channel control content has no inner content"
                                    );
                                }
                            }
                            Err(e) => {
                                tracing::error!(
                                    message_id = %msg.message_guid,
                                    error = %e,
                                    "Failed to parse Content from control message"
                                );
                                let _ = event_tx.send(InboxEvent::MessageFailed {
                                    message_id: msg.message_guid.clone(),
                                    error: format!("Failed to parse Content: {}", e),
                                });
                                stats.write().messages_failed += 1;
                            }
                        }
                    }
                    Ok(RouteResult::Processed) => {
                        // Control message (SKDM, request) processed successfully
                        tracing::debug!(
                            message_id = %msg.message_guid,
                            "Processed sender key control message"
                        );
                        let _ = event_tx.send(InboxEvent::MessageProcessed {
                            message_id: msg.message_guid.clone(),
                        });
                        stats.write().messages_processed += 1;
                    }
                    Ok(RouteResult::NeedsSenderKey { channel_id, sender_id }) => {
                        // Missing sender key - need to request SKDM
                        tracing::info!(
                            message_id = %msg.message_guid,
                            channel_id = %channel_id,
                            sender_id = %sender_id,
                            "Need sender key for channel message, requesting SKDM"
                        );
                        // TODO: Queue SKDM request and buffer message for retry
                        let _ = event_tx.send(InboxEvent::MessageFailed {
                            message_id: msg.message_guid.clone(),
                            error: format!("Missing sender key from {}", sender_id),
                        });
                        stats.write().messages_failed += 1;
                    }
                    Ok(RouteResult::MailboxAnnouncement { channel_id, announcement }) => {
                        // Gateway detected GroupMailboxAddress - process via coordinator
                        if let Err(e) = group_coordinator.process_announcement(sender_id, &announcement) {
                            tracing::error!(
                                channel_id = %channel_id,
                                sender_id = %sender_id,
                                error = %e,
                                "Failed to process group mailbox announcement"
                            );
                        } else {
                            tracing::info!(
                                channel_id = %channel_id,
                                sender_id = %sender_id,
                                "Processed group mailbox announcement from device inbox"
                            );
                            let _ = event_tx.send(InboxEvent::GroupMailboxAnnouncement {
                                channel_id,
                                member_id: sender_id,
                                mailbox_pk: announcement.mailbox_pk.clone(),
                                timestamp: announcement.timestamp,
                            });
                        }
                        stats.write().messages_processed += 1;
                    }
                    Err(e) => {
                        tracing::error!(
                            message_id = %msg.message_guid,
                            error = %e,
                            "Failed to route encrypted message"
                        );
                        let _ = event_tx.send(InboxEvent::MessageFailed {
                            message_id: msg.message_guid.clone(),
                            error: e.to_string(),
                        });
                        stats.write().messages_failed += 1;
                    }
                }
                return;
            }
        }

        // Fall back to sealed sender processing
        let sealed_sender_msg = SealedSenderMessage {
            sealed_body: msg.envelope.clone(),
            server_key_id: 0,
            timestamp: msg.server_timestamp,
        };

        // Decrypt using MessageGateway (returns () on success, stores message internally)
        match message_gateway.process_incoming_message(sealed_sender_msg.clone()).await {
            Ok(()) => {
                let envelope_type = EnvelopeType::SealedSender;

                // Create a Message for pipeline processing
                let proto_msg = crate::proto::chat_v1::Message {
                    id: msg.message_guid.clone(),
                    channel_id: "pending".to_string(),
                    author_id: msg.source_user_id.clone(),
                    timestamp: msg.server_timestamp,
                    content: Some(crate::proto::chat_v1::message::Content {
                        r#type: Some(crate::proto::chat_v1::message::content::Type::SealedSender(
                            sealed_sender_msg
                        )),
                    }),
                    ..Default::default()
                };

                // Pipeline processing
                match pipeline.process(proto_msg) {
                    PipelineResult::Ready(_) => {
                        let _ = event_tx.send(InboxEvent::MessageReceived {
                            message_id: msg.message_guid.clone(),
                            envelope_type,
                        });
                        let _ = event_tx.send(InboxEvent::MessageProcessed {
                            message_id: msg.message_guid.clone(),
                        });
                        let mut s = stats.write();
                        s.messages_received += 1;
                        s.messages_processed += 1;
                    }
                    PipelineResult::Buffered => {
                        let _ = event_tx.send(InboxEvent::MessageReceived {
                            message_id: msg.message_guid.clone(),
                            envelope_type,
                        });
                        stats.write().messages_received += 1;
                    }
                    PipelineResult::Duplicate => {
                        tracing::debug!(message_id = %msg.message_guid, "Duplicate message skipped");
                    }
                    PipelineResult::Invalid(err) => {
                        let _ = event_tx.send(InboxEvent::MessageFailed {
                            message_id: msg.message_guid.clone(),
                            error: err.to_string(),
                        });
                        let mut s = stats.write();
                        s.messages_received += 1;
                        s.messages_failed += 1;
                    }
                    PipelineResult::Dropped => {
                        tracing::warn!(message_id = %msg.message_guid, "Message dropped (buffer full)");
                        let mut s = stats.write();
                        s.messages_received += 1;
                        s.messages_failed += 1;
                    }
                }
            }
            Err(e) => {
                let e: ServiceError = e;
                let _ = event_tx.send(InboxEvent::MessageFailed {
                    message_id: msg.message_guid.clone(),
                    error: e.to_string(),
                });
                let mut s = stats.write();
                s.messages_received += 1;
                s.messages_failed += 1;
                tracing::error!(message_id = %msg.message_guid, error = %e, "Failed to decrypt message");
            }
        }
    }

    /// Dispatch channel control content based on type.
    ///
    /// Parses the Content proto inner type and emits appropriate channel events.
    ///
    /// Note: Channel control messages (typing, reaction, edit, pin, delete) currently
    /// lack channel_id in the encrypted payload. They are decrypted and logged but
    /// full event dispatch requires proto changes to include channel context.
    /// DeleteForMeSync works because it includes conversation_id.
    /// GroupMailboxAddress announcements are handled by MessageGateway (MailboxAnnouncement variant).
    fn dispatch_channel_control_content(
        inner: content::Content,
        sender_id: &UserId,
        message_guid: &str,
        event_tx: &broadcast::Sender<InboxEvent>,
        stats: &Arc<RwLock<InboxStats>>,
    ) {
        match inner {
            content::Content::Typing(typing) => {
                // TypingMessage includes channel_id for group context
                if typing.channel_id.is_empty() {
                    tracing::debug!(
                        sender_id = %sender_id,
                        action = typing.action,
                        "Channel typing received but missing channel_id"
                    );
                } else if let Ok(channel_id) = typing.channel_id.parse::<ChannelId>() {
                    tracing::debug!(
                        sender_id = %sender_id,
                        channel_id = %channel_id,
                        action = typing.action,
                        "Channel typing indicator received"
                    );
                    let _ = event_tx.send(InboxEvent::ChannelTyping {
                        channel_id,
                        sender_id: sender_id.clone(),
                        action: typing.action,
                    });
                } else {
                    tracing::warn!(
                        sender_id = %sender_id,
                        channel_id = %typing.channel_id,
                        "Channel typing received with invalid channel_id"
                    );
                }
                stats.write().messages_processed += 1;
            }
            content::Content::Reaction(reaction) => {
                // Reaction uses channel_id encoded in target_author_aci (format: "channel:{id}:{author}")
                // or from associated DataMessage channel_id
                tracing::debug!(
                    sender_id = %sender_id,
                    emoji = %reaction.emoji,
                    remove = reaction.remove,
                    target_author = %reaction.target_author_aci,
                    target_ts = reaction.target_sent_timestamp,
                    "Channel reaction received"
                );
                // Parse channel_id from encoded target_author_aci if present
                if let Some((channel_id_str, actual_author)) = Self::parse_channel_reaction_author(&reaction.target_author_aci) {
                    if let Ok(channel_id) = channel_id_str.parse::<ChannelId>() {
                        let _ = event_tx.send(InboxEvent::ChannelReaction {
                            channel_id,
                            sender_id: sender_id.clone(),
                            emoji: reaction.emoji,
                            remove: reaction.remove,
                            target_author_aci: actual_author,
                            target_sent_timestamp: reaction.target_sent_timestamp,
                        });
                    } else {
                        tracing::warn!(
                            sender_id = %sender_id,
                            channel_id = %channel_id_str,
                            "Channel reaction received with invalid channel_id"
                        );
                    }
                }
                stats.write().messages_processed += 1;
            }
            content::Content::Pin(pin) => {
                // PinMessage includes channel_id for group context
                if pin.channel_id.is_empty() {
                    tracing::debug!(
                        sender_id = %sender_id,
                        target_author = %pin.target_author_aci,
                        target_ts = pin.target_sent_timestamp,
                        unpin = pin.unpin,
                        "Channel pin received but missing channel_id"
                    );
                } else if let Ok(channel_id) = pin.channel_id.parse::<ChannelId>() {
                    tracing::debug!(
                        sender_id = %sender_id,
                        channel_id = %channel_id,
                        target_author = %pin.target_author_aci,
                        target_ts = pin.target_sent_timestamp,
                        unpin = pin.unpin,
                        "Channel pin received"
                    );
                    let _ = event_tx.send(InboxEvent::ChannelPin {
                        channel_id,
                        sender_id: sender_id.clone(),
                        target_author_aci: pin.target_author_aci,
                        target_sent_timestamp: pin.target_sent_timestamp,
                        unpin: pin.unpin,
                    });
                } else {
                    tracing::warn!(
                        sender_id = %sender_id,
                        channel_id = %pin.channel_id,
                        "Channel pin received with invalid channel_id"
                    );
                }
                stats.write().messages_processed += 1;
            }
            content::Content::Edit(edit) => {
                // EditMessage includes channel_id in the embedded DataMessage
                let channel_id_str = edit.data_message.as_ref().map(|dm| dm.channel_id.clone()).unwrap_or_default();
                let new_body = edit.data_message.as_ref().map(|dm| dm.body.clone()).unwrap_or_default();
                let new_body_preview = if new_body.len() > 50 { format!("{}...", &new_body[..47]) } else { new_body.clone() };

                if channel_id_str.is_empty() {
                    tracing::debug!(
                        sender_id = %sender_id,
                        target_ts = edit.target_sent_timestamp,
                        preview = %new_body_preview,
                        "Channel edit received but missing channel_id"
                    );
                } else if let Ok(channel_id) = channel_id_str.parse::<ChannelId>() {
                    tracing::debug!(
                        sender_id = %sender_id,
                        channel_id = %channel_id,
                        target_ts = edit.target_sent_timestamp,
                        preview = %new_body_preview,
                        "Channel edit received"
                    );
                    let _ = event_tx.send(InboxEvent::ChannelEdit {
                        channel_id,
                        sender_id: sender_id.clone(),
                        target_sent_timestamp: edit.target_sent_timestamp,
                        new_body,
                    });
                } else {
                    tracing::warn!(
                        sender_id = %sender_id,
                        channel_id = %channel_id_str,
                        "Channel edit received with invalid channel_id"
                    );
                }
                stats.write().messages_processed += 1;
            }
            content::Content::DataMessage(dm) => {
                // DataMessage with delete field = delete for everyone
                if let Some(delete) = dm.delete {
                    if dm.channel_id.is_empty() {
                        tracing::debug!(
                            sender_id = %sender_id,
                            target_ts = delete.target_sent_timestamp,
                            "Channel delete for everyone received but missing channel_id"
                        );
                    } else if let Ok(channel_id) = dm.channel_id.parse::<ChannelId>() {
                        tracing::debug!(
                            sender_id = %sender_id,
                            channel_id = %channel_id,
                            target_ts = delete.target_sent_timestamp,
                            "Channel delete for everyone received"
                        );
                        let _ = event_tx.send(InboxEvent::ChannelDeleteForEveryone {
                            channel_id,
                            sender_id: sender_id.clone(),
                            target_sent_timestamp: delete.target_sent_timestamp,
                        });
                    } else {
                        tracing::warn!(
                            sender_id = %sender_id,
                            channel_id = %dm.channel_id,
                            "Channel delete for everyone received with invalid channel_id"
                        );
                    }
                    stats.write().messages_processed += 1;
                } else {
                    // Regular DataMessage in control context is unexpected
                    tracing::debug!(
                        message_id = %message_guid,
                        "Unexpected DataMessage in control content (expected delete)"
                    );
                }
            }
            content::Content::DeleteForMeSync(sync) => {
                // DeleteForMeSync has flat fields: conversation_id, target_author_aci, etc.
                tracing::debug!(
                    conversation_id = %sync.conversation_id,
                    target_author = %sync.target_author_aci,
                    target_ts = sync.target_sent_timestamp,
                    "Channel delete for me sync"
                );
                let _ = event_tx.send(InboxEvent::ChannelDeleteForMe {
                    conversation_id: sync.conversation_id,
                    target_author_aci: sync.target_author_aci,
                    target_sent_timestamp: sync.target_sent_timestamp,
                });
                stats.write().messages_processed += 1;
            }
            _ => {
                // Note: GroupMailboxAddress is handled by MessageGateway returning
                // RouteResult::MailboxAnnouncement before reaching this dispatch
                tracing::debug!(
                    message_id = %message_guid,
                    "Unhandled channel control content type"
                );
                stats.write().messages_processed += 1;
            }
        }
    }

    /// Parse channel_id and actual author from encoded reaction target_author_aci.
    ///
    /// The Reaction proto doesn't have a channel_id field, so for channel reactions
    /// we encode the channel context in target_author_aci as "channel:{channel_id}:{author_aci}".
    fn parse_channel_reaction_author(target_author_aci: &str) -> Option<(String, String)> {
        if target_author_aci.starts_with("channel:") {
            let parts: Vec<&str> = target_author_aci.splitn(3, ':').collect();
            if parts.len() == 3 {
                return Some((parts[1].to_string(), parts[2].to_string()));
            }
        }
        None
    }

    /// Subscribe to inbox events.
    ///
    /// Returns a receiver for all inbox events. Multiple subscribers are supported.
    pub fn subscribe(&self) -> broadcast::Receiver<InboxEvent> {
        self.event_tx.subscribe()
    }

    /// Get current inbox statistics.
    pub fn stats(&self) -> InboxStats {
        *self.stats.read()
    }

    /// Get current inbox state.
    pub fn state(&self) -> InboxState {
        *self.state.read()
    }

    /// Check if the provider is running.
    pub fn is_running(&self) -> bool {
        self.poller_handle.read().is_some()
    }

    /// Get a reference to the configuration.
    pub fn config(&self) -> &InboxConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inbox_config_defaults() {
        let config = InboxConfig::default();
        assert_eq!(config.poll_interval, Duration::from_secs(5));
        assert_eq!(config.poll_interval_idle, Duration::from_secs(30));
        assert_eq!(config.max_batch_size, 100);
        assert_eq!(config.retry_attempts, 3);
        assert_eq!(config.ack_batch_size, 50);
    }

    #[test]
    fn test_inbox_config_builder() {
        let config = InboxConfig::builder()
            .poll_interval(Duration::from_secs(10))
            .poll_interval_idle(Duration::from_secs(60))
            .max_batch_size(50)
            .retry_attempts(5)
            .ack_batch_size(25)
            .build();

        assert_eq!(config.poll_interval, Duration::from_secs(10));
        assert_eq!(config.poll_interval_idle, Duration::from_secs(60));
        assert_eq!(config.max_batch_size, 50);
        assert_eq!(config.retry_attempts, 5);
        assert_eq!(config.ack_batch_size, 25);
    }

    #[test]
    fn test_inbox_state_default() {
        let state = InboxState::default();
        assert_eq!(state, InboxState::Idle);
    }

    #[test]
    fn test_inbox_stats_default() {
        let stats = InboxStats::default();
        assert_eq!(stats.messages_received, 0);
        assert_eq!(stats.messages_processed, 0);
        assert_eq!(stats.messages_failed, 0);
        assert!(stats.last_poll_at.is_none());
        assert_eq!(stats.last_poll_count, 0);
    }

    #[test]
    fn test_inbox_event_variants() {
        // Test that all event variants can be constructed
        let events = vec![
            InboxEvent::MessageReceived {
                message_id: "msg-1".to_string(),
                envelope_type: EnvelopeType::SealedSender,
            },
            InboxEvent::MessageProcessed {
                message_id: "msg-1".to_string(),
            },
            InboxEvent::MessageFailed {
                message_id: "msg-1".to_string(),
                error: "validation failed".to_string(),
            },
            InboxEvent::PollCompleted {
                count: 10,
                has_more: true,
            },
            InboxEvent::PollFailed {
                error: "network error".to_string(),
            },
            InboxEvent::AcknowledgeFailed {
                message_ids: vec!["msg-1".to_string(), "msg-2".to_string()],
                error: "server error".to_string(),
            },
        ];

        assert_eq!(events.len(), 6);
    }

    #[test]
    fn test_inbox_error_display() {
        let err = InboxError::PollFailed("timeout".to_string());
        assert_eq!(err.to_string(), "poll failed: timeout");

        let err = InboxError::AuthFailed("invalid signature".to_string());
        assert_eq!(err.to_string(), "authentication failed: invalid signature");

        let err = InboxError::NotRunning;
        assert_eq!(err.to_string(), "provider not running");
    }
}
