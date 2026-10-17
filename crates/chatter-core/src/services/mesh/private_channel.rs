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

const INVITE_PREFIX: &str = "chatter-mesh-private-v2:";
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

impl LeaveNotice {
    fn preimage(&self) -> Option<Vec<u8>> {
        serde_json::to_vec(&LeavePreimage {
            domain: "chatter.mesh.private.leave.v1",
            id: &self.id,
            epoch: self.epoch,
            peer: &self.peer,
        })
        .ok()
    }

    pub fn encode(&self) -> Result<String, MeshError> {
        let bytes = serde_json::to_vec(self).map_err(|_| failure("invalid leave notice"))?;
        Ok(format!("{LEAVE_MAGIC}{}", URL_SAFE_NO_PAD.encode(bytes)))
    }

    pub fn decode(text: &str) -> Option<Self> {
        let encoded = text.strip_prefix(LEAVE_MAGIC)?;
        if encoded.len() > 1024 {
            return None;
        }
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(encoded).ok()?).ok()
    }
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

impl EpochProof {
    pub fn channel_id(&self) -> &str {
        &self.id
    }

    pub fn matches(&self, state: &PrivateChannel) -> bool {
        self.epoch == state.epoch && self.signature == state.signature
    }

    pub fn encode(&self) -> Option<Vec<u8>> {
        let mut bytes = b"CHTRPCH3".to_vec();
        bytes.extend(serde_json::to_vec(self).ok()?);
        (bytes.len() <= 4096).then_some(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if !bytes.starts_with(b"CHTRPCH3") || bytes.len() > 4096 {
            return None;
        }
        serde_json::from_slice(&bytes[8..]).ok()
    }

    pub fn verified_for(&self, state: &PrivateChannel) -> bool {
        if self.domain != "chatter.mesh.private.roster.v2"
            || self.id != state.id
            || self.creator != state.creator
            || self.creator_static != state.creator_static
            || self.epoch == 0
            || self.previous_hash.len() != 32
            || self.signature.len() != 64
            || self.members.len() > MAX_MEMBERS
            || self.members.is_empty()
            || self.members[0].peer != state.creator
            || self.members[0].signing_key != state.creator_key
        {
            return false;
        }
        let Ok(key) = <[u8; 32]>::try_from(state.creator_key.as_slice()) else {
            return false;
        };
        let Ok(sig) = <[u8; 64]>::try_from(self.signature.as_slice()) else {
            return false;
        };
        let preimage = EpochPreimage {
            domain: "chatter.mesh.private.roster.v2",
            id: &self.id,
            name: &self.name,
            creator: &self.creator,
            creator_static: &self.creator_static,
            epoch: self.epoch,
            members: &self.members,
            previous_hash: &self.previous_hash,
        };
        serde_json::to_vec(&preimage).is_ok_and(|bytes| verify_bytes(&sig, &bytes, &key))
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }
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

impl SenderDistribution {
    fn preimage(&self) -> Option<Vec<u8>> {
        serde_json::to_vec(&(
            self.domain.as_str(),
            &self.id,
            &self.roster_hash,
            &self.sender,
            self.generation,
            &self.key,
            &self.removal_hash,
        ))
        .ok()
    }
    pub fn encode(&self) -> Option<Zeroizing<Vec<u8>>> {
        let mut out = Zeroizing::new(DISTRIBUTION_MAGIC.to_vec());
        out.extend(serde_json::to_vec(self).ok()?);
        (out.len() <= 4096).then_some(out)
    }
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if !bytes.starts_with(DISTRIBUTION_MAGIC) || bytes.len() > 4096 {
            return None;
        }
        serde_json::from_slice(&bytes[8..]).ok()
    }
    pub fn verify(&self, state: &PrivateChannel, claimed: &PeerId) -> bool {
        if self.domain != "chatter.mesh.private.sender.distribution.v1"
            || self.id != state.id
            || self.sender != claimed.as_str()
            || self.roster_hash != state.chain_hash().unwrap_or_default()
            || self.key.len() != 32
            || self.generation == 0
            || self.signature.len() != 64
            || self.removal_hash != state.removal_hash()
            || state.member_key(claimed).is_none()
        {
            return false;
        }
        let Some(key) = state.member_key(claimed) else {
            return false;
        };
        let Ok(sig) = <[u8; 64]>::try_from(self.signature.as_slice()) else {
            return false;
        };
        self.preimage()
            .is_some_and(|bytes| verify_bytes(&sig, &bytes, &key))
    }
    pub fn ack(&self) -> Option<Vec<u8>> {
        let mut out = DISTRIBUTION_ACK_MAGIC.to_vec();
        out.extend(uuid::Uuid::parse_str(&self.id).ok()?.as_bytes());
        out.extend(&self.roster_hash);
        out.extend(&self.generation.to_be_bytes());
        Some(out)
    }
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
            sender_keys: vec![SenderKey {
                peer: identity.peer_id().to_string(),
                generation: 1,
                key,
            }],
            prior_sender_keys: vec![],
            pending_distributions: vec![],
            distribution_attempts: vec![],
            replay: vec![],
            previous_hash: vec![0; 32],
            signature: vec![],
            pending_invites: vec![],
            awaiting_rekey: vec![],
            stale: false,
            stale_epoch: 0,
            departed: vec![],
        };
        state.sign(identity)?;
        Ok(state)
    }

    fn preimage(&self) -> Result<Vec<u8>, MeshError> {
        serde_json::to_vec(&EpochPreimage {
            domain: "chatter.mesh.private.roster.v2",
            id: &self.id,
            name: &self.name,
            creator: &self.creator,
            creator_static: &self.creator_static,
            epoch: self.epoch,
            members: &self.members,
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
        if self.epoch == 0
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
            || self.sender_keys.len() > MAX_MEMBERS
            || self.prior_sender_keys.len() > 32
            || self.replay.len() > 512
            || self.sender_keys.iter().any(|key| {
                key.key.len() != 32
                    || key.generation == 0
                    || !self.members.iter().any(|member| member.peer == key.peer)
            })
            || self
                .pending_distributions
                .iter()
                .any(|peer| !self.members.iter().any(|member| &member.peer == peer))
            || self.distribution_attempts.len() > MAX_MEMBERS
            || self.departed.len() > MAX_MEMBERS
            || self
                .departed
                .iter()
                .any(|notice| !self.verify_leave(notice))
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

    pub fn leave_notice(&self, identity: &MeshIdentity) -> Result<LeaveNotice, MeshError> {
        if self.member_key(&identity.peer_id()).is_none() {
            return Err(failure("not a channel member"));
        }
        let mut notice = LeaveNotice {
            id: self.id.clone(),
            epoch: self.epoch,
            peer: identity.peer_id().to_string(),
            signature: vec![],
        };
        notice.signature = sign_bytes(
            identity,
            &notice
                .preimage()
                .ok_or_else(|| failure("invalid leave notice"))?,
        )
        .to_vec();
        Ok(notice)
    }

    pub fn verify_leave(&self, notice: &LeaveNotice) -> bool {
        if notice.id != self.id || notice.epoch != self.epoch || notice.signature.len() != 64 {
            return false;
        }
        let Some(key) = self.member_key(&PeerId::from_str(&notice.peer)) else {
            return false;
        };
        let Ok(sig) = <[u8; 64]>::try_from(notice.signature.as_slice()) else {
            return false;
        };
        notice
            .preimage()
            .is_some_and(|bytes| verify_bytes(&sig, &bytes, &key))
    }

    pub fn epoch_proof(&self) -> EpochProof {
        EpochProof {
            domain: "chatter.mesh.private.roster.v2".into(),
            id: self.id.clone(),
            name: self.name.clone(),
            creator: self.creator.clone(),
            creator_static: self.creator_static.clone(),
            epoch: self.epoch,
            members: self.members.clone(),
            previous_hash: self.previous_hash.clone(),
            signature: self.signature.clone(),
        }
    }

    pub fn member_key_for_packet(&self, peer: &PeerId, bytes: &[u8]) -> Option<[u8; 32]> {
        if self
            .departed
            .iter()
            .any(|notice| notice.peer == peer.as_str())
        {
            return None;
        }
        if !self.has_route_tag(bytes) {
            return None;
        }
        self.member_key(peer)
    }

    pub fn has_route_tag(&self, bytes: &[u8]) -> bool {
        if bytes.len() < 32 || !bytes.starts_with(FRAME_MAGIC) {
            return false;
        }
        let current = self.chain_hash().unwrap_or_default();
        self.sender_keys
            .iter()
            .any(|key| route_tag(&self.id, &current, &key.peer, key.generation) == bytes[8..24])
            || self.prior_sender_keys.iter().any(|prior| {
                route_tag(
                    &self.id,
                    &prior.roster_hash,
                    &prior.sender.peer,
                    prior.sender.generation,
                ) == bytes[8..24]
                    && prior.expires_at >= super::now_secs()
            })
    }

    pub fn missing_sender_keys(&self, own: &PeerId) -> usize {
        self.members
            .iter()
            .filter(|m| {
                m.peer != own.as_str()
                    && !self.departed.iter().any(|n| n.peer == m.peer)
                    && !self.sender_keys.iter().any(|key| key.peer == m.peer)
            })
            .count()
    }

    pub fn initialize_sender(&mut self, identity: &MeshIdentity) -> Result<(), MeshError> {
        let peer = identity.peer_id();
        if self.member_key(&peer).is_none() {
            return Err(failure("not a channel member"));
        }
        if self.sender_keys.iter().any(|key| key.peer == peer.as_str()) {
            return Ok(());
        }
        let mut key = Zeroizing::new(vec![0; 32]);
        OsRng
            .try_fill_bytes(&mut key)
            .map_err(|_| failure("randomness unavailable"))?;
        self.sender_keys.push(SenderKey {
            peer: peer.to_string(),
            generation: 1,
            key,
        });
        self.pending_distributions = self
            .members
            .iter()
            .filter(|m| m.peer != peer.as_str() && !self.departed.iter().any(|n| n.peer == m.peer))
            .map(|m| m.peer.clone())
            .collect();
        self.distribution_attempts.clear();
        Ok(())
    }

    pub fn rotate_sender(&mut self, identity: &MeshIdentity) -> Result<(), MeshError> {
        let peer = identity.peer_id();
        let pos = self
            .sender_keys
            .iter()
            .position(|key| key.peer == peer.as_str())
            .ok_or_else(|| failure("sender key missing"))?;
        let hash = self.chain_hash()?;
        let old = self.sender_keys[pos].clone();
        let generation = old
            .generation
            .checked_add(1)
            .ok_or_else(|| failure("sender generation exhausted"))?;
        self.prior_sender_keys.push(PriorSenderKey {
            sender: old,
            roster_hash: hash,
            expires_at: super::now_secs().saturating_add(300),
        });
        if self.prior_sender_keys.len() > 32 {
            self.prior_sender_keys.remove(0);
        }
        let mut key = Zeroizing::new(vec![0; 32]);
        OsRng
            .try_fill_bytes(&mut key)
            .map_err(|_| failure("randomness unavailable"))?;
        self.sender_keys[pos] = SenderKey {
            peer: peer.to_string(),
            generation,
            key,
        };
        self.pending_distributions = self
            .members
            .iter()
            .filter(|m| m.peer != peer.as_str() && !self.departed.iter().any(|n| n.peer == m.peer))
            .map(|m| m.peer.clone())
            .collect();
        self.distribution_attempts.clear();
        Ok(())
    }

    pub fn reset_distribution_attempt(&mut self, peer: &str) {
        self.distribution_attempts.retain(|(p, _)| p != peer);
    }

    pub fn mark_distribution_attempt(&mut self, peer: &str) -> bool {
        if !self.pending_distributions.iter().any(|p| p == peer) {
            return false;
        }
        if let Some((_, count)) = self
            .distribution_attempts
            .iter_mut()
            .find(|(p, _)| p == peer)
        {
            if *count >= 3 {
                return false;
            }
            *count += 1;
        } else {
            self.distribution_attempts.push((peer.to_owned(), 1));
        }
        true
    }

    pub fn invalidate_other_sender_keys(&mut self, own: &PeerId) {
        if let Ok(hash) = self.chain_hash() {
            for key in self
                .sender_keys
                .iter()
                .filter(|key| key.peer != own.as_str())
            {
                self.prior_sender_keys.push(PriorSenderKey {
                    sender: key.clone(),
                    roster_hash: hash.clone(),
                    expires_at: super::now_secs().saturating_add(300),
                });
            }
            if self.prior_sender_keys.len() > 32 {
                self.prior_sender_keys
                    .drain(..self.prior_sender_keys.len() - 32);
            }
        }
        self.sender_keys.retain(|key| key.peer == own.as_str());
    }

    pub fn distribution(&self, identity: &MeshIdentity) -> Option<SenderDistribution> {
        let peer = identity.peer_id();
        let sender = self
            .sender_keys
            .iter()
            .find(|key| key.peer == peer.as_str())?;
        let removal_hash = self.removal_hash();
        let mut distribution = SenderDistribution {
            domain: "chatter.mesh.private.sender.distribution.v1".into(),
            id: self.id.clone(),
            roster_hash: self.chain_hash().ok()?,
            sender: peer.to_string(),
            generation: sender.generation,
            key: sender.key.clone(),
            removal_hash,
            signature: vec![],
        };
        distribution.signature = sign_bytes(identity, &distribution.preimage()?).to_vec();
        Some(distribution)
    }

    fn removal_hash(&self) -> Vec<u8> {
        self.departed
            .iter()
            .filter_map(|notice| notice.preimage())
            .map(|bytes| Sha256::digest(bytes).to_vec())
            .max()
            .unwrap_or_else(|| self.previous_hash.clone())
    }

    pub fn accept_distribution(
        &mut self,
        distribution: &SenderDistribution,
        claimed: &PeerId,
    ) -> bool {
        if !distribution.verify(self, claimed) {
            return false;
        }
        if let Some(existing) = self
            .sender_keys
            .iter_mut()
            .find(|key| key.peer == distribution.sender)
        {
            if distribution.generation <= existing.generation {
                return false;
            }
            let old = existing.clone();
            self.prior_sender_keys.push(PriorSenderKey {
                sender: old,
                roster_hash: distribution.roster_hash.clone(),
                expires_at: super::now_secs().saturating_add(300),
            });
            if self.prior_sender_keys.len() > 32 {
                self.prior_sender_keys.remove(0);
            }
            *existing = SenderKey {
                peer: distribution.sender.clone(),
                generation: distribution.generation,
                key: distribution.key.clone(),
            };
        } else {
            self.sender_keys.push(SenderKey {
                peer: distribution.sender.clone(),
                generation: distribution.generation,
                key: distribution.key.clone(),
            });
        }
        true
    }

    pub fn record_message(&mut self, id: &str) -> bool {
        if self.replay.iter().any(|seen| seen == id) {
            return false;
        }
        self.replay.push(id.to_owned());
        if self.replay.len() > 512 {
            self.replay.remove(0);
        }
        true
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
        self.departed.clear();
        self.remember_current();
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
        self.sender_keys
            .retain(|key| self.members.iter().any(|m| m.peer == key.peer));
        self.invalidate_other_sender_keys(&identity.peer_id());
        self.pending_distributions = self
            .members
            .iter()
            .filter(|m| m.peer != identity.peer_id().as_str())
            .map(|m| m.peer.clone())
            .collect();
        self.distribution_attempts.clear();
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
            && state.awaiting_rekey.is_empty()
            && !state.stale
            && state.stale_epoch == 0
            && state.departed.is_empty()
            && state.sender_keys.is_empty()
            && state.prior_sender_keys.is_empty()
            && state.pending_distributions.is_empty()
            && state.distribution_attempts.is_empty()
            && state.replay.is_empty()
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
        let sender_key = self
            .sender_keys
            .iter()
            .find(|key| key.peer == sender.as_str())
            .ok_or_else(|| failure("sender key missing"))?;
        let roster_hash = self.chain_hash()?;
        let mut nonce = [0; 24];
        OsRng
            .try_fill_bytes(&mut nonce)
            .map_err(|_| failure("randomness unavailable"))?;
        let mut header = FRAME_MAGIC.to_vec();
        header.extend(route_tag(
            &self.id,
            &roster_hash,
            sender.as_str(),
            sender_key.generation,
        ));
        header.extend(sender_key.generation.to_be_bytes());
        header.extend(roster_hash);
        header.extend(
            sender
                .to_routing_bytes()
                .ok_or_else(|| failure("invalid sender"))?,
        );
        header.extend(message_id.as_bytes());
        header.extend(nonce);
        let mut aad = header.clone();
        aad.extend(sender.as_str().as_bytes());
        let cipher = XChaCha20Poly1305::new_from_slice(&sender_key.key)
            .map_err(|_| failure("invalid sender key"))?;
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
        if bytes.len() < 112 + 16
            || !bytes.starts_with(FRAME_MAGIC)
            || bytes[64..72] != sender.to_routing_bytes()?
        {
            return None;
        }
        let generation = u64::from_be_bytes(bytes[24..32].try_into().ok()?);
        let roster_hash = &bytes[32..64];
        let current_hash = self.chain_hash().ok()?;
        let key = self
            .sender_keys
            .iter()
            .find(|key| {
                key.peer == sender.as_str()
                    && key.generation == generation
                    && roster_hash == current_hash
            })
            .map(|key| key.key.as_slice())
            .or_else(|| {
                self.prior_sender_keys
                    .iter()
                    .find(|prior| {
                        prior.sender.peer == sender.as_str()
                            && prior.sender.generation == generation
                            && prior.roster_hash == roster_hash
                            && prior.expires_at >= super::now_secs()
                    })
                    .map(|prior| prior.sender.key.as_slice())
            })?;
        if bytes[8..24] != route_tag(&self.id, roster_hash, sender.as_str(), generation) {
            return None;
        }
        let nonce = XNonce::from_slice(&bytes[88..112]);
        let mut aad = bytes[..112].to_vec();
        aad.extend(sender.as_str().as_bytes());
        let plaintext = XChaCha20Poly1305::new_from_slice(key)
            .ok()?
            .decrypt(
                nonce,
                Payload {
                    msg: &bytes[112..],
                    aad: &aad,
                },
            )
            .ok()?;
        let text = String::from_utf8(plaintext).ok()?;
        if text.is_empty() {
            return None;
        }
        let id = uuid::Uuid::from_bytes(bytes[72..88].try_into().ok()?).to_string();
        Some((id, text))
    }

    pub fn remember_previous(&mut self, old: &Self) {
        self.sender_keys = old.sender_keys.clone();
        self.prior_sender_keys = old.prior_sender_keys.clone();
        if let Ok(hash) = old.chain_hash() {
            for key in &old.sender_keys {
                self.prior_sender_keys.push(PriorSenderKey {
                    sender: key.clone(),
                    roster_hash: hash.clone(),
                    expires_at: super::now_secs().saturating_add(300),
                });
            }
            if self.prior_sender_keys.len() > 32 {
                self.prior_sender_keys
                    .drain(..self.prior_sender_keys.len() - 32);
            }
        }
        self.replay = old.replay.clone();
        self.pending_distributions = old.pending_distributions.clone();
        self.distribution_attempts = old.distribution_attempts.clone();
    }

    fn remember_current(&mut self) {
        if let Ok(hash) = self.chain_hash() {
            for key in &self.sender_keys {
                self.prior_sender_keys.push(PriorSenderKey {
                    sender: key.clone(),
                    roster_hash: hash.clone(),
                    expires_at: super::now_secs().saturating_add(300),
                });
            }
            if self.prior_sender_keys.len() > 32 {
                self.prior_sender_keys
                    .drain(..self.prior_sender_keys.len() - 32);
            }
        }
    }
}

fn route_tag(id: &str, roster_hash: &[u8], sender: &str, generation: u64) -> [u8; 16] {
    let mut hash = Sha256::new();
    hash.update(b"chatter.mesh.private.sender.route.v1\0");
    hash.update(id.as_bytes());
    hash.update(roster_hash);
    hash.update(sender.as_bytes());
    hash.update(generation.to_be_bytes());
    hash.finalize()[..16].try_into().expect("digest length")
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
        state.rotate_sender(identity)?;
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

    pub fn save_pending_leave(&self, leave: &PendingLeave) -> Result<(), MeshError> {
        let bytes = serde_json::to_vec(leave).map_err(|_| failure("invalid leave notice"))?;
        self.db
            .connection()?
            .execute(
                "INSERT INTO mesh_private_joins(channel_id,invite) VALUES(?1,?2)
             ON CONFLICT(channel_id) DO UPDATE SET invite=excluded.invite",
                params![format!("leave:{}", leave.notice.id), bytes],
            )
            .map_err(db_error)?;
        Ok(())
    }

    pub fn pending_leaves_for(&self, recipient: &PeerId) -> Result<Vec<PendingLeave>, MeshError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn
            .prepare("SELECT invite FROM mesh_private_joins WHERE channel_id LIKE 'leave:%'")
            .map_err(db_error)?;
        let rows = stmt
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .map_err(db_error)?;
        let mut leaves = Vec::new();
        for row in rows {
            let bytes = row.map_err(db_error)?;
            let leave: PendingLeave =
                serde_json::from_slice(&bytes).map_err(|_| failure("corrupt pending leave"))?;
            if leave
                .recipients
                .iter()
                .any(|peer| peer == recipient.as_str())
                && !leave
                    .acknowledged
                    .iter()
                    .any(|peer| peer == recipient.as_str())
            {
                leaves.push(leave);
            }
        }
        Ok(leaves)
    }

    pub fn clear_pending_leave(
        &self,
        id: &str,
        epoch: u64,
        recipient: &PeerId,
    ) -> Result<(), MeshError> {
        for mut leave in self.pending_leaves_for(recipient)? {
            if leave.notice.id == id && leave.notice.epoch == epoch {
                leave.acknowledged.push(recipient.to_string());
                if leave.acknowledged.len() == leave.recipients.len() {
                    self.db
                        .connection()?
                        .execute(
                            "DELETE FROM mesh_private_joins WHERE channel_id=?1",
                            params![format!("leave:{id}")],
                        )
                        .map_err(db_error)?;
                } else {
                    self.save_pending_leave(&leave)?;
                }
            }
        }
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
