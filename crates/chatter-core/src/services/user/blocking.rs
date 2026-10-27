use super::*;

impl UserService {
    // ========================================================================
    // Blocking Operations
    // ========================================================================

    /// Block a user.
    pub async fn block_user(&self, user_id: &UserId) -> Result<(), UserError> {
        let current_user = self.settings_repo.current_user_id()?;

        let mut user_client = self.user_client();
        let user_id_str = user_id.to_string();
        let mut stream = user_client.block_user(&user_id_str).await?;

        // Drain to the end, as `unblock_user` does: the first event is the
        // PENDING sent before the block runs (friend.go `BlockUser`), and the
        // server closes the stream only once the block is committed (or with
        // the error). Breaking on that PENDING wrote the local Blocked row
        // before — or even when — the server block never happened.
        while let Some(event_result) = stream.next().await {
            event_result?;
        }

        let friendship = Friendship {
            requester_id: current_user,
            addressee_id: *user_id,
            status: FriendshipStatus::Blocked,
            created_at: Utc::now(),
        };

        // FK on users(id): a peer known only from a channel or DM has no
        // users row, so the insert failed silently and the Blocked tab stayed
        // empty until `sync_blocked_users` (which does this) ran on restart.
        self.user_repo.ensure_user_exists(user_id).ok();
        if self.user_repo.update_friendship(&friendship).is_err() {
            if let Err(e) = self.user_repo.insert_friendship(&friendship) {
                tracing::warn!(error = %e, "block_user: local block row not written");
            }
        }
        if self.user_repo.is_blocked(&current_user, user_id)? {
            self.user_repo.start_block_interval(&current_user, user_id)?;
        }

        Ok(())
    }

    /// Unblock a user.
    pub async fn unblock_user(&self, user_id: &UserId) -> Result<(), UserError> {
        let current_user = self.settings_repo.current_user_id()?;

        let mut user_client = self.user_client();
        let user_id_str = user_id.to_string();
        let mut stream = user_client.unblock_user(&user_id_str).await?;

        // Drain to the end: the server's FIRST event is a PENDING sent before
        // it unblocks (friend.go `UnblockUser`), and it closes the stream only
        // once the unblock is committed (or with the error). Stopping at that
        // first event raced the commit: the friend-list check below still saw
        // the block, deleted the local row with the friend's profile key, and
        // the friend came back as "User 1c242673" with no DM row.
        while let Some(event_result) = stream.next().await {
            event_result?;
        }

        // The server kept the friendship through the block (Signal-style), and
        // locally the block is only the row's status. Flip it back rather than
        // delete the row: the row holds the friend's profile key, and deleting
        // it is what brought an unblocked friend back as "User 1c242673".
        let accepted = Friendship {
            requester_id: current_user,
            addressee_id: *user_id,
            status: FriendshipStatus::Accepted,
            created_at: Utc::now(),
        };
        let _ = self.user_repo.update_friendship(&accepted);
        self.user_repo.end_block_interval(&current_user, user_id)?;

        // Someone who was never a friend has no business in the friends list.
        // judged on the first page only; with a full page (or
        // offline) the next `UserSync::sync_friends` prune removes the row.
        match self.list_friends_with_profiles(100, 0).await {
            Ok(friends)
                if friends.len() < 100
                    && !friends.iter().any(|(f, _)| f.addressee_id == *user_id) =>
            {
                let _ = self.user_repo.delete_friendship(&current_user, user_id);
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "unblock_user: friend list not refreshed"),
        }

        Ok(())
    }

    // Pulling the server's block list into the local `friendships` table is
    // `UserSync::sync_blocked_users` (`sync/user_sync.rs`), which the sync
    // engine runs every cycle. A duplicate of it lived here with no caller at
    // all, so a block made on another device never reached this one.
}
