//! Message synchronization.
//!
//! Per-channel message sync using server-side pagination.
//! Fetches messages since the last sync timestamp for each channel.

use std::sync::Arc;

use crate::domain::ids::ChannelId;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::proto::chat_v1::MessageSortDirection;
use crate::services::message::channel_inbound::{ChannelInbound, Ingested};
use crate::storage::repositories::settings_repo::SettingsRepository;

use super::channel_sync::SyncError;

const PAGE_SIZE: i32 = 50;

/// Message sync handler.
pub struct MessageSync {
    grpc_pool: Arc<GrpcClientPool>,
    settings_repo: Arc<SettingsRepository>,
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
        settings_repo: Arc<SettingsRepository>,
        inbound: Arc<ChannelInbound>,
    ) -> Self {
        Self {
            grpc_pool,
            settings_repo,
            inbound,
        }
    }

    /// Sync messages for a specific channel since the last sync.
    ///
    /// Uses `reference_time` to fetch only messages newer than our last sync.
    /// Messages are inserted with `INSERT OR IGNORE` to avoid duplicates.
    pub async fn sync_channel(
        &self,
        channel_id: &ChannelId,
    ) -> Result<MessageSyncResult, SyncError> {
        let channel_id_str = channel_id.to_string();
        let sync_key = format!("sync:messages:{}:last_at_ns", channel_id);
        let last_sync = self.settings_repo.get::<u64>(&sync_key).ok().flatten();

        let mut result = MessageSyncResult {
            channel_id: channel_id_str.clone(),
            ..Default::default()
        };

        // Capture the boundary before fetching, so messages arriving during
        // pagination remain eligible on the next sync. The wire uses Unix ns.
        let sync_started_ns = reference_time_ns(chrono::Utc::now())?;
        let mut page_token: Option<String> = None;
        let mut incomplete = false;

        loop {
            let mut client = self.grpc_pool.message_client();
            let response = client
                .get_messages(
                    &channel_id_str,
                    PAGE_SIZE,
                    None, // thread_root_id
                    page_token.as_deref(),
                    MessageSortDirection::SortOldestFirst,
                    None,      // reference_message_id
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
                    Ingested::OwnMessage | Ingested::Dropped => {}
                    Ingested::DeferredPendingSenderKey | Ingested::RetryableFailure => {
                        // A retained row can fail validation or wait for an SKDM
                        // indefinitely. Hold the checkpoint, but finish this page
                        // and later pages so it cannot starve newer offline rows.
                        incomplete = true;
                    }
                }
            }

            // Check for next page
            if response.next_page_token.is_empty() || page_count == 0 {
                break;
            }
            page_token = Some(response.next_page_token.clone());
        }

        // Only a fully applied traversal can advance the durable boundary.
        // Deferred sender-key rows are held in memory, so after a restart they
        // must still be eligible for a server fetch.
        if incomplete {
            return Err(SyncError::Storage(format!(
                "channel {}: message ingestion must be retried",
                channel_id
            )));
        }
        let _ = self.settings_repo.set(&sync_key, &sync_started_ns);

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
        let key = format!("sync:messages:{}:last_at_ns", channel_id);
        self.settings_repo.get::<u64>(&key).ok().flatten()
    }
}

// Keep the persisted checkpoint and GetMessages.reference_time in one unit.
fn reference_time_ns(now: chrono::DateTime<chrono::Utc>) -> Result<u64, SyncError> {
    now.timestamp_nanos_opt()
        .and_then(|ns| u64::try_from(ns).ok())
        .ok_or_else(|| SyncError::Network("sync timestamp out of range".into()))
}

#[cfg(test)]
mod timestamp_tests {
    use super::*;

    #[test]
    fn reference_time_round_trips_as_unix_nanoseconds() {
        let now = chrono::DateTime::from_timestamp(1_800_000_000, 123_456_789).unwrap();
        let reference = reference_time_ns(now).unwrap();
        assert_eq!(reference, 1_800_000_000_123_456_789);
        assert_eq!(
            chrono::DateTime::from_timestamp_nanos(reference as i64),
            now
        );
    }
}
