//! Creator-controlled, device-local private mesh channels.
//!
//! Each member owns a random message key, delivered only over authenticated
//! Noise. Packet signatures authenticate senders; this is not MLS and has no
//! post-compromise security against a current member. The QR is an
//! admission request bound to one mesh peer and the creator's full Noise key,
//! never a bearer copy of the key. It is device-targeted within its validity
//! window, not bound to one Noise handshake transcript.

use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use chatter_mesh::ids::PeerId;
use chatter_mesh_noise::MeshIdentity;
use chatter_mesh_noise::sign::{sign_bytes, verify_bytes};
use rand::TryRngCore;
use rand::rngs::OsRng;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use sha2_10::{Digest, Sha256};
use zeroize::Zeroizing;

use super::MeshError;
use crate::storage::Database;

/// uuid 16, creator 8, target 8, creator key 32, creator static 32, nonce 16,
/// expiry 8, signature 64; the name follows.
const INVITE_FIXED_LEN: usize = 184;
const INVITE_PREFIX: &str = "chatter-mesh-private-v3:";
const UPDATE_MAGIC: &[u8; 8] = b"CHTRPCU3";
const FRAME_MAGIC: &[u8; 8] = b"CHTRPSM3";
const DISTRIBUTION_MAGIC: &[u8; 8] = b"CHTRPSD3";
const DISTRIBUTION_ACK_MAGIC: &[u8; 8] = b"CHTRPSA3";
const LEAVE_MAGIC: &str = "CHTRPCL1:";
const MAX_MEMBERS: usize = 16;
const MAX_TEXT: usize = 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Member {
    pub peer: String,
    pub signing_key: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PrivateChannel {
    pub id: String,
    pub name: String,
    pub creator: String,
    pub creator_key: Vec<u8>,
    pub creator_static: Vec<u8>,
    pub epoch: u64,
    pub members: Vec<Member>,
    /// Local-only sender secrets. Never included in a roster update.
    #[serde(default)]
    pub sender_keys: Vec<SenderKey>,
    #[serde(default)]
    pub prior_sender_keys: Vec<PriorSenderKey>,
    /// Peers still owed our current sender key over authenticated Noise.
    #[serde(default)]
    pub pending_distributions: Vec<String>,
    #[serde(default)]
    pub distribution_attempts: Vec<(String, u8)>,
    /// Recent authenticated message UUIDs, independent of packet dedup.
    #[serde(default)]
    pub replay: Vec<String>,
    pub previous_hash: Vec<u8>,
    pub signature: Vec<u8>,
    #[serde(default)]
    pub pending_invites: Vec<Invite>,
    /// Local list of members still awaiting this epoch over Noise.
    #[serde(default)]
    pub awaiting_rekey: Vec<String>,
    /// A newer authenticated frame was seen before its key update arrived.
    #[serde(default)]
    pub stale: bool,
    #[serde(default)]
    pub stale_epoch: u64,
    /// Signed departures received before the creator's next rotation.
    #[serde(default)]
    pub departed: Vec<LeaveNotice>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LeaveNotice {
    pub id: String,
    pub epoch: u64,
    pub peer: String,
    pub signature: Vec<u8>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PendingLeave {
    pub notice: LeaveNotice,
    pub creator: String,
    pub recipients: Vec<String>,
    pub acknowledged: Vec<String>,
}

#[derive(Serialize)]
struct LeavePreimage<'a> {
    domain: &'static str,
    id: &'a str,
    epoch: u64,
    peer: &'a str,
}

#[derive(Serialize, Deserialize)]
pub struct EpochProof {
    domain: String,
    id: String,
    name: String,
    creator: String,
    creator_static: Vec<u8>,
    epoch: u64,
    members: Vec<Member>,
    previous_hash: Vec<u8>,
    signature: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SenderKey {
    pub peer: String,
    pub generation: u64,
    pub key: Zeroizing<Vec<u8>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PriorSenderKey {
    pub sender: SenderKey,
    pub roster_hash: Vec<u8>,
    pub expires_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SenderDistribution {
    domain: String,
    pub id: String,
    pub roster_hash: Vec<u8>,
    pub sender: String,
    pub generation: u64,
    pub key: Zeroizing<Vec<u8>>,
    pub removal_hash: Vec<u8>,
    signature: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Invite {
    pub channel_id: String,
    pub name: String,
    pub creator: String,
    pub creator_key: Vec<u8>,
    pub creator_static: Vec<u8>,
    pub target: String,
    pub nonce: Vec<u8>,
    pub expires_at: u64,
    pub signature: Vec<u8>,
}

#[derive(Serialize)]
struct EpochPreimage<'a> {
    domain: &'static str,
    id: &'a str,
    name: &'a str,
    creator: &'a str,
    creator_static: &'a [u8],
    epoch: u64,
    members: &'a [Member],
    previous_hash: &'a [u8],
}

#[derive(Serialize)]
struct CurrentEpoch<'a> {
    id: &'a str,
    name: &'a str,
    creator: &'a str,
    creator_key: &'a [u8],
    creator_static: &'a [u8],
    epoch: u64,
    members: &'a [Member],
    previous_hash: &'a [u8],
    signature: &'a [u8],
}

fn valid_name(name: &str) -> Result<String, MeshError> {
    let name = name.trim();
    if name.is_empty() || name.len() > 32 || name.chars().any(char::is_control) {
        return Err(failure("channel name must be 1-32 bytes"));
    }
    Ok(name.to_owned())
}
fn failure(reason: &str) -> MeshError {
    MeshError::Binding(reason.into())
}
fn db_error(error: rusqlite::Error) -> MeshError {
    MeshError::Storage(error.into())
}

mod channel;
mod message;
mod protocol;
mod sender;
mod store;

pub use store::PrivateChannelStore;
