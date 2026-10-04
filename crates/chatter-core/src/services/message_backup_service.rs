//! Message-history backup service (L7 message-backup).
//!
//! Orchestrates the encrypted message-history backup in both modes:
//!
//! - **Local**: export the encrypted container to a user-chosen file path /
//!   restore from a file. Fully offline; KEK from a user passphrase
//!   (Argon2id, SVR parameters + entropy floor).
//! - **Server**: upload the same container to RustFS via a presigned URL
//!   (`SvrService.SetMessageBackup`) / fetch + restore on a fresh install
//!   after login + PIN. KEK = HKDF(svr_secret, "Chatter Message Backup KEK
//!   v1"); index = HMAC(svr_secret, "MessageBackupId"). The server stores an
//!   opaque blob and never holds key material.
//!
//! What's inside the container and the idempotent import rules live in
//! `storage::backup_data`; the container crypto lives in
//! `crypto::message_backup`.

use std::path::Path;
use std::sync::Arc;

use zeroize::Zeroize;

use crate::crypto::message_backup::{
    decrypt_backup_file, derive_message_backup_id, derive_passphrase_kek, derive_server_kek,
    encrypt_backup_file, parse_backup_file, ARGON2_SALT_LEN, KEY_MODE_LINK, KEY_MODE_PASSPHRASE,
    KEY_MODE_SVR,
};
use crate::error::{CryptoError, NetworkError, StorageError};
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::network::storage_http::storage_client;
use crate::storage::backup_data::{dump_frames, import_frames, ImportSummary};
use crate::storage::repositories::settings_repo::{keys, SettingsRepository};
use crate::storage::Database;

/// Errors from message-backup operations.
#[derive(Debug, thiserror::Error)]
pub enum MessageBackupError {
    #[error("crypto error: {0}")]
    Crypto(#[from] CryptoError),

    #[error("network error: {0}")]
    Network(#[from] NetworkError),

    #[error("storage error: {0}")]
    Storage(#[from] StorageError),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// No backup stored on the server for this PIN.
    #[error("no message backup found on server")]
    BackupNotFound,

    /// Orchestration-level failures (missing settings, HTTP transfer).
    #[error("message backup: {0}")]
    Generic(String),
}

// Presigned URLs are bearer capabilities, including on body-read failures.
fn storage_transfer_error(context: &str, error: reqwest::Error) -> MessageBackupError {
    let error = error.without_url();
    tracing::warn!(operation = context, error = ?error, "Storage archive transfer failed");
    MessageBackupError::Generic(format!("{context}: {error}"))
}

// Both svr.proto and provisioning.proto cap encrypted archives at 1 GiB.
const MAX_ARCHIVE_BYTES: usize = 1_073_741_824;

fn append_archive_chunk(
    body: &mut Vec<u8>,
    chunk: &[u8],
    limit: usize,
) -> Result<(), MessageBackupError> {
    crate::network::anon_http::append_storage_chunk(body, chunk, limit).map_err(|_| {
        MessageBackupError::Generic(
            "archive exceeds its byte limit or cannot allocate memory".into(),
        )
    })
}

async fn read_archive(
    mut response: reqwest::Response,
    context: &str,
) -> Result<Vec<u8>, MessageBackupError> {
    // Count actual chunks; never trust Content-Length for bounds or allocation.
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| storage_transfer_error(context, e))?
    {
        append_archive_chunk(&mut body, &chunk, MAX_ARCHIVE_BYTES)?;
    }
    Ok(body)
}

/// Outcome of a successful export/upload.
#[derive(Debug, Clone, Copy)]
pub struct BackupStats {
    /// Rows serialized into the payload.
    pub rows: usize,
    /// Encrypted container size in bytes.
    pub bytes: usize,
}

/// Message-history backup service. Stateless besides its dependencies;
/// created lazily by `ChatterClient::message_backup()`.
pub struct MessageBackupService {
    database: Arc<Database>,
    settings: Arc<SettingsRepository>,
    grpc_pool: Arc<GrpcClientPool>,
}

impl MessageBackupService {
    pub fn new(
        database: Arc<Database>,
        settings: Arc<SettingsRepository>,
        grpc_pool: Arc<GrpcClientPool>,
    ) -> Self {
        Self {
            database,
            settings,
            grpc_pool,
        }
    }

    /// Build the encrypted container from the local store.
    fn build_container(
        &self,
        kek: &[u8; 32],
        key_mode: u32,
        argon2_salt: &[u8],
    ) -> Result<(Vec<u8>, usize), MessageBackupError> {
        let user_id = self
            .settings
            .get_string(keys::CURRENT_USER_ID)?
            .unwrap_or_default();
        let (mut payload, rows) = {
            let conn = self.database.connection()?;
            dump_frames(&conn, &user_id)?
        };
        let container = encrypt_backup_file(&payload, kek, key_mode, argon2_salt)?;
        payload.zeroize();
        Ok((container, rows))
    }

    /// Decrypt a container and import its rows (idempotent, skip-if-exists —
    /// see `backup_data::import_frames`).
    fn decrypt_and_import(
        &self,
        file_bytes: &[u8],
        kek: &[u8; 32],
        expected_mode: u32,
    ) -> Result<ImportSummary, MessageBackupError> {
        let parsed = parse_backup_file(file_bytes)?;
        if parsed.header.key_mode != expected_mode {
            return Err(MessageBackupError::Generic(format!(
                "backup key mode {} does not match this restore flow",
                parsed.header.key_mode
            )));
        }
        let mut payload = decrypt_backup_file(&parsed, kek)?;
        let summary = {
            let mut conn = self.database.connection()?;
            import_frames(&mut conn, &payload)
        };
        payload.zeroize();
        Ok(summary?)
    }

    // ── Local mode ──────────────────────────────────────────────────

    /// Export an encrypted backup to `path`. Works fully offline.
    ///
    /// The passphrase must clear the same shape/entropy floor as an SVR PIN
    /// (`svr::validate_pin`) — it is the file's only defense off-device.
    pub async fn export_to_file(
        &self,
        path: &Path,
        passphrase: &str,
    ) -> Result<BackupStats, MessageBackupError> {
        crate::crypto::svr::validate_pin(passphrase)
            .map_err(|e| MessageBackupError::Generic(format!("weak passphrase: {e}")))?;

        let salt: [u8; ARGON2_SALT_LEN] = {
            let mut s = [0u8; ARGON2_SALT_LEN];
            use rand::TryRngCore;
            rand::rngs::OsRng
                .try_fill_bytes(&mut s)
                .expect("OS CSPRNG must not fail");
            s
        };

        // Argon2id burns ~450ms of CPU — keep it off the async worker (see
        // `derive_svr_secret_async` for the failure mode).
        let pass = passphrase.to_owned();
        let mut kek = tokio::task::spawn_blocking(move || derive_passphrase_kek(&pass, &salt))
            .await
            .map_err(|e| MessageBackupError::Generic(format!("kdf task failed: {e}")))??;

        let result = self.build_container(&kek, KEY_MODE_PASSPHRASE, &salt);
        kek.zeroize();
        let (container, rows) = result?;

        tokio::fs::write(path, &container).await?;
        tracing::info!(rows, bytes = container.len(), path = %path.display(),
            "message backup exported to file");
        Ok(BackupStats {
            rows,
            bytes: container.len(),
        })
    }

    /// Restore from an encrypted backup file. Wrong passphrase fails at the
    /// file-key unwrap (GCM tag) — nothing is imported in that case.
    pub async fn import_from_file(
        &self,
        path: &Path,
        passphrase: &str,
    ) -> Result<ImportSummary, MessageBackupError> {
        let file_bytes = tokio::fs::read(path).await?;
        let parsed = parse_backup_file(&file_bytes)?;
        if parsed.header.key_mode != KEY_MODE_PASSPHRASE {
            return Err(MessageBackupError::Generic(
                "this file is a server-mode backup; restore it via the PIN flow".into(),
            ));
        }
        let salt = parsed.header.argon2_salt.clone();
        drop(parsed);

        let pass = passphrase.to_owned();
        let mut kek = tokio::task::spawn_blocking(move || derive_passphrase_kek(&pass, &salt))
            .await
            .map_err(|e| MessageBackupError::Generic(format!("kdf task failed: {e}")))??;

        let result = self.decrypt_and_import(&file_bytes, &kek, KEY_MODE_PASSPHRASE);
        kek.zeroize();
        let summary = result?;
        tracing::info!(
            rows_seen = summary.rows_seen,
            rows_inserted = summary.rows_inserted,
            "message backup imported from file"
        );
        Ok(summary)
    }

    // ── Server mode ─────────────────────────────────────────────────

    /// Derive `(kek, backup_id)` from the PIN, off the async worker.
    async fn derive_server_keys(
        &self,
        pin: &str,
        username: &str,
    ) -> Result<([u8; 32], [u8; 32]), MessageBackupError> {
        let mut svr_secret = crate::crypto::svr::derive_svr_secret_async(pin, username).await?;
        let kek = derive_server_kek(&svr_secret);
        let backup_id = derive_message_backup_id(&svr_secret);
        svr_secret.zeroize();
        Ok((kek, backup_id))
    }

    /// SVR salt = the exact username typed at register/login, persisted under
    /// `SVR_SALT_USERNAME` by the auth flows.
    fn salt_username(&self) -> Result<String, MessageBackupError> {
        self.settings
            .get_string(keys::SVR_SALT_USERNAME)?
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                MessageBackupError::Generic(
                    "no stored login username — log out and log in once, then retry".into(),
                )
            })
    }

    /// Create/refresh the server-side backup: build the container, register
    /// it (`SetMessageBackup`), PUT the blob to the presigned URL. One backup
    /// per user — this replaces any previous one.
    pub async fn upload_to_server(&self, pin: &str) -> Result<BackupStats, MessageBackupError> {
        // Shape-only: this uses the EXISTING account PIN. Strength rules only
        // apply where a PIN is chosen; a blocklist entry added after this PIN
        // was set must not block the user's own backup (see validate_pin_shape).
        crate::crypto::svr::validate_pin_shape(pin)
            .map_err(|e| MessageBackupError::Generic(format!("invalid PIN: {e}")))?;
        let username = self.salt_username()?;
        let (mut kek, backup_id) = self.derive_server_keys(pin, &username).await?;

        let result = self.build_container(&kek, KEY_MODE_SVR, &[]);
        kek.zeroize();
        let (container, rows) = result?;

        let mut client = self.grpc_pool.svr_client();
        let upload = client
            .set_message_backup(backup_id.to_vec(), container.len() as u64)
            .await?;

        self.storage_put(&upload.upload_url, container.clone(), "blob upload failed")
            .await?;

        tracing::info!(rows, bytes = container.len(), "message backup uploaded to server");
        Ok(BackupStats {
            rows,
            bytes: container.len(),
        })
    }

    /// Restore history from the server-side backup (fresh install after
    /// login): derive keys from the PIN, fetch the presigned URL, download,
    /// decrypt, import. A wrong PIN either derives a `backup_id` the server
    /// doesn't know (`BackupNotFound`) or — if it somehow located a blob —
    /// fails the KEK unwrap; either way nothing is imported.
    pub async fn restore_from_server(
        &self,
        pin: &str,
    ) -> Result<ImportSummary, MessageBackupError> {
        let username = self.salt_username()?;
        let (mut kek, backup_id) = self.derive_server_keys(pin, &username).await?;

        let mut client = self.grpc_pool.svr_client();
        let download = match client.get_message_backup(backup_id.to_vec()).await {
            Ok(d) => d,
            Err(NetworkError::NotFound(_)) => {
                kek.zeroize();
                return Err(MessageBackupError::BackupNotFound);
            }
            Err(e) => {
                kek.zeroize();
                return Err(e.into());
            }
        };

        let blob = self.storage_get(&download.download_url, "blob download failed").await;
        let blob = match blob {
            Ok(b) => b,
            Err(e) => {
                kek.zeroize();
                return Err(e);
            }
        };

        let result = self.decrypt_and_import(&blob, &kek, KEY_MODE_SVR);
        kek.zeroize();
        let summary = result?;
        tracing::info!(
            rows_seen = summary.rows_seen,
            rows_inserted = summary.rows_inserted,
            "message backup restored from server"
        );
        Ok(summary)
    }

    /// Delete the server-side backup (metadata row + blob).
    pub async fn delete_server_backup(&self, pin: &str) -> Result<(), MessageBackupError> {
        let username = self.salt_username()?;
        let mut svr_secret =
            crate::crypto::svr::derive_svr_secret_async(pin, &username).await?;
        let backup_id = derive_message_backup_id(&svr_secret);
        svr_secret.zeroize();

        let mut client = self.grpc_pool.svr_client();
        client.delete_message_backup(backup_id.to_vec()).await?;
        tracing::info!("server message backup deleted");
        Ok(())
    }

    // ── Link'n'Sync mode (§30) ──────────────────────────────────────
    //
    // A device-to-device history transfer at link time. Same container and the
    // same `dump_frames`/`import_frames` payload as every other mode — only the
    // KEK derivation (ephemeral key from the provision envelope, see
    // `derive_link_sync_kek`) and the transport (the provisioning service's
    // presigned URLs, driven by the caller) differ. The KEK is supplied by the
    // caller because it is derived from envelope material the caller holds
    // (ephemeral key + ACI + primary registration id), not from local settings.

    /// PUT an archive to a presigned storage URL: over Tor when the pool is on
    /// the onion transport (Shield), direct otherwise. Never falls back to a
    /// direct request in Shield — that would hand storage the real IP.
    async fn storage_put(
        &self,
        url: &str,
        body: Vec<u8>,
        context: &str,
    ) -> Result<(), MessageBackupError> {
        #[cfg(feature = "onion")]
        if self.grpc_pool.transport_mode() == crate::network::grpc::client_pool::TransportMode::Onion {
            let tor = self.grpc_pool.onion_tor();
            let resp = crate::network::anon_http::put(&tor, url, body, "application/octet-stream")
                .await
                .map_err(|e| MessageBackupError::Generic(format!("{context}: {e}")))?;
            if !(200..300).contains(&resp.status) {
                return Err(MessageBackupError::Generic(format!(
                    "{context} with status {}",
                    resp.status
                )));
            }
            return Ok(());
        }

        let response = storage_client().map_err(MessageBackupError::Generic)?
            .put(url)
            .header("Content-Type", "application/octet-stream")
            .body(body)
            .send()
            .await
            .map_err(|e| storage_transfer_error(context, e))?;
        if !response.status().is_success() {
            return Err(MessageBackupError::Generic(format!(
                "{context} with status {}",
                response.status()
            )));
        }
        Ok(())
    }

    /// GET an archive from a presigned storage URL, over Tor in Shield (see
    /// [`Self::storage_put`]). The Tor path caps the body at the attachment
    /// response limit, so an archive larger than that fails rather than
    /// going direct.
    async fn storage_get(&self, url: &str, context: &str) -> Result<Vec<u8>, MessageBackupError> {
        #[cfg(feature = "onion")]
        if self.grpc_pool.transport_mode() == crate::network::grpc::client_pool::TransportMode::Onion {
            let tor = self.grpc_pool.onion_tor();
            return crate::network::anon_http::get(&tor, url)
                .await
                .map_err(|e| MessageBackupError::Generic(format!("{context}: {e}")));
        }

        let response = storage_client().map_err(MessageBackupError::Generic)?
            .get(url)
            .send()
            .await
            .map_err(|e| storage_transfer_error(context, e))?;
        if !response.status().is_success() {
            return Err(MessageBackupError::Generic(format!(
                "{context} with status {}",
                response.status()
            )));
        }
        read_archive(response, context).await
    }

    /// Build the encrypted Link'n'Sync transfer archive from the local store.
    /// Returns the ciphertext and its stats; the caller declares `bytes` to
    /// `SetTransferArchive` and then uploads via `upload_link_archive`.
    pub fn build_link_archive(
        &self,
        kek: &[u8; 32],
    ) -> Result<(Vec<u8>, BackupStats), MessageBackupError> {
        let (container, rows) = self.build_container(kek, KEY_MODE_LINK, &[])?;
        let bytes = container.len();
        Ok((container, BackupStats { rows, bytes }))
    }

    /// PUT an already-built transfer archive to the primary-supplied presigned
    /// URL. The bytes are opaque to the server — only the new device holds the
    /// KEK.
    pub async fn upload_link_archive(
        &self,
        container: &[u8],
        upload_url: &str,
    ) -> Result<(), MessageBackupError> {
        self.storage_put(upload_url, container.to_vec(), "transfer archive upload failed")
            .await
    }

    /// Download the transfer archive from the primary-supplied presigned URL,
    /// verify (KEY_MODE_LINK + header-as-AAD MAC), decrypt, and import its
    /// frames (idempotent). A wrong KEK fails at the file-key unwrap, so nothing
    /// is imported.
    pub async fn download_and_import_link_archive(
        &self,
        download_url: &str,
        kek: &[u8; 32],
    ) -> Result<ImportSummary, MessageBackupError> {
        let blob = self
            .storage_get(download_url, "transfer archive download failed")
            .await?;

        let summary = self.decrypt_and_import(&blob, kek, KEY_MODE_LINK)?;
        tracing::info!(
            rows_seen = summary.rows_seen,
            rows_inserted = summary.rows_inserted,
            "link transfer archive imported"
        );
        Ok(summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::message_backup::{
        decrypt_backup_file, derive_link_sync_kek, parse_backup_file, KEY_MODE_LINK,
    };
    use crate::domain::ids::UserId;
    use crate::domain::user::User;
    use crate::network::grpc::client_pool::GrpcClientPool;
    use crate::storage::backup_data::import_frames;
    use crate::storage::repositories::user_repo::UserRepository;
    use crate::storage::Database;
    use tempfile::NamedTempFile;

    fn svc_with_db() -> (MessageBackupService, Arc<Database>, NamedTempFile) {
        let file = NamedTempFile::new().unwrap();
        let db = Arc::new(Database::open(file.path(), "test_passphrase").unwrap());
        db.run_migrations().unwrap();
        let settings = Arc::new(SettingsRepository::new(db.clone()));
        let grpc = Arc::new(GrpcClientPool::new("http://127.0.0.1:0").unwrap());
        (
            MessageBackupService::new(db.clone(), settings, grpc),
            db,
            file,
        )
    }

    #[test]
    fn archive_collection_rejects_excess_before_appending() {
        let mut body = vec![1, 2];
        append_archive_chunk(&mut body, &[3, 4], 4).unwrap();
        assert_eq!(body, [1, 2, 3, 4]);
        assert!(append_archive_chunk(&mut body, &[5], 4).is_err());
        assert_eq!(body, [1, 2, 3, 4]);
        let mut empty = Vec::new();
        assert!(append_archive_chunk(&mut empty, &[1, 2], 1).is_err());
        assert!(empty.is_empty());
        append_archive_chunk(&mut empty, &[], 0).unwrap();
    }

    #[tokio::test]
    async fn link_transfers_reject_cleartext_without_leaking_capabilities() {
        let (service, _db, _file) = svc_with_db();
        let url = "http://127.0.0.1:1/private-object?secret=capability";
        let upload = service.upload_link_archive(b"archive", url).await.unwrap_err();
        let download = service
            .download_and_import_link_archive(url, &[7; 32])
            .await
            .unwrap_err();
        for error in [upload, download] {
            for rendered in [error.to_string(), format!("{error:?}")] {
                assert!(!rendered.contains("127.0.0.1"));
                assert!(!rendered.contains("private-object"));
                assert!(!rendered.contains("capability"));
                assert!(rendered.contains("transfer archive"));
            }
        }
    }

    fn user(id: &str, name: &str) -> User {
        User {
            id: id.parse::<UserId>().unwrap(),
            username: name.to_string(),
            svr_public_key: Vec::new(),
            last_active: chrono::Utc::now(),
            created_at: chrono::Utc::now(),
        }
    }

    // The property PR3/PR4 exist for: a device's local rows, exported under the
    // ephemeral-key KEK in KEY_MODE_LINK, transfer byte-exact into a fresh
    // device — and a wrong ephemeral key decrypts nothing. This exercises the
    // real dump_frames -> encrypt -> decrypt -> import_frames path with a live
    // SQLite store, which the crypto-layer container test cannot.
    #[test]
    fn link_archive_round_trips_between_devices() {
        let (src, src_db, _f1) = svc_with_db();
        let aci = "11111111-1111-1111-1111-111111111111";
        src.settings.set_string(keys::CURRENT_USER_ID, aci).unwrap();
        let contact = "22222222-2222-2222-2222-222222222222";
        UserRepository::new(src_db.clone())
            .insert(&user(contact, "bob"))
            .unwrap();

        // Primary builds the archive keyed to the single-use ephemeral key.
        let kek = derive_link_sync_kek(&[7u8; 32], aci, 42);
        let (container, stats) = src.build_link_archive(&kek).unwrap();
        assert!(stats.bytes > 0);
        assert_eq!(
            parse_backup_file(&container).unwrap().header.key_mode,
            KEY_MODE_LINK
        );

        // New device: fresh store, derive the same KEK, decrypt + import.
        let (_dst, dst_db, _f2) = svc_with_db();
        let payload =
            decrypt_backup_file(&parse_backup_file(&container).unwrap(), &kek).unwrap();
        {
            let mut conn = dst_db.connection().unwrap();
            import_frames(&mut conn, &payload).unwrap();
        }
        assert!(
            UserRepository::new(dst_db.clone())
                .get(&contact.parse::<UserId>().unwrap())
                .unwrap()
                .is_some(),
            "contact row transferred to the newly linked device"
        );

        // A wrong ephemeral key (attacker holding the CDN blob but not the
        // envelope) fails at the file-key unwrap — nothing to import.
        let wrong = derive_link_sync_kek(&[9u8; 32], aci, 42);
        assert!(
            decrypt_backup_file(&parse_backup_file(&container).unwrap(), &wrong).is_err()
        );
    }
}
