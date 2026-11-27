//! App lock glue: renders `AppLockPane` (ui/app_lock.slint) from
//! `ChatterClient` and forwards its callbacks. Core decides everything —
//! whether the app is locked, PIN checks, backoff, the duress wipe.

use std::sync::Arc;

use slint::ComponentHandle;

use chatter_core::client::{AppLockError, AppUnlock, ChatterClient, LockTimeout};

use crate::{AppLockPane, MainWindow};

pub(crate) fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    refresh(window, ui_ctx);
    let pane = window.global::<AppLockPane>();

    {
        let weak = window.as_weak();
        let ui_ctx = ui_ctx.clone();
        pane.on_lock_now(move || {
            let Some(w) = weak.upgrade() else { return };
            ui_ctx.lock_app_now();
            clear_inputs(&w.global::<AppLockPane>());
            refresh(&w, &ui_ctx);
        });
    }

    {
        let weak = window.as_weak();
        let ui_ctx = ui_ctx.clone();
        pane.on_unlock(move |pin| {
            let Some(w) = weak.upgrade() else { return };
            let pane = w.global::<AppLockPane>();
            if pane.get_verifying() {
                return;
            }
            pane.set_verifying(true);
            pane.set_error("".into());
            let pin = pin.to_string();
            let weak = weak.clone();
            let ui_ctx = ui_ctx.clone();
            tokio::spawn(async move {
                let result = ui_ctx.unlock_app(pin).await;
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(w) = weak.upgrade() else { return };
                    let pane = w.global::<AppLockPane>();
                    match result {
                        // The wipe already ran; the screen keeps showing the
                        // same "checking" state until the relaunch.
                        Ok(AppUnlock::Wiped) => restart_after_wipe(),
                        Ok(AppUnlock::Unlocked) => {
                            pane.set_pin("".into());
                            pane.set_verifying(false);
                            refresh(&w, &ui_ctx);
                        }
                        Ok(AppUnlock::Wrong { retry_after_secs }) => {
                            pane.set_pin("".into());
                            pane.set_verifying(false);
                            pane.set_error(wrong_message(retry_after_secs).into());
                        }
                        Ok(AppUnlock::Throttled { retry_after_secs }) => {
                            pane.set_verifying(false);
                            pane.set_error(wait_message(retry_after_secs).into());
                        }
                        Err(error) => {
                            pane.set_verifying(false);
                            pane.set_error(error.to_string().into());
                        }
                    }
                });
            });
        });
    }

    {
        let weak = window.as_weak();
        let ui_ctx = ui_ctx.clone();
        pane.on_enable(move |pin, confirm| {
            let Some(w) = weak.upgrade() else { return };
            let (ctx, pin, confirm) = (ui_ctx.clone(), pin.to_string(), confirm.to_string());
            run(&w, &ui_ctx, async move {
                ctx.enable_app_lock(None, pin, confirm, LockTimeout::OneMinute)
                    .await
                    .map(|()| chatter_i18n::tr("App lock is on."))
            });
        });
    }
    {
        let weak = window.as_weak();
        let ui_ctx = ui_ctx.clone();
        pane.on_disable(move |current| {
            let Some(w) = weak.upgrade() else { return };
            let (ctx, current) = (ui_ctx.clone(), current.to_string());
            run(&w, &ui_ctx, async move {
                ctx.disable_app_lock(current)
                    .await
                    .map(|()| chatter_i18n::tr("App lock is off."))
            });
        });
    }
    {
        let weak = window.as_weak();
        let ui_ctx = ui_ctx.clone();
        pane.on_set_duress(move |current, duress, confirm| {
            let Some(w) = weak.upgrade() else { return };
            let ctx = ui_ctx.clone();
            let (current, duress, confirm) =
                (current.to_string(), duress.to_string(), confirm.to_string());
            run(&w, &ui_ctx, async move {
                ctx.set_duress_pin(current, duress, confirm)
                    .await
                    .map(|()| chatter_i18n::tr("Duress PIN set."))
            });
        });
    }
    {
        let weak = window.as_weak();
        let ui_ctx = ui_ctx.clone();
        pane.on_clear_duress(move |current| {
            let Some(w) = weak.upgrade() else { return };
            let (ctx, current) = (ui_ctx.clone(), current.to_string());
            run(&w, &ui_ctx, async move {
                ctx.clear_duress_pin(current)
                    .await
                    .map(|()| chatter_i18n::tr("Duress PIN removed."))
            });
        });
    }

    {
        let weak = window.as_weak();
        let ui_ctx = ui_ctx.clone();
        pane.on_timeout_selected(move |key| {
            let Some(w) = weak.upgrade() else { return };
            let Some(timeout) = LockTimeout::from_key(&key) else { return };
            if let Err(error) = ui_ctx.set_app_lock_timeout(timeout) {
                w.global::<AppLockPane>()
                    .set_settings_error(error.to_string().into());
            }
            refresh(&w, &ui_ctx);
        });
    }
}

/// One settings action: busy while core works, then message or error.
fn run(
    window: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
    action: impl std::future::Future<Output = Result<String, AppLockError>> + Send + 'static,
) {
    let pane = window.global::<AppLockPane>();
    if pane.get_busy() {
        return;
    }
    pane.set_busy(true);
    pane.set_settings_error("".into());
    pane.set_settings_message("".into());
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();
    tokio::spawn(async move {
        let result = action.await;
        let _ = slint::invoke_from_event_loop(move || {
            let Some(w) = weak.upgrade() else { return };
            let pane = w.global::<AppLockPane>();
            pane.set_busy(false);
            match result {
                Ok(message) => {
                    clear_inputs(&pane);
                    pane.set_settings_message(message.into());
                }
                Err(error) => pane.set_settings_error(error.to_string().into()),
            }
            refresh(&w, &ui_ctx);
        });
    });
}

fn clear_inputs(pane: &AppLockPane<'_>) {
    for set in [
        AppLockPane::set_new_pin,
        AppLockPane::set_confirm_pin,
        AppLockPane::set_current_pin,
        AppLockPane::set_duress_pin,
        AppLockPane::set_duress_confirm,
    ] {
        set(pane, "".into());
    }
}

/// Re-read lock state and settings from core. Call on every lifecycle edge
/// (focus, Android resume/pause) so the lock screen covers the app as soon
/// as core says it is locked.
pub(crate) fn refresh(window: &MainWindow, ui_ctx: &ChatterClient) {
    let pane = window.global::<AppLockPane>();
    let locked = ui_ctx.app_lock_is_locked();
    pane.set_locked(locked);
    // Locking removes the chats from the system share sheet; unlocking restores them.
    #[cfg(target_os = "android")]
    crate::platform::android::share_targets::refresh_current();
    // The UnifiedPush consent prompt is answerable only while unlocked.
    #[cfg(target_os = "android")]
    crate::platform::android::system_settings::prepare_unified_push_prompt(!locked);
    match ui_ctx.app_lock_settings() {
        Ok(settings) => {
            pane.set_enabled(settings.enabled);
            pane.set_timeout(settings.timeout.key().into());
            pane.set_duress_enabled(settings.duress_enabled);
        }
        Err(error) => tracing::warn!(%error, "could not read app lock settings"),
    }
}

fn wrong_message(retry_after_secs: u64) -> String {
    if retry_after_secs == 0 {
        chatter_i18n::tr("Wrong PIN")
    } else if retry_after_secs >= 120 {
        let minutes = retry_after_secs.div_ceil(60);
        chatter_i18n::trf("Wrong PIN. Try again in {} minutes.", &[&minutes])
    } else {
        chatter_i18n::trf("Wrong PIN. Try again in {} seconds.", &[&retry_after_secs])
    }
}

fn wait_message(secs: u64) -> String {
    if secs >= 120 {
        let minutes = secs.div_ceil(60);
        chatter_i18n::trf("Try again in {} minutes.", &[&minutes])
    } else {
        chatter_i18n::trf("Try again in {} seconds.", &[&secs])
    }
}

/// After the duress wipe: relaunch into the fresh-install screen.
fn restart_after_wipe() {
    #[cfg(target_os = "android")]
    {
        // Kotlin cancels notifications, deletes the Keystore aliases and
        // SharedPreferences, relaunches the activity and ends this process.
        if let Err(error) = crate::platform::android::activity::wipe_and_restart() {
            tracing::error!(%error, "wipe relaunch failed; exiting");
        }
        unsafe extern "C" {
            fn _exit(status: std::ffi::c_int) -> !;
        }
        unsafe { _exit(0) }
    }
    #[cfg(not(target_os = "android"))]
    {
        if let Ok(exe) = std::env::current_exe() {
            if let Err(error) = std::process::Command::new(exe)
                .args(std::env::args_os().skip(1))
                .spawn()
            {
                tracing::error!(%error, "could not relaunch after the wipe");
            }
        }
        std::process::exit(0);
    }
}
