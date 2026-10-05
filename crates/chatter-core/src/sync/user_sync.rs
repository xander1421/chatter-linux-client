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
    /// Blocked users reconciled from the server this cycle.
    pub blocked_synced: usize,
    /// Friendships newly accepted this pass (inserted, or pending → accepted).
    /// One of these is enough for the contacts list: the UI reloads every
    /// friend from storage when it hears `FriendAccepted`.
    pub new_friend_ids: Vec<UserId>,
    /// A row failed. The pass returned what it could, but `last_synced_at`
    /// was not advanced and the engine must run this step again.
    pub incomplete: bool,
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

                // The server sends display_name ENCRYPTED (Signal-style, per
                // friend profile key). Decrypt it here or fall back to a safe
                // name — never persist ciphertext, or reloads render base64.
                let existing_user = self.user_repo.get(&user_id).ok().flatten();
                let existing_profile = self.user_repo.get_profile(&user_id).ok().flatten();

                // Decrypt with the friend's profile key. No key yet (handshake
                // pending) → keep the name we already have, else a placeholder.
                // We never store the undecryptable server value.
                let display_name = crate::services::user::decrypt_friend_display_name(
                    &self.user_repo,
                    &current_user,
                    &user_id,
                    &friend_profile.display_name,
                )
                .or_else(|| {
                    existing_profile
                        .as_ref()
                        .map(|p| p.display_name.clone())
                        .filter(|n| !n.is_empty())
                })
                .or_else(|| {
                    existing_user
                        .as_ref()
                        .map(|u| u.username.clone())
                        .filter(|n| !n.is_empty())
                })
                .unwrap_or_else(|| {
                    crate::services::user::friend_display_name_placeholder(&user_id)
                });

                // Username requires a non-empty value; the resolver never
                // returns empty, so reuse the resolved (decrypted) name.
                if existing_user.is_none() {
                    let user = User::new(user_id, display_name.clone(), vec![]);
                    if let Err(e) = self.user_repo.insert(&user) {
                        had_failure = true;
                        tracing::debug!(
                            user_id = %user_id,
                            error = %e,
                            "Failed to insert user during sync (may already exist)"
                        );
                    }
                }

                // Status is E2E-encrypted like the name; decrypt with the
                // friend's key, else keep what we already have.
                let status_message = crate::services::user::decrypt_friend_status(
                    &self.user_repo,
                    &current_user,
                    &user_id,
                    &friend_profile.status_message,
                )
                .map(StatusMessage::new)
                .or_else(|| {
                    existing_profile
                        .as_ref()
                        .and_then(|p| p.status_message.clone())
                });

                let presence = presence_from_proto(friend_profile.presence);

                let profile = Profile {
                    user_id,
                    display_name,
                    avatar_bytes: None,
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
                    // A Blocked row stays blocked: the server lists blocked
                    // friends too, and the block is `sync_blocked_users`'s.
                    Ok(Some(existing)) if existing.status == FriendshipStatus::Pending => {
                        let mut updated = existing;
                        updated.status = FriendshipStatus::Accepted;
                        if let Err(e) = self.user_repo.update_friendship(&updated) {
                            had_failure = true;
                            tracing::debug!(
                                user_id = %user_id,
                                error = %e,
                                "Failed to update friendship during sync"
                            );
                        } else {
                            result.new_friend_ids.push(user_id);
                        }
                    }
                    Ok(Some(_)) => {} // accepted or blocked, nothing to do
                    Ok(None) => {
                        let mut friendship = Friendship::new_request(current_user, user_id);
                        friendship.status = FriendshipStatus::Accepted;
                        match self.user_repo.insert_friendship(&friendship) {
                            Ok(()) => {
                                result.friendships_inserted += 1;
                                result.new_friend_ids.push(user_id);
                            }
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
        result.incomplete = had_failure;
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

    /// Reconcile the server's block list into the local `friendships` table.
    ///
    /// Without this a block only ever exists on the device that made it:
    /// `UserService::block_user` writes the row locally and tells the server,
    /// and nothing on any *other* device ever asks. Since client-side block
    /// enforcement reads that table
    /// (`services::gateway::decryption::sender_is_blocked`), a user who blocks
    /// someone on their phone would keep receiving that person's DMs on their
    /// laptop forever.
    ///
    /// Two-way, like the friends pass: rows the server no longer reports as
    /// blocked are cleared, so an unblock on another device also lands. Only
    /// `Blocked` rows are touched — a `Pending` or `Accepted` friendship is not
    /// this RPC's business. A lifted block goes back to `Accepted` rather than
    /// being deleted, so a friend keeps their profile key; the `sync_friends`
    /// pass that runs after this prunes it if they were never a friend.
    ///
    /// Returns the server's block count and whether the local set changed.
    pub async fn sync_blocked_users(&self) -> Result<(usize, bool), SyncError> {
        use std::collections::HashSet;

        let current_user = self
            .settings_repo
            .current_user_id()
            .map_err(|e| SyncError::Storage(format!("current user: {}", e)))?;

        let mut server_blocked: HashSet<UserId> = HashSet::new();
        let mut offset: i32 = 0;

        loop {
            let mut client = self.grpc_pool.user_client();
            let response = client
                .list_blocked_users(PAGE_SIZE, offset)
                .await
                .map_err(|e| SyncError::Network(format!("list blocked users: {}", e)))?;

            let page_len = response.blocked_users.len();

            for proto_user in &response.blocked_users {
                match proto_user.id.parse::<UserId>() {
                    Ok(user_id) => {
                        server_blocked.insert(user_id);
                    }
                    Err(e) => {
                        // Skip the row rather than abort the pass. Failing the
                        // whole sync on one bad id would leave every OTHER
                        // block unenforced, which is the worse direction.
                        tracing::warn!(error = %e, "Invalid blocked user ID during sync");
                    }
                }
            }

            if (page_len as i32) < PAGE_SIZE {
                break;
            }
            offset += PAGE_SIZE;
        }

        let before: HashSet<UserId> = self
            .user_repo
            .list_blocked_by(&current_user)
            .unwrap_or_default()
            .into_iter()
            .collect();

        for user_id in &server_blocked {
            // FK on users(id) — the blocked user may never have been seen on
            // this device, which is precisely the case that needs the row.
            let _ = self.user_repo.ensure_user_exists(user_id);
            let friendship = Friendship {
                requester_id: current_user,
                addressee_id: *user_id,
                status: FriendshipStatus::Blocked,
                created_at: Utc::now(),
            };
            if self.user_repo.update_friendship(&friendship).is_err() {
                let _ = self.user_repo.insert_friendship(&friendship);
            }
        }

        // Clear local blocks the server no longer reports (an unblock made on
        // another device).
        if let Ok(local) = self.user_repo.list_friends(&current_user) {
            for friendship in local {
                if friendship.status == FriendshipStatus::Blocked
                    && friendship.requester_id == current_user
                    && !server_blocked.contains(&friendship.addressee_id)
                {
                    let _ = self.user_repo.update_friendship(&Friendship {
                        status: FriendshipStatus::Accepted,
                        ..friendship
                    });
                }
            }
        }

        let changed = before != server_blocked;
        tracing::info!(
            blocked = server_blocked.len(),
            changed,
            "Blocked-user sync complete"
        );
        Ok((server_blocked.len(), changed))
    }

    /// Get the last sync timestamp.
    pub fn last_synced_at(&self) -> Option<i64> {
        self.settings_repo.get::<i64>(SYNC_KEY).ok().flatten()
    }
}
