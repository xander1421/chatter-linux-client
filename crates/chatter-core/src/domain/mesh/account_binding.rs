//! Explicit account proof for one live Noise peer. Never sent in an announce.
//!
//! The fixed-width encoding is canonical: each field has exactly one byte
//! representation, and signatures cover all fields before either signature.
//! No account key is trusted merely because this proof verifies against itself.

use chatter_mesh_noise::MeshIdentity;
use chatter_mesh_noise::sign::{sign_bytes, verify_bytes};
use libsignal_protocol::{IdentityKeyPair, PublicKey};
use rand::TryRngCore;
use sha2_10::{Digest, Sha256};

const DOMAIN: &[u8] = b"chatter.mesh.account-binding.v1\0";
const QR_DOMAIN: &[u8] = b"chatter.mesh.account-qr.v1\0";
const MAGIC: &[u8; 8] = b"CHTRBND1";
const QR_MAGIC: &[u8; 8] = b"CHTRQR01";
pub const PROOF_TTL_SECS: u64 = 600;
pub const CLOCK_SKEW_SECS: u64 = 120;
const UNSIGNED_LEN: usize = 8 + 16 + 4 + 33 + 32 + 32 + 32 + 16 + 16 + 8 + 8 + 8;
const SIGNED_LEN: usize = UNSIGNED_LEN + 64 + 64;
const QR_UNSIGNED_LEN: usize = 8 + 16 + 4 + 33 + 32 + 32 + 16 + 8;
const QR_SIGNED_LEN: usize = QR_UNSIGNED_LEN + 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindingV1 {
    pub account_id: [u8; 16],
    pub device_id: u32,
    pub signal_identity: [u8; 33],
    pub mesh_static: [u8; 32],
    pub mesh_signing: [u8; 32],
    pub handshake_hash: [u8; 32],
    /// Sender's challenge from its encrypted Hello.
    pub sender_nonce: [u8; 16],
    /// Recipient's challenge from its encrypted Hello.
    pub recipient_nonce: [u8; 16],
    pub created_at: u64,
    pub expires_at: u64,
    pub generation: u64,
    pub account_signature: [u8; 64],
    pub mesh_signature: [u8; 64],
}

impl BindingV1 {
    fn unsigned(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(UNSIGNED_LEN);
        b.extend_from_slice(MAGIC);
        b.extend_from_slice(&self.account_id);
        b.extend_from_slice(&self.device_id.to_be_bytes());
        b.extend_from_slice(&self.signal_identity);
        b.extend_from_slice(&self.mesh_static);
        b.extend_from_slice(&self.mesh_signing);
        b.extend_from_slice(&self.handshake_hash);
        b.extend_from_slice(&self.sender_nonce);
        b.extend_from_slice(&self.recipient_nonce);
        b.extend_from_slice(&self.created_at.to_be_bytes());
        b.extend_from_slice(&self.expires_at.to_be_bytes());
        b.extend_from_slice(&self.generation.to_be_bytes());
        b
    }

    fn preimage(&self) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(DOMAIN);
        h.update(self.unsigned());
        h.finalize().into()
    }

    /// One acceptance per account/device/key and pair of live challenges,
    /// even if a sender re-signs with a fresh XEdDSA nonce or timestamp.
    pub fn replay_token(&self) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(b"chatter.mesh.binding-replay.v1\0");
        h.update(self.account_id);
        h.update(self.device_id.to_be_bytes());
        h.update(self.mesh_static);
        h.update(self.generation.to_be_bytes());
        h.update(self.sender_nonce);
        h.update(self.recipient_nonce);
        h.finalize().into()
    }

    pub fn sign(mut self, account: &IdentityKeyPair, mesh: &MeshIdentity) -> Option<Self> {
        if self.signal_identity.as_slice() != account.identity_key().serialize().as_ref()
            || self.mesh_static != mesh.static_public()
            || self.mesh_signing != mesh.signing_public()
        {
            return None;
        }
        let hash = self.preimage();
        let mut rng = rand::rngs::OsRng.unwrap_err();
        self.account_signature = account
            .private_key()
            .calculate_signature(&hash, &mut rng)
            .ok()?
            .into_vec()
            .try_into()
            .ok()?;
        self.mesh_signature = sign_bytes(mesh, &hash);
        Some(self)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut b = self.unsigned();
        b.extend_from_slice(&self.account_signature);
        b.extend_from_slice(&self.mesh_signature);
        b
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() != SIGNED_LEN || &b[..8] != MAGIC {
            return None;
        }
        let mut p = 8;
        fn take<const N: usize>(b: &[u8], p: &mut usize) -> Option<[u8; N]> {
            let x = b.get(*p..*p + N)?.try_into().ok()?;
            *p += N;
            Some(x)
        }
        Some(Self {
            account_id: take(b, &mut p)?,
            device_id: u32::from_be_bytes(take(b, &mut p)?),
            signal_identity: take(b, &mut p)?,
            mesh_static: take(b, &mut p)?,
            mesh_signing: take(b, &mut p)?,
            handshake_hash: take(b, &mut p)?,
            sender_nonce: take(b, &mut p)?,
            recipient_nonce: take(b, &mut p)?,
            created_at: u64::from_be_bytes(take(b, &mut p)?),
            expires_at: u64::from_be_bytes(take(b, &mut p)?),
            generation: u64::from_be_bytes(take(b, &mut p)?),
            account_signature: take(b, &mut p)?,
            mesh_signature: take(b, &mut p)?,
        })
    }

    /// Proof of possession only. The caller must also pin `signal_identity`
    /// through a known contact or an in-person QR before marking verified.
    pub fn verify_live(
        &self,
        mesh_static: &[u8; 32],
        transcript: &[u8; 32],
        local_nonce: &[u8; 16],
        remote_nonce: &[u8; 16],
        now: u64,
    ) -> bool {
        if &self.mesh_static != mesh_static
            || &self.handshake_hash != transcript
            || &self.recipient_nonce != local_nonce
            || &self.sender_nonce != remote_nonce
            || self.device_id == 0
            || self.generation == 0
            || self.expires_at <= self.created_at
            || self.expires_at - self.created_at > PROOF_TTL_SECS
            || self.created_at > now.saturating_add(CLOCK_SKEW_SECS)
            || now > self.expires_at.saturating_add(CLOCK_SKEW_SECS)
        {
            return false;
        }
        let Ok(key) = PublicKey::deserialize(&self.signal_identity) else {
            return false;
        };
        let hash = self.preimage();
        key.verify_signature(&hash, &self.account_signature)
            && verify_bytes(&self.mesh_signature, &hash, &self.mesh_signing)
    }
}

/// A QR invite pins both identities but does not by itself establish liveness.
/// The final binding must also match the current Noise transcript and nonces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindingQr {
    pub account_id: [u8; 16],
    pub device_id: u32,
    pub signal_identity: [u8; 33],
    pub mesh_static: [u8; 32],
    pub mesh_signing: [u8; 32],
    pub nonce: [u8; 16],
    pub expires_at: u64,
    pub signature: [u8; 64],
}

impl BindingQr {
    fn unsigned(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(QR_UNSIGNED_LEN);
        b.extend_from_slice(QR_MAGIC);
        b.extend_from_slice(&self.account_id);
        b.extend_from_slice(&self.device_id.to_be_bytes());
        b.extend_from_slice(&self.signal_identity);
        b.extend_from_slice(&self.mesh_static);
        b.extend_from_slice(&self.mesh_signing);
        b.extend_from_slice(&self.nonce);
        b.extend_from_slice(&self.expires_at.to_be_bytes());
        b
    }

    fn hash(&self) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(QR_DOMAIN);
        h.update(self.unsigned());
        h.finalize().into()
    }

    pub fn sign(mut self, account: &IdentityKeyPair, mesh: &MeshIdentity) -> Option<Self> {
        if self.signal_identity.as_slice() != account.identity_key().serialize().as_ref()
            || self.mesh_static != mesh.static_public()
            || self.mesh_signing != mesh.signing_public()
        {
            return None;
        }
        let mut rng = rand::rngs::OsRng.unwrap_err();
        self.signature = account
            .private_key()
            .calculate_signature(&self.hash(), &mut rng)
            .ok()?
            .into_vec()
            .try_into()
            .ok()?;
        Some(self)
    }

    pub fn to_text(&self) -> String {
        let mut b = self.unsigned();
        b.extend_from_slice(&self.signature);
        format!("chatter-mesh:{}", hex::encode(b))
    }

    pub fn from_text(text: &str, now: u64) -> Option<Self> {
        let bytes = hex::decode(text.trim().strip_prefix("chatter-mesh:")?).ok()?;
        if bytes.len() != QR_SIGNED_LEN || &bytes[..8] != QR_MAGIC {
            return None;
        }
        let mut p = 8;
        fn take<const N: usize>(b: &[u8], p: &mut usize) -> Option<[u8; N]> {
            let x = b.get(*p..*p + N)?.try_into().ok()?;
            *p += N;
            Some(x)
        }
        let qr = Self {
            account_id: take(&bytes, &mut p)?,
            device_id: u32::from_be_bytes(take(&bytes, &mut p)?),
            signal_identity: take(&bytes, &mut p)?,
            mesh_static: take(&bytes, &mut p)?,
            mesh_signing: take(&bytes, &mut p)?,
            nonce: take(&bytes, &mut p)?,
            expires_at: u64::from_be_bytes(take(&bytes, &mut p)?),
            signature: take(&bytes, &mut p)?,
        };
        if qr.device_id == 0
            || now > qr.expires_at.saturating_add(CLOCK_SKEW_SECS)
            || qr.expires_at > now.saturating_add(PROOF_TTL_SECS + CLOCK_SKEW_SECS)
        {
            return None;
        }
        let key = PublicKey::deserialize(&qr.signal_identity).ok()?;
        key.verify_signature(&qr.hash(), &qr.signature)
            .then_some(qr)
    }

    pub fn matches(&self, proof: &BindingV1) -> bool {
        self.account_id == proof.account_id
            && self.device_id == proof.device_id
            && self.signal_identity == proof.signal_identity
            && self.mesh_static == proof.mesh_static
            && self.mesh_signing == proof.mesh_signing
            && self.nonce == proof.sender_nonce
    }
}
