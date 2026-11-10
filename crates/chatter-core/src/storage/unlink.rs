//! Local data of a device the account unlinked, handled as Signal-Desktop
//! handles it.
//!
//! Signal-Desktop, told by the server that it is no longer linked (401/403 on
//! the authenticated socket, `SocketManager.preload.ts` `authError`), runs
//! `unlinkAndDisconnect` (`background.preload.ts`), which immediately deletes
//! the device's *configuration* — keys, sessions, prekeys, sender keys, queued
//! work, credentials (`removeAllConfiguration`, `sql/Server.node.ts`) — and
//! keeps the message history and settings. The history goes only when the
//! device is linked again with a history transfer, or to a different account
//! (`removeAllData`: `DialogRelink.dom.tsx` / `AccountManager.preload.ts`
//! `createAccount`). [`forget_unlinked_device`] is the first step and
//! [`wipe_account_data`] the second.
//!
//! Both run only after the server's dedicated "device unlinked" answer
//! (`network::grpc::interceptors::DEVICE_UNLINKED_MESSAGE`), never on an ended
//! session, a network error or a server failure.
//!
//! The SQLCipher passphrase stays: the database file it protects stays too.

use rusqlite::{Transaction, params};

use super::Database;
use super::repositories::settings_repo::keys;
use crate::domain::ids::UserId;
use crate::error::StorageError;

/// This device's own Signal state, which an unlinked device can never use
/// again: identity keypair, prekeys, ratchet sessions, sender keys and the
/// resend log of messages encrypted under them, and the queued sends that would
/// need those keys (Signal's `jobs`). Remote identities stay, as Signal keeps
/// its `identityKeys` table (the safety numbers the user saw).
const DEVICE_CONFIGURATION_TABLES: &[&str] = &[
    "dm_outbox",
    "pending_dm_messages",
    "pending_channel_messages",
    "pending_anon_group_messages",
    "pending_anon_group_invites",
    "kyber_prekey_usage",
    "sender_keys",
    "sender_key_metadata",
    "sessions",
    "kyber_prekeys",
    "signed_prekeys",
    "prekeys",
    "local_identity",
    "send_log_recipients",
    "send_log_payloads",
];

/// Settings that belong to the device's membership of the account. The
/// profile key, display name and app preferences stay, as Signal keeps
/// `profileKey` and its UI settings.
const DEVICE_CONFIGURATION_SETTINGS: &[&str] = &[
    keys::DEVICE_AUTH_TOKEN,
    keys::CURRENT_USER_ID,
    keys::CURRENT_DEVICE_ID,
    keys::PUSH_TOPIC,
    keys::DEVICE_WAKE_TOPIC,
    keys::PIN_FINGERPRINT,
    keys::IDENTITY_KEY_PUBLIC,
    keys::KEY_REPUBLISH_PENDING,
    keys::PROFILE_KEY_NEXT,
    keys::PROFILE_KEY_ROTATION_PENDING,
    keys::SENDER_CERTIFICATE,
    keys::SENDER_CERTIFICATE_REFRESH_AFTER,
    keys::SENDER_CERTIFICATE_SERVER_KEY_ID,
];

/// Signal's `removeAllConfiguration` for this client, in one transaction:
/// delete the device's keys, sessions and credentials, keep the history, and
/// record `former_owner` as the account this install was unlinked from.
pub fn forget_unlinked_device(db: &Database, former_owner: UserId) -> Result<(), StorageError> {
    let mut conn = db.connection()?;
    let tx = conn.transaction()?;
    for table in DEVICE_CONFIGURATION_TABLES {
        tx.execute(&format!("DELETE FROM {table}"), [])?;
    }
    for key in DEVICE_CONFIGURATION_SETTINGS {
        tx.execute("DELETE FROM settings WHERE key = ?1", params![key])?;
    }
    tx.execute(
        "DELETE FROM settings WHERE key = ?1",
        params![keys::unacknowledged_recovery_codes_key(former_owner)],
    )?;
    let owner = serde_json::to_string(&former_owner.to_string())
        .map_err(|e| StorageError::InvalidData(format!("unlinked account id: {e}")))?;
    tx.execute(
        "INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        params![
            keys::UNLINKED_ACCOUNT_ID,
            owner,
            chrono::Utc::now().to_rfc3339()
        ],
    )?;
    tx.commit()?;
    Ok(())
}

/// Signal's `removeAllData` for this client's database, in one transaction:
/// every row of every table except the migration ledger, the mesh tables and
/// the `mesh.*` settings.
///
/// Mesh is left alone on purpose: it is a separate device identity with its
/// own threat model and its own wipe (`services::mesh::control` `panic_wipe`),
/// and its link to the former account was removed when the device was
/// unlinked. FTS shadow tables are skipped; their virtual table is emptied.
///
/// A trigger can write a row into a table already emptied, so the pass repeats
/// until every table is empty; one that never empties fails the wipe rather
/// than reporting a partial one as done.
pub fn wipe_account_data(db: &Database) -> Result<(), StorageError> {
    let mut conn = db.connection()?;
    let tx = conn.transaction()?;
    tx.execute_batch("PRAGMA defer_foreign_keys = ON")?;
    let tables: Vec<String> = {
        let mut stmt = tx.prepare(
            r"SELECT name FROM pragma_table_list
              WHERE schema = 'main' AND type IN ('table', 'virtual')
                AND name NOT LIKE 'sqlite\_%' ESCAPE '\'
                AND name NOT LIKE 'mesh\_%' ESCAPE '\'
                AND name NOT IN ('schema_version', 'settings')",
        )?;
        stmt.query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?
    };
    for _ in 0..4 {
        for table in &tables {
            tx.execute(
                &format!("DELETE FROM \"{}\"", table.replace('"', "\"\"")),
                [],
            )?;
        }
        tx.execute("DELETE FROM settings WHERE key NOT LIKE 'mesh.%'", [])?;
        if !any_rows(&tx, &tables)? {
            tx.commit()?;
            return Ok(());
        }
    }
    Err(StorageError::InvalidData(
        "account data reappeared on every wipe pass".into(),
    ))
}

/// The account this install's local data belongs to, read from the self-owned
/// SavedMessages channel (`channel_type = 3`, whose `owner_id` is the account's
/// own user id — every account auto-creates exactly one). `None` when there is
/// no local account data to attribute (a genuinely fresh device).
///
/// Nothing is persisted to answer this: the owner lives in the very rows a wipe
/// removes, so a different account signing in both learns the mismatch and
/// erases the evidence in the same step — no retained former-account marker.
pub(crate) fn local_account_owner(db: &Database) -> Result<Option<UserId>, StorageError> {
    let conn = db.connection()?;
    let owner: Option<String> = match conn.query_row(
        "SELECT owner_id FROM channels WHERE channel_type = 3 LIMIT 1",
        [],
        |row| row.get(0),
    ) {
        Ok(v) => Some(v),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(e) => return Err(StorageError::Database(e)),
    };
    Ok(owner.and_then(|s| s.parse().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::channel::{Channel, ChannelType};
    use crate::domain::ids::{ChannelId, UserId};
    use crate::storage::repositories::channel_repo::ChannelRepository;
    use std::sync::Arc;

    fn test_db() -> (Arc<Database>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("owner.db"), "testpass").unwrap();
        db.run_migrations().unwrap();
        (Arc::new(db), dir)
    }

    #[test]
    fn local_account_owner_reads_only_the_saved_messages_channel() {
        let (db, _dir) = test_db();
        let repo = ChannelRepository::new(db.clone());

        // A fresh install attributes to no account.
        assert_eq!(local_account_owner(&db).unwrap(), None);

        // A joined public channel is NOT the install owner — only the
        // self-owned SavedMessages channel is.
        repo.insert(&Channel::new(
            ChannelId::new(),
            "general".into(),
            ChannelType::Public,
            UserId::new(),
        ))
        .unwrap();
        assert_eq!(local_account_owner(&db).unwrap(), None);

        // The SavedMessages channel's owner is the account this install holds.
        let owner = UserId::new();
        repo.insert(&Channel::new(
            ChannelId::new(),
            "saved".into(),
            ChannelType::SavedMessages,
            owner,
        ))
        .unwrap();
        assert_eq!(local_account_owner(&db).unwrap(), Some(owner));

        // A full wipe erases the owner signal with the rest of the account data,
        // so nothing is retained to leak across accounts.
        wipe_account_data(&db).unwrap();
        assert_eq!(local_account_owner(&db).unwrap(), None);
    }
}

fn any_rows(tx: &Transaction<'_>, tables: &[String]) -> Result<bool, StorageError> {
    for table in tables {
        let present: bool = tx.query_row(
            &format!(
                "SELECT EXISTS (SELECT 1 FROM \"{}\")",
                table.replace('"', "\"\"")
            ),
            [],
            |row| row.get(0),
        )?;
        if present {
            return Ok(true);
        }
    }
    Ok(false)
}
