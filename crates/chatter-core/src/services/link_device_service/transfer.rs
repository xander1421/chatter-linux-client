use super::*;

impl LinkDeviceService {
    /// Fetch + import the Link'n'Sync transfer archive on the new device (§30).
    /// Never returns an error: the device is already linked, so any failure is
    /// downgraded to "linked without history".
    pub(super) async fn import_transfer_archive(
        &self,
        uuid: &str,
        ephemeral_backup_key: &[u8; 32],
        aci: &str,
        primary_registration_id: u32,
    ) {
        let backup = MessageBackupService::new(
            self.database.clone(),
            self.settings_repo.clone(),
            self.grpc_pool.clone(),
        );
        let mut kek = derive_link_sync_kek(ephemeral_backup_key, aci, primary_registration_id);

        let result: Result<Option<ImportSummary>, LinkDeviceError> = async {
            match self.poll_transfer_archive(uuid).await? {
                Some(download_url) => {
                    let summary = backup
                        .download_and_import_link_archive(&download_url, &kek)
                        .await
                        .map_err(|e| {
                            LinkDeviceError::Invalid(format!("import transfer archive: {e}"))
                        })?;
                    Ok(Some(summary))
                }
                // Primary failed/declined to export, or the wait timed out —
                // link without history.
                None => Ok(None),
            }
        }
        .await;
        kek.zeroize();

        match result {
            Ok(Some(summary)) => tracing::info!(
                rows_inserted = summary.rows_inserted,
                "link transfer archive imported"
            ),
            Ok(None) => tracing::info!("no transfer archive for this link; linked without history"),
            Err(e) => tracing::warn!(
                error = %e,
                "link transfer archive import failed; linked without history"
            ),
        }
    }

    /// Poll `GetTransferArchive` until Ready (returns the download URL), Failed
    /// (returns None — link without history), or a bounded deadline elapses
    /// (also None). Mirrors `poll_until_envelope`'s cadence.
    async fn poll_transfer_archive(&self, uuid: &str) -> Result<Option<String>, LinkDeviceError> {
        // A primary export of up to 1 GiB plus the upload can take a while;
        // 3 minutes is generous without hanging a linked device indefinitely.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
        let mut client = self.provisioning_client()?;
        loop {
            match client.get_transfer_archive(uuid).await? {
                TransferArchivePoll::Ready { download_url, .. } => {
                    return Ok(Some(download_url));
                }
                TransferArchivePoll::Failed { message } => {
                    tracing::info!(%message, "transfer archive unavailable for this link");
                    return Ok(None);
                }
                TransferArchivePoll::Pending { retry_after_ms } => {
                    if tokio::time::Instant::now() >= deadline {
                        tracing::warn!("transfer archive poll timed out; linking without history");
                        return Ok(None);
                    }
                    let delay = retry_after_ms.clamp(1_000, 10_000);
                    tokio::time::sleep(Duration::from_millis(delay as u64)).await;
                }
            }
        }
    }
    /// Export + upload the Link'n'Sync transfer archive for a linking session
    /// (§30). Never returns an error: linking has already succeeded by the time
    /// this runs, so any failure is downgraded to an `export_failed` signal that
    /// tells the polling new device to proceed without history.
    pub(super) async fn export_transfer_archive(
        &self,
        uuid: &str,
        ephemeral_backup_key: &[u8; 32],
        aci: &str,
        primary_registration_id: u32,
    ) {
        let backup = MessageBackupService::new(
            self.database.clone(),
            self.settings_repo.clone(),
            self.grpc_pool.clone(),
        );
        let mut kek = derive_link_sync_kek(ephemeral_backup_key, aci, primary_registration_id);

        let result: Result<BackupStats, LinkDeviceError> = async {
            let (container, stats) = backup
                .build_link_archive(&kek)
                .map_err(|e| LinkDeviceError::Invalid(format!("build transfer archive: {e}")))?;
            let mut client = self.provisioning_client()?;
            let upload_url = client
                .set_transfer_archive(uuid, container.len() as u64, false)
                .await?;
            backup
                .upload_link_archive(&container, &upload_url)
                .await
                .map_err(|e| LinkDeviceError::Invalid(format!("upload transfer archive: {e}")))?;
            Ok(stats)
        }
        .await;
        kek.zeroize();

        match result {
            Ok(stats) => {
                tracing::info!(
                    rows = stats.rows,
                    bytes = stats.bytes,
                    "link transfer archive exported"
                );
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "link transfer archive export failed; new device will link without history"
                );
                if let Ok(mut client) = self.provisioning_client() {
                    // Signal the new device to stop polling. Best-effort: if
                    // even this fails, the new device's own poll deadline ends
                    // the wait.
                    let _ = client.set_transfer_archive(uuid, 0, true).await;
                }
            }
        }
    }
}
