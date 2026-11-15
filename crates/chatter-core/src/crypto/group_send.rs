//! Group send endorsements (libsignal `zkgroup` group_send_endorsement): the
//! membership proof an anonymous channel send carries instead of the sender's
//! identity (Signal: `GroupSendEndorsementsResponse::receive_with_service_ids`
//! on the client, `GroupSendFullToken::verify` on the chat server).
//!
//! The server issues them (`ChannelService.GetGroupSendEndorsements`) over the
//! ACTIVE members of the verified membership state plus an epoch pseudo-member
//! bound to the state hash; see `tools/username-verify/src/group_send.rs` in
//! the server repo for why, and for the issuing half. This module is the
//! receiving half: it checks the issuance under the PINNED server params
//! (`config::GROUP_SEND_PUBLIC_PARAMS_HEX`) over the set computed from OUR
//! verified state, so a server that endorsed another set, another epoch, or
//! used another key for us than for other members is refused here. The token
//! is a function of channel, set and expiry only: every member sends the same
//! bytes.

use libsignal_core::{Aci, ServiceId};
use sha2_10::{Digest, Sha256};
use zkgroup::groups::{
    GroupMasterKey, GroupSecretParams, GroupSendEndorsement, GroupSendEndorsementsResponse,
};
use zkgroup::{ServerPublicParams, Timestamp};

pub const MASTER_DOMAIN: &[u8] = b"chatter-group-send-master-v1\0";
pub const EPOCH_DOMAIN: &[u8] = b"chatter-group-send-epoch-v1\0";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GroupSendError {
    #[error("group send endorsements do not verify for our verified membership")]
    Verification,
    #[error("malformed group send endorsements")]
    Malformed,
    #[error("no members to endorse")]
    NoMembers,
}

/// The public, per-channel group key (the server knows the members anyway).
pub fn group_params(channel_id: &str) -> GroupSecretParams {
    let key: [u8; 32] = Sha256::new()
        .chain_update(MASTER_DOMAIN)
        .chain_update(channel_id.as_bytes())
        .finalize()
        .into();
    GroupSecretParams::derive_from_master_key(GroupMasterKey::new(key))
}

/// The epoch pseudo-member of (channel, verified state hash).
pub fn epoch_member(channel_id: &str, state_hash: &[u8; 32]) -> ServiceId {
    let d = Sha256::new()
        .chain_update(EPOCH_DOMAIN)
        .chain_update(channel_id.as_bytes())
        .chain_update(state_hash)
        .finalize();
    let mut b = [0u8; 16];
    b.copy_from_slice(&d[..16]);
    Aci::from_uuid_bytes(b).into()
}

/// The pinned server params this build verifies issuances under.
pub fn pinned_server_params() -> Result<ServerPublicParams, GroupSendError> {
    let bytes =
        hex::decode(crate::config::GROUP_SEND_PUBLIC_PARAMS_HEX).map_err(|_| GroupSendError::Malformed)?;
    zkgroup::deserialize(&bytes).map_err(|_| GroupSendError::Malformed)
}

/// The expiration (epoch seconds) of an issuance.
pub fn expiration(response: &[u8]) -> Result<u64, GroupSendError> {
    zkgroup::deserialize::<GroupSendEndorsementsResponse>(response)
        .map(|r| r.expiration().epoch_seconds())
        .map_err(|_| GroupSendError::Malformed)
}

/// Verify an issuance for `members` (the ACTIVE members of our verified state
/// at `state_hash`) and turn it into the serialized `GroupSendFullToken` a
/// send to all of them presents. `now` is epoch seconds.
pub fn full_token(
    response: &[u8],
    channel_id: &str,
    state_hash: &[u8; 32],
    members: &[uuid::Uuid],
    now: u64,
    server: &ServerPublicParams,
) -> Result<Vec<u8>, GroupSendError> {
    if members.is_empty() {
        return Err(GroupSendError::NoMembers);
    }
    let response = zkgroup::deserialize::<GroupSendEndorsementsResponse>(response)
        .map_err(|_| GroupSendError::Malformed)?;
    let expiration = response.expiration();
    let mut set: Vec<ServiceId> = members
        .iter()
        .map(|u| Aci::from_uuid_bytes(*u.as_bytes()).into())
        .collect();
    set.push(epoch_member(channel_id, state_hash));
    let params = group_params(channel_id);
    let received = response
        .receive_with_service_ids(set, Timestamp::from_epoch_seconds(now), &params, server)
        .map_err(|_| GroupSendError::Verification)?;
    let combined = GroupSendEndorsement::combine(received.iter().map(|e| e.decompressed));
    Ok(zkgroup::serialize(
        &combined.to_token(params).into_full_token(expiration),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zkgroup::ServerSecretParams;
    use zkgroup::groups::GroupSendDerivedKeyPair;

    const CH: &str = "0b5e1c1e-7c2a-4a43-9d7e-0000000000aa";
    const NOW: u64 = 1_800_000_000;

    /// Server-side derivations and token, asserted by the same values in
    /// tools/username-verify/src/group_send.rs (EPOCH_VECTOR, GO_TOKEN_VECTOR)
    /// and internal/utils/libsignal/groupsend_test.go.
    const EPOCH_VECTOR: &str = "293fcacdbe5dabe1cdcc79b9b3e32aa1";
    const GO_TOKEN_VECTOR: &str =
        "001000000000000000f41539de8136fbb3be8ab81658b087fa80044c6b00000000";

    fn members(n: u8) -> Vec<uuid::Uuid> {
        (1..=n).map(|i| uuid::Uuid::from_bytes([i; 16])).collect()
    }

    /// The server's issuance (tools/username-verify/src/group_send.rs `issue`).
    fn issue(secret: &ServerSecretParams, hash: &[u8; 32], ms: &[uuid::Uuid], randomness: [u8; 32]) -> Vec<u8> {
        let params = group_params(CH);
        let expiration =
            GroupSendEndorsementsResponse::default_expiration(Timestamp::from_epoch_seconds(NOW));
        let kp = GroupSendDerivedKeyPair::for_expiration(expiration, secret);
        let mut set: Vec<ServiceId> = ms.iter().map(|u| Aci::from_uuid_bytes(*u.as_bytes()).into()).collect();
        set.push(epoch_member(CH, hash));
        zkgroup::serialize(&GroupSendEndorsementsResponse::issue(
            set.into_iter().map(|s| params.encrypt_service_id(s)),
            &kp,
            randomness,
        ))
    }

    #[test]
    fn derivations_match_the_server() {
        assert_eq!(hex::encode(epoch_member(CH, &[7u8; 32]).service_id_binary()), EPOCH_VECTOR);
    }

    #[test]
    fn token_matches_the_one_the_go_server_accepts() {
        let secret = ServerSecretParams::generate([3u8; 32]);
        let resp = issue(&secret, &[9u8; 32], &members(3), [4u8; 32]);
        let token = full_token(&resp, CH, &[9u8; 32], &members(3), NOW, &secret.get_public_params()).unwrap();
        assert_eq!(hex::encode(token), GO_TOKEN_VECTOR);
    }

    #[test]
    fn refuses_issuance_for_another_set_epoch_or_key() {
        let secret = ServerSecretParams::generate([3u8; 32]);
        let public = secret.get_public_params();
        let h = [9u8; 32];
        let honest = issue(&secret, &h, &members(3), [4u8; 32]);
        // Every member, whatever the issuance randomness, gets the same token.
        let again = issue(&secret, &h, &members(3), [5u8; 32]);
        assert_eq!(
            full_token(&honest, CH, &h, &members(3), NOW, &public),
            full_token(&again, CH, &h, &members(3), NOW, &public)
        );
        let err = |resp: &[u8], ms: &[uuid::Uuid], hash: &[u8; 32], p: &ServerPublicParams| {
            full_token(resp, CH, hash, ms, NOW, p).unwrap_err()
        };
        // Server tags us with a different epoch / member set.
        assert_eq!(err(&issue(&secret, &[1u8; 32], &members(3), [4u8; 32]), &members(3), &h, &public), GroupSendError::Verification);
        assert_eq!(err(&issue(&secret, &h, &members(4), [4u8; 32]), &members(3), &h, &public), GroupSendError::Verification);
        // Issued under a key other than the pinned one.
        let other = ServerSecretParams::generate([6u8; 32]);
        assert_eq!(err(&issue(&other, &h, &members(3), [4u8; 32]), &members(3), &h, &public), GroupSendError::Verification);
        assert_eq!(err(b"junk", &members(3), &h, &public), GroupSendError::Malformed);
        assert_eq!(full_token(&honest, CH, &h, &[], NOW, &public).unwrap_err(), GroupSendError::NoMembers);
    }

    #[test]
    fn pinned_params_parse() {
        pinned_server_params().expect("the pinned group send params parse");
    }
}
