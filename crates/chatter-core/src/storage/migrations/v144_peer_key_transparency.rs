use super::Migration;

pub struct V144PeerKeyTransparency;

impl Migration for V144PeerKeyTransparency {
    fn version(&self) -> u32 {
        144
    }

    fn description(&self) -> &'static str {
        "Track transparency verification of exact Signal identity keys"
    }

    fn up(&self) -> &'static str {
        r#"
        ALTER TABLE dm_outbox ADD COLUMN mailbox_transport INTEGER NOT NULL DEFAULT 0;
        ALTER TABLE dm_outbox ADD COLUMN mailbox_started INTEGER NOT NULL DEFAULT 0;
        CREATE TABLE peer_key_transparency (
            account TEXT NOT NULL,
            identity_key BLOB NOT NULL,
            state TEXT NOT NULL CHECK (state IN ('legacy', 'legacy-unlisted', 'unverified-inbound', 'verified')),
            PRIMARY KEY (account, identity_key)
        );
        INSERT INTO peer_key_transparency SELECT address, identity_key, 'legacy' FROM remote_identities;
        CREATE TRIGGER peer_kt_identity_insert AFTER INSERT ON remote_identities BEGIN
            INSERT OR IGNORE INTO peer_key_transparency VALUES (NEW.address, NEW.identity_key, 'unverified-inbound');
        END;
        CREATE TRIGGER peer_kt_identity_update AFTER UPDATE OF identity_key ON remote_identities
        WHEN NEW.identity_key != OLD.identity_key BEGIN
            INSERT OR IGNORE INTO peer_key_transparency VALUES (NEW.address, NEW.identity_key, 'unverified-inbound');
        END;
        "#
    }
}
