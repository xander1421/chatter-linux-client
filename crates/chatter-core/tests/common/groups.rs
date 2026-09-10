//! Group fixtures: create a channel and bring the named users into it the way
//! a real user gets in — by ACCEPTING an invitation.
//!
//! # Why this exists (AUTHZ-CREATE-1)
//!
//! `CreateChannel` used to force-join every id in `initial_member_ids` into
//! `channel_members` and into SpiceDB as `member`, with no consent check, no
//! block check and no authorization check — any user could put any other user
//! in a channel. That is fixed on the server: the requested users are now
//! INVITED (a PENDING `channel_invitations` row + a SpiceDB `invited`
//! relation), and only `RespondToInvite(accept = true)` promotes an invitee to
//! a member.
//!
//! So `create_channel(..., vec![b, c])` no longer yields a group containing B
//! and C, and every proof that asserted delivery to B and C started failing at
//! its membership positive control — correctly, because those assertions had
//! become vacuous. This helper restores the *precondition* (B and C really are
//! members) without weakening the assertion: it drives the same three RPCs a
//! human does — create, discover the invite, accept — and it does not return
//! until the SERVER lists every invitee as a member.
//!
//! It deliberately talks to the same façade the shipping Slint client uses
//! (`ChatterClient::list_pending_channel_invites` /
//! `respond_to_channel_invite` / `sync`), so a regression in the real accept
//! path shows up here rather than being papered over by a test-only shortcut.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use chatter_core::client::ChatterClient;
use chatter_core::domain::channel::{Channel, ChannelConfig, ChannelType};
use chatter_core::domain::ids::{ChannelId, UserId};

/// How long to wait for one server-side condition (invite visible, membership
/// recorded). Generous on purpose: the shared dev stack runs these proofs
/// concurrently, and channel creation is a Temporal workflow.
const GROUP_TIMEOUT: Duration = Duration::from_secs(90);

/// Poll interval for the waits below. The probes are server RPCs, so this is
/// deliberately not a tight loop.
const GROUP_POLL: Duration = Duration::from_millis(500);

/// One invitee: the client that will accept, and the user id it registered as.
pub type Invitee<'a> = (&'a Arc<ChatterClient>, &'a UserId);

/// Create a channel owned by `creator` and return only once every invitee has
/// accepted and the SERVER records them as a member.
///
/// Panics — loudly, and naming the step that failed — if the invite never
/// becomes visible to the invitee, if the accept RPC fails, or if the server
/// still does not list the invitee as a member. A test whose positive control
/// depends on this membership must not proceed on a maybe.
pub async fn create_group_with_members(
    creator: &Arc<ChatterClient>,
    name: &str,
    channel_type: ChannelType,
    config: Option<ChannelConfig>,
    description: Option<&str>,
    invitees: &[Invitee<'_>],
) -> Channel {
    let invited_ids: Vec<UserId> = invitees.iter().map(|(_, uid)| (*uid).clone()).collect();

    let channel = creator
        .create_channel(
            name,
            channel_type,
            config,
            description,
            None,
            None,
            invited_ids.clone(),
        )
        .await
        .unwrap_or_else(|e| panic!("create_channel({name}) failed: {e:?}"));

    for (client, uid) in invitees {
        accept_channel_invite(client, uid, &channel.id).await;
    }

    wait_for_server_membership(creator, &channel.id, &invited_ids).await;

    channel
}

/// Have one invitee discover and accept its invitation to `channel_id`, then
/// bring its local state up to date the way a fresh login would.
///
/// The three post-accept steps mirror `ChatterClient::join_channel` — the
/// other way into a channel — because `respond_to_channel_invite` does none of
/// them itself (see `docs/OPEN-ITEMS.md` §69).
pub async fn accept_channel_invite(
    invitee: &Arc<ChatterClient>,
    invitee_id: &UserId,
    channel_id: &ChannelId,
) {
    wait_for_pending_invite(invitee, invitee_id, channel_id).await;

    invitee
        .respond_to_channel_invite(channel_id, true)
        .await
        .unwrap_or_else(|e| {
            panic!("invitee {invitee_id} failed to accept the invite to {channel_id}: {e:?}")
        });

    // (1) pull the channel row + its membership list into the local store,
    // (2) make the live message stream cover the newly joined channel, and
    // (3) ask the existing members for their sender keys, or the invitee
    //     cannot decrypt anything they send next.
    invitee.sync().await;
    let _ = invitee.channels().sync_channel_members(channel_id).await;
    invitee.resubscribe_channel_messages();
    if let Err(e) = invitee
        .messages()
        .request_skdms_from_channel_members(channel_id)
        .await
    {
        // Not fatal: the SKDM also rides the first message. A failure here
        // costs latency, not correctness, and the tests warm up anyway.
        eprintln!("  request_skdms_from_channel_members({channel_id}) failed: {e:?}");
    }
}

/// Wait until `channel_id` shows up in the invitee's own pending-invite list.
///
/// This is the discovery half of the flow — the same `ListPendingInvites` call
/// the invites modal makes — so a server that creates the channel but never
/// creates the invitation fails HERE, with the reason, instead of surfacing 90
/// seconds later as "not a member".
async fn wait_for_pending_invite(
    invitee: &Arc<ChatterClient>,
    invitee_id: &UserId,
    channel_id: &ChannelId,
) {
    let wanted = channel_id.to_string();
    let deadline = Instant::now() + GROUP_TIMEOUT;
    loop {
        let invites = invitee.list_pending_channel_invites().await;
        if invites.iter().any(|i| i.channel_id == wanted) {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "INVITE NEVER ARRIVED: {invitee_id} has no pending invitation to {channel_id} \
                 after {GROUP_TIMEOUT:?} (saw {} other pending invite(s)). CreateChannel is \
                 supposed to create a PENDING channel_invitations row for every requested \
                 member (AUTHZ-CREATE-1); without it the user has no way into the channel.",
                invites.len()
            );
        }
        tokio::time::sleep(GROUP_POLL).await;
    }
}

/// Wait until the SERVER lists every id in `expected` as a member of the
/// channel. Asked over the wire, not against the creator's local cache, so it
/// proves the thing the tests' positive controls go on to assert.
async fn wait_for_server_membership(
    creator: &Arc<ChatterClient>,
    channel_id: &ChannelId,
    expected: &[UserId],
) {
    let deadline = Instant::now() + GROUP_TIMEOUT;
    let mut last: Vec<UserId> = Vec::new();
    loop {
        last = creator
            .channels()
            .get_all_channel_members(channel_id)
            .await
            .map(|ms| ms.into_iter().map(|m| m.user_id).collect())
            .unwrap_or(last);

        if expected.iter().all(|uid| last.contains(uid)) {
            return;
        }
        if Instant::now() >= deadline {
            let missing: Vec<String> = expected
                .iter()
                .filter(|uid| !last.contains(uid))
                .map(|uid| uid.to_string())
                .collect();
            panic!(
                "ACCEPTED INVITE DID NOT PRODUCE MEMBERSHIP: {} is/are still not member(s) of \
                 {channel_id} after {GROUP_TIMEOUT:?}; the server lists {} member(s). Every \
                 delivery assertion below would be vacuous.",
                missing.join(", "),
                last.len()
            );
        }
        tokio::time::sleep(GROUP_POLL).await;
    }
}
