use super::*;

impl MessageService {
    /// Send a Signal-style profile-key handshake to a friend: an empty-body,
    /// no-attachment DataMessage whose only job is to carry our profile key.
    ///
    /// The accepter sends on friend-accept; the requester sends on acceptance
    /// if it already holds the peer's key, or replies when that key arrives.
    /// This lets each peer's real display name + avatar appear promptly —
    /// mirroring how the anon-DM init/accept messages piggyback the same
    /// key+encrypted-profile pair, instead of waiting for a first real
    /// message to carry it (`send_server_dm`'s `inject_profile_key_if_sharing`).
    /// The gateway recognizes the empty body and stores the key without
    /// creating a visible message (`store_data_message`).
    ///
    /// A failed send is retried with backoff ([`PROFILE_KEY_RETRY`]) until the
    /// server accepts it, the peer is no longer a friend, or attempts run out.
    /// The retry is not persisted; an app restart mid-retry ends it.
    pub async fn send_profile_key_handshake(&self, peer_id: &UserId) -> Result<(), MessageError> {
        retry_while(
            PROFILE_KEY_RETRY,
            move || async move {
                let content = crate::services::build_data_message_content(
                    "",
                    vec![],
                    crate::util::time::TimestampNanos::now().as_u64(),
                );
                // Auto-enables profile sharing with an accepted friend and sets
                // `profile_key` on the DataMessage.
                let content = self.inject_profile_key_if_sharing(peer_id, content)?;
                match self.send_friend_dm_content(peer_id, content).await {
                    // Shield without a mailbox yet: queued for the bootstrap
                    // flush, which delivers it.
                    Ok(()) | Err(MessageError::BootstrapInitiated) => Ok(()),
                    Err(e) => Err(e),
                }
            },
            || self.is_accepted_friend(peer_id),
        )
        .await
    }

    /// The requester must wait for the accepter's key before sending its
    /// handshake: otherwise the first send exposes the requester via Basic
    /// auth just before the accepter's sealed-capable handshake arrives.
    /// The incoming-key handler calls this again once the key is stored.
    pub async fn send_profile_key_handshake_if_sealed(
        &self,
        peer_id: &UserId,
    ) -> Result<(), MessageError> {
        if !self.is_accepted_friend(peer_id) {
            return Ok(());
        }
        let me = self.current_user_id()?;
        if self
            .user_repo
            .get_friend_profile_key(&me, peer_id)?
            .is_none()
        {
            tracing::info!(
                peer_id = %peer_id,
                "Deferring profile-key handshake until peer key arrives"
            );
            return Ok(());
        }
        self.send_profile_key_handshake(peer_id).await
    }

    /// Whether `peer_id` is an accepted friend, in either direction of the
    /// original request (`UserService::is_friend_local`).
    fn is_accepted_friend(&self, peer_id: &UserId) -> bool {
        let Ok(me) = self.settings_repo.current_user_id() else {
            return false;
        };
        [(me, *peer_id), (*peer_id, me)].iter().any(|(a, b)| {
            matches!(
                self.user_repo.get_friendship(a, b),
                Ok(Some(f)) if f.status == crate::domain::user::FriendshipStatus::Accepted
            )
        })
    }

    /// Change the disappearing-messages timer of the DM with `peer_id`
    /// (`0` = off), Signal-style: adopt it locally at the next version, file the "You set
    /// disappearing messages to …" notice, and announce it to the friend with
    /// a timer-update `DataMessage` so their side adopts the same value.
    ///
    /// Returns the notice (for the UI to show), or `None` when the timer
    /// already had that value — nothing is written or sent then.
    ///
    /// The local change stands even if the announcement fails: the timer rides
    /// on every later message too, and the friend adopts it from the first one
    /// that arrives.
    pub async fn set_server_dm_disappearing_timer(
        &self,
        peer_id: &UserId,
        seconds: u32,
    ) -> Result<Option<Message>, MessageError> {
        self.set_server_dm_disappearing_window(
            peer_id,
            seconds,
            crate::services::disappearing::DEFAULT_ACTIVATION_SECONDS,
        )
        .await
    }

    pub async fn set_server_dm_disappearing_window(
        &self,
        peer_id: &UserId,
        seconds: u32,
        activation_seconds: u32,
    ) -> Result<Option<Message>, MessageError> {
        let me = self.settings_repo.current_user_id()?;
        let sent_at = crate::util::time::TimestampNanos::now().as_u64();
        let Some((notice, timer)) = crate::services::disappearing::change_server_dm_window(
            &self.settings_repo,
            &self.message_repo,
            peer_id,
            &me,
            seconds,
            activation_seconds,
            sent_at,
        )?
        else {
            return Ok(None);
        };

        let content = crate::services::disappearing::timer_update_content(timer, sent_at);
        match self.send_friend_dm_content(peer_id, content).await {
            // Shield without an established mailbox: queued for the bootstrap
            // flush, which is delivery as far as this change is concerned.
            Ok(()) | Err(MessageError::BootstrapInitiated) => Ok(Some(notice)),
            Err(e) => Err(e),
        }
    }
}

/// Attempts and backoff for [`MessageService::send_profile_key_handshake`]:
/// 1s, 2s, 4s … capped at 60s, eight attempts (about two minutes).
const PROFILE_KEY_RETRY: Backoff = Backoff {
    attempts: 8,
    first: std::time::Duration::from_secs(1),
    max: std::time::Duration::from_secs(60),
};

#[derive(Clone, Copy)]
pub(super) struct Backoff {
    attempts: u32,
    first: std::time::Duration,
    max: std::time::Duration,
}

/// Run `attempt` until it succeeds, retrying with doubling delays while
/// `wanted()` still holds after each wait. Returns the last error otherwise.
pub(super) async fn retry_while<A, Fut>(
    backoff: Backoff,
    mut attempt: A,
    wanted: impl Fn() -> bool,
) -> Result<(), MessageError>
where
    A: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), MessageError>>,
{
    let mut delay = backoff.first;
    let mut n = 1;
    loop {
        let error = match attempt().await {
            Ok(()) => return Ok(()),
            Err(e) => e,
        };
        if n >= backoff.attempts {
            return Err(error);
        }
        tracing::warn!(attempt = n, retry_in_ms = delay.as_millis() as u64, %error, "send failed; retrying");
        tokio::time::sleep(delay).await;
        if !wanted() {
            return Err(error);
        }
        delay = (delay * 2).min(backoff.max);
        n += 1;
    }
}
