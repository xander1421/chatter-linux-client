//! User synchronization.
//!
//! Syncs friends list and user profiles from the server.

use std::sync::Arc;

use chrono::Utc;

use crate::domain::ids::UserId;
use crate::domain::user::{Friendship, FriendshipStatus, Profile, StatusMessage, User};
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::services::current_user::CurrentUserProvider;
use crate::services::user::conversions::presence_from_proto;
use crate::storage::repositories::settings_repo::SettingsRepository;
use crate::storage::repositories::user_repo::UserRepository;

use super::channel_sync::SyncError;

const SYNC_KEY: &str = "sync:friends:last_at";
const PAGE_SIZE: i32 = 100;

/// User sync handler.
pub struct UserSync {
    grpc_pool: Arc<GrpcClientPool>,
    user_repo: Arc<UserRepository>,
    settings_repo: Arc<SettingsRepository>,
}

/// Result of a user sync operation.
#[derive(Debug, Default)]
pub struct UserSyncResult {
    pub friends_synced: usize,
    pub profiles_updated: usize,
    pub friendships_inserted: usize,
    pub friendships_removed: usize,
}

impl UserSync {
    pub fn new(
        grpc_pool: Arc<GrpcClientPool>,
        user_repo: Arc<UserRepository>,
        settings_repo: Arc<SettingsRepository>,
    ) -> Self {
        Self {
            grpc_pool,
            user_repo,
            settings_repo,
        }
    }

    /// Sync the friends list from the server.
    ///
    /// Fetches all friends via `ListFriends` RPC, upserts their user records
    /// and profiles, and reconciles the local `friendships` table so queries
    /// like `get_friends_local` reflect server state.
    pub async fn sync_friends(&self) -> Result<UserSyncResult, SyncError> {
        use std::collections::HashSet;

        let mut result = UserSyncResult::default();
        let mut offset: i32 = 0;

        // Track non-fatal failures (bad UUIDs, storage writes that failed
        // for a single row). If ANY step fails we do not advance the sync
        // timestamp — the next cycle must retry instead of skipping the work.
        let mut had_failure = false;

        // Look up the logged-in user once; required to write friendship rows.
        let current_user = self
            .settings_repo
            .current_user_id()
            .map_err(|e| SyncError::Storage(format!("current user: {}", e)))?;

        let mut server_friend_ids: HashSet<UserId> = HashSet::new();

        loop {
            let mut client = self.grpc_pool.user_client();
            let response = client
                .list_friends(PAGE_SIZE, offset)
                .await
                .map_err(|e| SyncError::Network(format!("list friends: {}", e)))?;

            let page_len = response.friends.len();

            for friend_profile in &response.friends {
                let user_id = match friend_profile.user_id.parse::<uuid::Uuid>() {
                    Ok(uuid) => UserId::from(uuid),
                    Err(e) => {
                        had_failure = true;
                        tracing::warn!(error = %e, "Invalid friend user ID during sync");
                        continue;
                    }
                };
                server_friend_ids.insert(user_id);

                // Upsert the base user record. Username requires a non-empty
                // value, so when the server returns an empty display_name we
                // fall back to the stringified UUID so downstream lookups work.
                let existing_user = self.user_repo.get(&user_id).ok().flatten();
                let username = if !friend_profile.display_name.is_empty() {
                    friend_profile.display_name.clone()
                } else if let Some(existing) = existing_user.as_ref() {
                    existing.username.clone()
                } else {
                    user_id.to_string()
                };

                if existing_user.is_none() {
                    let user = User::new(user_id, username, vec![]);
                    if let Err(e) = self.user_repo.insert(&user) {
                        had_failure = true;
                        tracing::debug!(
                            user_id = %user_id,
                            error = %e,
                            "Failed to insert user during sync (may already exist)"
                        );
                    }
                }

                // Upsert the profile, preserving server-provided status and
                // presence instead of clobbering with empty/Offline defaults.
                let existing_profile = self.user_repo.get_profile(&user_id).ok().flatten();

                let display_name = if !friend_profile.display_name.is_empty() {
                    friend_profile.display_name.clone()
                } else if let Some(existing) = existing_profile.as_ref() {
                    existing.display_name.clone()
                } else {
                    // Last resort: the stringified user id, so the UI has
                    // *something* to render rather than a blank row.
                    user_id.to_string()
                };

                let status_message = if friend_profile.status_message.is_empty() {
                    existing_profile.as_ref().and_then(|p| p.status_message.clone())
                } else {
                    Some(StatusMessage::new(friend_profile.status_message.clone()))
                };

                let presence = presence_from_proto(friend_profile.presence);

                let profile = Profile {
                    user_id,
                    display_name,
                    status_message,
                    presence,
                };

                match self.user_repo.upsert_profile(&profile) {
                    Ok(_) => {
                        result.profiles_updated += 1;
                    }
                    Err(e) => {
                        had_failure = true;
                        tracing::debug!(
                            user_id = %user_id,
                            error = %e,
                            "Failed to upsert profile during sync"
                        );
                    }
                }

                // Persist the friendship edge (current_user → friend) so
                // local-only friend queries (get_friends_local, etc.) return
                // the same list the server just handed us. `ListFriends` only
                // returns accepted friendships.
                match self.user_repo.get_friendship(&current_user, &user_id) {
                    Ok(Some(existing)) if existing.status != FriendshipStatus::Accepted => {
                        let mut updated = existing;
                        updated.status = FriendshipStatus::Accepted;
                        if let Err(e) = self.user_repo.update_friendship(&updated) {
                            had_failure = true;
                            tracing::debug!(
                                user_id = %user_id,
                                error = %e,
                                "Failed to update friendship during sync"
                            );
                        }
                    }
                    Ok(Some(_)) => {} // already accepted, nothing to do
                    Ok(None) => {
                        let mut friendship = Friendship::new_request(current_user, user_id);
                        friendship.status = FriendshipStatus::Accepted;
                        match self.user_repo.insert_friendship(&friendship) {
                            Ok(()) => result.friendships_inserted += 1,
                            Err(e) => {
                                had_failure = true;
                                tracing::debug!(
                                    user_id = %user_id,
                                    error = %e,
                                    "Failed to insert friendship during sync"
                                );
                            }
                        }
                    }
                    Err(e) => {
                        had_failure = true;
                        tracing::debug!(
                            user_id = %user_id,
                            error = %e,
                            "Failed to read existing friendship during sync"
                        );
                    }
                }

                result.friends_synced += 1;
            }

            if (page_len as i32) < PAGE_SIZE {
                break;
            }
            offset += PAGE_SIZE;
        }

        // Prune locally-recorded accepted friendships that the server no
        // longer reports. Leave Pending/Blocked rows alone — they live in
        // separate RPC flows and aren't part of the friends list.
        match self.user_repo.list_friends(&current_user) {
            Ok(local) => {
                for friendship in local {
                    if friendship.status == FriendshipStatus::Accepted
                        && !server_friend_ids.contains(&friendship.addressee_id)
                    {
                        match self
                            .user_repo
                            .delete_friendship(&friendship.requester_id, &friendship.addressee_id)
                        {
                            Ok(()) => result.friendships_removed += 1,
                            Err(e) => {
                                had_failure = true;
                                tracing::debug!(
                                    user_id = %friendship.addressee_id,
                                    error = %e,
                                    "Failed to delete stale friendship during sync"
                                );
                            }
                        }
                    }
                }
            }
            Err(e) => {
                had_failure = true;
                tracing::debug!(error = %e, "Failed to list local friends during prune pass");
            }
        }

        // Only advance the sync timestamp on full success.
        if !had_failure {
            let now = Utc::now().timestamp_millis();
            let _ = self.settings_repo.set(SYNC_KEY, &now);
        }

        tracing::info!(
            friends = result.friends_synced,
            profiles = result.profiles_updated,
            friendships_inserted = result.friendships_inserted,
            friendships_removed = result.friendships_removed,
            advanced_timestamp = !had_failure,
            "User sync complete"
        );

        Ok(result)
    }

    /// Get the last sync timestamp.
    pub fn last_synced_at(&self) -> Option<i64> {
        self.settings_repo.get::<i64>(SYNC_KEY).ok().flatten()
    }
}
