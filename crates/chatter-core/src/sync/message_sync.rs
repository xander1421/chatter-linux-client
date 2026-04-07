//! Message synchronization.
//!
//! Per-channel message sync using server-side pagination.
//! Fetches messages since the last sync timestamp for each channel.

use std::sync::Arc;

use crate::domain::ids::{ChannelId, UserId};
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::proto::chat_v1::MessageSortDirection;
use crate::services::message::conversion::proto_to_domain_message_with_sender;
use crate::storage::repositories::settings_repo::SettingsRepository;
use crate::storage::repositories::unified_message_repo::UnifiedMessageRepository;

use super::channel_sync::SyncError;

const PAGE_SIZE: i32 = 50;

/// Message sync handler.
pub struct MessageSync {
    grpc_pool: Arc<GrpcClientPool>,
    message_repo: Arc<UnifiedMessageRepository>,
    settings_repo: Arc<SettingsRepository>,
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
    ) -> Self {
        Self {
            grpc_pool,
            message_repo,
            settings_repo,
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
                // Parse sender ID
                let sender_id: Option<UserId> = if proto_msg.author_id.is_empty() {
                    None
                } else {
                    proto_msg.author_id.parse().ok()
                };

                // Convert proto to domain message using existing conversion
                let message = match proto_to_domain_message_with_sender(proto_msg, sender_id) {
                    Ok(msg) => msg,
                    Err(e) => {
                        tracing::debug!(
                            channel_id = %channel_id,
                            error = %e,
                            "Failed to convert synced message"
                        );
                        continue;
                    }
                };

                // Insert (INSERT OR IGNORE handles duplicates)
                match self.message_repo.insert(&message) {
                    Ok(_) => {
                        result.inserted += 1;
                    }
                    Err(e) => {
                        tracing::debug!(
                            channel_id = %channel_id,
                            message_id = %message.id,
                            error = %e,
                            "Failed to insert synced message (may be duplicate)"
                        );
                    }
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
