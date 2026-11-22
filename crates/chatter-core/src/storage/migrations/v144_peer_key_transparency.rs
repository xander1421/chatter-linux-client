use super::Migration;

pub struct V144PeerKeyTransparency;

impl Migration for V144PeerKeyTransparency {
    fn version(&self) -> u32 {
        144
    }

    fn description(&self) -> &'static str {
        "Track verification of exact Signal identities and their lookup routes"
    }

    fn up(&self) -> &'static str {
        r#"
        CREATE TABLE peer_key_transparency (
            account TEXT NOT NULL,
            identity_key BLOB NOT NULL,
            state TEXT NOT NULL CHECK (state IN (
                'legacy', 'legacy-unlisted', 'unverified-inbound', 'verified', 'rejected'
            )),
            PRIMARY KEY (account, identity_key)
        );
        INSERT INTO peer_key_transparency
            SELECT address, identity_key, 'legacy' FROM remote_identities;
        CREATE TRIGGER peer_kt_identity_insert AFTER INSERT ON remote_identities BEGIN
            INSERT OR IGNORE INTO peer_key_transparency
                VALUES (NEW.address, NEW.identity_key, 'unverified-inbound');
        END;
        CREATE TRIGGER peer_kt_identity_update AFTER UPDATE OF identity_key ON remote_identities
        WHEN NEW.identity_key != OLD.identity_key BEGIN
            INSERT OR IGNORE INTO peer_key_transparency
                VALUES (NEW.address, NEW.identity_key, 'unverified-inbound');
        END;
        CREATE TABLE peer_kt_jobs (
            account TEXT NOT NULL,
            identity_key BLOB NOT NULL,
            route INTEGER NOT NULL CHECK (route IN (0, 1)),
            attempts INTEGER NOT NULL DEFAULT 0,
            archived INTEGER NOT NULL DEFAULT 0 CHECK (archived IN (0, 1)),
            due_at INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (account, identity_key, route)
        );
        CREATE INDEX peer_kt_due ON peer_kt_jobs(route, due_at);
        CREATE TABLE peer_kt_archived_receives (
            account TEXT NOT NULL,
            pinned_key BLOB NOT NULL,
            archived_key BLOB NOT NULL,
            route INTEGER NOT NULL,
            PRIMARY KEY(account,pinned_key,archived_key,route)
        );
        CREATE TABLE dm_bootstrap_replies (
            owner_id TEXT NOT NULL,
            envelope_id TEXT NOT NULL,
            peer_id TEXT NOT NULL,
            device_id INTEGER NOT NULL,
            payload BLOB NOT NULL,
            next_attempt_at INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY(owner_id,envelope_id)
        );
        CREATE INDEX dm_bootstrap_replies_due ON dm_bootstrap_replies(owner_id,next_attempt_at);
        "#
    }
}
