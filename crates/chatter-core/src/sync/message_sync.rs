//! Message synchronization.
//!
//! Per-channel message sync using server-side pagination.
//! Fetches messages since the last sync timestamp for each channel.

use std::sync::Arc;

use crate::domain::ids::ChannelId;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::proto::chat_v1::MessageSortDirection;
use crate::services::message::channel_inbound::{ChannelInbound, Ingested};
use crate::storage::repositories::channel_repo::ChannelRepository;
use crate::storage::repositories::settings_repo::SettingsRepository;
use crate::storage::repositories::unified_message_repo::UnifiedMessageRepository;

use super::channel_sync::SyncError;

const PAGE_SIZE: i32 = 50;

/// Message sync handler.
pub struct MessageSync {
    grpc_pool: Arc<GrpcClientPool>,
    #[allow(dead_code)]
    message_repo: Arc<UnifiedMessageRepository>,
    settings_repo: Arc<SettingsRepository>,
    #[allow(dead_code)]
    channel_repo: Arc<ChannelRepository>,
    /// Shared decrypt → persist path. Without it this sync wrote every
    /// sender-key encrypted channel message to disk as the literal string
    /// "[Encrypted message]" (`proto_content_to_domain`'s placeholder), so
    /// catching up after being offline produced unreadable history.
    inbound: Arc<ChannelInbound>,
}

/// Result of a message sync operation for one channel.
#[derive(Debug, Default)]
pub struct MessageSyncResult {
    pub channel_id: String,
    pub fetched: usize,
    pub inserted: usize,
    pub pages: usize,
}

/// Result of a full message sync across all channels.
#[derive(Debug, Default)]
pub struct FullMessageSyncResult {
    pub channels_synced: usize,
    pub total_fetched: usize,
    pub total_inserted: usize,
    pub errors: Vec<String>,
}

impl MessageSync {
    pub fn new(
        grpc_pool: Arc<GrpcClientPool>,
        message_repo: Arc<UnifiedMessageRepository>,
        settings_repo: Arc<SettingsRepository>,
        channel_repo: Arc<ChannelRepository>,
        inbound: Arc<ChannelInbound>,
    ) -> Self {
        Self {
            grpc_pool,
            message_repo,
            settings_repo,
            channel_repo,
            inbound,
        }
    }

    /// Sync messages for a specific channel since the last sync.
    ///
    /// Uses `reference_time` to fetch only messages newer than our last sync.
    /// Messages are inserted with `INSERT OR IGNORE` to avoid duplicates.
    pub async fn sync_channel(&self, channel_id: &ChannelId) -> Result<MessageSyncResult, SyncError> {
        let channel_id_str = channel_id.to_string();
        let sync_key = format!("sync:messages:{}:last_at", channel_id);
        let last_sync = self.settings_repo.get::<u64>(&sync_key)
            .ok()
            .flatten();

        let mut result = MessageSyncResult {
            channel_id: channel_id_str.clone(),
            ..Default::default()
        };

        let mut page_token: Option<String> = None;

        loop {
            let mut client = self.grpc_pool.message_client();
            let response = client
                .get_messages(
                    &channel_id_str,
                    PAGE_SIZE,
                    None, // thread_root_id
                    page_token.as_deref(),
                    MessageSortDirection::SortOldestFirst,
                    None, // reference_message_id
                    last_sync, // reference_time — only fetch messages after this
                )
                .await
                .map_err(|e| SyncError::Network(format!("channel {}: {}", channel_id, e)))?;

            result.pages += 1;
            let page_count = response.messages.len();
            result.fetched += page_count;

            for proto_msg in &response.messages {
                // Same decrypt → persist path the live stream uses, so a
                // message read back from the server is as readable as one that
                // arrived live. `emit = false`: a catch-up of a hundred old
                // messages must not fire a hundred UI events.
                match self.inbound.ingest(proto_msg, *channel_id, false).await {
                    Ingested::Stored => result.inserted += 1,
                    // Own messages and messages parked on a missing sender key
                    // are expected outcomes, not sync failures.
                    Ingested::OwnMessage
                    | Ingested::DeferredPendingSenderKey
                    | Ingested::Dropped => {}
                }
            }

            // Check for next page
            if response.next_page_token.is_empty() || page_count == 0 {
                break;
            }
            page_token = Some(response.next_page_token.clone());
        }

        // Update the per-channel sync timestamp
        let now = chrono::Utc::now().timestamp_millis() as u64;
        let _ = self.settings_repo.set(&sync_key, &now);

        if result.inserted > 0 {
            tracing::info!(
                channel_id = %channel_id,
                fetched = result.fetched,
                inserted = result.inserted,
                pages = result.pages,
                "Message sync complete for channel"
            );
        }

        Ok(result)
    }

    /// Sync messages for all given channels.
    pub async fn sync_all(&self, channel_ids: &[ChannelId]) -> FullMessageSyncResult {
        let mut full_result = FullMessageSyncResult::default();

        for channel_id in channel_ids {
            match self.sync_channel(channel_id).await {
                Ok(result) => {
                    full_result.channels_synced += 1;
                    full_result.total_fetched += result.fetched;
                    full_result.total_inserted += result.inserted;
                }
                Err(e) => {
                    tracing::warn!(
                        channel_id = %channel_id,
                        error = %e,
                        "Message sync failed for channel"
                    );
                    full_result.errors.push(format!("{}: {}", channel_id, e));
                }
            }
        }

        tracing::info!(
            channels = full_result.channels_synced,
            fetched = full_result.total_fetched,
            inserted = full_result.total_inserted,
            errors = full_result.errors.len(),
            "Full message sync complete"
        );

        full_result
    }

    /// Get the last sync timestamp for a channel.
    pub fn last_synced_at(&self, channel_id: &ChannelId) -> Option<u64> {
        let key = format!("sync:messages:{}:last_at", channel_id);
        self.settings_repo.get::<u64>(&key).ok().flatten()
    }
}
