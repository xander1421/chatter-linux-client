//! Thin presentation wiring for the signed core update service.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

async fn check(client: Arc<ChatterClient>, weak: Weak<MainWindow>, manual: bool) {
    if manual {
        let loading = weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(window) = loading.upgrade() {
                let global = window.global::<UpdateGlobal>();
                global.set_state("checking".into());
                global.set_error("".into());
                global.set_notice("".into());
            }
        });
    }
    match client.check_available_update().await {
        Ok(found) => {
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak.upgrade() {
                    let global = window.global::<UpdateGlobal>();
                    if let Some(manifest) = found {
                        global.set_version(manifest.version_name.into());
                        global.set_state("available".into());
                    } else {
                        global.set_state("idle".into());
                        if manual {
                            global.set_notice("You're up to date".into());
                        }
                    }
                }
            });
        }
        Err(error) => {
            if manual {
                show_error(weak, error);
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
    _this: jni::objects::JObject,
    reason: jni::objects::JString,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let reason: String = reason.try_to_string(&env)?.into();
        let weak = INSTALL_WINDOW.lock().ok().and_then(|guard| guard.clone());
        if let Some(weak) = weak {
            let reason = if reason.is_empty() { "Update install failed".to_string() } else { reason };
            show_error(weak, format!("System installer refused the update: {reason}"));
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
    global.set_play_distribution(update::channel() == "play");

    let manual_client = client.clone();
    let manual_window = window.as_weak();
    global.on_check(move || {
        crate::rt_handle().spawn(check(manual_client.clone(), manual_window.clone(), true));
    });

    let periodic_client = Arc::downgrade(client);
    let periodic_window = window.as_weak();
    crate::rt_handle().spawn(async move {
        // Give the launch UI time to restore its selected Shield transport.
        tokio::time::sleep(Duration::from_secs(5)).await;
        loop {
            let Some(client) = periodic_client.upgrade() else {
                break;
            };
            check(client, periodic_window.clone(), false).await;
            let jitter = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|time| time.subsec_nanos() as u64 % 1800)
                .unwrap_or(0);
            tokio::time::sleep(Duration::from_secs(4 * 3600 + jitter)).await;
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
        if update::channel() == "play" {
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
                    match std::process::Command::new(path).spawn() {
                        Ok(_) => {
                            let _ = slint::quit_event_loop();
                        }
                        Err(_) => {
                            show_error(weak, "Could not restart into the verified update".into())
                        }
                    }
                }
            }
            return;
        }
        if global.get_state() == "downloading" {
            return;
        }
        let Some(manifest_before_download) = client.available_update() else {
            show_error(weak, "No verified update is available".into());
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
                        "Large download on cellular. Tap Download now to continue.".into(),
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
        global.set_state("downloading".into());
        global.set_error("".into());
        global.set_notice("".into());
        global.set_progress("Downloading and verifying…".into());
        let size = manifest_before_download.size;
        let download_manifest = manifest_before_download;
        crate::rt_handle().spawn(async move {
            let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<u64>();
            let progress_window = weak.clone();
            let progress_task = tokio::spawn(async move {
                while let Some(bytes) = receiver.recv().await {
                    let label = format!("Downloading {}%", bytes.saturating_mul(100) / size);
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
                        let install = crate::platform::android::activity::install_verified_update(
                            &path.to_string_lossy(),
                        );
                        match install {
                            Ok(()) => {
                                let _ = slint::invoke_from_event_loop(move || {
                                    if let Some(window) = weak.upgrade() {
                                        window
                                            .global::<UpdateGlobal>()
                                            .set_state("installing".into());
                                    }
                                });
                            }
                            Err(error) => show_error(weak, error),
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
                            Ok(Err(error)) => show_error(weak, error),
                            Err(_) => show_error(weak, "Could not stage Linux update".into()),
                        }
                    }
                }
                Err(error) => show_error(weak, error),
            }
        });
    });
}
