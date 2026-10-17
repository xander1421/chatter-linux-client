//! Creator-controlled, device-local private mesh channels.
//!
//! Review before release: one random XChaCha20-Poly1305 epoch key is shared by
//! the roster. Packet signatures authenticate senders, but this is not Signal
//! Sender Keys or MLS, and it has no post-compromise security within an epoch.
//! Epoch keys move only inside established Noise sessions. The QR is an
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

const INVITE_PREFIX: &str = "chatter-mesh-private-v2:";
const UPDATE_MAGIC: &[u8; 8] = b"CHTRPCU2";
const FRAME_MAGIC: &[u8; 8] = b"CHTRPCM2";
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
    pub key: Zeroizing<Vec<u8>>,
    pub previous_hash: Vec<u8>,
    pub signature: Vec<u8>,
    /// Bounded local holdback for ciphertext delayed across rotations.
    #[serde(default)]
    pub prior_epochs: Vec<PriorEpoch>,
    #[serde(default)]
    pub pending_invites: Vec<Invite>,
    /// Local list of members still awaiting this epoch over Noise.
    #[serde(default)]
    pub awaiting_rekey: Vec<String>,
    /// A newer authenticated frame was seen before its key update arrived.
    #[serde(default)]
    pub stale: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PriorEpoch {
    epoch: u64,
    key: Zeroizing<Vec<u8>>,
    members: Vec<Member>,
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
    key_hash: Vec<u8>,
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
    key: &'a Zeroizing<Vec<u8>>,
    previous_hash: &'a [u8],
    signature: &'a [u8],
}

impl PrivateChannel {
    pub fn create(name: &str, identity: &MeshIdentity) -> Result<Self, MeshError> {
        let name = valid_name(name)?;
        let mut key = Zeroizing::new(vec![0; 32]);
        OsRng
            .try_fill_bytes(&mut key)
            .map_err(|_| failure("randomness unavailable"))?;
        let mut state = Self {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            creator: identity.peer_id().to_string(),
            creator_key: identity.signing_public().to_vec(),
            creator_static: identity.static_public().to_vec(),
            epoch: 1,
            members: vec![Member {
                peer: identity.peer_id().to_string(),
                signing_key: identity.signing_public().to_vec(),
            }],
            key,
            previous_hash: vec![0; 32],
            signature: vec![],
            prior_epochs: vec![],
            pending_invites: vec![],
            awaiting_rekey: vec![],
            stale: false,
        };
        state.sign(identity)?;
        Ok(state)
    }

    fn preimage(&self) -> Result<Vec<u8>, MeshError> {
        serde_json::to_vec(&EpochPreimage {
            domain: "chatter.mesh.private.roster.v1",
            id: &self.id,
            name: &self.name,
            creator: &self.creator,
            creator_static: &self.creator_static,
            epoch: self.epoch,
            members: &self.members,
            key_hash: Sha256::digest(&self.key).to_vec(),
            previous_hash: &self.previous_hash,
        })
        .map_err(|_| failure("invalid channel roster"))
    }

    pub fn chain_hash(&self) -> Result<Vec<u8>, MeshError> {
        Ok(Sha256::digest(self.preimage()?).to_vec())
    }

    fn sign(&mut self, identity: &MeshIdentity) -> Result<(), MeshError> {
        if self.creator != identity.peer_id().as_str() {
            return Err(failure("only the creator can change membership"));
        }
        self.signature = sign_bytes(identity, &self.preimage()?).to_vec();
        Ok(())
    }

    pub fn verify(&self) -> bool {
        if self.key.len() != 32
            || self.epoch == 0
            || uuid::Uuid::parse_str(&self.id).is_err()
            || self.creator_key.len() != 32
            || self.creator_static.len() != 32
            || self.previous_hash.len() != 32
            || self.signature.len() != 64
            || self.members.is_empty()
            || self.members.len() > MAX_MEMBERS
            || self.members[0].peer != self.creator
            || self.members[0].signing_key != self.creator_key
            || self.members.iter().any(|m| {
                m.signing_key.len() != 32 || PeerId::from_str(&m.peer).to_routing_bytes().is_none()
            })
            || self.prior_epochs.len() > 4
            || self
                .prior_epochs
                .iter()
                .any(|prior| prior.key.len() != 32 || prior.epoch >= self.epoch)
            || self
                .members
                .iter()
                .enumerate()
                .any(|(i, m)| self.members[..i].iter().any(|before| before.peer == m.peer))
            || valid_name(&self.name).is_err()
        {
            return false;
        }
        let (Ok(key), Ok(sig), Ok(preimage)) = (
            <[u8; 32]>::try_from(self.creator_key.as_slice()),
            <[u8; 64]>::try_from(self.signature.as_slice()),
            self.preimage(),
        ) else {
            return false;
        };
        verify_bytes(&sig, &preimage, &key)
    }

    pub fn member_key(&self, peer: &PeerId) -> Option<[u8; 32]> {
        self.members
            .iter()
            .find(|m| m.peer == peer.as_str())
            .and_then(|m| m.signing_key.as_slice().try_into().ok())
    }

    pub fn member_key_for_packet(&self, peer: &PeerId, bytes: &[u8]) -> Option<[u8; 32]> {
        if bytes.len() < 24 {
            return None;
        }
        if bytes[8..24] == self.routing_tag() {
            return self.member_key(peer);
        }
        if self.prior_epochs.is_empty() {
            return None;
        }
        // A removed sender cannot revive its old epoch after rotation.
        self.member_key(peer)?;
        self.prior_epochs
            .iter()
            .find(|prior| bytes[8..24] == route_tag(&self.id, prior.epoch, &prior.key))?
            .members
            .iter()
            .find(|member| member.peer == peer.as_str())?
            .signing_key
            .as_slice()
            .try_into()
            .ok()
    }

    pub fn has_route_tag(&self, bytes: &[u8]) -> bool {
        bytes.len() >= 24
            && (bytes[8..24] == self.routing_tag()
                || self
                    .prior_epochs
                    .iter()
                    .any(|prior| bytes[8..24] == route_tag(&self.id, prior.epoch, &prior.key)))
    }

    pub fn routing_tag(&self) -> [u8; 16] {
        route_tag(&self.id, self.epoch, &self.key)
    }

    pub fn invite(
        &mut self,
        identity: &MeshIdentity,
        target: &PeerId,
        creator_static: [u8; 32],
        now: u64,
    ) -> Result<String, MeshError> {
        if self.creator != identity.peer_id().as_str() {
            return Err(failure("only the creator can invite"));
        }
        if self.creator_static.as_slice() != creator_static.as_slice() {
            return Err(failure("creator Noise identity changed"));
        }
        if self.members.len() >= MAX_MEMBERS {
            return Err(failure("private channel is full (16 members)"));
        }
        if target.to_routing_bytes().is_none() {
            return Err(failure("invite target is not a mesh device"));
        }
        if self.member_key(target).is_some() {
            return Err(failure("peer is already a member"));
        }
        let mut nonce = vec![0; 16];
        OsRng
            .try_fill_bytes(&mut nonce)
            .map_err(|_| failure("randomness unavailable"))?;
        let mut invite = Invite {
            channel_id: self.id.clone(),
            name: self.name.clone(),
            creator: self.creator.clone(),
            creator_key: self.creator_key.clone(),
            creator_static: creator_static.to_vec(),
            target: target.to_string(),
            nonce,
            expires_at: now.saturating_add(600),
            signature: vec![],
        };
        invite.signature = sign_bytes(identity, &invite.preimage()?).to_vec();
        self.pending_invites
            .retain(|i| i.expires_at >= now && i.target != target.as_str());
        if self.pending_invites.len() >= 32 {
            return Err(failure("too many pending private channel invites"));
        }
        self.pending_invites.push(invite.clone());
        let bytes = serde_json::to_vec(&invite).map_err(|_| failure("invalid invite"))?;
        Ok(format!("{INVITE_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes)))
    }

    pub fn rotate(
        &mut self,
        identity: &MeshIdentity,
        member: Option<Member>,
        remove: Option<&PeerId>,
    ) -> Result<(), MeshError> {
        if self.creator != identity.peer_id().as_str() {
            return Err(failure("membership is frozen until the creator returns"));
        }
        if let Some(ref member) = member {
            if self.members.len() >= MAX_MEMBERS
                || member.signing_key.len() != 32
                || self.members.iter().any(|m| m.peer == member.peer)
            {
                return Err(failure("invalid or full roster"));
            }
        }
        if let Some(remove) = remove {
            if remove.as_str() == self.creator {
                return Err(failure("creator cannot leave"));
            }
            if !self.members.iter().any(|m| m.peer == remove.as_str()) {
                return Err(failure("peer is not a member"));
            }
        }
        self.previous_hash = Sha256::digest(self.preimage()?).to_vec();
        if remove.is_some() {
            self.prior_epochs.clear();
        } else {
            self.remember_current();
        }
        if let Some(member) = member {
            self.members.push(member);
        }
        if let Some(remove) = remove {
            self.members.retain(|m| m.peer != remove.as_str());
        }
        self.awaiting_rekey = self
            .members
            .iter()
            .filter(|m| m.peer != self.creator)
            .map(|m| m.peer.clone())
            .collect();
        self.epoch = self
            .epoch
            .checked_add(1)
            .ok_or_else(|| failure("epoch exhausted"))?;
        OsRng
            .try_fill_bytes(&mut self.key)
            .map_err(|_| failure("randomness unavailable"))?;
        self.sign(identity)
    }

    pub fn update_bytes(&self) -> Result<Zeroizing<Vec<u8>>, MeshError> {
        let mut out = Zeroizing::new(UPDATE_MAGIC.to_vec());
        // Serialize only the signed current roster; local state never crosses Noise.
        let wire = CurrentEpoch {
            id: &self.id,
            name: &self.name,
            creator: &self.creator,
            creator_key: &self.creator_key,
            creator_static: &self.creator_static,
            epoch: self.epoch,
            members: &self.members,
            key: &self.key,
            previous_hash: &self.previous_hash,
            signature: &self.signature,
        };
        out.extend(serde_json::to_vec(&wire).map_err(|_| failure("invalid roster"))?);
        Ok(out)
    }

    pub fn decode_update(bytes: &[u8]) -> Option<Self> {
        if !bytes.starts_with(UPDATE_MAGIC) || bytes.len() > 8192 {
            return None;
        }
        let state: Self = serde_json::from_slice(&bytes[8..]).ok()?;
        (state.pending_invites.is_empty()
            && state.prior_epochs.is_empty()
            && state.awaiting_rekey.is_empty()
            && !state.stale
            && state.verify())
        .then_some(state)
    }

    pub fn encrypt(&self, sender: &PeerId, text: &str) -> Result<Vec<u8>, MeshError> {
        self.encrypt_with_id(sender, text, uuid::Uuid::new_v4())
    }

    pub fn encrypt_with_id(
        &self,
        sender: &PeerId,
        text: &str,
        message_id: uuid::Uuid,
    ) -> Result<Vec<u8>, MeshError> {
        if self.member_key(sender).is_none() || text.is_empty() || text.len() > MAX_TEXT {
            return Err(failure("invalid private channel sender or text"));
        }
        let mut nonce = [0; 24];
        OsRng
            .try_fill_bytes(&mut nonce)
            .map_err(|_| failure("randomness unavailable"))?;
        let mut header = FRAME_MAGIC.to_vec();
        header.extend_from_slice(&self.routing_tag());
        header.extend_from_slice(&self.epoch.to_be_bytes());
        header.extend_from_slice(message_id.as_bytes());
        header.extend_from_slice(&nonce);
        let mut aad = header.clone();
        aad.extend_from_slice(sender.as_str().as_bytes());
        let cipher = XChaCha20Poly1305::new_from_slice(&self.key)
            .map_err(|_| failure("invalid epoch key"))?;
        let ciphertext = cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: text.as_bytes(),
                    aad: &aad,
                },
            )
            .map_err(|_| failure("private channel encryption failed"))?;
        header.extend(ciphertext);
        Ok(header)
    }

    pub fn decrypt(&self, sender: &PeerId, bytes: &[u8]) -> Option<(String, String)> {
        if bytes.len() < 72 {
            return None;
        }
        if bytes[8..24] == self.routing_tag() {
            return decrypt_with(
                &self.id,
                self.epoch,
                &self.key,
                &self.members,
                sender,
                bytes,
            );
        }
        let prior = self
            .prior_epochs
            .iter()
            .find(|prior| bytes[8..24] == route_tag(&self.id, prior.epoch, &prior.key))?;
        decrypt_with(
            &self.id,
            prior.epoch,
            &prior.key,
            &prior.members,
            sender,
            bytes,
        )
    }

    pub fn remember_previous(&mut self, old: &Self) {
        self.prior_epochs = old.prior_epochs.clone();
        self.prior_epochs.push(PriorEpoch {
            epoch: old.epoch,
            key: old.key.clone(),
            members: old.members.clone(),
        });
        if self.prior_epochs.len() > 4 {
            self.prior_epochs.remove(0);
        }
    }

    fn remember_current(&mut self) {
        self.prior_epochs.push(PriorEpoch {
            epoch: self.epoch,
            key: self.key.clone(),
            members: self.members.clone(),
        });
        if self.prior_epochs.len() > 4 {
            self.prior_epochs.remove(0);
        }
    }
}

fn route_tag(id: &str, epoch: u64, key: &[u8]) -> [u8; 16] {
    let mut hash = Sha256::new();
    hash.update(b"chatter.mesh.private.route.v1\0");
    hash.update(id.as_bytes());
    hash.update(epoch.to_be_bytes());
    hash.update(key);
    let digest = hash.finalize();
    digest[..16].try_into().expect("digest length")
}

fn decrypt_with(
    id: &str,
    epoch: u64,
    key: &[u8],
    members: &[Member],
    sender: &PeerId,
    bytes: &[u8],
) -> Option<(String, String)> {
    if bytes.len() < 72 + 16
        || bytes.len() > 72 + MAX_TEXT + 16
        || !bytes.starts_with(FRAME_MAGIC)
        || bytes[8..24] != route_tag(id, epoch, key)
        || bytes[24..32] != epoch.to_be_bytes()
        || !members.iter().any(|m| m.peer == sender.as_str())
    {
        return None;
    }
    let nonce = XNonce::from_slice(&bytes[48..72]);
    let mut aad = bytes[..72].to_vec();
    aad.extend_from_slice(sender.as_str().as_bytes());
    let cipher = XChaCha20Poly1305::new_from_slice(key).ok()?;
    let plaintext = cipher
        .decrypt(
            nonce,
            Payload {
                msg: &bytes[72..],
                aad: &aad,
            },
        )
        .ok()?;
    let text = String::from_utf8(plaintext).ok()?;
    if text.is_empty() {
        return None;
    }
    let id = uuid::Uuid::from_bytes(bytes[32..48].try_into().ok()?).to_string();
    Some((id, text))
}

impl Invite {
    fn preimage(&self) -> Result<Vec<u8>, MeshError> {
        serde_json::to_vec(&(
            "chatter.mesh.private.invite.v1",
            &self.channel_id,
            &self.name,
            &self.creator,
            &self.creator_key,
            &self.creator_static,
            &self.target,
            &self.nonce,
            self.expires_at,
        ))
        .map_err(|_| failure("invalid invite"))
    }
    pub fn from_text(text: &str, now: u64) -> Option<Self> {
        let data = URL_SAFE_NO_PAD
            .decode(text.strip_prefix(INVITE_PREFIX)?)
            .ok()?;
        if data.len() > 1024 {
            return None;
        }
        let invite: Self = serde_json::from_slice(&data).ok()?;
        if invite.creator_key.len() != 32
            || invite.creator_static.len() != 32
            || invite.signature.len() != 64
            || invite.nonce.len() != 16
            || uuid::Uuid::parse_str(&invite.channel_id).is_err()
            || PeerId::from_str(&invite.creator)
                .to_routing_bytes()
                .is_none()
            || PeerId::from_str(&invite.target)
                .to_routing_bytes()
                .is_none()
            || invite.expires_at < now
            || invite.expires_at > now.saturating_add(600)
            || valid_name(&invite.name).is_err()
        {
            return None;
        }
        let key = invite.creator_key.as_slice().try_into().ok()?;
        let sig = invite.signature.as_slice().try_into().ok()?;
        verify_bytes(&sig, &invite.preimage().ok()?, &key).then_some(invite)
    }
    pub fn request_bytes(&self) -> Option<Vec<u8>> {
        let mut bytes = b"CHTRPCR2".to_vec();
        bytes.extend(serde_json::to_vec(self).ok()?);
        Some(bytes)
    }
    pub fn decode_request(bytes: &[u8], now: u64) -> Option<Self> {
        if !bytes.starts_with(b"CHTRPCR2") || bytes.len() > 1032 {
            return None;
        }
        let text = format!("{INVITE_PREFIX}{}", URL_SAFE_NO_PAD.encode(&bytes[8..]));
        Self::from_text(&text, now)
    }
}

pub struct PrivateChannelStore {
    db: Arc<Database>,
}
impl PrivateChannelStore {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }
    /// Redemption and nonce consumption share one SQL transaction.
    pub fn redeem(
        &self,
        invite: &Invite,
        identity: &MeshIdentity,
        member: Member,
    ) -> Result<Option<PrivateChannel>, MeshError> {
        use rusqlite::OptionalExtension;
        let mut conn = self.db.connection()?;
        let tx = conn.transaction().map_err(db_error)?;
        let bytes: Option<Vec<u8>> = tx
            .query_row(
                "SELECT state FROM mesh_private_channels WHERE channel_id=?1",
                params![invite.channel_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_error)?;
        let Some(bytes) = bytes.map(Zeroizing::new) else {
            return Ok(None);
        };
        let mut state: PrivateChannel =
            serde_json::from_slice(&bytes).map_err(|_| failure("corrupt channel state"))?;
        if !state.verify()
            || state.creator != invite.creator
            || state.creator_key != invite.creator_key
            || state.creator_static != invite.creator_static
            || invite.expires_at < super::now_secs()
            || !state.pending_invites.iter().any(|issued| {
                issued.nonce == invite.nonce
                    && issued.target == invite.target
                    && issued.signature == invite.signature
            })
        {
            return Ok(None);
        }
        state.rotate(identity, Some(member), None)?;
        state
            .pending_invites
            .retain(|issued| issued.nonce != invite.nonce);
        let new_bytes = Zeroizing::new(
            serde_json::to_vec(&state).map_err(|_| failure("invalid channel state"))?,
        );
        tx.execute(
            "UPDATE mesh_private_channels SET state=?2 WHERE channel_id=?1",
            params![invite.channel_id, new_bytes.as_slice()],
        )
        .map_err(db_error)?;
        tx.commit().map_err(db_error)?;
        Ok(Some(state))
    }
    pub fn save(&self, state: &PrivateChannel) -> Result<(), MeshError> {
        use rusqlite::OptionalExtension;
        if !state.verify() {
            return Err(failure("invalid private channel state"));
        }
        let bytes = Zeroizing::new(
            serde_json::to_vec(state).map_err(|_| failure("invalid channel state"))?,
        );
        let mut conn = self.db.connection()?;
        let tx = conn.transaction().map_err(db_error)?;
        let previous: Option<Vec<u8>> = tx
            .query_row(
                "SELECT state FROM mesh_private_channels WHERE channel_id=?1",
                params![state.id],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_error)?;
        if let Some(previous) = previous.map(Zeroizing::new) {
            let old: PrivateChannel =
                serde_json::from_slice(&previous).map_err(|_| failure("corrupt channel state"))?;
            if !old.verify()
                || state.epoch < old.epoch
                || (state.epoch == old.epoch && state.signature != old.signature)
            {
                return Err(failure("stale or conflicting channel epoch"));
            }
        }
        tx.execute(
            "INSERT INTO mesh_private_channels(channel_id,state) VALUES(?1,?2)
            ON CONFLICT(channel_id) DO UPDATE SET state=excluded.state",
            params![state.id, bytes.as_slice()],
        )
        .map_err(db_error)?;
        tx.commit().map_err(db_error)?;
        Ok(())
    }
    pub fn load(&self, id: &str) -> Result<Option<PrivateChannel>, MeshError> {
        use rusqlite::OptionalExtension;
        let conn = self.db.read_connection()?;
        let bytes: Option<Vec<u8>> = conn
            .query_row(
                "SELECT state FROM mesh_private_channels WHERE channel_id=?1",
                params![id],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_error)?;
        bytes
            .map(|b| {
                let b = Zeroizing::new(b);
                serde_json::from_slice::<PrivateChannel>(&b)
                    .map_err(|_| failure("corrupt channel state"))
            })
            .transpose()
            .and_then(|state| {
                if state.as_ref().is_some_and(|s| !s.verify()) {
                    Err(failure("invalid channel signature"))
                } else {
                    Ok(state)
                }
            })
    }
    pub fn list(&self) -> Result<Vec<PrivateChannel>, MeshError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn
            .prepare("SELECT state FROM mesh_private_channels")
            .map_err(db_error)?;
        let rows = stmt
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .map_err(db_error)?;
        let mut out = Vec::new();
        for row in rows {
            let bytes = Zeroizing::new(row.map_err(db_error)?);
            let state: PrivateChannel =
                serde_json::from_slice(&bytes).map_err(|_| failure("corrupt channel state"))?;
            if !state.verify() {
                return Err(failure("invalid channel signature"));
            }
            out.push(state);
        }
        Ok(out)
    }
    pub fn remove(&self, id: &str) -> Result<(), MeshError> {
        self.db
            .connection()?
            .execute(
                "DELETE FROM mesh_private_channels WHERE channel_id=?1",
                params![id],
            )
            .map_err(db_error)?;
        Ok(())
    }
    pub fn save_pending(&self, invite: &Invite) -> Result<(), MeshError> {
        let bytes = serde_json::to_vec(invite).map_err(|_| failure("invalid invite"))?;
        self.db
            .connection()?
            .execute(
                "INSERT INTO mesh_private_joins(channel_id,invite) VALUES(?1,?2)
            ON CONFLICT(channel_id) DO UPDATE SET invite=excluded.invite",
                params![invite.channel_id, bytes],
            )
            .map_err(db_error)?;
        Ok(())
    }
    pub fn pending(&self, id: &str) -> Result<Option<Invite>, MeshError> {
        use rusqlite::OptionalExtension;
        let conn = self.db.read_connection()?;
        let bytes: Option<Vec<u8>> = conn
            .query_row(
                "SELECT invite FROM mesh_private_joins WHERE channel_id=?1",
                params![id],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_error)?;
        bytes
            .map(|b| serde_json::from_slice(&b).map_err(|_| failure("corrupt pending invite")))
            .transpose()
    }
    pub fn clear_pending(&self, id: &str) -> Result<(), MeshError> {
        self.db
            .connection()?
            .execute(
                "DELETE FROM mesh_private_joins WHERE channel_id=?1",
                params![id],
            )
            .map_err(db_error)?;
        Ok(())
    }
    pub fn wipe(&self) -> Result<(), MeshError> {
        self.db
            .connection()?
            .execute("DELETE FROM mesh_private_channels", [])
            .map_err(db_error)?;
        self.db
            .connection()?
            .execute("DELETE FROM mesh_private_joins", [])
            .map_err(db_error)?;
        Ok(())
    }
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
