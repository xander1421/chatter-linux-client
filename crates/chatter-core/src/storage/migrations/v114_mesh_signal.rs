//! Local Signal mesh material. These tables are deliberately absent from logical backups.
use super::Migration;

pub struct V114MeshSignal;

impl Migration for V114MeshSignal {
    fn version(&self) -> u32 {
        114
    }
    fn description(&self) -> &'static str {
        "Device-local mesh Signal material"
    }
    fn up(&self) -> &'static str {
        "CREATE TABLE mesh_signal_prekeys (
            owner_id TEXT NOT NULL,
            kind INTEGER NOT NULL CHECK(kind IN (1,2,3)),
            key_id INTEGER NOT NULL,
            record BLOB NOT NULL,
            used INTEGER NOT NULL DEFAULT 0,
            expires_at INTEGER NOT NULL,
            PRIMARY KEY(owner_id,kind,key_id)
        );
        CREATE TABLE mesh_signal_prekey_uses (
            owner_id TEXT NOT NULL,
            kyber_id INTEGER NOT NULL,
            ec_id INTEGER NOT NULL,
            base_key BLOB NOT NULL,
            expires_at INTEGER NOT NULL,
            PRIMARY KEY(owner_id,kyber_id,ec_id,base_key)
        );
        CREATE TABLE mesh_signal_bundle_replays (
            owner_id TEXT NOT NULL,
            peer_id TEXT NOT NULL,
            bundle_id BLOB NOT NULL CHECK(length(bundle_id)=16),
            expires_at INTEGER NOT NULL,
            PRIMARY KEY(owner_id,peer_id,bundle_id)
        );
        CREATE TABLE mesh_signal_local_bundle (
            owner_id TEXT NOT NULL,
            peer_id TEXT NOT NULL,
            frame BLOB NOT NULL,
            expires_at INTEGER NOT NULL,
            PRIMARY KEY(owner_id,peer_id)
        );
        CREATE TABLE mesh_signal_remote_bundles (
            owner_id TEXT NOT NULL,
            peer_id TEXT NOT NULL,
            slot INTEGER NOT NULL,
            bundle BLOB NOT NULL,
            expires_at INTEGER NOT NULL,
            used INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY(owner_id,peer_id,slot)
        );
        CREATE TABLE mesh_signal_received (
            owner_id TEXT NOT NULL,
            peer_id TEXT NOT NULL,
            message_id BLOB NOT NULL CHECK(length(message_id)=16),
            kind INTEGER NOT NULL CHECK(kind IN (1,2)),
            expires_at INTEGER NOT NULL,
            PRIMARY KEY(owner_id,peer_id,message_id)
        );
        CREATE TABLE mesh_signal_outbox (
            owner_id TEXT NOT NULL,
            peer_id TEXT NOT NULL,
            message_id BLOB NOT NULL CHECK(length(message_id)=16),
            carrier BLOB NOT NULL,
            expires_at INTEGER NOT NULL,
            PRIMARY KEY(owner_id,peer_id,message_id)
        );
        CREATE TABLE mesh_signal_opt_in (
            owner_id TEXT NOT NULL,
            peer_id TEXT NOT NULL,
            enabled INTEGER NOT NULL CHECK(enabled IN (0,1)),
            PRIMARY KEY(owner_id,peer_id)
        );"
    }
}
