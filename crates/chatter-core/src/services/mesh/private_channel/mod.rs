//! Creator managed mesh groups using bitchat's signed state and epoch key.

use super::MeshError;
use crate::storage::Database;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use chatter_mesh::ids::PeerId;
use chatter_mesh_noise::sign::{sign_bytes, verify_bytes};
use chatter_mesh_noise::MeshIdentity;
use rand::TryRngCore;
use rand::rngs::OsRng;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use sha2_10::{Digest, Sha256};
use std::sync::Arc;
use zeroize::Zeroizing;

const INVITE_FIXED_LEN: usize = 184;
const INVITE_PREFIX: &str = "chatter-mesh-private-v3:";
const LEAVE_MAGIC: &str = "CHTRPCL1:";
const MAX_MEMBERS: usize = 16;
const MAX_TEXT: usize = 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Member {
    pub peer: String,
    pub fingerprint: String,
    pub signing_key: Vec<u8>,
    pub nickname: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PrivateChannel {
    pub id: String,
    pub name: String,
    pub creator: String,
    pub creator_key: Vec<u8>,
    pub creator_static: Vec<u8>,
    pub epoch: u32,
    pub members: Vec<Member>,
    pub key: Zeroizing<Vec<u8>>,
    pub signature: Vec<u8>,
    #[serde(default)]
    pub replay: Vec<String>,
    #[serde(default)]
    pub pending_invites: Vec<Invite>,
    #[serde(default)]
    pub awaiting_rekey: Vec<String>,
    #[serde(default)]
    pub stale: bool,
    #[serde(default)]
    pub stale_epoch: u32,
    #[serde(default)]
    pub departed: Vec<LeaveNotice>,
    #[serde(default)]
    pub hint_signature: Vec<u8>,
    #[serde(default)]
    pub removed_members: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LeaveNotice {
    pub id: String,
    pub epoch: u32,
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
    epoch: u32,
    peer: &'a str,
}

#[derive(Serialize, Deserialize)]
pub struct EpochProof {
    id: String,
    epoch: u32,
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

fn valid_name(name: &str) -> Result<String, MeshError> {
    let name = name.trim();
    if name.is_empty() || name.len() > 40 || name.chars().any(char::is_control) {
        return Err(failure("group name must be 1-40 bytes"));
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
mod store;
pub use store::PrivateChannelStore;
