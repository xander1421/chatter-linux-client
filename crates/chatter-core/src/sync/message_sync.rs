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
    /// Shared decrypt → persist path: catch-up decrypts sender-key channel
    /// messages exactly as live delivery does, instead of converting them
    /// with `proto_content_to_domain`, which holds no key.
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

impl FullMessageSyncResult {
    fn record_channel(&mut self, result: MessageSyncResult, outcome: Result<(), SyncError>) {
        // Persisted progress must reach SyncComplete even when this channel
        // needs retry (including a later-page RPC failure). Only completion
        // contributes to channels_synced; errors remain retry evidence.
        self.total_fetched += result.fetched;
        self.total_inserted += result.inserted;
        match outcome {
            Ok(()) => self.channels_synced += 1,
            Err(e) => {
                tracing::warn!(channel_id = %result.channel_id, error = %e,
                    "Message sync failed for channel");
                self.errors.push(format!("{}: {}", result.channel_id, e));
            }
        }
    }
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

    // Keep progress separate from completion so callers can report already
    // persisted rows even when an ingestion or later fetch fails.
    async fn sync_channel_into(
        &self,
        channel_id: &ChannelId,
        result: &mut MessageSyncResult,
    ) -> Result<(), SyncError> {
        let channel_id_str = channel_id.to_string();
        let sync_key = format!("sync:messages:{}:last_at_ns", channel_id);
        let last_sync = self.settings_repo.get::<u64>(&sync_key).ok().flatten();

        // Capture the boundary before fetching, so messages arriving during
        // pagination remain eligible on the next sync. The wire uses Unix ns.
        let sync_started_ns = reference_time_ns(chrono::Utc::now())?;
        let mut page_token: Option<String> = None;
        let mut incomplete = false;

        loop {
            let mut client = self.grpc_pool.message_client();
            let response = match client
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
            {
                Ok(response) => response,
                // Refused, not failed: we left or lost the channel after it
                // was listed (the channel sync files that departure), or the
                // server lists a channel it will not let us read. Retrying on
                // every sync cannot help; the checkpoint stays where it was.
                Err(e) if e.is_access_refused() => {
                    tracing::info!(%channel_id, error = %e, "Channel messages refused; skipping");
                    return Ok(());
                }
                Err(e) => return Err(SyncError::Network(format!("channel {}: {}", channel_id, e))),
            };

            result.pages += 1;
            let page_count = response.messages.len();
            result.fetched += page_count;

            for proto_msg in &response.messages {
                // Same decrypt → persist path the live stream uses, so a
                // message read back from the server is as readable as one that
                // arrived live. `live = false` retains our own history too;
                // newly inserted rows announce once, whichever path wins.
                // Existing rows and later encrypted stream duplicates stay silent.
                let outcome = self.inbound.ingest(proto_msg, *channel_id, false).await;
                if outcome == Ingested::Stored {
                    result.inserted += 1;
                }
                // Hold the checkpoint, but finish this page and later pages so
                // one stuck row cannot starve newer offline rows.
                incomplete |= holds_checkpoint(outcome);
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

        Ok(())
    }

    /// Sync messages for all given channels.
    pub async fn sync_all(&self, channel_ids: &[ChannelId]) -> FullMessageSyncResult {
        let mut full_result = FullMessageSyncResult::default();

        for channel_id in channel_ids {
            let mut result = MessageSyncResult {
                channel_id: channel_id.to_string(),
                ..Default::default()
            };
            let outcome = self.sync_channel_into(channel_id, &mut result).await;
            full_result.record_channel(result, outcome);
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

}

/// Whether this message must be fetched again, so the channel's checkpoint
/// may not move past it. A row waiting for an SKDM or a retry can still be
/// recovered; a message given up as a delivery issue cannot, and holding the
/// checkpoint for it re-failed the channel's sync every cycle.
pub(crate) fn holds_checkpoint(outcome: Ingested) -> bool {
    match outcome {
        Ingested::DeferredPendingSenderKey | Ingested::RetryableFailure => true,
        Ingested::Stored | Ingested::OwnMessage | Ingested::Dropped | Ingested::DeliveryIssue => {
            false
        }
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

#[cfg(test)]
mod partial_progress_tests {
    use super::*;
    use crate::sync::sync_engine::SyncCycleResult;

    #[test]
    fn partial_persistence_counts_survive_ingestion_and_later_page_network_errors() {
        for error in [
            SyncError::Storage("ingestion pending".into()),
            SyncError::Network("next page unavailable".into()),
        ] {
            let channel = ChannelId::new().to_string();
            let mut full = FullMessageSyncResult::default();
            full.record_channel(
                MessageSyncResult {
                    channel_id: channel.clone(),
                    fetched: 50,
                    inserted: 49,
                    pages: 1,
                },
                Err(error),
            );
            assert_eq!(full.total_fetched, 50);
            assert_eq!(
                full.total_inserted, 49,
                "stored rows must trigger aggregate refresh"
            );
            assert_eq!(full.channels_synced, 0, "partial channel is not complete");
            assert_eq!(full.errors.len(), 1);
            assert!(full.errors[0].starts_with(&channel));
            let cycle = SyncCycleResult {
                messages_failed: !full.errors.is_empty(),
                messages: full,
                ..Default::default()
            };
            assert!(
                cycle.needs_retry(),
                "refresh must not turn pending truth into success"
            );
            assert_eq!(cycle.messages.total_inserted, 49);
        }
    }
}
