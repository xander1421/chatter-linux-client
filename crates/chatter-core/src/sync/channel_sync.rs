//! Channel synchronization.
//!
//! Syncs the channel list and membership from the server to local storage.

use std::collections::HashSet;
use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::domain::channel::Channel;
use crate::domain::ids::ChannelId;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::services::channel::converters::{proto_to_domain_channel, proto_to_domain_membership};
use crate::services::channel_event_service::ChannelEventService;
use crate::services::current_user::CurrentUserProvider;
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
    /// A step failed and `last_synced_at` was not advanced. The engine retries.
    pub incomplete: bool,
}

impl ChannelSync {
    /// Local memberships define the channels covered by a successful global
    /// subscription until the next periodic channel-list reconciliation.
    pub(crate) fn local_channel_ids(&self) -> Result<Vec<ChannelId>, SyncError> {
        let me = self
            .settings_repo
            .current_user_id()
            .map_err(|e| SyncError::Storage(e.to_string()))?;
        self.channel_repo
            .list_channels_for_user(&me)
            .map_err(|e| SyncError::Storage(e.to_string()))
    }

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
        let last_sync = self
            .last_synced_at()
            .and_then(DateTime::from_timestamp_millis);

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
                        let is_new = self.channel_repo.get(&channel.id).ok().flatten().is_none();

                        self.channel_repo
                            .upsert(&channel)
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

        // A channel we are in locally that the server stopped listing is one
        // we lost while not looking. Only on a clean listing: a channel that
        // failed to parse would look lost too.
        if !had_failure {
            let listed: HashSet<ChannelId> = all_channels.iter().map(|c| c.id).collect();
            if let Err(e) = self.reconcile_lost_channels(&listed).await {
                had_failure = true;
                tracing::warn!(error = %e, "Failed to reconcile channels we are no longer in");
            }
        }

        // Sync memberships for each channel so local member queries reflect
        // server state (join/leave/role changes that happened while offline).
        for channel in &all_channels {
            match self.sync_channel_members(channel, last_sync).await {
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
        result.incomplete = had_failure;
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

    /// Channels we still hold our membership in but the server no longer
    /// lists for us: we were removed or banned, left on another device, or
    /// the channel was deleted while the live event could not reach us. Ask
    /// the server (`GetDeparture`: the signed change that removed us) and let
    /// the event service file the notice that event would have (F79).
    async fn reconcile_lost_channels(&self, listed: &HashSet<ChannelId>) -> Result<(), SyncError> {
        use crate::domain::channel::ChannelType;

        let Ok(me) = self.settings_repo.current_user_id() else {
            return Ok(());
        };
        let local = self
            .channel_repo
            .list_for_user(&me)
            .map_err(|e| SyncError::Storage(e.to_string()))?;
        let lost = local.into_iter().filter(|c| {
            // DMs and Saved Messages have no removal.
            matches!(c.channel_type, ChannelType::Public | ChannelType::Private)
                && !listed.contains(&c.id)
        });
        for channel in lost {
            let Some(own) = self
                .channel_repo
                .get_membership(&channel.id, &me)
                .map_err(|e| SyncError::Storage(e.to_string()))?
            else {
                continue;
            };
            let departure = self
                .grpc_pool
                .channel_client()
                .get_departure(&channel.id.to_string())
                .await
                .map_err(|e| SyncError::Network(e.to_string()))?;
            self.channel_events
                .record_departure(&own, &departure)
                .await
                .map_err(|e| SyncError::Network(e.to_string()))?;
        }
        Ok(())
    }

    /// Fetch the server-side member list for a channel and reconcile it with
    /// the local memberships table (`ChannelEventService::reconcile_roster`:
    /// signed log first, then the roster diff). Returns (added_or_updated, removed).
    async fn sync_channel_members(
        &self,
        channel: &Channel,
        last_sync: Option<DateTime<Utc>>,
    ) -> Result<(usize, usize), SyncError> {
        let channel_id = &channel.id;
        let mut client = self.grpc_pool.channel_client();
        // The one paginated fetch, shared with every other "everyone" caller.
        // This function used to carry its own page loop — correct, but a local
        // exception that let the truncating single-page pattern survive
        // everywhere else (OPEN-ITEMS §16).
        let all_members = match client
            .get_all_channel_members(&channel_id.to_string())
            .await
        {
            Ok(members) => members,
            // Refused, not failed (see `MessageSync::sync_channel_into`): it
            // must not hold the sync timestamp back and refetch every cycle.
            Err(e) if e.is_access_refused() => {
                tracing::info!(%channel_id, error = %e, "Channel members refused; skipping");
                return Ok((0, 0));
            }
            Err(e) => return Err(SyncError::Network(e.to_string())),
        };

        // Convert the whole page set before replacing. One bad proto must not
        // delete the member it failed to name.
        let mut memberships = Vec::with_capacity(all_members.len());
        for proto in &all_members {
            match proto_to_domain_membership(channel_id, proto) {
                Ok(membership) => memberships.push(membership),
                Err(e) => {
                    tracing::warn!(
                        channel_id = %channel_id,
                        error = %e,
                        "Failed to convert proto member during sync; roster not replaced"
                    );
                    return Err(SyncError::Storage(e.to_string()));
                }
            }
        }
        self.channel_events
            .reconcile_roster(channel_id, channel.channel_type, &memberships, last_sync)
            .await
            .map_err(|e| SyncError::Storage(e.to_string()))
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
}
