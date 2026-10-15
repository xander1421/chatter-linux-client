//! Per-device account association for explicitly selected mesh peers.
//! SQLCipher is the only persistence boundary; nothing is exported in backup.

use std::sync::Arc;

use chatter_mesh::ids::PeerId;
use prost::Message as _;
use rusqlite::{OptionalExtension, params};

use crate::error::StorageError;
use crate::services::mesh::account_binding::{BindingV1, CLOCK_SKEW_SECS};
use crate::services::mesh::signal_mesh::BundleFrame;
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
            "SELECT account_id,signal_identity FROM mesh_account_bindings WHERE owner_id=?1 AND peer_id=?2",
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

    /// Return the account identity only for a currently verified device.
    /// A quarantined binding is never a source of Signal trust.
    pub fn verified_identity(
        &self,
        owner: &str,
        peer: &PeerId,
    ) -> Result<Option<([u8; 16], u32, Vec<u8>)>, StorageError> {
        let conn = self.db.read_connection()?;
        let row: Option<(Vec<u8>, i64, Vec<u8>)> = conn
            .query_row(
                "SELECT account_id,device_id,signal_identity FROM mesh_account_bindings
             WHERE owner_id=?1 AND peer_id=?2 AND state=1",
                params![owner, peer.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        row.map(|(account, device, key)| {
            let account = account
                .try_into()
                .map_err(|_| StorageError::InvalidData("invalid mesh account ID".into()))?;
            let device = u32::try_from(device)
                .map_err(|_| StorageError::InvalidData("invalid mesh device ID".into()))?;
            Ok((account, device, key))
        })
        .transpose()
    }

    pub fn verified_signing_key(
        &self,
        owner: &str,
        peer: &PeerId,
    ) -> Result<Option<[u8; 32]>, StorageError> {
        let conn = self.db.read_connection()?;
        let key: Option<Vec<u8>> = conn.query_row(
            "SELECT mesh_signing FROM mesh_account_bindings WHERE owner_id=?1 AND peer_id=?2 AND state=1",
            params![owner, peer.as_str()], |r| r.get(0),
        ).optional()?;
        key.map(|key| {
            key.try_into()
                .map_err(|_| StorageError::InvalidData("invalid mesh signing key".into()))
        })
        .transpose()
    }

    /// Offline courier sealing uses the exact Noise static key covered by the
    /// locally verified MP1 binding, never a fresh unverified announce.
    pub fn verified_noise_key(
        &self,
        owner: &str,
        peer: &PeerId,
    ) -> Result<Option<[u8; 32]>, StorageError> {
        let conn = self.db.read_connection()?;
        let key: Option<Vec<u8>> = conn.query_row(
            "SELECT mesh_static FROM mesh_account_bindings WHERE owner_id=?1 AND peer_id=?2 AND state=1",
            params![owner, peer.as_str()], |r| r.get(0),
        ).optional()?;
        key.map(|key| {
            key.try_into()
                .map_err(|_| StorageError::InvalidData("invalid verified mesh static key".into()))
        })
        .transpose()
    }

    pub fn signal_opted_in(&self, owner: &str, peer: &PeerId) -> Result<bool, StorageError> {
        let conn = self.db.read_connection()?;
        let enabled: Option<i64> = conn
            .query_row(
                "SELECT enabled FROM mesh_signal_opt_in WHERE owner_id=?1 AND peer_id=?2",
                params![owner, peer.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        Ok(enabled == Some(1))
    }

    pub fn set_signal_opt_in(
        &self,
        owner: &str,
        peer: &PeerId,
        enabled: bool,
    ) -> Result<(), StorageError> {
        if enabled && !matches!(self.state(owner, peer)?, PeerBinding::Verified { .. }) {
            return Err(StorageError::InvalidData(
                "verify peer before Signal mesh opt-in".into(),
            ));
        }
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO mesh_signal_opt_in(owner_id,peer_id,enabled) VALUES(?1,?2,?3)
             ON CONFLICT(owner_id,peer_id) DO UPDATE SET enabled=excluded.enabled",
            params![owner, peer.as_str(), i64::from(enabled)],
        )?;
        if !enabled {
            tx.execute(
                "DELETE FROM mesh_signal_outbox WHERE owner_id=?1 AND peer_id=?2",
                params![owner, peer.as_str()],
            )?;
            tx.execute(
                "DELETE FROM mesh_signal_courier_handoffs WHERE owner_id=?1 AND peer_id=?2",
                params![owner, peer.as_str()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn queue_mesh_carrier(
        &self,
        owner: &str,
        peer: &PeerId,
        frame: &crate::services::mesh::signal_mesh::CarrierFrame,
    ) -> Result<Vec<u8>, StorageError> {
        let bytes = frame
            .encode()
            .map_err(|_| StorageError::InvalidData("invalid mesh Signal carrier".into()))?;
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        if tx.query_row("SELECT 1 FROM mesh_signal_outbox WHERE owner_id=?1 AND peer_id=?2 AND message_id=?3",
            params![owner, peer.as_str(), &frame.message_id[..]], |_| Ok(())).optional()?.is_some() {
            return Ok(bytes);
        }
        tx.execute(
            "DELETE FROM mesh_signal_outbox WHERE expires_at<strftime('%s','now')",
            [],
        )?;
        let pending: i64 = tx.query_row(
            "SELECT COUNT(*) FROM mesh_signal_outbox WHERE owner_id=?1 AND peer_id=?2",
            params![owner, peer.as_str()],
            |r| r.get(0),
        )?;
        if pending >= 64 {
            return Err(StorageError::InvalidData(
                "mesh Signal outbox is full".into(),
            ));
        }
        tx.execute(
            "INSERT INTO mesh_signal_outbox(owner_id,peer_id,message_id,carrier,expires_at) VALUES(?1,?2,?3,?4,?5)",
            params![owner, peer.as_str(), &frame.message_id[..], &bytes, i64::try_from(frame.expires_at).unwrap_or(i64::MAX)],
        )?;
        tx.commit()?;
        Ok(bytes)
    }

    pub fn pending_mesh_carriers(
        &self,
        owner: &str,
        peer: &PeerId,
        now: u64,
    ) -> Result<Vec<Vec<u8>>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT carrier FROM mesh_signal_outbox WHERE owner_id=?1 AND peer_id=?2 AND expires_at>=?3 ORDER BY rowid LIMIT 64",
        )?;
        let rows = stmt.query_map(
            params![owner, peer.as_str(), i64::try_from(now).unwrap_or(i64::MAX)],
            |r| r.get(0),
        )?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::Database)
    }

    /// At most 64 pending carriers are reconsidered when a new verified
    /// courier connects. The handoff ledger below enforces four distinct
    /// couriers across reconnects and restarts.
    pub fn pending_mesh_courier_carriers(
        &self,
        owner: &str,
        now: u64,
    ) -> Result<Vec<(PeerId, Vec<u8>)>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare("SELECT o.peer_id,o.carrier FROM mesh_signal_outbox o
            JOIN mesh_signal_opt_in i ON i.owner_id=o.owner_id AND i.peer_id=o.peer_id AND i.enabled=1
            WHERE o.owner_id=?1 AND o.expires_at>=?2 AND
            (SELECT COUNT(*) FROM mesh_signal_courier_handoffs h WHERE
                h.owner_id=o.owner_id AND h.peer_id=o.peer_id AND h.message_id=o.message_id)<4
            ORDER BY o.rowid LIMIT 64")?;
        let rows = stmt.query_map(
            params![owner, i64::try_from(now).unwrap_or(i64::MAX)],
            |r| {
                Ok((
                    PeerId::from_str(&r.get::<_, String>(0)?),
                    r.get::<_, Vec<u8>>(1)?,
                ))
            },
        )?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::Database)
    }

    pub fn reserve_mesh_courier(
        &self,
        owner: &str,
        peer: &PeerId,
        id: &[u8; 16],
        courier: &PeerId,
    ) -> Result<bool, StorageError> {
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        let pending: i64 = tx.query_row("SELECT COUNT(*) FROM mesh_signal_outbox
            WHERE owner_id=?1 AND peer_id=?2 AND message_id=?3 AND expires_at>=strftime('%s','now')",
            params![owner, peer.as_str(), &id[..]], |r| r.get(0))?;
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM mesh_signal_courier_handoffs
            WHERE owner_id=?1 AND peer_id=?2 AND message_id=?3",
            params![owner, peer.as_str(), &id[..]],
            |r| r.get(0),
        )?;
        if pending == 0 || count >= 4 {
            return Ok(false);
        }
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO mesh_signal_courier_handoffs
            (owner_id,peer_id,message_id,courier_id) VALUES(?1,?2,?3,?4)",
            params![owner, peer.as_str(), &id[..], courier.as_str()],
        )? == 1;
        tx.commit()?;
        Ok(inserted)
    }

    pub fn release_mesh_courier(
        &self,
        owner: &str,
        peer: &PeerId,
        id: &[u8; 16],
        courier: &PeerId,
    ) -> Result<(), StorageError> {
        self.db.connection()?.execute(
            "DELETE FROM mesh_signal_courier_handoffs
            WHERE owner_id=?1 AND peer_id=?2 AND message_id=?3 AND courier_id=?4",
            params![owner, peer.as_str(), &id[..], courier.as_str()],
        )?;
        Ok(())
    }

    pub fn mesh_received(
        &self,
        owner: &str,
        peer: &PeerId,
        id: &[u8; 16],
    ) -> Result<Option<i64>, StorageError> {
        let conn = self.db.read_connection()?;
        conn.query_row(
            "SELECT kind FROM mesh_signal_received WHERE owner_id=?1 AND peer_id=?2 AND message_id=?3",
            params![owner, peer.as_str(), &id[..]], |r| r.get(0),
        ).optional().map_err(StorageError::Database)
    }

    /// Save the pre-decrypt ratchet and one-time-key state before libsignal can
    /// mutate it. A pending row means recovery must restore this snapshot and
    /// replay the retained carrier, even when the timeline insert succeeded.
    pub fn begin_mesh_receive(
        &self,
        owner: &str,
        peer: &PeerId,
        id: &[u8; 16],
        carrier: &[u8],
        timestamp_ms: u64,
        session_address: &str,
        snapshot_prekeys: bool,
    ) -> Result<(), StorageError> {
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        let pending: i64 = tx.query_row(
            "SELECT COUNT(*) FROM mesh_signal_receive_journal WHERE owner_id=?1",
            [owner],
            |r| r.get(0),
        )?;
        if pending != 0 {
            return Err(StorageError::InvalidData(
                "mesh Signal receive recovery is pending".into(),
            ));
        }
        let old_session: Option<Vec<u8>> = tx
            .query_row(
                "SELECT record FROM sessions WHERE address=?1",
                [session_address],
                |r| r.get(0),
            )
            .optional()?;
        tx.execute(
            "INSERT INTO mesh_signal_receive_journal
            (owner_id,peer_id,message_id,carrier,received_at_ms,session_address,old_session,snapshot_prekeys)
            VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                owner,
                peer.as_str(),
                &id[..],
                carrier,
                i64::try_from(timestamp_ms).unwrap_or(i64::MAX),
                session_address,
                old_session, i64::from(snapshot_prekeys)
            ],
        )?;
        if snapshot_prekeys {
            tx.execute(
                "INSERT INTO mesh_signal_receive_prekeys
            SELECT ?1,?2,?3,kind,key_id,record,used,expires_at FROM mesh_signal_prekeys
            WHERE owner_id=?1",
                params![owner, peer.as_str(), &id[..]],
            )?;
            tx.execute(
                "INSERT INTO mesh_signal_receive_kyber_uses
            SELECT ?1,?2,?3,kyber_id,ec_id,base_key,expires_at FROM mesh_signal_prekey_uses
            WHERE owner_id=?1",
                params![owner, peer.as_str(), &id[..]],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Undo an interrupted receive in one SQLCipher transaction. The caller
    /// serializes all mesh Signal ratchets with the shared signal lock.
    pub fn abort_mesh_receive(
        &self,
        owner: &str,
        peer: &PeerId,
        id: &[u8; 16],
    ) -> Result<(), StorageError> {
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        let old: Option<(String, Option<Vec<u8>>, i64)> = tx
            .query_row(
                "SELECT session_address,old_session,snapshot_prekeys FROM mesh_signal_receive_journal
             WHERE owner_id=?1 AND peer_id=?2 AND message_id=?3",
                params![owner, peer.as_str(), &id[..]],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((address, record, snapshot_prekeys)) = old else {
            return Ok(());
        };
        match record {
            Some(record) => {
                tx.execute(
                    "UPDATE sessions SET record=?2 WHERE address=?1",
                    params![address, record],
                )?;
            }
            None => {
                tx.execute("DELETE FROM sessions WHERE address=?1", [address])?;
            }
        }
        if snapshot_prekeys == 1 {
            tx.execute("DELETE FROM mesh_signal_prekeys WHERE owner_id=?1", [owner])?;
            tx.execute(
                "INSERT INTO mesh_signal_prekeys
            SELECT owner_id,kind,key_id,record,used,expires_at FROM mesh_signal_receive_prekeys
            WHERE owner_id=?1 AND peer_id=?2 AND message_id=?3",
                params![owner, peer.as_str(), &id[..]],
            )?;
            tx.execute(
                "DELETE FROM mesh_signal_prekey_uses WHERE owner_id=?1",
                [owner],
            )?;
            tx.execute(
                "INSERT INTO mesh_signal_prekey_uses
            SELECT owner_id,kyber_id,ec_id,base_key,expires_at FROM mesh_signal_receive_kyber_uses
            WHERE owner_id=?1 AND peer_id=?2 AND message_id=?3",
                params![owner, peer.as_str(), &id[..]],
            )?;
        }
        Self::clear_receive_snapshot(&tx, owner, peer, id)?;
        tx.commit()?;
        Ok(())
    }

    fn clear_receive_snapshot(
        tx: &rusqlite::Transaction<'_>,
        owner: &str,
        peer: &PeerId,
        id: &[u8; 16],
    ) -> Result<(), StorageError> {
        for table in [
            "mesh_signal_receive_prekeys",
            "mesh_signal_receive_kyber_uses",
            "mesh_signal_receive_journal",
        ] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE owner_id=?1 AND peer_id=?2 AND message_id=?3"),
                params![owner, peer.as_str(), &id[..]],
            )?;
        }
        Ok(())
    }

    /// Mark the decrypt complete only after the timeline row is durable. For
    /// receipts, removal of the text outbox and its Delivered row is in this
    /// same transaction as dedup and journal removal.
    pub fn finish_mesh_receive(
        &self,
        owner: &str,
        peer: &PeerId,
        id: &[u8; 16],
        kind: i64,
        expires_at: u64,
        receipt_for: Option<(&[u8; 16], &str)>,
    ) -> Result<(bool, bool), StorageError> {
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO mesh_signal_received
            (owner_id,peer_id,message_id,kind,expires_at) VALUES(?1,?2,?3,?4,?5)",
            params![
                owner,
                peer.as_str(),
                &id[..],
                kind,
                i64::try_from(expires_at).unwrap_or(i64::MAX)
            ],
        )? == 1;
        let mut acknowledged = false;
        if let Some((message_id, delivery_id)) = receipt_for {
            acknowledged = tx.execute(
                "DELETE FROM mesh_signal_outbox WHERE owner_id=?1 AND peer_id=?2 AND message_id=?3",
                params![owner, peer.as_str(), &message_id[..]],
            )? == 1;
            if acknowledged {
                tx.execute("DELETE FROM mesh_signal_courier_handoffs WHERE owner_id=?1 AND peer_id=?2 AND message_id=?3",
                    params![owner, peer.as_str(), &message_id[..]])?;
                tx.execute(
                    "INSERT INTO mesh_message_delivery(message_id,state,updated_at)
                VALUES(?1,'delivered',?2) ON CONFLICT(message_id) DO UPDATE SET
                state='delivered',updated_at=excluded.updated_at",
                    params![delivery_id, chrono::Utc::now().to_rfc3339()],
                )?;
            }
        }
        Self::clear_receive_snapshot(&tx, owner, peer, id)?;
        tx.commit()?;
        Ok((inserted, acknowledged))
    }

    pub fn pending_mesh_receive_replays(
        &self,
        owner: &str,
    ) -> Result<Vec<(PeerId, [u8; 16], Vec<u8>, u64)>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare("SELECT peer_id,message_id,carrier,received_at_ms FROM mesh_signal_receive_journal WHERE owner_id=?1 LIMIT 64")?;
        let rows = stmt.query_map([owner], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, Vec<u8>>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (peer, id, carrier, timestamp) = row?;
            if let Ok(id) = <[u8; 16]>::try_from(id.as_slice()) {
                out.push((
                    PeerId::from_str(&peer),
                    id,
                    carrier,
                    u64::try_from(timestamp).unwrap_or(0),
                ));
            }
        }
        Ok(out)
    }

    pub fn mark_key_changed(&self, owner: &str, peer: &PeerId) -> Result<(), StorageError> {
        self.db.connection()?.execute(
            "UPDATE mesh_account_bindings SET state=2 WHERE owner_id=?1 AND peer_id=?2",
            params![owner, peer.as_str()],
        )?;
        Ok(())
    }

    /// Kind 1 is a one-time EC key, 2 a signed EC key, 3 a Kyber key.
    /// The Signal trait adapters use this local table instead of the online pool.
    pub fn mesh_prekey(
        &self,
        owner: &str,
        kind: i64,
        id: u32,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        self.db.read_connection()?.query_row(
            "SELECT record FROM mesh_signal_prekeys WHERE owner_id=?1 AND kind=?2 AND key_id=?3 AND used=0 AND expires_at>=strftime('%s','now')",
            params![owner, kind, id], |r| r.get(0),
        ).optional().map_err(StorageError::Database)
    }

    pub fn save_mesh_prekey(
        &self,
        owner: &str,
        kind: i64,
        id: u32,
        record: &[u8],
    ) -> Result<(), StorageError> {
        self.db.connection()?.execute(
            "INSERT INTO mesh_signal_prekeys(owner_id,kind,key_id,record,expires_at) VALUES(?1,?2,?3,?4,strftime('%s','now')+90000)",
            params![owner, kind, id, record],
        )?;
        Ok(())
    }

    pub fn consume_mesh_prekey(&self, owner: &str, id: u32) -> Result<(), StorageError> {
        let changed = self.db.connection()?.execute(
            "UPDATE mesh_signal_prekeys SET used=1,record=x'' WHERE owner_id=?1 AND kind=1 AND key_id=?2 AND used=0",
            params![owner, id],
        )?;
        if changed != 1 {
            return Err(StorageError::InvalidData(
                "mesh one-time prekey already consumed".into(),
            ));
        }
        Ok(())
    }

    pub fn mark_mesh_kyber_used(
        &self,
        owner: &str,
        kyber: u32,
        ec: u32,
        base: &[u8],
    ) -> Result<(), StorageError> {
        let changed = self.db.connection()?.execute(
            "INSERT OR IGNORE INTO mesh_signal_prekey_uses(owner_id,kyber_id,ec_id,base_key,expires_at) VALUES(?1,?2,?3,?4,strftime('%s','now')+90000)",
            params![owner, kyber, ec, base],
        )?;
        if changed != 1 {
            return Err(StorageError::InvalidData(
                "reused mesh Kyber base key".into(),
            ));
        }
        Ok(())
    }

    pub fn prune_mesh_signal_material(&self, owner: &str, now: u64) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let now = i64::try_from(now).unwrap_or(i64::MAX);
        conn.execute(
            "DELETE FROM mesh_signal_prekeys WHERE owner_id=?1 AND expires_at<?2",
            params![owner, now],
        )?;
        conn.execute(
            "DELETE FROM mesh_signal_prekey_uses WHERE owner_id=?1 AND expires_at<?2",
            params![owner, now],
        )?;
        conn.execute(
            "DELETE FROM mesh_signal_received WHERE owner_id=?1 AND expires_at<?2",
            params![owner, now],
        )?;
        conn.execute(
            "DELETE FROM mesh_signal_outbox WHERE owner_id=?1 AND expires_at<?2",
            params![owner, now],
        )?;
        conn.execute("DELETE FROM mesh_signal_courier_handoffs WHERE owner_id=?1 AND NOT EXISTS (
            SELECT 1 FROM mesh_signal_outbox o WHERE o.owner_id=mesh_signal_courier_handoffs.owner_id
            AND o.peer_id=mesh_signal_courier_handoffs.peer_id
            AND o.message_id=mesh_signal_courier_handoffs.message_id)", [owner])?;
        Ok(())
    }

    /// Accept a fresh live Noise bundle only for this exact verified account
    /// device and identity. Replay registration and replacement are atomic.
    pub fn receive_mesh_bundle(
        &self,
        owner: &str,
        peer: &PeerId,
        frame: &BundleFrame,
        now: u64,
    ) -> Result<bool, StorageError> {
        if frame.expires_at < now
            || frame.expires_at > now.saturating_add(720)
            || frame.bundles.is_empty()
            || frame.bundles.len() > 3
        {
            return Err(StorageError::InvalidData(
                "expired or empty mesh Signal bundle".into(),
            ));
        }
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        let pinned: Option<(Vec<u8>, i64, Vec<u8>)> = tx.query_row(
            "SELECT account_id,device_id,signal_identity FROM mesh_account_bindings WHERE owner_id=?1 AND peer_id=?2 AND state=1",
            params![owner, peer.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        ).optional()?;
        let (account, device, identity) =
            pinned.ok_or_else(|| StorageError::InvalidData("mesh peer is not verified".into()))?;
        let device = u32::try_from(device)
            .map_err(|_| StorageError::InvalidData("invalid mesh device".into()))?;
        if account.len() != 16
            || device == 0
            || frame.bundles.iter().any(|bundle| {
                bundle.device_id != device
                    || bundle.identity_key != identity
                    || bundle.registration_id == 0
                    || bundle.registration_id > 16_383
                    || bundle.signed_prekey.is_none()
                    || bundle.kyber_prekey.is_none()
                    || bundle.one_time_prekey.is_none()
            })
        {
            return Err(StorageError::InvalidData(
                "mesh Signal bundle identity mismatch".into(),
            ));
        }
        let mut ec_ids = std::collections::BTreeSet::new();
        let mut pq_ids = std::collections::BTreeSet::new();
        for bundle in &frame.bundles {
            let identity_key = libsignal_protocol::IdentityKey::decode(&bundle.identity_key)
                .map_err(|_| StorageError::InvalidData("invalid mesh Signal identity".into()))?;
            let signed = bundle
                .signed_prekey
                .as_ref()
                .ok_or_else(|| StorageError::InvalidData("missing mesh signed prekey".into()))?;
            let signed_key = libsignal_protocol::PublicKey::deserialize(&signed.public_key)
                .map_err(|_| StorageError::InvalidData("invalid mesh signed prekey".into()))?;
            let pq = bundle
                .kyber_prekey
                .as_ref()
                .ok_or_else(|| StorageError::InvalidData("missing mesh PQ prekey".into()))?;
            let ec = bundle
                .one_time_prekey
                .as_ref()
                .ok_or_else(|| StorageError::InvalidData("missing mesh one-time prekey".into()))?;
            if !ec_ids.insert(ec.id) || !pq_ids.insert(pq.id) {
                return Err(StorageError::InvalidData(
                    "reused mesh bundle prekey ID".into(),
                ));
            }
            libsignal_protocol::PublicKey::deserialize(&ec.public_key)
                .map_err(|_| StorageError::InvalidData("invalid mesh one-time prekey".into()))?;
            let pq_key: libsignal_protocol::kem::Key<libsignal_protocol::kem::Public> =
                libsignal_protocol::kem::Key::try_from(pq.public_key.as_slice())
                    .map_err(|_| StorageError::InvalidData("invalid mesh PQ prekey".into()))?;
            if !identity_key
                .public_key()
                .verify_signature(&signed_key.serialize(), &signed.signature)
                || !identity_key
                    .public_key()
                    .verify_signature(&pq_key.serialize(), &pq.signature)
            {
                return Err(StorageError::InvalidData(
                    "invalid mesh prekey signature".into(),
                ));
            }
        }
        tx.execute(
            "DELETE FROM mesh_signal_bundle_replays WHERE expires_at<?1",
            params![i64::try_from(now).unwrap_or(i64::MAX)],
        )?;
        let active: i64 = tx.query_row(
            "SELECT COUNT(*) FROM mesh_signal_bundle_replays WHERE owner_id=?1 AND peer_id=?2",
            params![owner, peer.as_str()],
            |r| r.get(0),
        )?;
        if active >= 16 {
            return Err(StorageError::InvalidData(
                "mesh Signal bundle rate limit".into(),
            ));
        }
        let fresh = tx.execute(
            "INSERT OR IGNORE INTO mesh_signal_bundle_replays(owner_id,peer_id,bundle_id,expires_at) VALUES(?1,?2,?3,?4)",
            params![owner, peer.as_str(), &frame.nonce[..], i64::try_from(frame.expires_at).unwrap_or(i64::MAX)],
        )?;
        if fresh == 0 {
            return Ok(false);
        }
        tx.execute(
            "DELETE FROM mesh_signal_remote_bundles WHERE owner_id=?1 AND peer_id=?2",
            params![owner, peer.as_str()],
        )?;
        for (slot, bundle) in frame.bundles.iter().enumerate() {
            tx.execute(
                "INSERT INTO mesh_signal_remote_bundles(owner_id,peer_id,slot,bundle,expires_at) VALUES(?1,?2,?3,?4,?5)",
                params![owner, peer.as_str(), slot as i64, bundle.encode_to_vec(), i64::try_from(frame.expires_at).unwrap_or(i64::MAX)],
            )?;
        }
        tx.commit()?;
        Ok(true)
    }

    pub fn local_mesh_bundle(
        &self,
        owner: &str,
        peer: &PeerId,
        now: u64,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        let conn = self.db.read_connection()?;
        conn.query_row(
            "SELECT frame FROM mesh_signal_local_bundle WHERE owner_id=?1 AND peer_id=?2 AND expires_at>=?3",
            params![owner, peer.as_str(), i64::try_from(now).unwrap_or(i64::MAX)], |r| r.get(0),
        ).optional().map_err(StorageError::Database)
    }

    pub fn save_local_mesh_bundle(
        &self,
        owner: &str,
        peer: &PeerId,
        bytes: &[u8],
        expires_at: u64,
    ) -> Result<(), StorageError> {
        self.db.connection()?.execute(
            "INSERT INTO mesh_signal_local_bundle(owner_id,peer_id,frame,expires_at) VALUES(?1,?2,?3,?4)
             ON CONFLICT(owner_id,peer_id) DO UPDATE SET frame=excluded.frame,expires_at=excluded.expires_at",
            params![owner, peer.as_str(), bytes, i64::try_from(expires_at).unwrap_or(i64::MAX)],
        )?;
        Ok(())
    }

    /// Reserve one bundle before PQXDH; even a failed establishment cannot
    /// silently reoffer the same one-time prekey to another initiation.
    pub fn take_remote_mesh_bundle(
        &self,
        owner: &str,
        peer: &PeerId,
        now: u64,
    ) -> Result<Option<crate::proto::chat_v1::SignalPreKeyBundle>, StorageError> {
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        let row: Option<(i64, Vec<u8>)> = tx.query_row(
            "SELECT slot,bundle FROM mesh_signal_remote_bundles WHERE owner_id=?1 AND peer_id=?2 AND expires_at>=?3 AND used=0 ORDER BY slot LIMIT 1",
            params![owner, peer.as_str(), i64::try_from(now).unwrap_or(i64::MAX)],
            |r| Ok((r.get(0)?, r.get(1)?)),
        ).optional()?;
        let Some((slot, bytes)) = row else {
            return Ok(None);
        };
        tx.execute(
            "UPDATE mesh_signal_remote_bundles SET used=1 WHERE owner_id=?1 AND peer_id=?2 AND slot=?3",
            params![owner, peer.as_str(), slot],
        )?;
        tx.commit()?;
        crate::proto::chat_v1::SignalPreKeyBundle::decode(bytes.as_slice())
            .map(Some)
            .map_err(|_| StorageError::InvalidData("corrupt remote mesh bundle".into()))
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
        let old_account = old
            .as_ref()
            .and_then(|(account, _, _, _, _, _)| uuid::Uuid::from_slice(account).ok());
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
        tx.execute(
            "DELETE FROM mesh_signal_remote_bundles WHERE owner_id=?1 AND peer_id=?2",
            params![owner, peer.as_str()],
        )?;
        tx.execute(
            "DELETE FROM mesh_signal_local_bundle WHERE owner_id=?1 AND peer_id=?2",
            params![owner, peer.as_str()],
        )?;
        // A new or renewed binding must not inherit a ratchet from an old mesh
        // key for this account. Keep this in the same transaction as the trust
        // decision so a crash cannot leave a Verified row with an old session.
        let prefix = format!("mesh:{owner}:{}:", uuid::Uuid::from_bytes(proof.account_id));
        tx.execute(
            "DELETE FROM sessions WHERE substr(address, 1, length(?1)) = ?1",
            params![prefix],
        )?;
        if let Some(old_account) = old_account {
            let old_prefix = format!("mesh:{owner}:{old_account}:");
            tx.execute(
                "DELETE FROM sessions WHERE substr(address, 1, length(?1)) = ?1",
                params![old_prefix],
            )?;
        }
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
        for table in [
            "mesh_signal_prekeys",
            "mesh_signal_prekey_uses",
            "mesh_signal_bundle_replays",
            "mesh_signal_local_bundle",
            "mesh_signal_remote_bundles",
            "mesh_signal_received",
            "mesh_signal_outbox",
            "mesh_signal_opt_in",
            "mesh_signal_courier_handoffs",
            "mesh_signal_receive_journal",
            "mesh_signal_receive_prekeys",
            "mesh_signal_receive_kyber_uses",
        ] {
            tx.execute(&format!("DELETE FROM {table} WHERE owner_id=?1"), [owner])?;
        }
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
        for table in [
            "mesh_signal_prekeys",
            "mesh_signal_prekey_uses",
            "mesh_signal_bundle_replays",
            "mesh_signal_local_bundle",
            "mesh_signal_remote_bundles",
            "mesh_signal_received",
            "mesh_signal_outbox",
            "mesh_signal_opt_in",
            "mesh_signal_courier_handoffs",
            "mesh_signal_receive_journal",
            "mesh_signal_receive_prekeys",
            "mesh_signal_receive_kyber_uses",
        ] {
            tx.execute(&format!("DELETE FROM {table}"), [])?;
        }
        tx.commit()?;
        Ok(())
    }
}
