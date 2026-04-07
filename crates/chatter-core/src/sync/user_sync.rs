//! User synchronization.
//!
//! Syncs friends list and user profiles from the server.

use std::sync::Arc;

use chrono::Utc;

use crate::domain::ids::UserId;
use crate::domain::user::{PresenceStatus, Profile, User};
use crate::network::grpc::client_pool::GrpcClientPool;
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
    /// Fetches all friends via `ListFriends` RPC and upserts their
    /// user records and profiles into local storage.
    pub async fn sync_friends(&self) -> Result<UserSyncResult, SyncError> {
        let mut result = UserSyncResult::default();
        let mut offset: i32 = 0;

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
                        tracing::warn!(error = %e, "Invalid friend user ID during sync");
                        continue;
                    }
                };

                // Upsert the base user record
                let username = if friend_profile.display_name.is_empty() {
                    user_id.to_string()
                } else {
                    friend_profile.display_name.clone()
                };

                let user = User::new(user_id, username, vec![]);
                if self.user_repo.get(&user_id).ok().flatten().is_none() {
                    if let Err(e) = self.user_repo.insert(&user) {
                        tracing::debug!(
                            user_id = %user_id,
                            error = %e,
                            "Failed to insert user during sync (may already exist)"
                        );
                    }
                }

                // Upsert the profile
                let profile = Profile {
                    user_id,
                    display_name: friend_profile.display_name.clone(),
                    avatar_url: if friend_profile.avatar_url.is_empty() {
                        None
                    } else {
                        Some(friend_profile.avatar_url.clone())
                    },
                    status_message: None,
                    presence: PresenceStatus::Offline,
                };

                match self.user_repo.upsert_profile(&profile) {
                    Ok(_) => {
                        result.profiles_updated += 1;
                    }
                    Err(e) => {
                        tracing::debug!(
                            user_id = %user_id,
                            error = %e,
                            "Failed to upsert profile during sync"
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

        // Update sync timestamp
        let now = Utc::now().timestamp_millis();
        let _ = self.settings_repo.set(SYNC_KEY, &now);

        tracing::info!(
            friends = result.friends_synced,
            profiles = result.profiles_updated,
            "User sync complete"
        );

        Ok(result)
    }

    /// Get the last sync timestamp.
    pub fn last_synced_at(&self) -> Option<i64> {
        self.settings_repo.get::<i64>(SYNC_KEY).ok().flatten()
    }
}
