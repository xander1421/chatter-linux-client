//! PQXDH material for a verified, device-local mesh association.
//!
//! Bundles are sent only inside authenticated Noise. Every key operation uses
//! libsignal records and the account's existing identity key; the framing below
//! adds no key agreement or signature primitive.

use libsignal_protocol::{
    GenericSignedPreKey, IdentityKeyPair, KeyPair, KyberPreKeyId, KyberPreKeyRecord, PreKeyId,
    PreKeyRecord, SignedPreKeyId, SignedPreKeyRecord, Timestamp, kem,
};
use prost::Message;
use rand::TryRngCore;
use rand::rngs::OsRng;
use std::sync::Arc;

use crate::proto::chat_v1::{SignalPreKeyBundle, signal_pre_key_bundle};
use crate::storage::{KeyStore, mesh_binding_store::MeshBindingStore};

use super::MeshError;

const MAGIC: &[u8; 8] = b"CHTRSB01";
const BUNDLE_COUNT: usize = 3;
// At most sixteen default 469-byte fragments on the existing BLE path.
const MAX_FRAME: usize = 16 * 469;
pub const BUNDLE_TTL_SECS: u64 = 10 * 60;
const CARRIER_MAGIC: &[u8; 8] = b"CHTRSM01";
const PLAIN_MAGIC: &[u8; 8] = b"CHTRSP01";
pub const MAX_CARRIER: usize = 4 * 1024;
pub const MAX_TEXT: usize = 2 * 1024;

/// The one allocated Chatter extension payload: headers are routing metadata,
/// while all content and receipt kind live inside Signal ciphertext.
pub struct CarrierFrame {
    pub message_id: [u8; 16],
    pub expires_at: u64,
    pub ciphertext_type: u8,
    pub ciphertext: Vec<u8>,
}

impl CarrierFrame {
    pub fn encode(&self) -> Result<Vec<u8>, MeshError> {
        if !matches!(self.ciphertext_type, 2 | 3)
            || self.ciphertext.is_empty()
            || self.ciphertext.len() > MAX_CARRIER - 36
        {
            return Err(MeshError::Binding(
                "invalid mesh Signal ciphertext size or type".into(),
            ));
        }
        let mut out = Vec::with_capacity(36 + self.ciphertext.len());
        out.extend_from_slice(CARRIER_MAGIC);
        out.push(1); // inner Chatter extension type: Signal ciphertext
        out.extend_from_slice(&self.message_id);
        out.extend_from_slice(&self.expires_at.to_be_bytes());
        out.push(self.ciphertext_type);
        out.extend_from_slice(&(self.ciphertext.len() as u16).to_be_bytes());
        out.extend_from_slice(&self.ciphertext);
        Ok(out)
    }

    pub fn decode(bytes: &[u8], now: u64) -> Option<Self> {
        if bytes.len() < 36
            || bytes.len() > MAX_CARRIER
            || !bytes.starts_with(CARRIER_MAGIC)
            || bytes[8] != 1
            || !matches!(bytes[33], 2 | 3)
        {
            return None;
        }
        let message_id = bytes[9..25].try_into().ok()?;
        let expires_at = u64::from_be_bytes(bytes[25..33].try_into().ok()?);
        if expires_at < now || expires_at > now.saturating_add(24 * 3600 + 120) {
            return None;
        }
        let length = u16::from_be_bytes(bytes[34..36].try_into().ok()?) as usize;
        if length == 0 || length != bytes.len() - 36 {
            return None;
        }
        Some(Self {
            message_id,
            expires_at,
            ciphertext_type: bytes[33],
            ciphertext: bytes[36..].to_vec(),
        })
    }
}

#[derive(Clone, Copy)]
pub enum PlainKind {
    Text,
    Delivered,
}

pub struct SignalPlaintext {
    pub kind: PlainKind,
    pub conversation_id: [u8; 16],
    pub message_id: [u8; 16],
    pub sender_device: u32,
    pub recipient_device: u32,
    pub expires_at: u64,
    pub text: String,
}

impl SignalPlaintext {
    pub fn encode(&self) -> Result<Vec<u8>, MeshError> {
        if self.text.len() > MAX_TEXT
            || (matches!(self.kind, PlainKind::Delivered) && !self.text.is_empty())
        {
            return Err(MeshError::Binding("invalid mesh Signal plaintext".into()));
        }
        let mut out = Vec::with_capacity(59 + self.text.len());
        out.extend_from_slice(PLAIN_MAGIC);
        out.push(match self.kind {
            PlainKind::Text => 1,
            PlainKind::Delivered => 2,
        });
        out.extend_from_slice(&self.conversation_id);
        out.extend_from_slice(&self.message_id);
        out.extend_from_slice(&self.sender_device.to_be_bytes());
        out.extend_from_slice(&self.recipient_device.to_be_bytes());
        out.extend_from_slice(&self.expires_at.to_be_bytes());
        out.extend_from_slice(&(self.text.len() as u16).to_be_bytes());
        out.extend_from_slice(self.text.as_bytes());
        Ok(out)
    }

    pub fn decode(bytes: &[u8], now: u64) -> Option<Self> {
        if bytes.len() < 59 || bytes.len() > 59 + MAX_TEXT || !bytes.starts_with(PLAIN_MAGIC) {
            return None;
        }
        let kind = match bytes[8] {
            1 => PlainKind::Text,
            2 => PlainKind::Delivered,
            _ => return None,
        };
        let conversation_id = bytes[9..25].try_into().ok()?;
        let message_id = bytes[25..41].try_into().ok()?;
        let sender_device = u32::from_be_bytes(bytes[41..45].try_into().ok()?);
        let recipient_device = u32::from_be_bytes(bytes[45..49].try_into().ok()?);
        let expires_at = u64::from_be_bytes(bytes[49..57].try_into().ok()?);
        if expires_at < now || expires_at > now.saturating_add(24 * 3600 + 120) {
            return None;
        }
        let length = u16::from_be_bytes(bytes[57..59].try_into().ok()?) as usize;
        if length != bytes.len() - 59 {
            return None;
        }
        let text = std::str::from_utf8(&bytes[59..]).ok()?.to_owned();
        if matches!(kind, PlainKind::Delivered) && !text.is_empty() {
            return None;
        }
        Some(Self {
            kind,
            conversation_id,
            message_id,
            sender_device,
            recipient_device,
            expires_at,
            text,
        })
    }
}

pub fn conversation_id(a: &chatter_mesh::ids::PeerId, b: &chatter_mesh::ids::PeerId) -> [u8; 16] {
    let (first, second) = if a.as_str() <= b.as_str() {
        (a, b)
    } else {
        (b, a)
    };
    let name = format!(
        "chatter.mesh.signal.dm.v1:{}:{}",
        first.as_str(),
        second.as_str()
    );
    *uuid::Uuid::new_v5(&uuid::Uuid::nil(), name.as_bytes()).as_bytes()
}

pub async fn encrypt(
    store: &mut crate::crypto::signal::store::SignalStore,
    remote: &libsignal_protocol::ProtocolAddress,
    local: &libsignal_protocol::ProtocolAddress,
    plaintext: &SignalPlaintext,
    outer_id: [u8; 16],
) -> Result<CarrierFrame, MeshError> {
    let mut identities = store.clone();
    let mut rng = OsRng.unwrap_err();
    let message = libsignal_protocol::message_encrypt(
        &plaintext.encode()?,
        remote,
        local,
        store,
        &mut identities,
        std::time::SystemTime::now(),
        &mut rng,
    )
    .await
    .map_err(|e| MeshError::Binding(format!("mesh Signal encrypt failed: {e}")))?;
    let ciphertext_type = match message.message_type() {
        libsignal_protocol::CiphertextMessageType::PreKey => 2,
        libsignal_protocol::CiphertextMessageType::Whisper => 3,
        _ => {
            return Err(MeshError::Binding(
                "unexpected mesh Signal ciphertext type".into(),
            ));
        }
    };
    Ok(CarrierFrame {
        message_id: outer_id,
        expires_at: plaintext.expires_at,
        ciphertext_type,
        ciphertext: message.serialize().to_vec(),
    })
}

pub async fn decrypt(
    store: &mut crate::crypto::signal::store::SignalStore,
    remote: &libsignal_protocol::ProtocolAddress,
    local: &libsignal_protocol::ProtocolAddress,
    frame: &CarrierFrame,
) -> Result<Vec<u8>, MeshError> {
    let mut identities = store.clone();
    let mut prekeys = store.clone();
    let signed = store.clone();
    let mut kyber = store.clone();
    let mut rng = OsRng.unwrap_err();
    let result = match frame.ciphertext_type {
        2 => {
            let message =
                libsignal_protocol::PreKeySignalMessage::try_from(frame.ciphertext.as_slice())
                    .map_err(|e| {
                        MeshError::Binding(format!("invalid mesh Signal prekey message: {e}"))
                    })?;
            libsignal_protocol::message_decrypt_prekey(
                &message,
                remote,
                local,
                store,
                &mut identities,
                &mut prekeys,
                &signed,
                &mut kyber,
                &mut rng,
            )
            .await
        }
        3 => {
            let message = libsignal_protocol::SignalMessage::try_from(frame.ciphertext.as_slice())
                .map_err(|e| MeshError::Binding(format!("invalid mesh Signal message: {e}")))?;
            libsignal_protocol::message_decrypt_signal(
                &message,
                remote,
                local,
                store,
                &mut identities,
                &mut rng,
            )
            .await
        }
        _ => {
            return Err(MeshError::Binding(
                "invalid mesh Signal ciphertext type".into(),
            ));
        }
    };
    result.map_err(|e| MeshError::Binding(format!("mesh Signal decrypt failed: {e}")))
}

pub struct BundleFrame {
    pub nonce: [u8; 16],
    pub expires_at: u64,
    pub reply_requested: bool,
    pub bundles: Vec<SignalPreKeyBundle>,
}

impl BundleFrame {
    pub fn encode(&self) -> Result<Vec<u8>, MeshError> {
        if self.bundles.is_empty() || self.bundles.len() > BUNDLE_COUNT {
            return Err(MeshError::Binding(
                "invalid mesh Signal bundle count".into(),
            ));
        }
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&self.nonce);
        out.extend_from_slice(&self.expires_at.to_be_bytes());
        out.push(u8::from(self.reply_requested));
        out.push(self.bundles.len() as u8);
        for bundle in &self.bundles {
            let body = bundle.encode_to_vec();
            let len = u16::try_from(body.len())
                .map_err(|_| MeshError::Binding("mesh Signal bundle too large".into()))?;
            out.extend_from_slice(&len.to_be_bytes());
            out.extend_from_slice(&body);
        }
        if out.len() > MAX_FRAME {
            return Err(MeshError::Binding(
                "mesh Signal bundle frame too large".into(),
            ));
        }
        Ok(out)
    }

    pub fn decode(bytes: &[u8], now: u64) -> Option<Self> {
        if bytes.len() < 34 || bytes.len() > MAX_FRAME || !bytes.starts_with(MAGIC) {
            return None;
        }
        let nonce = bytes[8..24].try_into().ok()?;
        let expires_at = u64::from_be_bytes(bytes[24..32].try_into().ok()?);
        if expires_at < now || expires_at > now.saturating_add(BUNDLE_TTL_SECS + 120) {
            return None;
        }
        let reply_requested = match bytes[32] {
            0 => false,
            1 => true,
            _ => return None,
        };
        let count = bytes[33] as usize;
        if count == 0 || count > BUNDLE_COUNT {
            return None;
        }
        let mut offset = 34;
        let mut bundles = Vec::with_capacity(count);
        for _ in 0..count {
            let len = u16::from_be_bytes(bytes.get(offset..offset + 2)?.try_into().ok()?) as usize;
            offset += 2;
            let end = offset.checked_add(len)?;
            let bundle = SignalPreKeyBundle::decode(bytes.get(offset..end)?).ok()?;
            if bundle.signed_prekey.is_none()
                || bundle.kyber_prekey.is_none()
                || bundle.one_time_prekey.is_none()
            {
                return None;
            }
            bundles.push(bundle);
            offset = end;
        }
        (offset == bytes.len()).then_some(Self {
            nonce,
            expires_at,
            reply_requested,
            bundles,
        })
    }
}

fn random_id(rng: &mut impl TryRngCore) -> Result<u32, MeshError> {
    rng.try_next_u32()
        .map_err(|_| MeshError::Binding("OS randomness unavailable".into()))
}

/// Generate three one-time EC/PQ combinations, all signed by the same local
/// account identity. The records remain in the mesh-only SQLCipher tables.
pub fn fresh_bundle(
    keys: &Arc<KeyStore>,
    bindings: &Arc<MeshBindingStore>,
    owner: &str,
    device_id: u32,
    reply_requested: bool,
    now: u64,
) -> Result<BundleFrame, MeshError> {
    libsignal_protocol::DeviceId::try_from(device_id)
        .map_err(|_| MeshError::Binding("invalid local mesh Signal device ID".into()))?;
    bindings.prune_mesh_signal_material(owner, now)?;
    let (registration_id, identity_bytes) = keys
        .get_local_identity()?
        .ok_or_else(|| MeshError::Binding("no local Signal identity".into()))?;
    let identity = IdentityKeyPair::try_from(identity_bytes.as_slice())
        .map_err(|_| MeshError::Binding("invalid local Signal identity".into()))?;
    let mut rng = OsRng.unwrap_err();
    let spk_id = random_id(&mut rng)?;
    let spk_pair = KeyPair::generate(&mut rng);
    let signature = identity
        .private_key()
        .calculate_signature(&spk_pair.public_key.serialize(), &mut rng)
        .map_err(|_| MeshError::Binding("cannot sign mesh prekey".into()))?;
    let millis = now.saturating_mul(1000);
    let signed = SignedPreKeyRecord::new(
        SignedPreKeyId::from(spk_id),
        Timestamp::from_epoch_millis(millis),
        &spk_pair,
        &signature.into_vec(),
    );
    bindings.save_mesh_prekey(
        owner,
        2,
        spk_id,
        &signed
            .serialize()
            .map_err(|_| MeshError::Binding("cannot serialize mesh signed prekey".into()))?,
    )?;
    let spk_public = signed
        .public_key()
        .map_err(|_| MeshError::Binding("invalid mesh signed prekey".into()))?;
    let spk_sig = signed
        .signature()
        .map_err(|_| MeshError::Binding("invalid mesh signed prekey signature".into()))?;
    let spk_proto = signal_pre_key_bundle::SignedPreKeyRecord {
        id: spk_id,
        public_key: spk_public.serialize().to_vec(),
        signature: spk_sig.to_vec(),
        timestamp: now.saturating_mul(1_000_000_000),
    };
    let mut bundles = Vec::with_capacity(BUNDLE_COUNT);
    for _ in 0..BUNDLE_COUNT {
        let ec_id = random_id(&mut rng)?;
        let ec = PreKeyRecord::new(PreKeyId::from(ec_id), &KeyPair::generate(&mut rng));
        bindings.save_mesh_prekey(
            owner,
            1,
            ec_id,
            &ec.serialize()
                .map_err(|_| MeshError::Binding("cannot serialize mesh one-time prekey".into()))?,
        )?;
        let pq_id = random_id(&mut rng)?;
        let pq = KyberPreKeyRecord::generate(
            kem::KeyType::Kyber1024,
            KyberPreKeyId::from(pq_id),
            identity.private_key(),
        )
        .map_err(|_| MeshError::Binding("cannot generate mesh PQ prekey".into()))?;
        bindings.save_mesh_prekey(
            owner,
            3,
            pq_id,
            &pq.serialize()
                .map_err(|_| MeshError::Binding("cannot serialize mesh PQ prekey".into()))?,
        )?;
        bundles.push(SignalPreKeyBundle {
            identity_key: identity.identity_key().serialize().to_vec(),
            registration_id,
            device_id,
            signed_prekey: Some(spk_proto.clone()),
            one_time_prekey: Some(signal_pre_key_bundle::PreKeyRecord {
                id: ec_id,
                public_key: ec
                    .public_key()
                    .map_err(|_| MeshError::Binding("invalid mesh one-time prekey".into()))?
                    .serialize()
                    .to_vec(),
            }),
            kyber_prekey: Some(signal_pre_key_bundle::KyberPreKeyRecord {
                id: pq_id,
                public_key: pq
                    .public_key()
                    .map_err(|_| MeshError::Binding("invalid mesh PQ prekey".into()))?
                    .serialize()
                    .to_vec(),
                signature: pq
                    .signature()
                    .map_err(|_| MeshError::Binding("invalid mesh PQ signature".into()))?
                    .to_vec(),
                timestamp: now.saturating_mul(1_000_000_000),
            }),
            unrestricted_unidentified_access: None,
        });
    }
    let mut nonce = [0; 16];
    getrandom::fill(&mut nonce)
        .map_err(|_| MeshError::Binding("OS randomness unavailable".into()))?;
    Ok(BundleFrame {
        nonce,
        expires_at: now + BUNDLE_TTL_SECS,
        reply_requested,
        bundles,
    })
}

/// Reuse the same live offer for a peer until it expires. A peer that saw the
/// first copy deduplicates its nonce; a peer that was still unverified can
/// accept a later copy without forcing fresh Kyber key generation per retry.
pub fn bundle_for_peer(
    keys: &Arc<KeyStore>,
    bindings: &Arc<MeshBindingStore>,
    owner: &str,
    peer: &chatter_mesh::ids::PeerId,
    device_id: u32,
    now: u64,
) -> Result<Vec<u8>, MeshError> {
    if let Some(bytes) = bindings.local_mesh_bundle(owner, peer, now)?
        && BundleFrame::decode(&bytes, now).is_some()
    {
        return Ok(bytes);
    }
    let frame = fresh_bundle(keys, bindings, owner, device_id, true, now)?;
    let bytes = frame.encode()?;
    bindings.save_local_mesh_bundle(owner, peer, &bytes, frame.expires_at)?;
    Ok(bytes)
}
