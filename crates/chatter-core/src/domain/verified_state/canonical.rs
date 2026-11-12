//! The canonical state encoding and its hash, exactly as defined in
//! `pkg/proto/chat/v1/channel/channel.proto` ("State hash"). It is NOT the
//! protobuf encoding of `MembershipState`: integers are big-endian and fixed
//! width, "str"/"bin" are a u32 length then the bytes, members are sorted by
//! user_id bytes and invites by invite_pub bytes (the `BTreeMap` order).

use sha2::{Digest, Sha256};

use super::{MembershipState, StateHash};

pub const STATE_DOMAIN: &[u8] = b"chatter-channel-state-v1\0";
pub const CHANGE_DOMAIN: &[u8] = b"chatter-channel-change-v1\0";
pub const INVITE_DOMAIN: &[u8] = b"chatter-channel-invite-v1\0";
pub const FORMAT_VERSION: u8 = 1;

fn put_bin(out: &mut Vec<u8>, b: &[u8]) {
    // Every field is bounded far below 4 GiB (keys 33 B, UUIDs 36 B).
    out.extend_from_slice(&(b.len() as u32).to_be_bytes());
    out.extend_from_slice(b);
}

/// `canonical_state` (without the domain prefix).
pub fn canonical_state(s: &MembershipState) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + s.members.len() * 80 + s.invites.len() * 64);
    out.push(FORMAT_VERSION);
    put_bin(&mut out, s.channel_id.as_bytes());
    out.extend_from_slice(&s.revision.to_be_bytes());
    let p = &s.policy;
    for r in [
        p.add_member,
        p.remove_member,
        p.set_role,
        p.manage_invites,
        p.set_disappearing,
    ] {
        out.push(r as i32 as u8);
    }
    out.push(p.join as i32 as u8);
    out.extend_from_slice(&s.disappearing_seconds.to_be_bytes());

    out.extend_from_slice(&(s.members.len() as u32).to_be_bytes());
    for (user_id, m) in &s.members {
        put_bin(&mut out, user_id.as_bytes());
        out.push(m.role as i32 as u8);
        out.push(m.status as i32 as u8);
        put_bin(&mut out, &m.identity_key);
    }

    out.extend_from_slice(&(s.invites.len() as u32).to_be_bytes());
    for (invite_pub, i) in &s.invites {
        put_bin(&mut out, invite_pub);
        out.push(i.role as i32 as u8);
        out.extend_from_slice(&i.expires_at.to_be_bytes());
        out.extend_from_slice(&i.max_uses.to_be_bytes());
        out.extend_from_slice(&i.uses.to_be_bytes());
        out.push(u8::from(i.needs_approval));
    }
    out
}

/// `SHA-256("chatter-channel-state-v1\0" || canonical_state)`.
pub fn state_hash(s: &MembershipState) -> StateHash {
    let mut h = Sha256::new();
    h.update(STATE_DOMAIN);
    h.update(canonical_state(s));
    h.finalize().into()
}

/// `domain || bytes`: the message a change signature / invite proof covers.
pub fn signed_message(domain: &[u8], actions: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(domain.len() + actions.len());
    m.extend_from_slice(domain);
    m.extend_from_slice(actions);
    m
}
