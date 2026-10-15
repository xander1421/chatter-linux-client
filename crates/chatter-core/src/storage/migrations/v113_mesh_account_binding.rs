//! Device-local, SQLCipher-protected mesh account bindings and proof replay log.
use super::Migration;

pub struct V113MeshAccountBinding;
impl Migration for V113MeshAccountBinding {
    fn version(&self) -> u32 {
        113
    }
    fn description(&self) -> &'static str {
        "Private mesh account bindings"
    }
    fn up(&self) -> &'static str {
        "CREATE TABLE mesh_account_bindings (
            owner_id TEXT NOT NULL,
            peer_id TEXT NOT NULL,
            account_id BLOB NOT NULL CHECK(length(account_id) = 16),
            device_id INTEGER NOT NULL,
            signal_identity BLOB NOT NULL CHECK(length(signal_identity) = 33),
            mesh_static BLOB NOT NULL CHECK(length(mesh_static) = 32),
            mesh_signing BLOB NOT NULL CHECK(length(mesh_signing) = 32),
            generation INTEGER NOT NULL,
            method INTEGER NOT NULL,
            state INTEGER NOT NULL,
            PRIMARY KEY(owner_id, peer_id)
        );
        CREATE TABLE mesh_binding_replays (
            owner_id TEXT NOT NULL,
            proof_hash BLOB NOT NULL CHECK(length(proof_hash) = 32),
            expires_at INTEGER NOT NULL,
            PRIMARY KEY(owner_id, proof_hash)
        );
        CREATE INDEX idx_mesh_binding_replays_expiry ON mesh_binding_replays(expires_at);
        CREATE TABLE mesh_binding_offers (
            owner_id TEXT NOT NULL,
            peer_id TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            PRIMARY KEY(owner_id, peer_id)
        );
        CREATE TABLE mesh_hello_nonces (
            peer_id TEXT NOT NULL,
            nonce BLOB NOT NULL CHECK(length(nonce) = 16),
            seen_at INTEGER NOT NULL,
            PRIMARY KEY(peer_id, nonce)
        );
        CREATE INDEX idx_mesh_hello_nonces_seen ON mesh_hello_nonces(seen_at);"
    }
}
