//! Settings modal callbacks.

use slint::{ComponentHandle, SharedString};

use chatter_linux::app::AppContext;

use crate::MainWindow;

pub fn wire_settings_callbacks(window: &MainWindow, ctx: &AppContext) {
    let grpc_pool = ctx.grpc_pool.clone();
    let settings_repo = ctx.settings_repo.clone();
    let signal_service = ctx.signal_service.clone();
    let auth_interceptor = ctx.auth_interceptor.clone();

    // Change password
    let weak_window = window.as_weak();
    let grpc_pool_pw = grpc_pool.clone();
    let settings_repo_pw = settings_repo.clone();
    let auth_interceptor_pw = auth_interceptor.clone();

    window.on_settings_change_password(move |current: SharedString, new: SharedString| {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool_pw.clone();
        let settings_repo = settings_repo_pw.clone();
        let auth_interceptor = auth_interceptor_pw.clone();
        let current = current.to_string();
        let new = new.to_string();

        tokio::spawn(async move {
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_settings_password_changing(true);
                    w.set_settings_password_error("".into());
                }
            });

            use chatter_linux::services::account_service::AccountService;
            let account_service =
                AccountService::new(grpc_pool.clone(), settings_repo.clone(), auth_interceptor);

            match account_service.change_pwd(&current, &new).await {
                Ok(()) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_settings_password_changing(false);
                            w.set_settings_current_password("".into());
                            w.set_settings_new_password("".into());
                            w.set_settings_confirm_password("".into());
                        }
                    });
                    tracing::info!("Password changed successfully");
                }
                Err(e) => {
                    let error_msg = e.to_string();
                    tracing::error!("Failed to change password: {}", error_msg);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_settings_password_changing(false);
                            w.set_settings_password_error(error_msg.into());
                        }
                    });
                }
            }
        });
    });

    // Read receipts toggle
    let settings_repo_read = settings_repo.clone();
    window.on_settings_read_receipts_changed(move |enabled: bool| {
        let settings_repo = settings_repo_read.clone();
        if let Err(e) = settings_repo.set(
            chatter_linux::storage::settings_keys::SEND_READ_RECEIPTS,
            &enabled,
        ) {
            tracing::error!("Failed to save read receipts setting: {}", e);
        } else {
            tracing::debug!("Read receipts setting changed to: {}", enabled);
        }
    });

    // Typing indicators toggle
    let settings_repo_typing = settings_repo.clone();
    window.on_settings_typing_indicators_changed(move |enabled: bool| {
        let settings_repo = settings_repo_typing.clone();
        if let Err(e) = settings_repo.set(
            chatter_linux::storage::settings_keys::SHOW_TYPING_INDICATORS,
            &enabled,
        ) {
            tracing::error!("Failed to save typing indicators setting: {}", e);
        } else {
            tracing::debug!("Typing indicators setting changed to: {}", enabled);
        }
    });

    // Copy fingerprint
    let signal_service_fp = signal_service.clone();
    window.on_settings_copy_fingerprint(move || {
        let fingerprint = match signal_service_fp.get_identity_fingerprint() {
            Ok(fp) => fp,
            Err(e) => {
                tracing::error!("Failed to get identity fingerprint: {}", e);
                return;
            }
        };

        use chatter_linux::system::Clipboard;
        match Clipboard::new() {
            Ok(mut clipboard) => {
                if let Err(e) = clipboard.set_text(&fingerprint) {
                    tracing::error!("Failed to copy fingerprint to clipboard: {}", e);
                } else {
                    tracing::info!("Fingerprint copied to clipboard");
                }
            }
            Err(e) => {
                tracing::error!("Failed to access clipboard: {}", e);
            }
        }
    });

    // Sealed sender toggle
    let settings_repo_sealed = settings_repo.clone();
    window.on_settings_sealed_sender_changed(move |unrestricted: bool| {
        let settings_repo = settings_repo_sealed.clone();
        if let Err(e) = settings_repo.set("privacy.sealed_sender_unrestricted", &unrestricted) {
            tracing::error!("Failed to save sealed sender setting: {}", e);
        } else {
            tracing::debug!(
                "Sealed sender unrestricted setting changed to: {}",
                unrestricted
            );
        }
    });

    // Refresh prekeys
    let weak_window = window.as_weak();
    let grpc_pool_keys = grpc_pool.clone();
    let signal_service_refresh = signal_service.clone();

    window.on_settings_refresh_keys(move || {
        let weak = weak_window.clone();
        let grpc_pool = grpc_pool_keys.clone();
        let signal_service = signal_service_refresh.clone();

        tokio::spawn(async move {
            let mut auth_client = chatter_linux::network::grpc::auth_client::AuthClient::new(
                grpc_pool.channel(),
                grpc_pool.auth_clone(),
            );

            match signal_service.refresh_prekeys(&mut auth_client, 100).await {
                Ok(uploaded_count) => {
                    // Get updated stats from server
                    match auth_client.get_pre_key_stats(0).await {
                        Ok(stats) => {
                            let count = stats.available_count;
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(w) = weak.upgrade() {
                                    w.set_settings_prekey_count(count);
                                }
                            });
                            tracing::info!("Refreshed {} prekeys, {} now available", uploaded_count, count);
                        }
                        Err(e) => {
                            tracing::warn!("Failed to get prekey stats after refresh: {}", e);
                            tracing::info!("Prekeys refreshed successfully ({} uploaded)", uploaded_count);
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to refresh prekeys: {}", e);
                }
            }
        });
    });

    // Edit profile
    let weak_window = window.as_weak();
    window.on_settings_edit_profile(move || {
        let weak = weak_window.clone();

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                w.set_profile_is_self(true);
                w.set_active_modal("user-profile".into());
            }
        });
    });

    // Delete account (placeholder)
    let weak_window = window.as_weak();
    window.on_settings_delete_account(move || {
        let weak = weak_window.clone();
        tracing::info!("Delete account requested - showing confirmation would be needed");

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(_w) = weak.upgrade() {
                // Future: Show delete confirmation modal
            }
        });
    });
}
