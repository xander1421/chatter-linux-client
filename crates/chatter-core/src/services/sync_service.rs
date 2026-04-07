//! Sync service.
//!
//! Public API for multi-device synchronization. Wraps the sync engine
//! and provides methods for triggering sync from the client/UI layer.

use std::sync::Arc;

use tokio::sync::watch;

use crate::events::EventBus;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::storage::repositories::channel_repo::ChannelRepository;
use crate::storage::repositories::settings_repo::SettingsRepository;
use crate::storage::repositories::unified_message_repo::UnifiedMessageRepository;
use crate::storage::repositories::user_repo::UserRepository;
use crate::sync::sync_engine::{SyncCycleResult, SyncEngine};

/// Sync service for multi-device synchronization.
///
/// Manages the background sync engine and provides an API for
/// triggering manual syncs.
pub struct SyncService {
    engine: Arc<SyncEngine>,
    shutdown_tx: watch::Sender<bool>,
}

impl SyncService {
    /// Create a new sync service.
    pub fn new(
        grpc_pool: Arc<GrpcClientPool>,
        channel_repo: Arc<ChannelRepository>,
        message_repo: Arc<UnifiedMessageRepository>,
        user_repo: Arc<UserRepository>,
        settings_repo: Arc<SettingsRepository>,
        event_bus: EventBus,
    ) -> Self {
        let (shutdown_tx, _shutdown_rx) = watch::channel(false);

        let engine = Arc::new(SyncEngine::new(
            grpc_pool,
            channel_repo,
            message_repo,
            user_repo,
            settings_repo,
            event_bus,
        ));

        Self {
            engine,
            shutdown_tx,
        }
    }

    /// Start the background sync loop.
    ///
    /// Returns a JoinHandle for the background task.
    /// The task runs until `stop()` is called.
    pub fn start(&self) -> tokio::task::JoinHandle<()> {
        let engine = Arc::clone(&self.engine);
        let shutdown_rx = self.shutdown_tx.subscribe();
        tokio::spawn(async move {
            engine.run(shutdown_rx).await;
        })
    }

    /// Stop the background sync loop.
    pub fn stop(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    /// Trigger an immediate sync cycle.
    pub async fn trigger_sync(&self) -> SyncCycleResult {
        self.engine.trigger_sync().await
    }

    /// Check if this is the first sync (new device / fresh install).
    pub fn is_initial_sync(&self) -> bool {
        self.engine.is_initial_sync()
    }
}
