//! V057: Durable blobs for the bitchat mesh store-and-forward stack.
//!
//! One row per `chatter_mesh_store::StorageKey` — the retained outbox, the
//! courier pool, the public gossip archive, the local metrics counters, and the
//! bridge-drop dedup set. The set of keys is a closed enum in that crate, so
//! this table stays small and fixed rather than growing a row per message.
//!
//! Every value in `sealed` is ciphertext produced by `MeshSealer`, under a key
//! that lives in its own OS-keyring entry — *not* the SQLCipher passphrase.
//! See `storage/mesh_store.rs` for why that is not redundant with the database
//! already being encrypted at rest.

use super::Migration;

pub struct V057MeshStore;

impl Migration for V057MeshStore {
    fn version(&self) -> u32 {
        57
    }

    fn description(&self) -> &'static str {
        "Sealed blob storage for the bitchat mesh store-and-forward stack"
    }

    fn up(&self) -> &'static str {
        // `store_key` rather than `key`: KEY is a SQLite keyword and reads
        // badly unquoted in every statement that touches this table.
        //
        // `sealed BLOB NOT NULL` is load bearing. The Storage contract says a
        // row that exists but cannot be read must surface as an error, never as
        // "absent" — a NULL here would be a row that exists whose value decodes
        // to nothing, which is exactly the ambiguity that makes a caller start
        // empty and overwrite mail it could not read.
        r#"
        CREATE TABLE IF NOT EXISTS mesh_store (
            store_key  TEXT PRIMARY KEY NOT NULL,
            sealed     BLOB NOT NULL,
            updated_at TEXT NOT NULL
        );
        "#
    }
}
