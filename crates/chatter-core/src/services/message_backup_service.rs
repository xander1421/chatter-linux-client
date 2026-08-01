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
    encrypt_backup_file, parse_backup_file, ARGON2_SALT_LEN, KEY_MODE_PASSPHRASE, KEY_MODE_SVR,
};
use crate::error::{CryptoError, NetworkError, StorageError};
use crate::network::grpc::client_pool::GrpcClientPool;
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
        crate::crypto::svr::validate_pin(pin)
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

        let response = reqwest::Client::new()
            .put(&upload.upload_url)
            .header("Content-Type", "application/octet-stream")
            .body(container.clone())
            .send()
            .await
            .map_err(|e| MessageBackupError::Generic(format!("blob upload failed: {e}")))?;
        if !response.status().is_success() {
            return Err(MessageBackupError::Generic(format!(
                "blob upload failed with status {}",
                response.status()
            )));
        }

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

        let blob = async {
            let response = reqwest::Client::new()
                .get(&download.download_url)
                .send()
                .await
                .map_err(|e| MessageBackupError::Generic(format!("blob download failed: {e}")))?;
            if !response.status().is_success() {
                return Err(MessageBackupError::Generic(format!(
                    "blob download failed with status {}",
                    response.status()
                )));
            }
            response
                .bytes()
                .await
                .map_err(|e| MessageBackupError::Generic(format!("blob read failed: {e}")))
        }
        .await;
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
}
