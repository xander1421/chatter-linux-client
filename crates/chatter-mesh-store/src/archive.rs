//! Durable, individually keyed public broadcast packets for gossip sync.
//!
//! These packets were already sent in the clear to every nearby radio. They
//! need no mesh seal; the application's database protects their rows at rest.

use std::sync::Arc;

use chatter_mesh::codec;
use chatter_mesh::packet::BitchatPacket;

use crate::error::StoreError;
use crate::storage::Storage;

#[derive(Clone)]
pub struct GossipMessageArchive {
    storage: Arc<dyn Storage>,
}

impl core::fmt::Debug for GossipMessageArchive {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GossipMessageArchive").finish_non_exhaustive()
    }
}

impl GossipMessageArchive {
    pub fn new(storage: Arc<dyn Storage>) -> Self {
        Self { storage }
    }

    /// Return the stored key too, so malformed or expired rows can be deleted.
    pub fn load_entries(&self) -> Result<Vec<([u8; 16], Option<BitchatPacket>)>, StoreError> {
        Ok(self.storage.load_archive_packets()?.into_iter().map(|(id, bytes)| {
            (id, codec::decode(&bytes).ok().filter(|packet| packet.packet_id() == id))
        }).collect())
    }

    pub fn load_packets(&self) -> Vec<BitchatPacket> {
        self.load_entries().unwrap_or_default().into_iter()
            .filter_map(|(_, packet)| packet).collect()
    }

    /// Atomically apply only rows touched since the last successful write.
    pub fn apply_changes(&self, changes: &[([u8; 16], Option<BitchatPacket>)]) -> Result<(), StoreError> {
        let encoded = changes.iter().map(|(id, packet)| {
            let bytes = packet.as_ref().map(|packet| codec::encode(packet, false))
                .transpose().map_err(|_| StoreError::Malformed("public archive packet"))?;
            Ok((*id, bytes))
        }).collect::<Result<Vec<_>, StoreError>>()?;
        self.storage.apply_archive_changes(&encoded)
    }

    pub fn wipe(&self) -> Result<(), StoreError> {
        self.storage.wipe_archive()
    }
}
