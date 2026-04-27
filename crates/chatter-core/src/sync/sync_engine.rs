//! Sync engine.
//!
//! Main synchronization orchestrator that coordinates channel, message,
//! and user sync operations. Runs as a background task with periodic
//! sync cycles and immediate sync on reconnect.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use crate::domain::ids::ChannelId;
use crate::events::EventBus;
use crate::inbox::DomainEvent;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::storage::repositories::channel_repo::ChannelRepository;
use crate::storage::repositories::settings_repo::SettingsRepository;
use crate::storage::repositories::unified_message_repo::UnifiedMessageRepository;
use crate::storage::repositories::user_repo::UserRepository;

use super::channel_sync::{ChannelSync, ChannelSyncResult};
use super::message_sync::{FullMessageSyncResult, MessageSync};
use super::user_sync::{UserSync, UserSyncResult};

/// Interval between periodic sync cycles (5 minutes).
const SYNC_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Short delay after reconnect before triggering sync.
const RECONNECT_SYNC_DELAY: Duration = Duration::from_secs(2);

/// Sync engine orchestrating all sync operations.
pub struct SyncEngine {
    channel_sync: ChannelSync,
    message_sync: MessageSync,
    user_sync: UserSync,
    event_bus: EventBus,
    #[allow(dead_code)]
    settings_repo: Arc<SettingsRepository>,
}

/// Result of a full sync cycle.
#[derive(Debug, Default)]
pub struct SyncCycleResult {
    pub channels: ChannelSyncResult,
    pub messages: FullMessageSyncResult,
    pub users: UserSyncResult,
    pub duration_ms: u64,
}

impl SyncEngine {
    pub fn new(
        grpc_pool: Arc<GrpcClientPool>,
        channel_repo: Arc<ChannelRepository>,
        message_repo: Arc<UnifiedMessageRepository>,
        user_repo: Arc<UserRepository>,
        settings_repo: Arc<SettingsRepository>,
        event_bus: EventBus,
    ) -> Self {
        Self {
            channel_sync: ChannelSync::new(
                grpc_pool.clone(),
                channel_repo.clone(),
                settings_repo.clone(),
            ),
            message_sync: MessageSync::new(
                grpc_pool.clone(),
                message_repo,
                settings_repo.clone(),
                channel_repo,
            ),
            user_sync: UserSync::new(
                grpc_pool,
                user_repo,
                settings_repo.clone(),
            ),
            event_bus,
            settings_repo,
        }
    }

    /// Run a full sync cycle: channels → messages → users.
    ///
    /// Channels are synced first so we know which channels to fetch messages for.
    pub async fn sync_all(&self) -> SyncCycleResult {
        let start = std::time::Instant::now();
        let mut result = SyncCycleResult::default();

        // Step 1: Sync channels
        match self.channel_sync.sync().await {
            Ok((channels, channel_result)) => {
                result.channels = channel_result;

                // Step 2: Sync messages for all channels
                let channel_ids: Vec<ChannelId> = channels.iter().map(|c| c.id).collect();
                result.messages = self.message_sync.sync_all(&channel_ids).await;
            }
            Err(e) => {
                tracing::error!(error = %e, "Channel sync failed — skipping message sync");
            }
        }

        // Step 3: Sync users/friends
        match self.user_sync.sync_friends().await {
            Ok(user_result) => {
                result.users = user_result;
            }
            Err(e) => {
                tracing::warn!(error = %e, "User sync failed");
            }
        }

        result.duration_ms = start.elapsed().as_millis() as u64;

        // Emit sync complete event
        let _ = self.event_bus.domain_sender().send(DomainEvent::SyncComplete {
            channels_synced: result.channels.synced,
            messages_synced: result.messages.total_inserted,
            duration_ms: result.duration_ms,
        });

        tracing::info!(
            channels = result.channels.synced,
            messages = result.messages.total_inserted,
            friends = result.users.friends_synced,
            duration_ms = result.duration_ms,
            "Sync cycle complete"
        );

        result
    }

    /// Run the sync engine as a background task.
    ///
    /// Performs an initial sync immediately, then periodic syncs at `SYNC_INTERVAL`.
    /// Shuts down when `shutdown_rx` receives `true`.
    pub async fn run(self: Arc<Self>, mut shutdown_rx: watch::Receiver<bool>) {
        tracing::info!("Sync engine started");

        // Initial sync after a short delay (let connections stabilize)
        tokio::select! {
            _ = tokio::time::sleep(RECONNECT_SYNC_DELAY) => {
                self.sync_all().await;
            }
            _ = shutdown_rx.changed() => {
                tracing::info!("Sync engine shutting down before initial sync");
                return;
            }
        }

        // Periodic sync loop
        let mut interval = tokio::time::interval(SYNC_INTERVAL);
        interval.tick().await; // Skip the first immediate tick (already synced above)

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    self.sync_all().await;
                }
                _ = shutdown_rx.changed() => {
                    tracing::info!("Sync engine shutting down");
                    return;
                }
            }
        }
    }

    /// Trigger an immediate sync (e.g., after reconnect).
    pub async fn trigger_sync(&self) -> SyncCycleResult {
        tracing::info!("Triggered immediate sync");
        self.sync_all().await
    }

    /// Check if this is the first sync (no channels synced yet).
    pub fn is_initial_sync(&self) -> bool {
        self.channel_sync.last_synced_at().is_none()
    }
}
