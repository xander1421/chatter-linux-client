//! Channel synchronization.
//!
//! Syncs the channel list and membership from the server to local storage.

use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::domain::channel::Channel;
use crate::domain::ids::UserId;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::services::channel_event_service::ChannelEventService;
use crate::services::current_user::CurrentUserProvider;
use crate::services::channel::converters::{proto_to_domain_channel, proto_to_domain_membership};
use crate::storage::repositories::channel_repo::ChannelRepository;
use crate::storage::repositories::settings_repo::SettingsRepository;

const SYNC_KEY: &str = "sync:channels:last_at";
const PAGE_SIZE: i32 = 100;

/// Channel sync handler.
pub struct ChannelSync {
    grpc_pool: Arc<GrpcClientPool>,
    channel_repo: Arc<ChannelRepository>,
    settings_repo: Arc<SettingsRepository>,
    /// Files the member notices for roster changes whose live event was missed.
    channel_events: Arc<ChannelEventService>,
}

/// Result of a channel sync operation.
#[derive(Debug, Default)]
pub struct ChannelSyncResult {
    pub synced: usize,
    pub new_channels: usize,
    pub updated_channels: usize,
    pub members_synced: usize,
    pub members_removed: usize,
    /// Per-channel proto→domain conversion failures.
    pub channel_parse_errors: usize,
    /// Per-channel member reconcile failures.
    pub member_sync_errors: usize,
}

impl ChannelSync {
    pub fn new(
        grpc_pool: Arc<GrpcClientPool>,
        channel_repo: Arc<ChannelRepository>,
        settings_repo: Arc<SettingsRepository>,
        channel_events: Arc<ChannelEventService>,
    ) -> Self {
        Self {
            grpc_pool,
            channel_repo,
            settings_repo,
            channel_events,
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

        // Track whether any non-fatal step failed (per-channel proto parse,
        // per-channel member reconcile). If so we intentionally do NOT
        // advance `last_synced_at` — otherwise the next sync would skip the
        // work that failed here and local state would stay drifted.
        let mut had_failure = false;
        // Joins after this happened while we weren't looking.
        let last_sync = self.last_synced_at().and_then(DateTime::from_timestamp_millis);

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
                        had_failure = true;
                        result.channel_parse_errors += 1;
                        tracing::warn!(error = %e, "Failed to convert proto channel during sync");
                    }
                }
            }

            if (page_len as i32) < PAGE_SIZE {
                break;
            }
            offset += PAGE_SIZE;
        }

        // Sync memberships for each channel so local member queries reflect
        // server state (join/leave/role changes that happened while offline).
        for channel in &all_channels {
            match self.sync_channel_members(&channel.id, last_sync).await {
                Ok((added, removed)) => {
                    result.members_synced += added;
                    result.members_removed += removed;
                }
                Err(e) => {
                    had_failure = true;
                    result.member_sync_errors += 1;
                    tracing::warn!(
                        channel_id = %channel.id,
                        error = %e,
                        "Failed to sync members for channel"
                    );
                }
            }
        }

        // Only advance the sync timestamp when every per-channel step
        // succeeded. Partial failure must cause the next cycle to re-try
        // the channels we skipped here.
        if !had_failure {
            let now = chrono::Utc::now().timestamp_millis();
            let _ = self.settings_repo.set(SYNC_KEY, &now);
        }

        tracing::info!(
            synced = result.synced,
            new = result.new_channels,
            updated = result.updated_channels,
            members_synced = result.members_synced,
            members_removed = result.members_removed,
            channel_parse_errors = result.channel_parse_errors,
            member_sync_errors = result.member_sync_errors,
            advanced_timestamp = !had_failure,
            "Channel sync complete"
        );

        Ok((all_channels, result))
    }

    /// Fetch the server-side member list for a channel and reconcile it with
    /// the local memberships table. Returns (added_or_updated, removed).
    ///
    /// A change found here is one whose live event we missed, so it files the
    /// same "X left/joined the channel" notice the event would have.
    async fn sync_channel_members(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        last_sync: Option<DateTime<Utc>>,
    ) -> Result<(usize, usize), SyncError> {
        use std::collections::HashSet;

        let mut client = self.grpc_pool.channel_client();
        // The one paginated fetch, shared with every other "everyone" caller.
        // This function used to carry its own page loop — correct, but a local
        // exception that let the truncating single-page pattern survive
        // everywhere else (OPEN-ITEMS §16).
        let all_members = client
            .get_all_channel_members(&channel_id.to_string())
            .await
            .map_err(|e| SyncError::Network(e.to_string()))?;

        let server_ids: HashSet<UserId> = all_members
            .iter()
            .filter_map(|m| m.user_id.parse::<UserId>().ok())
            .collect();

        let local = self
            .channel_repo
            .list_members(channel_id)
            .map_err(|e| SyncError::Storage(e.to_string()))?;
        let me = self.settings_repo.current_user_id().ok();

        // Delete local memberships no longer present on the server.
        let mut removed = 0;
        for member in &local {
            if !server_ids.contains(&member.user_id) {
                match self.channel_repo.delete_membership(channel_id, &member.user_id) {
                    Ok(()) => {
                        removed += 1;
                        if Some(member.user_id) != me {
                            self.channel_events.record_synced_departure(member);
                        }
                    }
                    Err(e) => tracing::warn!(
                        channel_id = %channel_id,
                        user_id = %member.user_id,
                        error = %e,
                        "Failed to delete stale membership during sync"
                    ),
                }
            }
        }

        let own_joined = all_members
            .iter()
            .filter_map(|m| proto_to_domain_membership(channel_id, m).ok())
            .find(|m| Some(m.user_id) == me)
            .map(|m| m.joined_at);

        // Insert any new memberships present on the server.
        let mut added = 0;
        for proto in &all_members {
            match proto_to_domain_membership(channel_id, proto) {
                Ok(membership) => {
                    match self.channel_repo.insert_or_ignore_membership(&membership) {
                        Ok(true) => {
                            added += 1;
                            if Some(membership.user_id) != me
                                && join_is_news(membership.joined_at, last_sync, own_joined)
                            {
                                self.channel_events.record_synced_join(&membership);
                            }
                        }
                        Ok(false) => {}
                        Err(e) => tracing::warn!(
                            channel_id = %channel_id,
                            user_id = %membership.user_id,
                            error = %e,
                            "Failed to insert membership during sync"
                        ),
                    }
                }
                Err(e) => tracing::warn!(
                    channel_id = %channel_id,
                    error = %e,
                    "Failed to convert proto member during sync"
                ),
            }
        }

        Ok((added, removed))
    }

    /// Get the last sync timestamp.
    pub fn last_synced_at(&self) -> Option<i64> {
        self.settings_repo.get::<i64>(SYNC_KEY).ok().flatten()
    }
}

/// Whether a member the sync is adding locally joined while we weren't
/// looking: after the last sync and after our own join. A first sync, or a
/// channel we just joined, backfills its roster without announcing anyone.
// ponytail: compares the server's joined_at with this device's clock; a skewed
// clock can drop or add an announcement near the boundary.
fn join_is_news(
    joined_at: DateTime<Utc>,
    last_sync: Option<DateTime<Utc>>,
    own_joined: Option<DateTime<Utc>>,
) -> bool {
    matches!((last_sync, own_joined), (Some(s), Some(o)) if joined_at > s && joined_at > o)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_joins_after_the_last_sync_and_our_own_are_news() {
        let t = |secs| DateTime::from_timestamp(secs, 0).unwrap();
        let (last_sync, own) = (Some(t(100)), Some(t(50)));
        assert!(join_is_news(t(150), last_sync, own), "joined while we were away");
        assert!(!join_is_news(t(90), last_sync, own), "already a member at the last sync");
        assert!(!join_is_news(t(150), last_sync, Some(t(200))), "was there before we joined");
        assert!(!join_is_news(t(150), None, own), "first sync backfills silently");
        assert!(!join_is_news(t(150), last_sync, None), "not a member ourselves");
    }
}
