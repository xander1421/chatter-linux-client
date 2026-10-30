//! Device-local courier budgets and crash recovery for mesh Signal receives.
use super::Migration;

pub struct V115MeshSignalCourier;

impl Migration for V115MeshSignalCourier {
    fn version(&self) -> u32 {
        115
    }
    fn description(&self) -> &'static str {
        "Mesh Signal courier and receive recovery"
    }
    fn up(&self) -> &'static str {
        "CREATE TABLE mesh_signal_courier_handoffs (
            owner_id TEXT NOT NULL, peer_id TEXT NOT NULL,
            message_id BLOB NOT NULL, courier_id TEXT NOT NULL,
            PRIMARY KEY(owner_id,peer_id,message_id,courier_id)
        );
        CREATE TABLE mesh_signal_receive_journal (
            owner_id TEXT NOT NULL, peer_id TEXT NOT NULL,
            message_id BLOB NOT NULL, carrier BLOB NOT NULL,
            received_at_ms INTEGER NOT NULL,
            session_address TEXT NOT NULL, old_session BLOB,
            snapshot_prekeys INTEGER NOT NULL CHECK(snapshot_prekeys IN (0,1)),
            PRIMARY KEY(owner_id,peer_id,message_id)
        );
        CREATE TABLE mesh_signal_receive_prekeys (
            owner_id TEXT NOT NULL, peer_id TEXT NOT NULL, message_id BLOB NOT NULL,
            kind INTEGER NOT NULL, key_id INTEGER NOT NULL,
            record BLOB NOT NULL, used INTEGER NOT NULL, expires_at INTEGER NOT NULL,
            PRIMARY KEY(owner_id,peer_id,message_id,kind,key_id)
        );
        CREATE TABLE mesh_signal_receive_kyber_uses (
            owner_id TEXT NOT NULL, peer_id TEXT NOT NULL, message_id BLOB NOT NULL,
            kyber_id INTEGER NOT NULL, ec_id INTEGER NOT NULL,
            base_key BLOB NOT NULL, expires_at INTEGER NOT NULL,
            PRIMARY KEY(owner_id,peer_id,message_id,kyber_id,ec_id,base_key)
        );"
    }
}
