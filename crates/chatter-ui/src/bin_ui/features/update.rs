//! Thin presentation wiring for the signed core update service.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
type UpdateOperation = Arc<tokio::sync::Mutex<()>>;

use chatter_core::client::ChatterClient;
use chatter_core::update;
use slint::{ComponentHandle, Weak};

use crate::{MainWindow, UpdateGlobal};

fn show_error(weak: Weak<MainWindow>, error: String) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            let global = window.global::<UpdateGlobal>();
            global.set_state("error".into());
            global.set_error(error.into());
        }
    });
}

fn check_may_replace_state(state: &str) -> bool {
    !matches!(
        state,
        "confirm-cellular" | "downloading" | "installing" | "ready"
    )
}

async fn check(
    client: Arc<ChatterClient>,
    weak: Weak<MainWindow>,
    manual: bool,
    play: bool,
    operations: UpdateOperation,
) {
    // A publication received during another check must wait, not disappear.
    let _operation = operations.lock_owned().await;
    if !manual && !client.is_app_visible() {
        return;
    }
    if play {
        #[cfg(target_os = "android")]
        if manual {
            if let Err(error) = crate::platform::android::activity::open_play_store_update() {
                show_error(weak, error);
            }
        }
        return;
    }
    if !matches!(update::platform(), "android" | "linux") {
        if manual {
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak.upgrade() {
                    let global = window.global::<UpdateGlobal>();
                    global.set_state("idle".into());
                    global.set_error("".into());
                    global.set_notice(
                        "In-app updates are not available for this platform yet".into(),
                    );
                }
            });
        }
        return;
    }
    let (send, receive) = tokio::sync::oneshot::channel();
    let allowed_window = weak.clone();
    if slint::invoke_from_event_loop(move || {
        let allowed = allowed_window.upgrade().is_some_and(|window| {
            let global = window.global::<UpdateGlobal>();
            let allowed = check_may_replace_state(global.get_state().as_str());
            global.set_checking(allowed);
            allowed
        });
        let _ = send.send(allowed);
    })
    .is_err()
        || receive.await != Ok(true)
    {
        return;
    }
    tracing::info!(manual, "update check started");
    if manual {
        let loading = weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(window) = loading.upgrade() {
                let global = window.global::<UpdateGlobal>();
                if global.get_state() != "available" {
                    global.set_state("checking".into());
                }
                global.set_error("".into());
                global.set_notice("".into());
            }
        });
    }
    let result = client.check_available_update().await;
    let checking_window = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = checking_window.upgrade() {
            window.global::<UpdateGlobal>().set_checking(false);
        }
    });
    match result {
        Ok(found) => {
            tracing::info!(available = found.is_some(), "update check completed");
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak.upgrade() {
                    let global = window.global::<UpdateGlobal>();
                    if !check_may_replace_state(global.get_state().as_str()) {
                        return;
                    }
                    global.set_error("".into());
                    global.set_notice("".into());
                    if let Some(manifest) = found {
                        global.set_version(manifest.version_name.into());
                        global.set_state("available".into());
                    } else {
                        global.set_state("idle".into());
                        if manual {
                            global.set_notice(chatter_i18n::tr("You're up to date").into());
                        }
                    }
                }
            });
        }
        Err(error) => {
            tracing::warn!(kind = %error, "update check failed");
            if manual {
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(window) = weak.upgrade() {
                        let global = window.global::<UpdateGlobal>();
                        if check_may_replace_state(global.get_state().as_str()) {
                            global.set_state("error".into());
                            global.set_error(error.into());
                        }
                    }
                });
            }
        }
    }
}

/// The window the Android install-result receiver reports into.
#[cfg(target_os = "android")]
static INSTALL_WINDOW: std::sync::Mutex<Option<Weak<MainWindow>>> = std::sync::Mutex::new(None);

/// PackageInstaller's final status for our session. Without it a refused
/// install (e.g. a signer mismatch) left the pill on "Confirm system install"
/// and the reason only in a toast.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_chatter_client_UpdateInstallReceiver_nativeOnInstallFailed(
    mut env: jni::EnvUnowned<'_>,
    _class: jni::objects::JClass,
    reason: jni::objects::JString,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let reason: String = reason.try_to_string(&env)?.into();
        let weak = INSTALL_WINDOW.lock().ok().and_then(|guard| guard.clone());
        if let Some(weak) = weak {
            let message = if reason.is_empty() {
                chatter_i18n::tr("Update not installed: Update install failed")
            } else {
                chatter_i18n::trf("Update not installed: {}", &[&reason])
            };
            show_error(weak, message);
        }
        Ok(())
    })
    .resolve::<jni::errors::LogErrorAndDefault>()
}

pub fn wire(window: &MainWindow, client: &Arc<ChatterClient>) {
    #[cfg(target_os = "android")]
    if let Ok(mut guard) = INSTALL_WINDOW.lock() {
        *guard = Some(window.as_weak());
    }
    let global = window.global::<UpdateGlobal>();
    global.set_installed_version(update::installed_version_name().into());
    global.set_build_code(update::installed_version_code().to_string().into());
    global.set_build_commit(env!("CHATTER_BUILD_COMMIT").into());
    #[cfg(target_os = "android")]
    let play = update::channel() == "play"
        || crate::platform::android::activity::is_play_installed().unwrap_or(true);
    #[cfg(not(target_os = "android"))]
    let play = false;
    global.set_play_distribution(play);

    let operations: UpdateOperation = Arc::new(tokio::sync::Mutex::new(()));
    let manual_operations = operations.clone();
    let manual_client = client.clone();
    let manual_window = window.as_weak();
    global.on_check(move || {
        if let Some(window) = manual_window.upgrade() {
            let global = window.global::<UpdateGlobal>();
            if global.get_checking() {
                return;
            }
            if !play
                && matches!(update::platform(), "android" | "linux")
                && check_may_replace_state(global.get_state().as_str())
            {
                global.set_checking(true);
            }
        }
        crate::rt_handle().spawn(check(
            manual_client.clone(),
            manual_window.clone(),
            true,
            play,
            manual_operations.clone(),
        ));
    });

    let automatic_client = Arc::downgrade(client);
    let automatic_window = window.as_weak();
    let automatic_operations = operations.clone();
    let mut automatic =
        update::automatic::AutomaticChecks::for_client(client.subscribe_update_checks());
    crate::rt_handle().spawn(async move {
        while automatic.next().await {
            let Some(client) = automatic_client.upgrade() else {
                break;
            };
            check(
                client,
                automatic_window.clone(),
                false,
                play,
                automatic_operations.clone(),
            )
            .await;
        }
    });

    let staged_executable: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));
    let update_client = client.clone();
    let update_window = window.as_weak();
    global.on_update(move || {
        let weak = update_window.clone();
        let client = update_client.clone();
        let staged = staged_executable.clone();
        let Some(window) = weak.upgrade() else {
            return;
        };
        let global = window.global::<UpdateGlobal>();
        if play {
            #[cfg(target_os = "android")]
            if let Err(error) = crate::platform::android::activity::open_play_store_update() {
                show_error(weak, error);
            }
            return;
        }
        if global.get_state() == "ready" {
            #[cfg(target_os = "linux")]
            {
                let path = staged.lock().ok().and_then(|guard| guard.clone());
                if let Some(path) = path {
                    global.set_state("downloading".into());
                    global.set_progress(chatter_i18n::tr("Starting verified update…").into());
                    crate::rt_handle().spawn(async move {
                        match tokio::task::spawn_blocking(move ||
                            update::restart_into_linux_update(&path)).await
                        {
                            Ok(Ok(())) => {
                                tracing::info!("Linux update restart handoff");
                                let _ = slint::invoke_from_event_loop(|| {
                                    let _ = slint::quit_event_loop();
                                });
                            }
                            Ok(Err(error)) => show_error(weak, error),
                            Err(_) => show_error(
                                weak,
                                chatter_i18n::tr("Could not restart into the verified update"),
                            ),
                        }
                    });
                }
            }
            return;
        }
        if global.get_state() == "downloading" {
            return;
        }
        let Some(manifest_before_download) = client.available_update() else {
            crate::rt_handle().spawn(check(client, weak, true, play, operations.clone()));
            return;
        };
        #[cfg(target_os = "android")]
        if manifest_before_download.size > 20 * 1024 * 1024
            && global.get_state() != "confirm-cellular"
        {
            match crate::platform::android::activity::update_network_metered() {
                Ok(true) => {
                    global.set_state("confirm-cellular".into());
                    global.set_notice(
                        chatter_i18n::tr(
                            "Large download on cellular. Tap Download now to continue.",
                        )
                        .into(),
                    );
                    return;
                }
                Ok(false) => {}
                Err(error) => {
                    show_error(weak, error);
                    return;
                }
            }
        }
        let retry = global.get_state() == "error";
        global.set_version(manifest_before_download.version_name.clone().into());
        global.set_state("downloading".into());
        tracing::info!(version = %manifest_before_download.version_name, size = manifest_before_download.size, retry, "update download requested");
        global.set_error("".into());
        global.set_notice("".into());
        global.set_progress(chatter_i18n::tr("Downloading and verifying…").into());
        let size = manifest_before_download.size;
        let download_manifest = manifest_before_download;
        let download_operations = operations.clone();
        crate::rt_handle().spawn(async move {
            let _operation = download_operations.lock_owned().await;
            if client.available_update().as_ref() != Some(&download_manifest) {
                let latest = client.available_update();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(window) = weak.upgrade() {
                        let global = window.global::<UpdateGlobal>();
                        if let Some(manifest) = latest {
                            global.set_version(manifest.version_name.into());
                            global.set_state("available".into());
                            global.set_notice(chatter_i18n::tr("Update information changed. Tap Update to continue.").into());
                        } else { global.set_state("idle".into()); }
                    }
                });
                return;
            }
            let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<u64>();
            let progress_window = weak.clone();
            let progress_task = tokio::spawn(async move {
                while let Some(bytes) = receiver.recv().await {
                    let percent = bytes.saturating_mul(100) / size;
                    let label = chatter_i18n::trf("Downloading {}%", &[&percent]);
                    let weak = progress_window.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(window) = weak.upgrade() {
                            window.global::<UpdateGlobal>().set_progress(label.into());
                        }
                    });
                }
            });
            let result = client
                .download_available_update_with_progress(&|bytes| {
                    let _ = sender.send(bytes);
                })
                .await;
            drop(sender);
            progress_task.abort();
            match result {
                Ok(path) => {
                    #[cfg(target_os = "android")]
                    {
                        let install = update::android_handoff_version_code(&path, &download_manifest)
                            .and_then(|code| crate::platform::android::activity::install_verified_update(
                                &path.to_string_lossy(), code,
                            ));
                        match install {
                            Ok(()) => {
                                tracing::info!("Android update installer handoff");
                                let _ = slint::invoke_from_event_loop(move || {
                                    if let Some(window) = weak.upgrade() {
                                        window
                                            .global::<UpdateGlobal>()
                                            .set_state("installing".into());
                                    }
                                });
                            }
                            Err(error) => { tracing::warn!(kind = %error, "Android update installer handoff failed"); show_error(weak, error) },
                        }
                    }
                    #[cfg(target_os = "linux")]
                    {
                        let data = client.directories().data_dir().to_path_buf();
                        match tokio::task::spawn_blocking(move || {
                            update::stage_linux_archive(&path, &download_manifest, &data)
                        })
                        .await
                        {
                            Ok(Ok(executable)) => {
                                if let Ok(mut guard) = staged.lock() {
                                    *guard = Some(executable);
                                }
                                let _ = slint::invoke_from_event_loop(move || {
                                    if let Some(window) = weak.upgrade() {
                                        window.global::<UpdateGlobal>().set_state("ready".into());
                                    }
                                });
                            }
                            Ok(Err(error)) => { tracing::warn!(kind = %error, "Linux update staging failed"); show_error(weak, error) },
                            Err(_) => {
                                show_error(weak, chatter_i18n::tr("Could not stage Linux update"))
                            }
                        }
                    }
                }
                Err(error) => { tracing::warn!(kind = %error, "update download failed"); show_error(weak, error) },
            }
        });
    });
}
