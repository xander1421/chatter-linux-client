//! The group send token of a private channel (crypto/group_send.rs): fetched
//! once per verified membership state and cached, as Signal fetches
//! endorsements with a group state change rather than per send. Fetching per
//! send would hand the server an authenticated request right before each
//! anonymous one.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::crypto::group_send;
use crate::domain::ids::ChannelId;
use crate::domain::verified_state::{StateHash, VerifiedMembership};
use crate::services::channel::ChannelError;

use super::ChannelService;

/// Refetch this long before a token expires (libsignal's client refuses an
/// issuance with less than two hours left).
const REFRESH_MARGIN_SECS: u64 = 3 * 3600;

#[derive(Clone)]
struct Cached {
    state_hash: StateHash,
    expiration: u64,
    token: Vec<u8>,
}

#[derive(Default)]
pub(crate) struct TokenCache(Mutex<HashMap<ChannelId, Cached>>);

impl TokenCache {
    fn get(&self, channel_id: &ChannelId, state_hash: &StateHash, now: u64) -> Option<Vec<u8>> {
        let map = self.0.lock().ok()?;
        map.get(channel_id)
            .filter(|c| c.state_hash == *state_hash && c.expiration > now + REFRESH_MARGIN_SECS)
            .map(|c| c.token.clone())
    }

    fn put(&self, channel_id: &ChannelId, state_hash: StateHash, expiration: u64, token: Vec<u8>) {
        if let Ok(mut map) = self.0.lock() {
            map.insert(*channel_id, Cached { state_hash, expiration, token });
        }
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

impl ChannelService {
    /// The serialized `GroupSendFullToken` that authorizes an anonymous send
    /// to every ACTIVE member of our verified state for `channel_id`.
    ///
    /// The issuance must be for exactly our verified revision: a server ahead
    /// of us is caught up first (the new state may have removed someone), a
    /// server behind us is refused.
    pub async fn group_send_token(&self, channel_id: &ChannelId) -> Result<Vec<u8>, ChannelError> {
        let now = now_secs();
        let verified = self.verified_for_send(channel_id)?;
        let hash = verified.state_hash();
        if let Some(token) = self.group_send_tokens.get(channel_id, &hash, now) {
            return Ok(token);
        }
        let resp = self
            .channel_client()
            .get_group_send_endorsements(&channel_id.to_string())
            .await?;
        let verified = if resp.revision > verified.revision() {
            self.membership_log.catch_up(channel_id).await?;
            self.verified_for_send(channel_id)?
        } else {
            verified
        };
        if resp.revision != verified.revision() {
            return Err(ChannelError::InvalidInput(format!(
                "group send endorsements are for revision {}, our verified state is at {}",
                resp.revision,
                verified.revision()
            )));
        }
        let members: Vec<uuid::Uuid> = verified
            .state()
            .active_members()
            .map(|(id, _)| uuid::Uuid::parse_str(id))
            .collect::<Result<_, _>>()
            .map_err(|e| ChannelError::InvalidInput(format!("member id: {e}")))?;
        let server = group_send::pinned_server_params().map_err(group_send_error)?;
        let cid = channel_id.to_string();
        let hash = verified.state_hash();
        let expiration = group_send::expiration(&resp.endorsements).map_err(group_send_error)?;
        let token = group_send::full_token(&resp.endorsements, &cid, &hash, &members, now, &server)
            .map_err(group_send_error)?;
        self.group_send_tokens.put(channel_id, hash, expiration, token.clone());
        Ok(token)
    }

    fn verified_for_send(
        &self,
        channel_id: &ChannelId,
    ) -> Result<crate::domain::verified_state::VerifiedChannel, ChannelError> {
        self.membership_log
            .load(channel_id)?
            .ok_or_else(|| ChannelError::InvalidInput("no verified membership for this channel".into()))
    }
}

fn group_send_error(e: group_send::GroupSendError) -> ChannelError {
    ChannelError::InvalidInput(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_is_per_state_and_refreshes_before_expiry() {
        let cache = TokenCache::default();
        let ch = ChannelId::new();
        let (h1, h2) = ([1u8; 32], [2u8; 32]);
        cache.put(&ch, h1, 100_000, b"t".to_vec());
        assert_eq!(cache.get(&ch, &h1, 1_000), Some(b"t".to_vec()));
        assert_eq!(cache.get(&ch, &h2, 1_000), None, "a new verified state needs a new token");
        assert_eq!(cache.get(&ch, &h1, 100_000 - REFRESH_MARGIN_SECS), None, "refetched before expiry");
        assert_eq!(cache.get(&ChannelId::new(), &h1, 1_000), None);
    }
}
