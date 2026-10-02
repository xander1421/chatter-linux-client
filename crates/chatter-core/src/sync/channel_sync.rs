//! Channel synchronization.
//!
//! Syncs the channel list and membership from the server to local storage.

use std::collections::HashSet;
use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::domain::channel::Channel;
use crate::domain::ids::{ChannelId, UserId};
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::services::channel_event_service::{ChannelEventService, MemberRemoval};
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
    /// A step failed and `last_synced_at` was not advanced. The engine retries.
    pub incomplete: bool,
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
    /// lists for us: we were removed, banned or the channel was deleted while
    /// the live event could not reach us. Ask the server what it recorded and
    /// file the notice that event would have; the channel stays listed,
    /// read-only (Signal keeps an ended group).
    async fn reconcile_lost_channels(&self, listed: &HashSet<ChannelId>) -> Result<(), SyncError> {
        use crate::domain::channel::ChannelType;
        use crate::proto::chat_v1::departure::Reason;

        let Ok(me) = self.settings_repo.current_user_id() else { return Ok(()) };
        let local = self
            .channel_repo
            .list_for_user(&me)
            .map_err(|e| SyncError::Storage(e.to_string()))?;
        let lost = local.into_iter().filter(|c| {
            // DMs and Saved Messages have no removal.
            matches!(c.channel_type, ChannelType::Public | ChannelType::Private) && !listed.contains(&c.id)
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
                .get_departure(&channel.id.to_string(), &me.to_string())
                .await
                .map_err(|e| SyncError::Network(e.to_string()))?;
            match Reason::try_from(departure.reason) {
                // The listing moved under our paging; nothing was lost.
                Ok(Reason::Member) => {}
                Ok(Reason::ChannelDeleted) => self.channel_events.record_channel_deleted(&channel.id),
                reason => {
                    let by = departure.moderator_id.parse::<uuid::Uuid>().ok().map(UserId::from);
                    let removal = match (reason, by) {
                        (Ok(Reason::Banned), Some(by)) => MemberRemoval::Banned(by),
                        _ => MemberRemoval::Unexplained,
                    };
                    tracing::info!(channel_id = %channel.id, ?removal, "No longer in a channel the sync missed the removal of");
                    self.channel_events.record_synced_departure(&own, removal);
                }
            }
        }
        Ok(())
    }

    /// Why `member` left `channel_id`, as far as the server recorded it: a
    /// ban we would have heard live, else unexplained (reads as a leave). A
    /// failed ask is unexplained too; the departure itself is certain.
    async fn departure_reason(&self, channel_id: &ChannelId, member: &UserId) -> MemberRemoval {
        use crate::proto::chat_v1::departure::Reason;
        let departure = self
            .grpc_pool
            .channel_client()
            .get_departure(&channel_id.to_string(), &member.to_string())
            .await;
        match departure {
            Ok(d) if d.reason == Reason::Banned as i32 => d
                .moderator_id
                .parse::<uuid::Uuid>()
                .map(|by| MemberRemoval::Banned(UserId::from(by)))
                .unwrap_or(MemberRemoval::Unexplained),
            Ok(_) => MemberRemoval::Unexplained,
            Err(e) => {
                tracing::debug!(%channel_id, %member, error = %e, "Departure reason unavailable");
                MemberRemoval::Unexplained
            }
        }
    }

    /// Fetch the server-side member list for a channel and reconcile it with
    /// the local memberships table. Returns (added_or_updated, removed).
    ///
    /// A change found here is one whose live event we missed, so it files the
    /// same "X left/joined the channel" notice the event would have.
    async fn sync_channel_members(
        &self,
        channel_id: &ChannelId,
        last_sync: Option<DateTime<Utc>>,
    ) -> Result<(usize, usize), SyncError> {
        let mut client = self.grpc_pool.channel_client();
        // The one paginated fetch, shared with every other "everyone" caller.
        // This function used to carry its own page loop — correct, but a local
        // exception that let the truncating single-page pattern survive
        // everywhere else (OPEN-ITEMS §16).
        let all_members = client
            .get_all_channel_members(&channel_id.to_string())
            .await
            .map_err(|e| SyncError::Network(e.to_string()))?;

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
        let me = self.settings_repo.current_user_id().ok();
        let own_joined = memberships
            .iter()
            .find(|m| Some(m.user_id) == me)
            .map(|m| m.joined_at);
        let refresh = self
            .channel_repo
            .replace_memberships(channel_id, &memberships)
            .map_err(|e| SyncError::Storage(e.to_string()))?;
        for member in &refresh.removed {
            if Some(member.user_id) != me {
                let reason = self.departure_reason(channel_id, &member.user_id).await;
                self.channel_events.record_synced_departure(member, reason);
            }
        }
        for membership in &refresh.added {
            if Some(membership.user_id) != me && join_is_news(membership.joined_at, last_sync, own_joined)
            {
                self.channel_events.record_synced_join(membership);
            }
        }

        Ok((refresh.added.len(), refresh.removed.len()))
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
