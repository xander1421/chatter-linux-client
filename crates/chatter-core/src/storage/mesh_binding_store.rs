//! Per-device account association for explicitly selected mesh peers.
//! SQLCipher is the only persistence boundary; nothing is exported in backup.

use std::sync::Arc;

use chatter_mesh::ids::PeerId;
use rusqlite::{OptionalExtension, params};

use crate::error::StorageError;
use crate::services::mesh::account_binding::{BindingV1, CLOCK_SKEW_SECS};
use crate::storage::Database;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindingMethod {
    PinnedContact,
    InPersonQr,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PeerBinding {
    Unverified,
    BindingOffered,
    Verified {
        account_id: [u8; 16],
        device_id: u32,
        method: BindingMethod,
    },
    KeyChanged,
}

impl PeerBinding {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Unverified => "Mesh peer · self-asserted name",
            Self::BindingOffered => "Binding pending",
            Self::Verified { .. } => "Verified Chatter contact",
            Self::KeyChanged => "Identity changed · verify again",
        }
    }
}

pub struct MeshBindingStore {
    db: Arc<Database>,
}

impl MeshBindingStore {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    pub fn note_hello(
        &self,
        peer: &PeerId,
        nonce: &[u8; 16],
        now: u64,
    ) -> Result<bool, StorageError> {
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM mesh_hello_nonces WHERE seen_at < ?1",
            params![i64::try_from(now.saturating_sub(24 * 3600)).unwrap_or(i64::MAX)],
        )?;
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO mesh_hello_nonces(peer_id,nonce,seen_at) VALUES(?1,?2,?3)",
            params![
                peer.as_str(),
                &nonce[..],
                i64::try_from(now).unwrap_or(i64::MAX)
            ],
        )?;
        tx.commit()?;
        Ok(inserted == 1)
    }

    pub fn offer(&self, owner: &str, peer: &PeerId, now: u64) -> Result<(), StorageError> {
        self.db.connection()?.execute(
            "INSERT INTO mesh_binding_offers(owner_id,peer_id,created_at) VALUES(?1,?2,?3)
             ON CONFLICT(owner_id,peer_id) DO UPDATE SET created_at=excluded.created_at",
            params![owner, peer.as_str(), i64::try_from(now).unwrap_or(i64::MAX)],
        )?;
        Ok(())
    }

    pub fn has_offer(&self, owner: &str, peer: &PeerId) -> Result<bool, StorageError> {
        let conn = self.db.read_connection()?;
        Ok(conn
            .query_row(
                "SELECT 1 FROM mesh_binding_offers WHERE owner_id=?1 AND peer_id=?2",
                params![owner, peer.as_str()],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    pub fn state(&self, owner: &str, peer: &PeerId) -> Result<PeerBinding, StorageError> {
        let conn = self.db.read_connection()?;
        let row: Option<(Vec<u8>, i64, i64, i64)> = conn.query_row(
            "SELECT account_id, device_id, method, state FROM mesh_account_bindings WHERE owner_id=?1 AND peer_id=?2",
            params![owner, peer.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        ).optional()?;
        let Some((account, device_id, method, state)) = row else {
            return Ok(PeerBinding::Unverified);
        };
        if state == 2 {
            return Ok(PeerBinding::KeyChanged);
        }
        let account_id: [u8; 16] = account
            .try_into()
            .map_err(|_| StorageError::InvalidData("invalid mesh binding account ID".into()))?;
        let device_id = u32::try_from(device_id)
            .map_err(|_| StorageError::InvalidData("invalid mesh binding device ID".into()))?;
        let method = match method {
            1 => BindingMethod::PinnedContact,
            2 => BindingMethod::InPersonQr,
            _ => {
                return Err(StorageError::InvalidData(
                    "invalid mesh binding method".into(),
                ));
            }
        };
        Ok(PeerBinding::Verified {
            account_id,
            device_id,
            method,
        })
    }

    pub fn observe_live_key(
        &self,
        owner: &str,
        peer: &PeerId,
        key: &[u8; 32],
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "UPDATE mesh_account_bindings SET state=2 WHERE owner_id=?1 AND peer_id=?2 AND mesh_static<>?3",
            params![owner, peer.as_str(), &key[..]],
        )?;
        Ok(())
    }

    pub fn pinned_account_key(
        &self,
        owner: &str,
        peer: &PeerId,
    ) -> Result<Option<([u8; 16], Vec<u8>)>, StorageError> {
        let conn = self.db.read_connection()?;
        let row: Option<(Vec<u8>, Vec<u8>)> = conn.query_row(
            "SELECT account_id,signal_identity FROM mesh_account_bindings WHERE owner_id=?1 AND peer_id=?2 AND method=1",
            params![owner, peer.as_str()], |r| Ok((r.get(0)?, r.get(1)?)),
        ).optional()?;
        row.map(|(id, key)| {
            let id = id
                .try_into()
                .map_err(|_| StorageError::InvalidData("invalid mesh account ID".into()))?;
            Ok((id, key))
        })
        .transpose()
    }

    pub fn mark_key_changed(&self, owner: &str, peer: &PeerId) -> Result<(), StorageError> {
        self.db.connection()?.execute(
            "UPDATE mesh_account_bindings SET state=2 WHERE owner_id=?1 AND peer_id=?2",
            params![owner, peer.as_str()],
        )?;
        Ok(())
    }

    /// Accept once and atomically record both replay token and trust decision.
    /// A changed account key or mesh key quarantines the old binding.
    pub fn accept(
        &self,
        owner: &str,
        peer: &PeerId,
        proof: &BindingV1,
        method: BindingMethod,
        now: u64,
        reverified: bool,
    ) -> Result<PeerBinding, StorageError> {
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM mesh_binding_replays WHERE expires_at < ?1",
            params![i64::try_from(now.saturating_sub(CLOCK_SKEW_SECS)).unwrap_or(i64::MAX)],
        )?;
        let old: Option<(Vec<u8>, i64, Vec<u8>, Vec<u8>, Vec<u8>, i64)> = tx.query_row(
            "SELECT account_id,device_id,signal_identity,mesh_static,mesh_signing,generation FROM mesh_account_bindings WHERE owner_id=?1 AND peer_id=?2",
            params![owner, peer.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        ).optional()?;
        if old.as_ref().is_some_and(|(_, _, _, _, _, generation)| {
            *generation < 0 || proof.generation < *generation as u64
        }) {
            return Err(StorageError::InvalidData(
                "stale mesh binding generation".into(),
            ));
        }
        let max_generation: Option<i64> = tx.query_row(
            "SELECT MAX(generation) FROM mesh_account_bindings WHERE owner_id=?1 AND account_id=?2 AND device_id=?3",
            params![owner, &proof.account_id[..], proof.device_id],
            |r| r.get(0),
        )?;
        if max_generation
            .is_some_and(|generation| generation < 0 || proof.generation < generation as u64)
        {
            return Err(StorageError::InvalidData(
                "stale mesh binding generation".into(),
            ));
        }
        let same_peer_change =
            old.is_some_and(|(account, device, signal, static_key, signing, _)| {
                account != proof.account_id
                    || device != i64::from(proof.device_id)
                    || signal != proof.signal_identity
                    || static_key != proof.mesh_static
                    || signing != proof.mesh_signing
            });
        let moved_peers = tx.execute(
            "UPDATE mesh_account_bindings SET state=2 WHERE owner_id=?1 AND account_id=?2
             AND device_id=?3 AND peer_id<>?4 AND mesh_static<>?5",
            params![
                owner,
                &proof.account_id[..],
                proof.device_id,
                peer.as_str(),
                &proof.mesh_static[..]
            ],
        )?;
        let changed = !reverified && (same_peer_change || moved_peers != 0);
        if !changed {
            let hash = proof.replay_token();
            let fresh = tx.execute(
                "INSERT OR IGNORE INTO mesh_binding_replays(owner_id,proof_hash,expires_at) VALUES(?1,?2,?3)",
                params![owner, &hash[..], i64::try_from(proof.expires_at).unwrap_or(i64::MAX)],
            )?;
            if fresh == 0 {
                return Err(StorageError::InvalidData(
                    "replayed mesh account proof".into(),
                ));
            }
        }
        let state_number = if changed { 2 } else { 1 };
        let method_number = match method {
            BindingMethod::PinnedContact => 1,
            BindingMethod::InPersonQr => 2,
        };
        tx.execute(
            "INSERT INTO mesh_account_bindings(owner_id,peer_id,account_id,device_id,signal_identity,mesh_static,mesh_signing,generation,method,state)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
             ON CONFLICT(owner_id,peer_id) DO UPDATE SET account_id=excluded.account_id,device_id=excluded.device_id,
             signal_identity=excluded.signal_identity,mesh_static=excluded.mesh_static,mesh_signing=excluded.mesh_signing,
             generation=excluded.generation,method=excluded.method,state=excluded.state",
            params![owner, peer.as_str(), &proof.account_id[..], proof.device_id,
                    &proof.signal_identity[..], &proof.mesh_static[..], &proof.mesh_signing[..],
                    i64::try_from(proof.generation).unwrap_or(i64::MAX), method_number, state_number],
        )?;
        tx.execute(
            "DELETE FROM mesh_binding_offers WHERE owner_id=?1 AND peer_id=?2",
            params![owner, peer.as_str()],
        )?;
        tx.commit()?;
        if changed {
            Ok(PeerBinding::KeyChanged)
        } else {
            Ok(PeerBinding::Verified {
                account_id: proof.account_id,
                device_id: proof.device_id,
                method,
            })
        }
    }

    pub fn wipe_owner(&self, owner: &str) -> Result<(), StorageError> {
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM mesh_account_bindings WHERE owner_id=?1",
            [owner],
        )?;
        tx.execute(
            "DELETE FROM mesh_binding_replays WHERE owner_id=?1",
            [owner],
        )?;
        tx.execute("DELETE FROM mesh_binding_offers WHERE owner_id=?1", [owner])?;
        tx.commit()?;
        Ok(())
    }

    pub fn wipe_all(&self) -> Result<(), StorageError> {
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM mesh_account_bindings", [])?;
        tx.execute("DELETE FROM mesh_binding_replays", [])?;
        tx.execute("DELETE FROM mesh_binding_offers", [])?;
        tx.execute("DELETE FROM mesh_hello_nonces", [])?;
        tx.commit()?;
        Ok(())
    }
}
