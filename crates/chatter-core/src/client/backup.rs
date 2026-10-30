use super::*;

impl ChatterClient {
    /// Get the SVR backup service (lazy-initialized, persistent instance).
    pub fn svr_backup(&self) -> &crate::services::svr_backup_service::SvrBackupService {
        self.svr_backup_service.get_or_init(|| {
            let settings = Arc::new(
                crate::storage::repositories::settings_repo::SettingsRepository::new(
                    self.services.database.clone(),
                ),
            );
            crate::services::svr_backup_service::SvrBackupService::new(
                self.services.grpc_pool.clone(),
                settings,
                self.signal().clone(),
            )
        })
    }

    /// Get the message-history backup service (lazy-initialized, persistent).
    pub fn message_backup(&self) -> &crate::services::message_backup_service::MessageBackupService {
        self.message_backup_service.get_or_init(|| {
            let settings = Arc::new(
                crate::storage::repositories::settings_repo::SettingsRepository::new(
                    self.services.database.clone(),
                ),
            );
            crate::services::message_backup_service::MessageBackupService::new(
                self.services.database.clone(),
                settings,
                self.services.grpc_pool.clone(),
            )
        })
    }

    // ── Message-history backup façade (UI entry points) ─────────────

    /// Export an encrypted message-history backup to `path` (local mode,
    /// works offline). Passphrase must clear the SVR PIN entropy floor.
    pub async fn export_message_backup(
        &self,
        path: &std::path::Path,
        passphrase: &str,
    ) -> Result<crate::services::message_backup_service::BackupStats, ClientError> {
        self.message_backup()
            .export_to_file(path, passphrase)
            .await
            .map_err(|e| ClientError::Validation(format!("backup export failed: {e}")))
    }

    /// Restore message history from an encrypted backup file. Idempotent
    /// (skip-if-exists) — never clobbers newer local rows.
    pub async fn import_message_backup(
        &self,
        path: &std::path::Path,
        passphrase: &str,
    ) -> Result<ImportSummary, ClientError> {
        self.message_backup()
            .import_from_file(path, passphrase)
            .await
            .map(Into::into)
            .map_err(ClientError::BackupRestore)
    }

    /// Create/refresh the server-side encrypted message backup (PIN-keyed,
    /// server never sees content).
    pub async fn enable_server_message_backup(
        &self,
        pin: &str,
    ) -> Result<crate::services::message_backup_service::BackupStats, ClientError> {
        self.message_backup()
            .upload_to_server(pin)
            .await
            .map_err(|e| ClientError::Validation(format!("server backup failed: {e}")))
    }

    /// Restore message history from the server-side backup (fresh install
    /// after login + PIN).
    pub async fn restore_server_message_backup(
        &self,
        pin: &str,
    ) -> Result<ImportSummary, ClientError> {
        self.message_backup()
            .restore_from_server(pin)
            .await
            .map(Into::into)
            .map_err(ClientError::BackupRestore)
    }

}
