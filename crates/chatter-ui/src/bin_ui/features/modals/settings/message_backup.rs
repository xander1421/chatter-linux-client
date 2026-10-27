//! Settings message backup callbacks.

use super::*;

pub(super) fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    // ── Message-history backup (L7) ─────────────────────────────────
    // Thin wires only: native file pickers + status text live here; ALL
    // backup logic (KDF, container crypto, dump/import, server RPCs) lives
    // in chatter-core behind the ChatterClient façade methods.

    fn set_message_backup_status(weak: &slint::Weak<MainWindow>, status: String) {
        let weak = weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                let settings = w.global::<crate::SettingsPane>();
                settings.set_settings_message_backup_status(status.into());
                settings.set_settings_message_backup_busy(false);
            }
        });
    }

    fn begin_message_backup(weak: &slint::Weak<MainWindow>) -> bool {
        let Some(window) = weak.upgrade() else { return false };
        let settings = window.global::<crate::SettingsPane>();
        if settings.get_settings_message_backup_busy() {
            return false;
        }
        settings.set_settings_message_backup_busy(true);
        settings.set_settings_message_backup_status("".into());
        true
    }

    fn cancel_message_backup(weak: &slint::Weak<MainWindow>) {
        let weak = weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(window) = weak.upgrade() {
                window
                    .global::<crate::SettingsPane>()
                    .set_settings_message_backup_busy(false);
            }
        });
    }

    // Export to file (save picker → core export)
    let ui_ctx_mb_export = ui_ctx.clone();
    let weak_mb_export = window.as_weak();
    window
        .global::<crate::SettingsPane>()
        .on_settings_export_message_backup(move |passphrase: SharedString| {
            if !begin_message_backup(&weak_mb_export) {
                return;
            }
            let ui_ctx = ui_ctx_mb_export.clone();
            let weak = weak_mb_export.clone();
            let passphrase = passphrase.to_string();
            tokio::spawn(async move {
                #[cfg(target_os = "android")]
                {
                    use crate::platform::android::backup_document;
                    let staged =
                        match tempfile::NamedTempFile::new_in(ui_ctx.directories().cache_dir()) {
                            Ok(file) => file,
                            Err(_) => {
                                set_message_backup_status(&weak, "Could not prepare the backup file.".into());
                                return;
                            }
                        };
                    let stats = match ui_ctx
                        .export_message_backup(staged.path(), &passphrase)
                        .await
                    {
                        Ok(stats) => stats,
                        Err(_) => {
                            set_message_backup_status(&weak, "Could not export backup. Check available storage and retry.".into());
                            return;
                        }
                    };
                    match backup_document::create().await {
                        Ok(true) => {
                            let status = match backup_document::write_document(staged.path()) {
                                Ok(()) => format!("Backup exported ({} rows)", stats.rows),
                                Err(_) => "Could not save the backup file.".into(),
                            };
                            set_message_backup_status(&weak, status);
                        }
                        Ok(false) => cancel_message_backup(&weak),
                        Err(_) => set_message_backup_status(&weak, "Could not open the backup destination.".into()),
                    }
                }
                #[cfg(not(target_os = "android"))]
                {
                    use chatter_core::media::{FileFilter, FilePicker, FilePickerConfig};
                    let Some(path) = FilePicker::save_file(
                        FilePickerConfig::new()
                            .with_title("Export message backup")
                            .with_filter(FileFilter::new("Chatter backup", vec!["chbk"]))
                            .with_default_path(
                                dirs::document_dir()
                                    .or_else(dirs::home_dir)
                                    .unwrap_or_default()
                                    .join(format!(
                                        "chatter-backup-{}.chbk",
                                        chrono::Local::now().format("%Y-%m-%d")
                                    )),
                            ),
                    )
                    .await
                    else {
                        cancel_message_backup(&weak);
                        return; // user cancelled
                    };
                    let status = match ui_ctx.export_message_backup(&path, &passphrase).await {
                        Ok(stats) => format!("Backup exported ({} rows)", stats.rows),
                        Err(_) => "Could not export backup. Check available storage and retry.".into(),
                    };
                    set_message_backup_status(&weak, status);
                }
            });
        });

    // Restore from file (open picker → core import)
    let ui_ctx_mb_import = ui_ctx.clone();
    let weak_mb_import = window.as_weak();
    window
        .global::<crate::SettingsPane>()
        .on_settings_import_message_backup(move |passphrase: SharedString| {
            if !begin_message_backup(&weak_mb_import) {
                return;
            }
            let ui_ctx = ui_ctx_mb_import.clone();
            let weak = weak_mb_import.clone();
            let passphrase = passphrase.to_string();
            tokio::spawn(async move {
                #[cfg(target_os = "android")]
                {
                    use crate::platform::android::backup_document;
                    match backup_document::open().await {
                        Ok(Some(path)) => {
                            let result = ui_ctx.import_message_backup(&path, &passphrase).await;
                            let _ = tokio::fs::remove_file(&path).await;
                            let status = message_restore_status(result);
                            set_message_backup_status(&weak, status);
                        }
                        Ok(None) => cancel_message_backup(&weak),
                        Err(_) => set_message_backup_status(&weak, "Could not open the backup file.".into()),
                    }
                }
                #[cfg(not(target_os = "android"))]
                {
                    use chatter_core::media::{FileFilter, FilePicker, FilePickerConfig};
                    let Some(path) = FilePicker::pick_file(
                        FilePickerConfig::new()
                            .with_title("Restore message backup")
                            .with_filter(FileFilter::new("Chatter backup", vec!["chbk"])),
                    )
                    .await
                    else {
                        cancel_message_backup(&weak);
                        return; // user cancelled
                    };
                    let status = message_restore_status(
                        ui_ctx.import_message_backup(&path, &passphrase).await,
                    );
                    set_message_backup_status(&weak, status);
                }
            });
        });

    // Back up to server (PIN-keyed, server never sees content)
    let ui_ctx_mb_server = ui_ctx.clone();
    let weak_mb_server = window.as_weak();
    window
        .global::<crate::SettingsPane>()
        .on_settings_server_message_backup(move |pin: SharedString| {
            if !begin_message_backup(&weak_mb_server) {
                return;
            }
            let ui_ctx = ui_ctx_mb_server.clone();
            let weak = weak_mb_server.clone();
            let pin = pin.to_string();
            tokio::spawn(async move {
                let status = match ui_ctx.enable_server_message_backup(&pin).await {
                    Ok(stats) => format!("Server backup updated ({} rows)", stats.rows),
                    Err(_) => "Could not back up messages. Check your connection and PIN, then retry.".into(),
                };
                set_message_backup_status(&weak, status);
            });
        });

    // Restore from server (fresh install after login + PIN)
    let ui_ctx_mb_restore = ui_ctx.clone();
    let weak_mb_restore = window.as_weak();
    window
        .global::<crate::SettingsPane>()
        .on_settings_restore_message_backup(move |pin: SharedString| {
            if !begin_message_backup(&weak_mb_restore) {
                return;
            }
            let ui_ctx = ui_ctx_mb_restore.clone();
            let weak = weak_mb_restore.clone();
            let pin = pin.to_string();
            tokio::spawn(async move {
                let status =
                    message_restore_status(ui_ctx.restore_server_message_backup(&pin).await);
                set_message_backup_status(&weak, status);
            });
        });
}

fn message_restore_status(result: Result<ImportSummary, ClientError>) -> String {
    match result {
        Ok(summary) => format!(
            "Restored {} rows ({} already present or kept locally)",
            summary.rows_inserted, summary.rows_skipped
        ),
        Err(error) if error.is_unsupported_backup() => {
            "This backup was made by a newer version of Chatter. Update the app to restore it."
                .into()
        }
        Err(_) => "Could not restore backup. Check your passphrase or PIN, then retry.".into(),
    }
}
