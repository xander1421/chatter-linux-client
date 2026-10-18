use super::*;
use crate::storage::{MemoryStorage, Storage, StorageKey};
use chatter_mesh::tlv::request_sync::MAX_FRAGMENT_ID_FILTER_COUNT;
use chatter_mesh_noise::prekey::Prekey;
use std::sync::Arc;

/// A fixed "now" so every age in these tests is exact.
const NOW: u64 = 1_700_000_000;
const NOW_MS: u64 = NOW * 1000;

fn peer(hex: &str) -> PeerId {
    PeerId::from_str(hex)
}

fn us() -> PeerId {
    peer("0011223344556677")
}

fn them() -> PeerId {
    peer("aabbccddeeff0011")
}

fn sender() -> [u8; SENDER_ID_LEN] {
    [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x00, 0x11]
}

fn packet_of(msg_type: MessageType, payload: &[u8], timestamp_ms: u64) -> BitchatPacket {
    BitchatPacket::new(msg_type, sender(), None, timestamp_ms, payload.to_vec(), 7)
}

fn message(payload: &[u8], timestamp_ms: u64) -> BitchatPacket {
    packet_of(MessageType::Message, payload, timestamp_ms)
}

fn fragment(stream: [u8; 8], index: u16, timestamp_ms: u64) -> BitchatPacket {
    let mut payload = Vec::new();
    payload.extend_from_slice(&stream);
    payload.extend_from_slice(&index.to_be_bytes());
    payload.extend_from_slice(&4u16.to_be_bytes());
    payload.push(MessageType::FileTransfer.as_u8());
    payload.push(0xEE);
    packet_of(MessageType::Fragment, &payload, timestamp_ms)
}

fn manager() -> GossipSyncManager {
    GossipSyncManager::new(&us(), SyncConfig::default(), None, NOW)
}

fn manager_with(config: SyncConfig) -> GossipSyncManager {
    GossipSyncManager::new(&us(), config, None, NOW)
}

/// A request whose filter is empty: "I hold nothing, send me everything."
fn empty_request(types: SyncTypeFlags) -> RequestSyncPacket {
    RequestSyncPacket {
        p: 7,
        m: 1,
        data: Vec::new(),
        types: Some(types.raw()),
        since_timestamp_ms: None,
        fragment_id_filter: None,
    }
}

fn decode_request(send: &SyncSend) -> RequestSyncPacket {
    RequestSyncPacket::decode(&send.packet.payload).expect("request decodes")
}

fn bundle_packet(owner_static: [u8; 32], timestamp_ms: u64) -> BitchatPacket {
    let bundle = PrekeyBundle {
        owner_static,
        prekeys: vec![Prekey {
            id: 0,
            public_key: [0x11; 32],
        }],
        generated_at: timestamp_ms,
        signature: [0u8; 64],
    };
    packet_of(
        MessageType::PrekeyBundle,
        &bundle.encode().expect("bundle encodes"),
        timestamp_ms,
    )
}

mod protocol;
mod storage;
