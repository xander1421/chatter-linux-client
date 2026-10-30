//! Chatter capability negotiation carried only by an established Noise transport.
//!
//! This is an inner application frame, not a bitchat packet type. The magic is
//! inspected only after Noise decryption, so radio observers and relay peers
//! cannot distinguish it from any other encrypted mesh traffic.

use std::collections::{BTreeMap, VecDeque};

use chatter_mesh::ids::PeerId;

pub const HELLO_VERSION: u8 = 1;
pub const SIGNAL_MESH: u8 = 1;
pub const QR_CHANNELS: u8 = 2;
const MAGIC: &[u8; 8] = b"CHTRHLO1";
const MAX_VERSIONS: usize = 8;
const REPLAY_LIMIT: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatterHello {
    pub versions: Vec<u8>,
    pub capabilities: u8,
    pub max_carrier_bytes: u32,
    pub nonce: [u8; 16],
}

impl ChatterHello {
    pub fn new(nonce: [u8; 16]) -> Self {
        Self {
            versions: vec![HELLO_VERSION],
            // Advertise only inside Noise; neither capability reaches announces.
            capabilities: SIGNAL_MESH | QR_CHANNELS,
            max_carrier_bytes: 4 * 1024,
            nonce,
        }
    }

    pub fn encode(&self) -> Option<Vec<u8>> {
        if self.versions.is_empty()
            || self.versions.len() > MAX_VERSIONS
            || self.versions.windows(2).any(|pair| pair[0] >= pair[1])
            || self.max_carrier_bytes == 0
            || self.max_carrier_bytes > 1024 * 1024
        {
            return None;
        }
        let mut bytes = Vec::with_capacity(30 + self.versions.len());
        bytes.extend_from_slice(MAGIC);
        bytes.push(self.versions.len() as u8);
        bytes.extend_from_slice(&self.versions);
        bytes.push(self.capabilities);
        bytes.extend_from_slice(&self.max_carrier_bytes.to_be_bytes());
        bytes.extend_from_slice(&self.nonce);
        Some(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if !bytes.starts_with(MAGIC) {
            return None;
        }
        let count = *bytes.get(MAGIC.len())? as usize;
        if count == 0 || count > MAX_VERSIONS || bytes.len() != MAGIC.len() + 1 + count + 1 + 4 + 16
        {
            return None;
        }
        let versions = bytes[9..9 + count].to_vec();
        if versions.windows(2).any(|pair| pair[0] >= pair[1]) {
            return None;
        }
        let capabilities = bytes[9 + count];
        let max_carrier_bytes = u32::from_be_bytes(bytes[10 + count..14 + count].try_into().ok()?);
        if max_carrier_bytes == 0 || max_carrier_bytes > 1024 * 1024 {
            return None;
        }
        let nonce = bytes[14 + count..].try_into().ok()?;
        Some(Self {
            versions,
            capabilities,
            max_carrier_bytes,
            nonce,
        })
    }

    pub fn compatible(&self) -> bool {
        self.versions.contains(&HELLO_VERSION)
    }
}

#[derive(Default)]
pub struct HelloPeers {
    seen: BTreeMap<PeerId, VecDeque<[u8; 16]>>,
    capable: BTreeMap<PeerId, ChatterHello>,
    local: BTreeMap<PeerId, [u8; 16]>,
}

impl HelloPeers {
    /// Reject a nonce repeated by this peer, including across Noise rekeys.
    pub fn accept(&mut self, peer: &PeerId, hello: ChatterHello) -> bool {
        if !hello.compatible() {
            return false;
        }
        let seen = self.seen.entry(peer.clone()).or_default();
        if seen.contains(&hello.nonce) {
            return false;
        }
        if seen.len() == REPLAY_LIMIT {
            seen.pop_front();
        }
        seen.push_back(hello.nonce);
        self.capable.insert(peer.clone(), hello);
        true
    }

    pub fn get(&self, peer: &PeerId) -> Option<&ChatterHello> {
        self.capable.get(peer)
    }
    pub fn local_nonce(&self, peer: &PeerId) -> Option<[u8; 16]> {
        self.local.get(peer).copied()
    }
    pub fn set_local_nonce(&mut self, peer: &PeerId, nonce: [u8; 16]) {
        self.local.insert(peer.clone(), nonce);
    }
    pub fn clear_live(&mut self) {
        self.capable.clear();
        self.local.clear();
    }

    pub fn forget_live(&mut self, peer: &PeerId) {
        self.capable.remove(peer);
        self.local.remove(peer);
    }
}
