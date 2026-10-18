//! Group message operations.
//!
//! Sender key distribution, group mailbox announcements, and N-way fanout delivery.

use crate::domain::ids::{ChannelId, UserId};
use crate::network::grpc::auth_client::AuthClient;
use crate::network::grpc::message_client::MessageClient;
use crate::storage::repositories::settings_repo::keys;
use crate::util::time::TimestampNanos;

use super::{MessageError, MessageService};
use crate::services::channel::verified_state::{
    PeerStateCheck, VerificationStatus, VerifiedChannel, VerifiedMembership, compare_peer_state,
};
use crate::storage::repositories::settings_repo::SettingsRepository;
use crate::storage::repositories::verified_membership_repo::VerifiedMembershipRepository;
use std::sync::Arc;

mod devices;
mod distribution;
mod pending;
mod resend;
mod state;

pub use state::ChannelStateSync;
pub(crate) use state::GroupStateGate;
pub(crate) use state::stamp_group_data_message;

fn sender_key_distribution_result(
    delivered: usize,
    recipients: usize,
    failure: Option<MessageError>,
) -> Result<(), MessageError> {
    if delivered == 0 && recipients > 0 {
        return Err(failure.unwrap_or(MessageError::Network(
            crate::error::NetworkError::Unavailable,
        )));
    }
    Ok(())
}

/// Original recipient devices whose user is still a member.
///
/// An empty `original` stays empty. A missing send-time record is not
/// "everyone who is a member now", and a user absent from `current_members`
/// (they left) is dropped with every device of theirs.
pub(super) fn resend_targets(
    original: &[(UserId, crate::domain::ids::DeviceId)],
    current_members: &[UserId],
) -> Vec<(UserId, crate::domain::ids::DeviceId)> {
    original
        .iter()
        .copied()
        .filter(|(user, _)| current_members.contains(user))
        .collect()
}

/// The verified distribution set as user ids.
fn verified_user_ids(verified: &VerifiedChannel) -> Vec<UserId> {
    verified
        .members()
        .iter()
        .filter_map(|u| u.parse().ok())
        .collect()
}

/// Device ids in a prekey-bundle response (multi-device list or the
/// single-bundle fallback).
fn bundle_device_ids(
    resp: crate::proto::chat_v1::GetPreKeyBundleResponse,
) -> std::collections::HashSet<u32> {
    use crate::proto::chat_v1::get_pre_key_bundle_response::Result;
    match resp.result {
        Some(Result::Devices(list)) => list.bundles.iter().map(|b| b.device_id).collect(),
        Some(Result::Bundle(b)) => [b.device_id].into(),
        None => Default::default(),
    }
}

/// How long a group message parked for a missing sender key waits for its
/// SKDM before we ask the sender to resend it instead.
const PENDING_GROUP_DECRYPT_TTL: std::time::Duration = std::time::Duration::from_secs(300);

/// Whether a cached own-device list that lacks a device must be refetched
/// before calling that device retired: it is over a minute old, or the message
/// is newer than the list (a device linked since). 30s of slack for the gap
/// between the server's clock, which stamps `sent_at`, and ours.
fn own_device_list_stale(fetched_at: u64, now: u64, sent_at: u64) -> bool {
    const MAX_AGE: u64 = 60 * 1_000_000_000;
    const CLOCK_SLACK: u64 = 30 * 1_000_000_000;
    now.saturating_sub(fetched_at) > MAX_AGE || sent_at.saturating_add(CLOCK_SLACK) > fetched_at
}

/// Per sender: start of the current one-minute window, and resend requests
/// sent in it for parked messages the cap evicted.
pub(crate) type EvictionResendGate = std::collections::HashMap<UserId, (std::time::Instant, u32)>;

/// At most 10 eviction resend requests per sender per minute.
pub(crate) fn eviction_resend_due(
    gate: &mut EvictionResendGate,
    sender: UserId,
    now: std::time::Instant,
) -> bool {
    const WINDOW: std::time::Duration = std::time::Duration::from_secs(60);
    const PER_WINDOW: u32 = 10;
    // never pruned; one entry per sender that ever overflowed the cap.
    let entry = gate.entry(sender).or_insert((now, 0));
    if now.duration_since(entry.0) >= WINDOW {
        *entry = (now, 0);
    }
    if entry.1 >= PER_WINDOW {
        return false;
    }
    entry.1 += 1;
    true
}

/// Backoff state per `(channel, sender, device)`: when the next request may go
/// out, and how many have gone out without an answer.
pub(crate) type SenderKeyRequestGate =
    std::collections::HashMap<(ChannelId, UserId, u32), (std::time::Instant, u32)>;

/// Whether `user` (whose sender-key devices are `devices`) has a device that
/// does not hold our current sender key, compared on (user, device,
/// registration id). A member other than us with no known device has never
/// been reached and lacks it; our own user with none has no other device.
fn lacks_sender_key(
    holders: &std::collections::HashSet<(String, u32, u32)>,
    user: &str,
    devices: &[(u32, Option<u32>)],
    is_self: bool,
) -> bool {
    if devices.is_empty() {
        return !is_self;
    }
    devices
        .iter()
        .any(|&(d, r)| r.is_none_or(|r| !holders.contains(&(user.to_string(), d, r))))
}

/// 30s, 60s, 120s ... capped at 15 min between unanswered requests.
fn sender_key_request_due(
    gate: &mut SenderKeyRequestGate,
    key: (ChannelId, UserId, u32),
    now: std::time::Instant,
) -> bool {
    const BASE: std::time::Duration = std::time::Duration::from_secs(30);
    const MAX: std::time::Duration = std::time::Duration::from_secs(15 * 60);
    let entry = gate.entry(key).or_insert((now, 0));
    if now < entry.0 {
        return false;
    }
    let delay = BASE.saturating_mul(1u32 << entry.1.min(10)).min(MAX);
    *entry = (now + delay, entry.1 + 1);
    true
}

#[cfg(test)]
mod tests;
