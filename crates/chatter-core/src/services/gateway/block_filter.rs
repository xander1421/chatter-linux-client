//! The client-side block filter.
//!
//! Blocking a user has to be enforced by the RECEIVING client, after
//! decryption, because it cannot be enforced anywhere else. For a sealed send
//! the server never learns who sent the message —
//! `types.DeviceMessage.SourceUserID` is empty on that path by construction —
//! and the two server guards that were supposed to cover it do not: the prekey
//! bundle fetch is skipped entirely once a session exists (`has_session`
//! short-circuits in `services/signal/session.rs`), and that RPC answers
//! anonymous blind-signed callers while still returning the target's UAK. Every
//! server-side alternative, UAK rotation included, needs a sender identity
//! bound to a sealed send, which is the exact guarantee sealed sender exists to
//! remove.
//!
//! Signal reaches the same conclusion: Signal-Server holds no block list, and
//! Signal-Desktop drops in `MessageReceiver` *after* sealed-sender decryption,
//! where the sender certificate authenticates the sender. This module is that
//! point.
//!
//! It lives in its own file rather than inline in `decryption.rs` so
//! `tests/production_wiring.rs` can see whether anything still calls it — a
//! block filter with no callers is the failure mode this whole feature is
//! about.

use crate::domain::ids::UserId;
use crate::error::ServiceError;
use crate::storage::repositories::settings_repo::SettingsRepository;
use crate::storage::repositories::UserRepository;

/// Has the logged-in user blocked `sender_id`?
///
/// Called from every inbound path at the first instant the sender is
/// authenticated — immediately after the protocol address is parsed, before
/// any content is decoded, stored or dispatched. That ordering is the whole
/// design: earlier than this the sender is a claim, and later than this the
/// message has already had an effect.
///
/// Reads the durable `friendships` table, which `UserService::block_user`
/// writes and `UserSync::sync_blocked_users` refreshes at login. It
/// deliberately does NOT consult `state::user_state::UserState::blocked_users`
/// — that is an in-memory `HashSet` nothing populates in production, so it
/// answers "not blocked" for every server-side block, and a fail-open set is
/// worse than no set at all.
///
/// Errors propagate rather than defaulting either way. A `Retain` from the
/// ack policy (which classifies storage failures transient) redelivers the
/// envelope on the next poll; guessing "not blocked" would deliver a blocked
/// sender's message on a locked database, and guessing "blocked" would delete
/// a stranger's mail with no way back.
pub fn sender_is_blocked(
    user_repo: &UserRepository,
    settings_repo: &SettingsRepository,
    sender_id: &UserId,
) -> Result<bool, ServiceError> {
    use crate::services::current_user::CurrentUserProvider;
    let me = settings_repo.current_user_id()?;
    Ok(user_repo.is_blocked(&me, sender_id)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::user::{Friendship, FriendshipStatus};
    use crate::storage::repositories::settings_repo::keys;
    use crate::storage::Database;
    use chrono::Utc;
    use std::sync::Arc;

    fn setup() -> (tempfile::TempDir, UserRepository, SettingsRepository, UserId) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open(&dir.path().join("gw.db"), "testpass").unwrap());
        db.run_migrations().unwrap();
        let users = UserRepository::new(db.clone());
        let settings = SettingsRepository::new(db);
        let me = UserId::new();
        settings
            .set_string(keys::CURRENT_USER_ID, &me.to_string())
            .unwrap();
        users.ensure_user_exists(&me).unwrap();
        (dir, users, settings, me)
    }

    /// The check every inbound path makes at the first instant the sender is
    /// authenticated. If it answers "no" for a sender the user blocked, the
    /// message goes on to be parsed, stored and shown — which is the state the
    /// client shipped in, because the only thing that ever answered this
    /// question was `state::user_state::blocked_users`, an in-memory set that
    /// nothing populates.
    #[test]
    fn a_blocked_sender_is_recognised_from_the_durable_table_alone() {
        let (_d, users, settings, me) = setup();
        let sender = UserId::new();
        users.ensure_user_exists(&sender).unwrap();
        users
            .insert_friendship(&Friendship {
                requester_id: me,
                addressee_id: sender,
                status: FriendshipStatus::Blocked,
                created_at: Utc::now(),
            })
            .unwrap();

        assert!(
            sender_is_blocked(&users, &settings, &sender).unwrap(),
            "nothing in-memory was primed — the only record of the block is the \
             `friendships` row `block_user` wrote, and that has to be enough"
        );
    }

    #[test]
    fn an_unblocked_sender_is_delivered() {
        let (_d, users, settings, _me) = setup();
        let sender = UserId::new();
        users.ensure_user_exists(&sender).unwrap();

        assert!(
            !sender_is_blocked(&users, &settings, &sender).unwrap(),
            "dropping a stranger's message is unrecoverable — it is acked, so the \
             server deletes it, and nothing is stored"
        );
    }

    /// Not knowing who we are is not the same as "not blocked". Returning
    /// `false` here would deliver a blocked sender's message during any window
    /// where the settings read fails; the error instead reaches the ack policy,
    /// which classifies `NotLoggedIn` transient and retains the envelope for a
    /// later poll. Neither delivered nor destroyed.
    #[test]
    fn an_unreadable_identity_is_an_error_not_a_pass() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open(&dir.path().join("gw.db"), "testpass").unwrap());
        db.run_migrations().unwrap();
        let users = UserRepository::new(db.clone());
        let settings = SettingsRepository::new(db);

        let err = sender_is_blocked(&users, &settings, &UserId::new())
            .expect_err("no current user must not read as `not blocked`");
        assert!(
            matches!(err, ServiceError::NotLoggedIn),
            "the ack policy classifies NotLoggedIn transient, so the envelope is \
             retained rather than delivered or deleted; got {err:?}"
        );
    }
}
