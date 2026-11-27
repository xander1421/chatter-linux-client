//! Compact, per-pair rendezvous beacon for BLE service data.

/// A 13-byte recognition tag for a short clock window.
/// A beacon selects a possible contact. The ensuing Noise session must prove
/// possession of the pairing key before it can authorize traffic.
pub type PairingBeacon = [u8; 13];

/// Each short window cycles eight slots in BLE service data and GATT announces.
/// Unused slots carry independent decoy tags with the same wire shape.
pub const PAIRING_BEACON_SLOTS: usize = 8;
