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
use crate::services::message::channel_inbound::ChannelInbound;
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
        inbound: Arc<ChannelInbound>,
    ) -> Self {
        let (shutdown_tx, _shutdown_rx) = watch::channel(false);

        let engine = Arc::new(SyncEngine::new(
            grpc_pool,
            channel_repo,
            message_repo,
            user_repo,
            settings_repo,
            event_bus,
            inbound,
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
    ///
    /// Called from `ChatterClient::start`, which every authenticated entry
    /// path funnels through (login, register, link-device, restored session).
    /// This is the ONLY production driver of `SyncEngine` — if this loses its
    /// caller, every sync pass dies with it, including
    /// `UserSync::sync_blocked_users`, which the inbound block filter depends
    /// on for blocks made on other devices. `tests/production_wiring.rs` pins
    /// it. (The distinctive name over plain `start` is what makes that pin
    /// possible — the wiring check is textual.)
    pub fn start_background_sync(&self) -> tokio::task::JoinHandle<()> {
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
