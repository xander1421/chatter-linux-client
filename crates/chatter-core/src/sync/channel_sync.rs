//! Channel synchronization.
//!
//! Syncs the channel list and membership from the server to local storage.

use std::sync::Arc;

use crate::domain::channel::Channel;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::services::channel::converters::proto_to_domain_channel;
use crate::storage::repositories::channel_repo::ChannelRepository;
use crate::storage::repositories::settings_repo::SettingsRepository;

const SYNC_KEY: &str = "sync:channels:last_at";
const PAGE_SIZE: i32 = 100;

/// Channel sync handler.
pub struct ChannelSync {
    grpc_pool: Arc<GrpcClientPool>,
    channel_repo: Arc<ChannelRepository>,
    settings_repo: Arc<SettingsRepository>,
}

/// Result of a channel sync operation.
#[derive(Debug, Default)]
pub struct ChannelSyncResult {
    pub synced: usize,
    pub new_channels: usize,
    pub updated_channels: usize,
}

impl ChannelSync {
    pub fn new(
        grpc_pool: Arc<GrpcClientPool>,
        channel_repo: Arc<ChannelRepository>,
        settings_repo: Arc<SettingsRepository>,
    ) -> Self {
        Self {
            grpc_pool,
            channel_repo,
            settings_repo,
        }
    }

    /// Sync all channels from the server.
    ///
    /// Fetches the full channel list with pagination and upserts into local DB.
    /// Returns the list of synced channels and stats.
    pub async fn sync(&self) -> Result<(Vec<Channel>, ChannelSyncResult), SyncError> {
        let mut result = ChannelSyncResult::default();
        let mut all_channels = Vec::new();
        let mut offset: i32 = 0;

        loop {
            let mut client = self.grpc_pool.channel_client();
            let response = client
                .list_channels(PAGE_SIZE, offset, None)
                .await
                .map_err(|e| SyncError::Network(e.to_string()))?;

            let page_len = response.channels.len();

            for proto in &response.channels {
                match proto_to_domain_channel(proto) {
                    Ok(channel) => {
                        let is_new = self.channel_repo.get(&channel.id)
                            .ok()
                            .flatten()
                            .is_none();

                        self.channel_repo.upsert(&channel)
                            .map_err(|e| SyncError::Storage(e.to_string()))?;

                        if is_new {
                            result.new_channels += 1;
                        } else {
                            result.updated_channels += 1;
                        }

                        all_channels.push(channel);
                        result.synced += 1;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to convert proto channel during sync");
                    }
                }
            }

            if (page_len as i32) < PAGE_SIZE {
                break;
            }
            offset += PAGE_SIZE;
        }

        // Update sync timestamp
        let now = chrono::Utc::now().timestamp_millis();
        let _ = self.settings_repo.set(SYNC_KEY, &now);

        tracing::info!(
            synced = result.synced,
            new = result.new_channels,
            updated = result.updated_channels,
            "Channel sync complete"
        );

        Ok((all_channels, result))
    }

    /// Get the last sync timestamp.
    pub fn last_synced_at(&self) -> Option<i64> {
        self.settings_repo.get::<i64>(SYNC_KEY).ok().flatten()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("network error: {0}")]
    Network(String),
    #[error("storage error: {0}")]
    Storage(String),
    #[error("conversion error: {0}")]
    Conversion(String),
}
