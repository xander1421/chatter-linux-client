//! Signed membership changes for the raw-gRPC channel tests.
//!
//! Every membership RPC now takes a `ChannelChange` signed by the caller's
//! identity key and chained to the channel's current state (Design B). The
//! raw tests talk to `ChannelClient` directly, so they author changes here
//! with the test user's own identity key, through the client's own rules
//! (`rules::preview`) and signer (`sign_channel_change`) — the same bytes the
//! services produce.

#![allow(dead_code)]

use chatter_core::crypto::signal::identity::sign_channel_change;
use chatter_core::network::grpc::ChannelClient;
use chatter_core::proto::chat_v1::{self as pb, channel_action::Action};
use chatter_core::services::channel::verified_state::{rules, MembershipState, Policy};
use libsignal_protocol::IdentityKeyPair;

use super::fixtures::TestUser;

fn act(a: Action) -> pb::ChannelAction {
    pb::ChannelAction { action: Some(a) }
}

fn now_nanos() -> u64 {
    chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0) as u64
}

/// Sign `actions` by `user` on top of `prev` (None = genesis).
pub fn signed(user: &TestUser, channel_id: &str, prev: Option<&MembershipState>, actions: Vec<pb::ChannelAction>) -> pb::ChannelChange {
    let kp = IdentityKeyPair::try_from(user.identity_key_pair_bytes.as_slice()).expect("test identity key pair");
    let key: [u8; 33] = kp.public_key().serialize().as_ref().try_into().expect("33-byte key");
    let revision = prev.map_or(1, |p| p.revision + 1);
    let ts = now_nanos();
    let new = rules::preview(prev, &actions, channel_id, revision, &user.user_id, key, ts).expect("change allowed");
    let a = pb::channel_change::Actions {
        channel_id: channel_id.to_string(),
        revision,
        prev_state_hash: prev.map(|p| p.hash().to_vec()).unwrap_or_default(),
        author_user_id: user.user_id.clone(),
        author_identity_key: key.to_vec(),
        timestamp: ts,
        actions,
        new_state_hash: new.hash().to_vec(),
    };
    let (actions, signature) = sign_channel_change(&kp, &a).expect("sign");
    pb::ChannelChange { actions, signature, invite_proof: Vec::new() }
}

/// A fresh channel id and its genesis, `user` the owner.
pub fn genesis(user: &TestUser, public: bool) -> (String, pb::ChannelChange) {
    let channel_id = uuid::Uuid::new_v4().to_string();
    let g = act(Action::Genesis(pb::Genesis { policy: Some(Policy::defaults(public).to_proto()), disappearing_seconds: 0 }));
    let change = signed(user, &channel_id, None, vec![g]);
    (channel_id, change)
}

/// The channel's current state, as the server presents it.
pub async fn current_state(client: &mut ChannelClient, channel_id: &str) -> MembershipState {
    let p = client.get_channel_state(channel_id, 0, &[]).await.expect("GetChannelState");
    MembershipState::from_proto(&p).expect("valid state")
}

/// `user` self-joins an OPEN channel.
pub async fn open_join(user: &TestUser, client: &mut ChannelClient, channel_id: &str) -> pb::ChannelChange {
    let state = current_state(client, channel_id).await;
    signed(user, channel_id, Some(&state), vec![act(Action::JoinViaInvite(pb::JoinViaInvite { invite_pub: Vec::new() }))])
}

/// `user` leaves.
pub async fn leave(user: &TestUser, client: &mut ChannelClient, channel_id: &str) -> pb::ChannelChange {
    let state = current_state(client, channel_id).await;
    signed(
        user,
        channel_id,
        Some(&state),
        vec![act(Action::RemoveMember(pb::RemoveMember {
            user_id: user.user_id.clone(),
            reason: pb::remove_member::Reason::Left as i32,
        }))],
    )
}

fn identity_key(user: &TestUser) -> Vec<u8> {
    IdentityKeyPair::try_from(user.identity_key_pair_bytes.as_slice())
        .expect("test identity key pair")
        .public_key()
        .serialize()
        .to_vec()
}

/// `author` invites `invitee` (signed `AddMember{pending}`).
pub async fn invite(author: &TestUser, client: &mut ChannelClient, channel_id: &str, invitee: &TestUser) -> pb::ChannelChange {
    let state = current_state(client, channel_id).await;
    signed(
        author,
        channel_id,
        Some(&state),
        vec![act(Action::AddMember(pb::AddMember {
            user_id: invitee.user_id.clone(),
            role: pb::ChannelRole::Member as i32,
            identity_key: identity_key(invitee),
            pending: true,
        }))],
    )
}

/// `user` accepts a pending invite.
pub async fn accept(user: &TestUser, client: &mut ChannelClient, channel_id: &str) -> pb::ChannelChange {
    let state = current_state(client, channel_id).await;
    signed(user, channel_id, Some(&state), vec![act(Action::AcceptInvite(pb::AcceptInvite {}))])
}

/// `author` kicks `target_id`.
pub async fn kick(author: &TestUser, client: &mut ChannelClient, channel_id: &str, target_id: &str) -> pb::ChannelChange {
    let state = current_state(client, channel_id).await;
    signed(
        author,
        channel_id,
        Some(&state),
        vec![act(Action::RemoveMember(pb::RemoveMember {
            user_id: target_id.to_string(),
            reason: pb::remove_member::Reason::Kicked as i32,
        }))],
    )
}
