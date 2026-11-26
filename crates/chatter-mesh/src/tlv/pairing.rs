//! Compact, per-pair rendezvous beacon for BLE service data.

/// Five bytes of broadcast epoch nonce and an eight-byte recognition tag.
/// A beacon selects a possible contact. The ensuing Noise session must prove
/// possession of the pairing key before it can authorize traffic.
pub type PairingBeacon = [u8; 13];

pub const PAIRING_BEACON_NONCE_LEN: usize = 5;

/// Each epoch cycles eight slots in BLE service data and GATT announces.
/// Unused slots carry decoy tags with the same epoch nonce and wire shape.
pub const PAIRING_BEACON_SLOTS: usize = 8;
