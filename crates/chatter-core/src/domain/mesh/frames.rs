//! Mesh Signal wire frames: the carrier for one Signal ciphertext and the
//! frame that carries PQXDH prekey bundles inside authenticated Noise.

use prost::Message;

use chatter_proto::chat_v1::SignalPreKeyBundle;

const MAGIC: &[u8; 8] = b"CHTRSB01";
pub(crate) const BUNDLE_COUNT: usize = 3;
// At most sixteen default 469-byte fragments on the existing BLE path.
const MAX_FRAME: usize = 16 * 469;
pub const BUNDLE_TTL_SECS: u64 = 10 * 60;
const CARRIER_MAGIC: &[u8; 8] = b"CHTRSM01";
pub const MAX_CARRIER: usize = 4 * 1024;

/// The one allocated Chatter extension payload: headers are routing metadata,
/// while all content and receipt kind live inside Signal ciphertext.
pub struct CarrierFrame {
    pub message_id: [u8; 16],
    pub expires_at: u64,
    pub ciphertext_type: u8,
    pub ciphertext: Vec<u8>,
}

impl CarrierFrame {
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        if !matches!(self.ciphertext_type, 2 | 3)
            || self.ciphertext.is_empty()
            || self.ciphertext.len() > MAX_CARRIER - 36
        {
            return Err("invalid mesh Signal ciphertext size or type".into());
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

pub struct BundleFrame {
    pub nonce: [u8; 16],
    pub expires_at: u64,
    pub reply_requested: bool,
    pub bundles: Vec<SignalPreKeyBundle>,
}

impl BundleFrame {
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        if self.bundles.is_empty() || self.bundles.len() > BUNDLE_COUNT {
            return Err("invalid mesh Signal bundle count".into());
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
                .map_err(|_| "mesh Signal bundle too large".to_string())?;
            out.extend_from_slice(&len.to_be_bytes());
            out.extend_from_slice(&body);
        }
        if out.len() > MAX_FRAME {
            return Err("mesh Signal bundle frame too large".into());
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
